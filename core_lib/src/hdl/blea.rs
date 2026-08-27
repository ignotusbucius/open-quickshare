use std::sync::Arc;
use std::time::Duration;

use bluer::UuidExt;
use bluer::adv::Advertisement;
use bytes::Bytes;
use once_cell::sync::Lazy;
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::hdl::{BleScanSuppressor, Visibility, scanning_suppressed};

/// Rings when a connection has consumed the receiver advertisement and the
/// link is gone, so [`ReceiverAdvertiser`] should put a fresh one on the air.
/// See [`request_advert_cycle`].
static ADV_CYCLE: Lazy<Notify> = Lazy::new(Notify::new);

/// Ask the [`ReceiverAdvertiser`] to drop its current registration and put up
/// a fresh one.
///
/// The GATT server calls this after a weave session's peer disconnects: a
/// connectable advertising set is consumed by the connection that opened the
/// session, and BlueZ gives us no signal of that -- `ActiveInstances` keeps
/// counting the registration while the controller no longer broadcasts it.
/// The session itself is the only reliable evidence.
pub(crate) fn request_advert_cycle() {
    ADV_CYCLE.notify_one();
}

const SERVICE_DATA: Bytes = Bytes::from_static(&[
    252, 18, 142, 1, 66, 0, 0, 0, 0, 0, 0, 0, 0, 0, 191, 45, 91, 160, 225, 216, 117, 36, 202, 0,
]);

const INNER_NAME: &str = "BleAdvertiser";

#[derive(Debug, Clone)]
pub struct BleAdvertiser {
    adapter: Arc<bluer::Adapter>,
}

impl BleAdvertiser {
    pub async fn new() -> Result<Self, anyhow::Error> {
        let session = bluer::Session::new().await?;
        let adapter = session.default_adapter().await?;
        adapter.set_powered(true).await?;

        Ok(Self {
            adapter: Arc::new(adapter),
        })
    }

    pub async fn run(&self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        info!(
            "{INNER_NAME}: advertising on Bluetooth adapter {} with address {}",
            self.adapter.name(),
            self.adapter.address().await?
        );

        // Nothing needs the BLE scanner while we're the one sharing -- targets
        // are found over mDNS -- and scanning would take airtime away from this
        // advertisement and pause it outright under LL privacy.
        let _suppressor = BleScanSuppressor::new();

        let service_uuid = Uuid::from_u16(0xFE2C);
        let handle = self
            .adapter
            .advertise(self.get_advertisement(service_uuid, SERVICE_DATA))
            .await?;
        ctk.cancelled().await;
        info!("{INNER_NAME}: tracker cancelled, returning");
        drop(handle);

        Ok(())
    }

    fn get_advertisement(&self, service_uuid: Uuid, adv_data: Bytes) -> Advertisement {
        Advertisement {
            advertisement_type: bluer::adv::Type::Broadcast,
            service_data: [(service_uuid, adv_data.into())].into(),
            ..Default::default()
        }
    }
}

