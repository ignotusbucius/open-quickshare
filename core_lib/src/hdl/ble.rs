use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::anyhow;
use btleplug::api::{AddressType, Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::{Adapter, Manager};
use futures::stream::StreamExt;
use tokio::sync::broadcast::Sender;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::{Uuid, uuid};

const SERVICE_UUID_SHARING: Uuid = uuid!("0000fe2c-0000-1000-8000-00805f9b34fb");

const INNER_NAME: &str = "BleListener";

// The radio cannot scan and advertise at the same time.
//
// With LL privacy enabled (BlueZ's KernelExperimental UUID
// 15c0a148-c273-11ea-b3de-0242ac130004) the kernel pauses *every* advertising
// instance for as long as an active scan is running, because active scanning
// turns off address resolution and RPA generation depends on it. Even without
// it, one antenna time-shares scanning against advertising and against any live
// connection.
//
// Scanning back-to-back therefore keeps us permanently off the air as a Quick
// Share receiver: the phone lists us from a cached advertisement but has no
// window to connect into. So duty-cycle instead. A phone that is sharing
// repeats its 0xFE2C beacon for as long as its share sheet is open, so a short
// window every few seconds still catches it well within the 10s alert rate
// limit below.
const SCAN_WINDOW: Duration = Duration::from_secs(3);
const SCAN_PAUSE: Duration = Duration::from_secs(7);
/// Hard cap on any single D-Bus round-trip. The BLE D-Bus client machinery
/// wedged silently once, taking the whole stack quiet with it -- an unbounded
/// await here would freeze the listener loop for good.
const DBUS_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Quick Share seems to emit an LE advert every 10 seconds, so don't alert more
/// often than that.
const ALERT_RATE_LIMIT: Duration = Duration::from_secs(10);

/// Number of things currently asking us to stay off the air (an in-progress
/// GATT session, an outgoing-share advertisement, ...).
static SCAN_SUPPRESSORS: AtomicUsize = AtomicUsize::new(0);

/// Suppresses BLE scanning for as long as it is held, so that the advertising
/// and connection work the radio is busy with gets the whole antenna.
#[derive(Debug)]
pub struct BleScanSuppressor(());

impl BleScanSuppressor {
    pub fn new() -> Self {
        SCAN_SUPPRESSORS.fetch_add(1, Ordering::SeqCst);
        Self(())
    }
}

impl Default for BleScanSuppressor {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BleScanSuppressor {
    fn drop(&mut self) {
        SCAN_SUPPRESSORS.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(crate) fn scanning_suppressed() -> bool {
    SCAN_SUPPRESSORS.load(Ordering::SeqCst) > 0
}

pub struct BleListener {
    adapter: Adapter,
    sender: Sender<()>,
}

impl BleListener {
    pub async fn new(sender: Sender<()>) -> Result<Self, anyhow::Error> {
        let manager = Manager::new().await?;
        let adapters = manager.adapters().await?;
        if adapters.is_empty() {
            return Err(anyhow!("no bluetooth adapter"));
        }

        Ok(Self {
            adapter: adapters[0].clone(),
            sender,
        })
    }

    pub async fn run(self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        info!("{INNER_NAME}: service starting");

        let mut events = self.adapter.events().await?;
        // Filter on the NearyShare/QuickShare services UUID

        // Not using the ScanFilter here to filter out advertisements
        // not matching the Nearby Share service UUID, it seems to
        // exclude Nearby Share advertisements despite its UUID being
        // in the filter.
        //
        // Perhaps broken?
        //
        // ...The filtering is being done only here now.

        let mut last_alert: SystemTime = SystemTime::UNIX_EPOCH;
        let mut scanning = false;
        // Fires immediately, which opens the first scan window.
        let mut phase_deadline = Instant::now();

        loop {
            // While a window is open, wake early every 500ms so a suppressor
            // appearing mid-window (a slot-0 fetch or weave session starting)
            // aborts the scan immediately instead of at the window's end.
            let wake = if scanning {
                phase_deadline.min(Instant::now() + Duration::from_millis(500))
            } else {
                phase_deadline
            };
            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("{INNER_NAME}: tracker cancelled, breaking");
                    break;
                }
                _ = tokio::time::sleep_until(wake) => {
                    if scanning && Instant::now() < phase_deadline && !scanning_suppressed() {
                        // Mid-window early check: nothing changed, keep going.
                        continue;
                    }
                    if scanning {
                        match tokio::time::timeout(DBUS_CALL_TIMEOUT, self.adapter.stop_scan()).await {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => debug!("{INNER_NAME}: couldn't stop the scan: {e}"),
                            Err(_) => debug!("{INNER_NAME}: stop-scan timed out"),
                        }
                        scanning = false;
                        phase_deadline = Instant::now() + SCAN_PAUSE;
                    } else if scanning_suppressed() {
                        // Something else needs the radio; skip this window.
                        phase_deadline = Instant::now() + SCAN_PAUSE;
                    } else if self.phone_connected().await {
                        // A phone is connected to us -- almost certainly a
                        // Quick Share GATT fetch or transfer in flight. Scan
                        // windows were measured stretching its ATT round-trips
                        // from ~30ms to ~370ms, enough to blow the phone-side
                        // fetch timeout, so stay off the air until it's done.
                        phase_deadline = Instant::now() + SCAN_PAUSE;
                    } else {
                        match tokio::time::timeout(
                            DBUS_CALL_TIMEOUT,
                            self.adapter.start_scan(ScanFilter::default()),
                        )
                        .await
                        {
                            Ok(Ok(())) => {
                                scanning = true;
                                phase_deadline = Instant::now() + SCAN_WINDOW;
                            }
                            Ok(Err(e)) => {
                                warn!("{INNER_NAME}: couldn't start the scan: {e}");
                                phase_deadline = Instant::now() + SCAN_PAUSE;
                            }
                            Err(_) => {
                                warn!("{INNER_NAME}: start-scan timed out");
                                phase_deadline = Instant::now() + SCAN_PAUSE;
                            }
                        }
                    }
                }
                Some(e) = events.next() => {
                    match e {
                        CentralEvent::ServiceDataAdvertisement { id, service_data } => {
                            // Sanity check as per: https://github.com/Martichou/rquickshare/issues/74
                            // Seems like the filtering is not enough, so we'll add a check before
                            // proceeding with the service_data.
                            if let Some(service_data) = service_data.get(&SERVICE_UUID_SHARING) {
                                if self.alert(&mut last_alert) {
                                    debug!("{INNER_NAME}: A device ({id}) is sharing ({}) nearby", hex::encode(service_data));
                                }
                            }
                        },
                        CentralEvent::DeviceDiscovered(id) => {
                            // BlueZ reports the first advertisement of a device
                            // it isn't already caching through InterfacesAdded,
                            // which btleplug turns into DeviceDiscovered and
                            // *not* into a ServiceDataAdvertisement. Every scan
                            // window that follows a cache eviction would
                            // otherwise drop its first beacon, so read the
                            // service data off the peripheral instead.
                            let Ok(peripheral) = self.adapter.peripheral(&id).await else {
                                continue;
                            };
                            let Ok(Some(props)) = peripheral.properties().await else {
                                continue;
                            };
                            if let Some(service_data) = props.service_data.get(&SERVICE_UUID_SHARING) {
                                if self.alert(&mut last_alert) {
                                    debug!("{INNER_NAME}: A device ({id}) is sharing ({}) nearby", hex::encode(service_data));
                                }
                            }
                        },
                        // Not interesting for us
                        _ => {
                            // trace!("{INNER_NAME}: Another CentralEvent got the same services: {:?}", e);
                        }
                    }
                }
            }
        }

        if scanning {
            let _ = self.adapter.stop_scan().await;
        }

        Ok(())
    }

    /// Is a phone currently connected to us over LE?
    ///
    /// Phones use resolvable private addresses: type Random with the top two
    /// bits of the most significant octet reading 0b01. Static-random gadgets
    /// (mice, pads) read 0b11 there and public-address devices are excluded by
    /// the type, so neither keeps this true while idle-connected.
    async fn phone_connected(&self) -> bool {
        // Bounded as a whole: on a wedged D-Bus round-trip, "no phone" and a
        // normal scan window beat freezing the listener loop forever.
        tokio::time::timeout(DBUS_CALL_TIMEOUT, self.phone_connected_inner())
            .await
            .unwrap_or(false)
    }

    async fn phone_connected_inner(&self) -> bool {
        let Ok(peripherals) = self.adapter.peripherals().await else {
            return false;
        };
        for peripheral in peripherals {
            if !matches!(peripheral.is_connected().await, Ok(true)) {
                continue;
            }
            let Ok(Some(props)) = peripheral.properties().await else {
                continue;
            };
            if props.address_type == Some(AddressType::Random)
                && props.address.into_inner()[0] & 0xC0 == 0x40
            {
                return true;
            }
        }
        false
    }

    /// Pokes the mDNS server so it re-announces us, at most once per
    /// [`ALERT_RATE_LIMIT`]. Returns whether the alert was actually sent.
    fn alert(&self, last_alert: &mut SystemTime) -> bool {
        let now = SystemTime::now();
        // A clock that went backwards shouldn't wedge the listener, so treat an
        // un-orderable pair of timestamps as "long enough ago".
        if matches!(now.duration_since(*last_alert), Ok(d) if d <= ALERT_RATE_LIMIT) {
            return false;
        }

        *last_alert = now;
        // The only receiver is the mDNS server; if it's gone there's nothing to
        // poke, but that's no reason to tear the listener down.
        self.sender.send(()).is_ok()
    }
}