// ----------------------------------------------------------------------------
// QuickShare *receiver* discovery over BLE (service UUID 0xFEF3).
//
// Unlike `BleAdvertiser` (which emits the 0xFE2C "I want to send" FastInit
// beacon during the discovery/send phase), this advertises the device as a
// discoverable Nearby Connections *endpoint* so a phone that has dropped off
// Wi-Fi during its browse phase (the Pixel "AirDrop update" behaviour) can
// still list us as a target. Once the user selects us, the phone reconnects to
// Wi-Fi and the transfer completes over the normal Wi-Fi-LAN (mDNS + TCP) path,
// which is why the advertised endpoint_id MUST match the one used by MDnsServer.
//
// The byte layout was reverse-engineered from a live capture of a Pixel 9 Pro
// advertising in "Everyone" mode. The identity/MAC/trailing-presence segments
// are reused verbatim from that capture (they are not validated for discovery);
// only the endpoint_id, device name and the length fields vary.
const RX_INNER_NAME: &str = "ReceiverAdvertiser";
const QS_SERVICE_UUID: u16 = 0xFEF3;
// 3-byte hash of the "NearbySharing" service id (matches the mDNS _FC9F5ED42C8A type).
const QS_SVC_HASH: [u8; 3] = [0xfc, 0x9f, 0x5e];
// endpoint_info identity bytes come from crate::utils::ENDPOINT_IDENTITY so
// the BLE advertisement and the mDNS TXT record present the same identity and
// the phone merges them into one share target.
// Connections advertisement trailer after endpoint_info: bluetooth MAC(6) + extra(2).
//
// The MAC must stay all-zero. It is the `bluetooth_mac_address` field of the
// Nearby Connections advertisement, and a phone that reads a valid-looking
// address there will prefer BR/EDR over BLE and try to open an RFCOMM channel
// to it before anything else. We don't run an RFCOMM listener, so whatever we
// put here is a dead end the phone has to time out on -- once against the
// address that was captured from the Pixel this layout was derived from, and
// once against our own adapter if we advertised that instead. Nearby's own
// encoder writes zeros when Bluetooth isn't available and its parser rejects
// an all-zero address, which is exactly the "skip BR/EDR, go straight to BLE"
// answer we want.
const QS_CONN_MAC_EXTRA: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
// Random per-run device token carried in the mediums advertisement trailer.
static DEVICE_TOKEN: Lazy<[u8; 2]> = Lazy::new(|| {
    let mut token = [0u8; 2];
    rand::rng().fill_bytes(&mut token);
    token
});

/// Build the 0xFEF3 service data advertising this device as a QuickShare
/// receiver endpoint. `endpoint_id` must be the same 4 bytes used by MDnsServer.
pub fn receiver_service_data(
    endpoint_id: [u8; 4],
    device_type: u8,
    device_name: &str,
    l2cap_psm: Option<u16>,
) -> Vec<u8> {
    // Inner Nearby Share application advertisement (== the mDNS "n" TXT record):
    //   1B header: version(3b)=1 | visibility(1b)=0(visible) | device_type(3b) | reserved
    //   16B identity (2B salt + 14B metadata-key hash)
    //   1B name length + UTF-8 name (plaintext, since we're visible to "Everyone")
    let mut einfo: Vec<u8> = Vec::new();
    // Byte-identical to the mDNS TXT "n" record (see gen_mdns_endpoint_info):
    // version 0, visible, same shared identity, same name. Any differing byte
    // makes the phone list the BLE- and mDNS-discovered endpoints as two
    // separate devices.
    einfo.push((device_type & 0x7) << 1);
    einfo.extend_from_slice(crate::utils::ENDPOINT_IDENTITY.as_slice());
    let mut name = device_name.as_bytes().to_vec();
    name.truncate(255);
    einfo.push(name.len() as u8);
    einfo.extend_from_slice(&name);

    // Nearby Connections offline BLE advertisement carrying the endpoint id + info.
    let mut data: Vec<u8> = Vec::new();
    data.push(0x23); // version(3b)=1 | pcp(5b)
    data.extend_from_slice(&QS_SVC_HASH);
    data.extend_from_slice(&endpoint_id);
    data.push(einfo.len() as u8);
    data.extend_from_slice(&einfo);
    data.extend_from_slice(&QS_CONN_MAC_EXTRA);

    // Mediums BLE advertisement wrapper. Trailer: device_token(2), then the
    // extra-fields bitmask -- bit 0 carries our L2CAP PSM so phones connect
    // straight to the CoC socket and skip GATT discovery entirely. (The old
    // build copied the capture's 69 trailing bytes verbatim, which advertised
    // the *Pixel's* PSM and instant-connection blob: phones were being sent
    // to an L2CAP port nobody listened on.)
    let mut sd: Vec<u8> = Vec::new();
    sd.push(0x48); // version(3b)=2 | socket_version(3b)=2 | fast(1b)=0 | reserved
    sd.extend_from_slice(&QS_SVC_HASH);
    sd.extend_from_slice(&(data.len() as u32).to_be_bytes());
    sd.extend_from_slice(&data);
    sd.extend_from_slice(DEVICE_TOKEN.as_slice());
    match l2cap_psm {
        Some(psm) => {
            sd.push(0x01); // extra fields: PSM present
            sd.extend_from_slice(&psm.to_be_bytes());
        }
        None => sd.push(0x00), // extra fields: none
    }
    sd
}

// ----------------------------------------------------------------------------
// Mediums BLE advertisement *header* (Nearby Connections BLE v2).
//
// Our full receiver advertisement is ~121 bytes -- far past the 31-byte legacy
// BLE payload limit, so BlueZ can only emit it as an *extended* advertisement.
// Phones can see an extended advert but connecting into one (chasing the
// AUX_ADV_IND on a secondary channel) is flaky, which is the "Connecting..."
// roulette.
//
// Nearby's actual design for this is the two-tier advertisement: put a tiny
// fixed-size *header* on the air (fits legacy), and serve the full payload from
// a GATT characteristic ("slot 0"). A scanning phone reads the header, checks
// its Bloom filter for a service id it cares about, and if interested connects
// and reads the real advertisement from slot 0 -- the characteristic this GATT
// server already exposes at `00000000-0000-3000-8000-000000000000`.
//
// Layout (see google/nearby ble_advertisement_header.cc), 15 bytes:
//   [1] version(3b)=2 | extended(1b)=0 | num_slots(4b)=1   => 0x41
//   [10] service-id Bloom filter
//   [4] advertisement hash (dedup key only; never verified against slot 0)
// An optional 2-byte PSM may follow; we omit it (PSM 0 => no L2CAP, GATT read).

/// The service id a Nearby Share receiver filters on. Its SHA-256 prefix is the
/// `fc9f5e` service hash used throughout this crate.
const NEARBY_SERVICE_ID: &[u8] = b"NearbySharing";
/// version=kV2(2)<<5 | extended-bit slot | num_slots=1. One GATT advertisement
/// slot; bit 4 says "the full advertisement is also on the air as an extended
/// advertisement", which lets extended-capable phones skip the GATT fetch.
const HEADER_VERSION_BYTE: u8 = (2 << 5) | (1 & 0x0f);
const HEADER_EXTENDED_BIT: u8 = 0x10;
const BLOOM_FILTER_BYTES: usize = 10;
const ADVERTISEMENT_HASH_BYTES: usize = 4;
/// Length of the anonymising random service id the real client mixes in.
const DUMMY_SERVICE_ID_LEN: usize = 128;

/// MurmurHash3 x64_128 (canonical, aappleby/smhasher) -- the hash Nearby's
/// Bloom filter is built on. Only the low 64 bits of the 128-bit result are
/// used, so that is all we return.
fn murmur3_x64_128_low(data: &[u8]) -> u64 {
    const C1: u64 = 0x87c3_7b91_1142_53d5;
    const C2: u64 = 0x4cf5_ad43_2745_937f;
    let mut h1: u64 = 0;
    let mut h2: u64 = 0;

    let nblocks = data.len() / 16;
    for i in 0..nblocks {
        let b = i * 16;
        let mut k1 = u64::from_le_bytes(data[b..b + 8].try_into().unwrap());
        let mut k2 = u64::from_le_bytes(data[b + 8..b + 16].try_into().unwrap());

        k1 = k1.wrapping_mul(C1);
        k1 = k1.rotate_left(31);
        k1 = k1.wrapping_mul(C2);
        h1 ^= k1;
        h1 = h1.rotate_left(27);
        h1 = h1.wrapping_add(h2);
        h1 = h1.wrapping_mul(5).wrapping_add(0x52dce729);

        k2 = k2.wrapping_mul(C2);
        k2 = k2.rotate_left(33);
        k2 = k2.wrapping_mul(C1);
        h2 ^= k2;
        h2 = h2.rotate_left(31);
        h2 = h2.wrapping_add(h1);
        h2 = h2.wrapping_mul(5).wrapping_add(0x38495ab5);
    }

    // Tail -- mirrors the C switch's fall-through exactly.
    let t = &data[nblocks * 16..];
    let n = t.len();
    let mut k1: u64 = 0;
    let mut k2: u64 = 0;
    if n >= 15 { k2 ^= (t[14] as u64) << 48; }
    if n >= 14 { k2 ^= (t[13] as u64) << 40; }
    if n >= 13 { k2 ^= (t[12] as u64) << 32; }
    if n >= 12 { k2 ^= (t[11] as u64) << 24; }
    if n >= 11 { k2 ^= (t[10] as u64) << 16; }
    if n >= 10 { k2 ^= (t[9] as u64) << 8; }
    if n >= 9 {
        k2 ^= t[8] as u64;
        k2 = k2.wrapping_mul(C2);
        k2 = k2.rotate_left(33);
        k2 = k2.wrapping_mul(C1);
        h2 ^= k2;
    }
    if n >= 8 { k1 ^= (t[7] as u64) << 56; }
    if n >= 7 { k1 ^= (t[6] as u64) << 48; }
    if n >= 6 { k1 ^= (t[5] as u64) << 40; }
    if n >= 5 { k1 ^= (t[4] as u64) << 32; }
    if n >= 4 { k1 ^= (t[3] as u64) << 24; }
    if n >= 3 { k1 ^= (t[2] as u64) << 16; }
    if n >= 2 { k1 ^= (t[1] as u64) << 8; }
    if n >= 1 {
        k1 ^= t[0] as u64;
        k1 = k1.wrapping_mul(C1);
        k1 = k1.rotate_left(31);
        k1 = k1.wrapping_mul(C2);
        h1 ^= k1;
    }

    let len = data.len() as u64;
    h1 ^= len;
    h2 ^= len;
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    h1 = fmix64(h1);
    h2 = fmix64(h2);
    h1 = h1.wrapping_add(h2);
    // h2 is the high 64 bits; unused.
    h1
}

fn fmix64(mut k: u64) -> u64 {
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    k ^= k >> 33;
    k
}

/// The five bit positions an element sets/tests in an `nbits`-wide Bloom filter,
/// per Nearby's `BloomFilter::GetHashes` (5 reps over the murmur low-64 halves).
fn bloom_positions(element: &[u8], nbits: usize) -> [usize; 5] {
    let low = murmur3_x64_128_low(element);
    let hash1 = (low & 0xffff_ffff) as u32 as i32;
    let hash2 = ((low >> 32) & 0xffff_ffff) as u32 as i32;
    let mut out = [0usize; 5];
    for (idx, slot) in out.iter_mut().enumerate() {
        let i = (idx + 1) as i32;
        let mut combined = hash1.wrapping_add(i.wrapping_mul(hash2));
        // Flip the bits of a negative value to guarantee non-negative, exactly
        // as the C++ does before the `% size` (which then can't go haywire).
        if combined < 0 {
            combined = !combined;
        }
        *slot = (combined as usize) % nbits;
    }
    out
}

/// Builds the 10-byte service-id Bloom filter over the given elements. Bit
/// position p lives at `bytes[p / 8] |= 1 << (p % 8)` (Nearby's serialisation).
fn build_bloom_filter(elements: &[&[u8]]) -> [u8; BLOOM_FILTER_BYTES] {
    let nbits = BLOOM_FILTER_BYTES * 8;
    let mut bytes = [0u8; BLOOM_FILTER_BYTES];
    for element in elements {
        for pos in bloom_positions(element, nbits) {
            bytes[pos / 8] |= 1 << (pos % 8);
        }
    }
    bytes
}

fn sha256_prefix(data: &[u8], n: usize) -> Vec<u8> {
    Sha256::digest(data)[..n].to_vec()
}

/// Nearby's chained advertisement hash: seed with SHA-256 of the dummy service
/// id, then fold in each slot's advertisement. Truncated to 4 bytes. This is
/// only ever used as a cache/dedup key by scanners -- it is never checked
/// against the bytes actually served from slot 0 -- so its exact value doesn't
/// gate discovery; we still compute it faithfully.
fn advertisement_hash(dummy_service_id: &[u8], slots: &[&[u8]]) -> Vec<u8> {
    let mut hash = sha256_prefix(dummy_service_id, ADVERTISEMENT_HASH_BYTES);
    for slot in slots {
        let mut body = Vec::with_capacity(hash.len() + slot.len());
        body.extend_from_slice(&hash);
        body.extend_from_slice(slot);
        hash = sha256_prefix(&body, ADVERTISEMENT_HASH_BYTES);
    }
    hash
}

/// Builds the 15-byte legacy-sized advertisement header that points a scanning
/// phone at the full advertisement served from GATT slot 0 (`full_advert`).
///
/// The Bloom filter must answer "yes" for `PossiblyContains("NearbySharing")`
/// on the phone, or it discards us as uninteresting before ever reading slot 0;
/// the random dummy service id only anonymises the filter and never has to be
/// recovered. Computed once and kept stable for the lifetime of the advertiser
/// so the header's hash stays a consistent dedup key.
fn build_advertisement_header(
    full_advert: &[u8],
    extended_on_air: bool,
    l2cap_psm: Option<u16>,
) -> Vec<u8> {
    let mut dummy = [0u8; DUMMY_SERVICE_ID_LEN];
    rand::rng().fill_bytes(&mut dummy);

    let bloom = build_bloom_filter(&[&dummy, NEARBY_SERVICE_ID]);
    let hash = advertisement_hash(&dummy, &[full_advert]);

    let mut header = Vec::with_capacity(1 + BLOOM_FILTER_BYTES + ADVERTISEMENT_HASH_BYTES);
    let mut version_byte = HEADER_VERSION_BYTE;
    if extended_on_air {
        version_byte |= HEADER_EXTENDED_BIT;
    }
    header.push(version_byte);
    header.extend_from_slice(&bloom);
    header.extend_from_slice(&hash);
    if let Some(psm) = l2cap_psm {
        header.extend_from_slice(&psm.to_be_bytes());
    }
    header
}

#[derive(Debug, Clone)]
pub struct ReceiverAdvertiser {
    adapter: Arc<bluer::Adapter>,
    /// The advertising instances to keep on the air: `(label, service_data)`.
    /// In the default `dual` mode this is Google's own layout -- the full
    /// advertisement as an extended instance for modern phones (parsed straight
    /// off the air, no GATT fetch) plus the 15-byte legacy header, with its
    /// extended bit set, for phones that can't scan extended advertisements.
    payloads: Vec<(&'static str, Vec<u8>)>,
    /// Length of the slot-0 advertisement, for the startup log.
    slot0_len: usize,
    /// "dual", "header" or "full"; for logging only.
    mode: &'static str,
}

impl ReceiverAdvertiser {
    pub async fn new(
        endpoint_id: [u8; 4],
        device_type: u8,
        device_name: &str,
        l2cap_psm: Option<u16>,
    ) -> Result<Self, anyhow::Error> {
        let session = bluer::Session::new().await?;
        let adapter = session.default_adapter().await?;
        adapter.set_powered(true).await?;

        let full_advert = receiver_service_data(endpoint_id, device_type, device_name, l2cap_psm);
        // `PACKET_BLE_ADVERT` picks the layout: `dual` (default), `header`
        // (15-byte header only, everything via GATT fetch), or `full` (the
        // whole advertisement as one extended instance, the original layout).
        let mode_var = std::env::var("PACKET_BLE_ADVERT").unwrap_or_default();
        let (mode, payloads): (&'static str, Vec<(&'static str, Vec<u8>)>) =
            if mode_var.eq_ignore_ascii_case("full") {
                ("full", vec![("full", full_advert.clone())])
            } else if mode_var.eq_ignore_ascii_case("header") {
                ("header", vec![("header", build_advertisement_header(&full_advert, false, l2cap_psm))])
            } else {
                ("dual", vec![
                    ("full", full_advert.clone()),
                    ("header", build_advertisement_header(&full_advert, true, l2cap_psm)),
                ])
            };

        Ok(Self {
            adapter: Arc::new(adapter),
            payloads,
            slot0_len: full_advert.len(),
            mode,
        })
    }

    /// Backstop re-registration while idle. The event hooks (weave session
    /// end, slot-0 fetch end) cover the real consumption cases, and every
    /// refresh costs a sub-second off-air gap plus an on-lost flicker on
    /// phones, so this only guards against cases nothing else caught.
    const REFRESH: Duration = Duration::from_secs(180);
    /// How often the held registration is checked against the adapter's
    /// active-instance count while idle.
    const WATCH: Duration = Duration::from_secs(20);
    /// Hard cap on one RegisterAdvertisement round-trip. bluer's own D-Bus
    /// timeout is 120s, and a hung call parked the advertiser for good once --
    /// with zero instances on the air and no error logged.
    const REGISTER_TIMEOUT: Duration = Duration::from_secs(15);
    /// Delay before retrying a registration that outright failed.
    const RETRY: Duration = Duration::from_secs(3);
    /// Pause between dropping a consumed registration and registering anew.
    /// Generous on purpose: the unregisters run as spawned background tasks,
    /// and re-registering while they're still mid-flight on the same D-Bus
    /// session once raced the whole process into a silent lockup.
    const CYCLE_GRACE: Duration = Duration::from_millis(1500);

    pub async fn run(
        &self,
        mut visibility: watch::Receiver<Visibility>,
        ctk: CancellationToken,
    ) -> Result<(), anyhow::Error> {
        info!(
            "{RX_INNER_NAME}: advertising QuickShare receiver (0x{QS_SERVICE_UUID:04X}, mode={}, instances=[{}], {}-byte slot-0 advertisement) on adapter {} ({})",
            self.mode,
            self.payloads
                .iter()
                .map(|(name, data)| format!("{name}:{}B", data.len()))
                .collect::<Vec<_>>()
                .join(", "),
            self.slot0_len,
            self.adapter.name(),
            self.adapter.address().await?
        );

        loop {
            // Honour the visibility toggle. While "Hidden from everyone"
            // (Invisible), keep no advertisement on the air -- the same way
            // mDNS unregisters -- so the device is genuinely undiscoverable.
            // Idle here until the user turns visible again (or the tracker is
            // cancelled). This also covers starting up Invisible: nothing is
            // advertised until visibility flips.
            while *visibility.borrow_and_update() == Visibility::Invisible {
                info!("{RX_INNER_NAME}: visibility is Invisible; advertisement off");
                tokio::select! {
                    _ = ctk.cancelled() => {
                        info!("{RX_INNER_NAME}: tracker cancelled, returning");
                        return Ok(());
                    }
                    changed = visibility.changed() => {
                        // Err means the sender was dropped (app shutting down).
                        if changed.is_err() {
                            return Ok(());
                        }
                    }
                }
            }

            // Register, retrying failures (some controllers refuse a new
            // connectable set while a previous LE connection is still winding
            // down).
            let handles = loop {
                match self.register_all().await {
                    Ok(handles) => break handles,
                    Err(e) => {
                        warn!("{RX_INNER_NAME}: advertise failed ({e}); retrying");
                        tokio::select! {
                            _ = ctk.cancelled() => {
                                info!("{RX_INNER_NAME}: tracker cancelled, returning");
                                return Ok(());
                            }
                            _ = tokio::time::sleep(Self::RETRY) => {}
                        }
                    }
                }
            };

            // Hold the registration until there's a reason to replace it.
            let held_since = tokio::time::Instant::now();
            loop {
                tokio::select! {
                    _ = ctk.cancelled() => {
                        info!("{RX_INNER_NAME}: tracker cancelled, returning");
                        return Ok(());
                    }
                    _ = ADV_CYCLE.notified() => {
                        debug!("{RX_INNER_NAME}: advertisement consumed by a connection; cycling");
                        break;
                    }
                    changed = visibility.changed() => {
                        if changed.is_err() {
                            return Ok(());
                        }
                        if *visibility.borrow() == Visibility::Invisible {
                            info!("{RX_INNER_NAME}: visibility -> Invisible; taking the advertisement off the air");
                            break;
                        }
                    }
                    _ = tokio::time::sleep(Self::WATCH) => {
                        // Watchdog: instances vanishing under us (a bluetoothd
                        // restart, a Release we never saw) would otherwise go
                        // unnoticed -- the handles we hold don't know.
                        let live = self.adapter.active_advertising_instances().await.unwrap_or(0);
                        if (live as usize) < handles.len() {
                            warn!(
                                "{RX_INNER_NAME}: only {live} advertising instance(s) on the adapter, expected {}; re-registering",
                                handles.len()
                            );
                            break;
                        }
                        // Leave the registration alone while a session or an
                        // outgoing share has the radio -- swapping it out from
                        // under a phone mid-connect is worse than advertising
                        // stale for a while.
                        if held_since.elapsed() >= Self::REFRESH && !scanning_suppressed() {
                            debug!("{RX_INNER_NAME}: refreshing the advertisement");
                            break;
                        }
                    }
                }
            }

            // Full drop-then-register cycle. Registering the replacement first
            // and dropping the old one after sounds seamless, but it's exactly
            // what created phantom registrations: a connectable set registered
            // while the phone's connection was still up is accepted by
            // bluetoothd yet may never be enabled by the controller, and
            // nothing ever retries the enable. The sub-second gap of a clean
            // cycle is invisible next to the phone's scan interval.
            drop(handles);
            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("{RX_INNER_NAME}: tracker cancelled, returning");
                    return Ok(());
                }
                _ = tokio::time::sleep(Self::CYCLE_GRACE) => {}
            }
        }
    }

    /// Registers every payload as its own advertising instance. Partial
    /// success is fine (logged); only zero instances is an error.
    async fn register_all(&self) -> Result<Vec<bluer::adv::AdvertisementHandle>, anyhow::Error> {
        let mut handles = Vec::with_capacity(self.payloads.len());
        for (name, data) in &self.payloads {
            match self.register(data).await {
                Ok(handle) => handles.push(handle),
                Err(e) => warn!("{RX_INNER_NAME}: couldn't register the {name} advertisement: {e}"),
            }
        }
        if handles.is_empty() {
            return Err(anyhow::anyhow!("no advertising instance could be registered"));
        }
        Ok(handles)
    }


    async fn register(&self, data: &[u8]) -> Result<bluer::adv::AdvertisementHandle, anyhow::Error> {
        let uuid = Uuid::from_u16(QS_SERVICE_UUID);
        let build = |fast: bool| Advertisement {
            // Connectable, matching how the phone advertises as a receiver.
            advertisement_type: bluer::adv::Type::Peripheral,
            service_data: [(uuid, data.to_vec())].into(),
            discoverable: Some(true),
            // Advertise fast so the phone discovers and connects quickly
            // (BlueZ defaults to a slow ~1-2s interval).
            min_interval: fast.then(|| Duration::from_millis(100)),
            max_interval: fast.then(|| Duration::from_millis(150)),
            ..Default::default()
        };

        match self.advertise_bounded(build(true)).await {
            Ok(handle) => Ok(handle),
            Err(e) => {
                // Min/MaxInterval are experimental in BlueZ and a daemon
                // started without `-E` may refuse them. Advertising slowly
                // beats not advertising at all.
                debug!(
                    "{RX_INNER_NAME}: the adapter wouldn't take a fast advertising interval ({e}); using BlueZ defaults"
                );
                self.advertise_bounded(build(false)).await
            }
        }
    }

    /// `Adapter::advertise` with a hard deadline: dropping the timed-out
    /// future abandons the D-Bus call, so a wedged bluetoothd round-trip
    /// costs one retry instead of parking the advertiser forever.
    async fn advertise_bounded(
        &self,
        adv: Advertisement,
    ) -> Result<bluer::adv::AdvertisementHandle, anyhow::Error> {
        match tokio::time::timeout(Self::REGISTER_TIMEOUT, self.adapter.advertise(adv)).await {
            Ok(result) => Ok(result?),
            Err(_) => Err(anyhow::anyhow!(
                "RegisterAdvertisement didn't answer within {:?}",
                Self::REGISTER_TIMEOUT
            )),
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    // Golden vectors produced by compiling the canonical smhasher
    // MurmurHash3_x64_128 together with Nearby's exact GetHashes/BloomFilter
    // (see the C++ oracle used while writing this) and printing the results.

    #[test]
    fn murmur3_low64_matches_reference() {
        assert_eq!(murmur3_x64_128_low(b""), 0x0000_0000_0000_0000);
        assert_eq!(murmur3_x64_128_low(b"NearbySharing"), 0x7985_79c2_52d5_fe64);
        assert_eq!(murmur3_x64_128_low(b"ELEMENT_1"), 0x519f_d3f0_18f6_5142);
    }

    #[test]
    fn bloom_filter_matches_reference() {
        // 10-byte (80-bit) filter, one element at a time.
        assert_eq!(
            build_bloom_filter(&[b"NearbySharing"]),
            [0x20, 0x00, 0x00, 0x10, 0x02, 0x00, 0x00, 0x03, 0x00, 0x00]
        );
        assert_eq!(
            build_bloom_filter(&[b"ELEMENT_1"]),
            [0x00, 0x00, 0x04, 0x20, 0x04, 0x00, 0x04, 0x20, 0x00, 0x00]
        );
        // Fixed 16-byte dummy (0x00..0x0f) mixed with the service id.
        let dummy: Vec<u8> = (0u8..16).collect();
        assert_eq!(
            build_bloom_filter(&[&dummy, b"NearbySharing"]),
            [0x60, 0x04, 0x00, 0x10, 0x02, 0x04, 0x00, 0x8b, 0x00, 0x00]
        );
    }

    #[test]
    fn a_real_receiver_bloom_contains_the_service_id() {
        // Whatever random dummy is mixed in, the header the phone reads must
        // still test positive for "NearbySharing", or it never reads slot 0.
        let full = receiver_service_data([1, 2, 3, 4], 2, "Test PC", None);
        for _ in 0..64 {
            let header = build_advertisement_header(&full, false, None);
            assert_eq!(header.len(), 15);
            assert_eq!(header[0], 0x41); // v2, not-extended, 1 slot

            let bloom: [u8; BLOOM_FILTER_BYTES] = header[1..11].try_into().unwrap();
            // Recompute the phone's PossiblyContains("NearbySharing").
            let nbits = BLOOM_FILTER_BYTES * 8;
            let contained = bloom_positions(NEARBY_SERVICE_ID, nbits)
                .iter()
                .all(|&p| bloom[p / 8] & (1 << (p % 8)) != 0);
            assert!(contained, "bloom filter dropped the service id: {bloom:02x?}");
        }
    }

    #[test]
    fn header_version_byte_is_v2_one_slot() {
        assert_eq!(HEADER_VERSION_BYTE, 0x41);
        let full = receiver_service_data([1, 2, 3, 4], 2, "Test PC", None);
        // With the twin extended instance on the air, the header says so.
        assert_eq!(build_advertisement_header(&full, true, None)[0], 0x51);
        // With a PSM the header grows to its 17-byte form, PSM big-endian.
        let with_psm = build_advertisement_header(&full, true, Some(0x0083));
        assert_eq!(with_psm.len(), 17);
        assert_eq!(&with_psm[15..], &[0x00, 0x83]);
    }

    #[test]
    fn trailer_encodes_our_psm_not_the_pixels() {
        // Layout after the inner data: device_token(2) + mask + fields.
        let without = receiver_service_data([1, 2, 3, 4], 2, "PC", None);
        assert_eq!(without[without.len() - 1], 0x00, "no-extra-fields mask");
        let with = receiver_service_data([1, 2, 3, 4], 2, "PC", Some(0x0091));
        assert_eq!(&with[with.len() - 3..], &[0x01, 0x00, 0x91]);
        // Same bytes up to the trailer.
        assert_eq!(without[..without.len() - 1], with[..with.len() - 3]);
    }
}
