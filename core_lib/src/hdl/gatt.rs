use std::sync::Arc;
use std::time::Duration;

use bluer::gatt::local::{
    Application, Characteristic, CharacteristicNotify, CharacteristicNotifyMethod,
    CharacteristicNotifier, CharacteristicRead, CharacteristicWrite, CharacteristicWriteMethod,
    Service,
};
use bluer::{Adapter, Address, Uuid, UuidExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::sync::broadcast::Sender;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::channel::ChannelMessage;
use crate::errors::AppError;
use crate::hdl::{BleScanSuppressor, InboundRequest, request_advert_cycle};

const INNER_NAME: &str = "ReceiverGattServer";

// Nearby Connections copresence GATT service + advertisement slot-0 characteristic.
const QS_GATT_SERVICE: u16 = 0xFEF3;
const QS_ADV_SLOT0_UUID: &str = "00000000-0000-3000-8000-000000000000";
// BLE-"weave" GATT socket characteristics (Apple GNS* client code):
// ToPeripheral = phone -> us (write), FromPeripheral = us -> phone (notify).
const QS_WEAVE_TO_PERIPHERAL: &str = "00000100-0004-1000-8000-001a11000101";
const QS_WEAVE_FROM_PERIPHERAL: &str = "00000100-0004-1000-8000-001a11000102";

// Weave packet header bits (internal/weave/packet.cc).
const WEAVE_CONTROL: u8 = 0b1000_0000;
const WEAVE_CMD_MASK: u8 = 0b0000_1111;
const WEAVE_FIRST_BIT: u8 = 0b0000_1000;
const WEAVE_LAST_BIT: u8 = 0b0000_0100;
const WEAVE_COUNTER_MASK: u8 = 0b0111_0000;
const WEAVE_CMD_CONN_REQUEST: u8 = 0;
const WEAVE_CMD_CONN_CONFIRM: u8 = 1;
const WEAVE_CMD_ERROR: u8 = 2;
const WEAVE_PROTOCOL_VERSION: u16 = 1;

// BLE-socket demux (internal/platform .../ble_packet): [service_id_hash(3)][data];
// service_id_hash 00 00 00 marks a control packet (SocketControlFrame).
const QS_SVC_HASH: [u8; 3] = [0xfc, 0x9f, 0x5e];
const SOCKET_CTRL_INTRODUCTION: u8 = 1;
const SOCKET_CTRL_DISCONNECTION: u8 = 2;

/// How long to wait for the phone's weave connection request once it has
/// enabled notifications.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// How often an otherwise idle session checks that the link is still up.
const LIVENESS_TICK: Duration = Duration::from_secs(2);
/// Backstop for a link that goes quiet without BlueZ noticing. Generous,
/// because a session with no bandwidth upgrade stays open and idle while the
/// user decides whether to accept the transfer.
const IDLE_LIMIT: Duration = Duration::from_secs(120);
/// How long a new notification session waits for a superseded one to unwind.
const EVICT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long to wait for a finished session's peer to drop its LE connection
/// before cycling the advertisement anyway.
const PEER_GONE_WAIT: Duration = Duration::from_secs(15);
/// How often the peer's connection state is checked during that wait.
const PEER_GONE_POLL: Duration = Duration::from_millis(500);
/// Teardown grace when the session never learnt who its peer was.
const PEER_GONE_FALLBACK: Duration = Duration::from_secs(2);

/// The phone's writes to the weave characteristic, and the session consuming
/// them.
///
/// One `ReceiverGattServer` serves every connection for the lifetime of the
/// app, so this gets replaced wholesale when a session ends: whatever the
/// previous connection left queued is dropped along with the old channel rather
/// than being fed to the next connection's weave parser as if it were the start
/// of its stream.
struct WeaveChannel {
    tx: UnboundedSender<(Address, Vec<u8>)>,
    rx: Option<UnboundedReceiver<(Address, Vec<u8>)>>,
    /// Cancels whichever session currently holds `rx`.
    ctk: CancellationToken,
}

impl WeaveChannel {
    fn new() -> Self {
        let (tx, rx) = unbounded_channel();
        Self {
            tx,
            rx: Some(rx),
            ctk: CancellationToken::new(),
        }
    }
}

pub struct ReceiverGattServer {
    adapter: Arc<Adapter>,
    advertisement: Vec<u8>,
    sender: Sender<ChannelMessage>,
    tcp_port: u16,
}

impl ReceiverGattServer {
    pub async fn new(
        advertisement: Vec<u8>,
        sender: Sender<ChannelMessage>,
        tcp_port: u16,
    ) -> Result<Self, anyhow::Error> {
        let session = bluer::Session::new().await?;
        let adapter = session.default_adapter().await?;
        adapter.set_powered(true).await?;

        Ok(Self {
            adapter: Arc::new(adapter),
            advertisement,
            sender,
            tcp_port,
        })
    }

    pub async fn run(&self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        let service_uuid = Uuid::from_u16(QS_GATT_SERVICE);
        let slot0: Uuid = QS_ADV_SLOT0_UUID.parse()?;
        let weave_write: Uuid = QS_WEAVE_TO_PERIPHERAL.parse()?;
        let weave_notify: Uuid = QS_WEAVE_FROM_PERIPHERAL.parse()?;
        let advert = self.advertisement.clone();

        // Weave packets written by the phone to 0101 are forwarded to the notify
        // task (which owns the notifier needed to reply on 0102).
        let chan = Arc::new(Mutex::new(WeaveChannel::new()));
        let write_chan = chan.clone();
        let sender = self.sender.clone();
        let tcp_port = self.tcp_port;
        let adapter = self.adapter.clone();
        let slot0_adapter = self.adapter.clone();
        // Devices whose slot-0 connection we're already waiting out (a long
        // read arrives as several chunked requests; one waiter is enough).
        let slot0_pending: Arc<std::sync::Mutex<std::collections::HashSet<Address>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

        let app = Application {
            services: vec![Service {
                uuid: service_uuid,
                primary: true,
                characteristics: vec![
                    Characteristic {
                        uuid: slot0,
                        read: Some(CharacteristicRead {
                            read: true,
                            fun: Box::new(move |req| {
                                let advert = advert.clone();
                                let adapter = slot0_adapter.clone();
                                let pending = slot0_pending.clone();
                                Box::pin(async move {
                                    let addr = req.device_address;
                                    debug!("{INNER_NAME}: slot0 advertisement read by {addr} ({} bytes)", advert.len());
                                    // A slot-0 fetch is a *connection*, and it consumes
                                    // the connectable advertisement exactly like a weave
                                    // session's connection does. In header mode every
                                    // transfer starts with such a fetch, so without this
                                    // hook we'd be off the air at the precise moment the
                                    // phone comes back to connect for the transfer.
                                    let fresh = pending.lock().unwrap().insert(addr);
                                    if fresh {
                                        let pending = pending.clone();
                                        tokio::spawn(async move {
                                            cycle_advert_when_peer_gone(adapter, Some(addr)).await;
                                            pending.lock().unwrap().remove(&addr);
                                        });
                                    }
                                    Ok(advert)
                                })
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    Characteristic {
                        uuid: weave_write,
                        write: Some(CharacteristicWrite {
                            write: true,
                            // Write-with-response only: forces the phone to send weave
                            // packets serially so BlueZ delivers them to us in order.
                            write_without_response: false,
                            method: CharacteristicWriteMethod::Fun(Box::new(move |value, req| {
                                let write_chan = write_chan.clone();
                                Box::pin(async move {
                                    // Tag with the writer so a session can tell
                                    // its own peer's packets from a second
                                    // device's.
                                    let _ = write_chan
                                        .lock()
                                        .await
                                        .tx
                                        .send((req.device_address, value));
                                    Ok(())
                                })
                            })),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    Characteristic {
                        uuid: weave_notify,
                        notify: Some(CharacteristicNotify {
                            notify: true,
                            indicate: true,
                            method: CharacteristicNotifyMethod::Fun(Box::new(move |notifier| {
                                let chan = chan.clone();
                                let sender = sender.clone();
                                let adapter = adapter.clone();
                                Box::pin(async move {
                                    weave_session(notifier, chan, sender, tcp_port, adapter).await;
                                })
                            })),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };

        info!(
            "{INNER_NAME}: registering GATT service 0x{QS_GATT_SERVICE:04X} (slot0 {} bytes + weave socket)",
            self.advertisement.len()
        );
        let handle = self.adapter.serve_gatt_application(app).await?;
        ctk.cancelled().await;
        info!("{INNER_NAME}: tracker cancelled, returning");
        drop(handle);

        Ok(())
    }
}

/// Bridges the weave BLE socket to the (transport-generic) inbound handshake:
/// answers the Connection Request, then shuttles the `[hash][len][OfflineFrame]`
/// stream between the phone and an `InboundRequest` running over an in-memory duplex.
async fn weave_session(
    notifier: CharacteristicNotifier,
    chan: Arc<Mutex<WeaveChannel>>,
    sender: Sender<ChannelMessage>,
    tcp_port: u16,
    adapter: Arc<Adapter>,
) {
    // Claim the packet stream for this session. A notification session starting
    // while another still holds it means the phone reconnected, so evict the
    // old one -- the new connection is the one the user is waiting on -- rather
    // than turning the new one away or, worse, parking on a lock behind a
    // session that will never end.
    let deadline = Instant::now() + EVICT_TIMEOUT;
    let mut evicting = false;
    let (mut rx, ctk) = loop {
        {
            let mut guard = chan.lock().await;
            if let Some(rx) = guard.rx.take() {
                let ctk = CancellationToken::new();
                guard.ctk = ctk.clone();
                break (rx, ctk);
            }
            if !evicting {
                debug!("{INNER_NAME}: weave: a session is still open, closing it");
                evicting = true;
            }
            guard.ctk.cancel();
        }
        if Instant::now() >= deadline {
            warn!("{INNER_NAME}: weave: the previous session didn't release the socket, ignoring this one");
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // Scanning would take airtime away from this link, and under LL privacy it
    // pauses our advertisement outright.
    let _suppressor = BleScanSuppressor::new();

    let peer = weave_session_inner(notifier, &mut rx, ctk, sender, tcp_port).await;

    // Hand a fresh channel back, dropping anything this connection left queued
    // along with the old one.
    *chan.lock().await = WeaveChannel::new();
    info!("{INNER_NAME}: weave: session ended");

    // This session existing at all means a central connected to us, and that
    // connection consumed the connectable advertisement. Spawned rather than
    // awaited: this future is the body of BlueZ's StartNotify call, and its
    // D-Bus reply shouldn't wait out a disconnect.
    tokio::spawn(cycle_advert_when_peer_gone(adapter, peer));
}

/// Waits for the finished session's peer to drop its LE connection, then asks
/// [`ReceiverAdvertiser`](crate::hdl::ReceiverAdvertiser) for a fresh
/// advertisement. Re-registering while the link is still up is how phantom
/// never-enabled registrations are made, so the wait matters as much as the
/// signal.
pub(crate) async fn cycle_advert_when_peer_gone(adapter: Arc<Adapter>, peer: Option<Address>) {
    // Keep the scanner off the air while the peer finishes up and possibly
    // comes straight back to open the transfer socket -- scan windows were
    // measured stretching this phase's ATT round-trips from ~30ms to ~370ms.
    let _suppress = BleScanSuppressor::new();
    match peer {
        Some(addr) => {
            let deadline = Instant::now() + PEER_GONE_WAIT;
            loop {
                // An Err means BlueZ no longer tracks the device at all.
                let connected = match adapter.device(addr) {
                    Ok(device) => device.is_connected().await.unwrap_or(false),
                    Err(_) => false,
                };
                if !connected {
                    break;
                }
                if Instant::now() >= deadline {
                    debug!("{INNER_NAME}: peer {addr} still connected after {PEER_GONE_WAIT:?}; cycling the advertisement anyway");
                    break;
                }
                tokio::time::sleep(PEER_GONE_POLL).await;
            }
        }
        // The session never learnt who connected; give the teardown a moment.
        None => tokio::time::sleep(PEER_GONE_FALLBACK).await,
    }
    request_advert_cycle();
}

/// Returns the peer's address once the weave handshake has identified it.
async fn weave_session_inner(
    mut notifier: CharacteristicNotifier,
    rx: &mut UnboundedReceiver<(Address, Vec<u8>)>,
    ctk: CancellationToken,
    sender: Sender<ChannelMessage>,
    tcp_port: u16,
) -> Option<Address> {
    info!("{INNER_NAME}: weave: notify session open");

    // 1. Weave connection handshake.
    let handshake_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let (peer, selected) = loop {
        tokio::select! {
            _ = ctk.cancelled() => {
                debug!("{INNER_NAME}: weave: superseded before the handshake");
                return None;
            }
            _ = tokio::time::sleep_until(handshake_deadline) => {
                debug!("{INNER_NAME}: weave: no connection request within {HANDSHAKE_TIMEOUT:?}, closing");
                return None;
            }
            maybe_pkt = rx.recv() => match maybe_pkt {
                Some((addr, pkt))
                    if !pkt.is_empty()
                        && pkt[0] & WEAVE_CONTROL != 0
                        && pkt[0] & WEAVE_CMD_MASK == WEAVE_CMD_CONN_REQUEST =>
                {
                    let max = if pkt.len() >= 7 {
                        u16::from_be_bytes([pkt[5], pkt[6]])
                    } else {
                        100
                    };
                    break (addr, max.min(509).max(20));
                }
                Some(_) => continue,
                None => return None,
            },
        }
    };
    let confirm = vec![
        WEAVE_CONTROL | WEAVE_CMD_CONN_CONFIRM,
        (WEAVE_PROTOCOL_VERSION >> 8) as u8,
        (WEAVE_PROTOCOL_VERSION & 0xff) as u8,
        (selected >> 8) as u8,
        (selected & 0xff) as u8,
    ];
    if let Err(e) = notifier.notify(confirm).await {
        error!("{INNER_NAME}: weave: conn-confirm failed: {e}");
        return Some(peer);
    }
    let max_payload = (selected as usize).saturating_sub(1).max(19);
    info!("{INNER_NAME}: weave: connected to {peer} (selected={selected})");

    // Introduce ourselves on the BLE socket layer. Google's BleSocket sends an
    // INTRODUCTION control message from both sides at socket start, and the
    // phone stalls ~4 seconds at the head of every session waiting for ours.
    // Single weave data packet: counter 1, first+last set.
    {
        let intro_msg: &[u8] = &[
            0, 0, 0, // control service-id hash slot
            0x08, 0x01, // SocketControlFrame.type = INTRODUCTION
            0x12, 0x07, // introduction submessage, 7 bytes
            0x0a, 0x03, 0xfc, 0x9f, 0x5e, // service id hash
            0x10, 0x02, // socket version 2
        ];
        let mut wp = Vec::with_capacity(1 + intro_msg.len());
        wp.push(((1u8 & 0x07) << 4) | WEAVE_FIRST_BIT | WEAVE_LAST_BIT);
        wp.extend_from_slice(intro_msg);
        if let Err(e) = notifier.notify(wp).await {
            error!("{INNER_NAME}: weave: introduction failed: {e}");
            return Some(peer);
        }
    }

    // 2. Run the inbound handshake over an in-memory duplex; we bridge the bytes.
    //
    // Not aborted when this session ends: by then the payload has usually moved
    // to TCP via the bandwidth upgrade, and the duplex closing is what tells a
    // handshake that hasn't upgraded to give up.
    let (inbound_side, weave_side) = tokio::io::duplex(64 * 1024);
    let (mut weave_rd, mut weave_wr) = tokio::io::split(weave_side);
    let isender = sender.clone();
    tokio::spawn(async move {
        let mut ir = InboundRequest::new(
            crate::hdl::MigratableStream::Ble(inbound_side),
            "ble-weave".to_string(),
            isender,
        );
        ir.set_bwu_tcp_port(tcp_port);
        loop {
            if let Err(e) = ir.handle().await {
                if !matches!(e.downcast_ref(), Some(AppError::NotAnError)) {
                    debug!("{INNER_NAME}: weave inbound ended: {e}");
                }
                break;
            }
            // Once the encrypted connection is up, upgrade the payload to Wi-Fi.
            if ir.take_bwu_pending() || ir.bwu_retry_due() {
                if let Err(e) = ir.do_bwu().await {
                    warn!("{INNER_NAME}: BWU failed, staying on BLE: {e}");
                }
            }
        }
    });

    // 3. Shuttle loop.
    let mut reasm: Vec<u8> = Vec::new(); // reassembles fragmented weave messages
    let mut out_buf: Vec<u8> = Vec::new(); // accumulates inbound's [len][frame] writes
    let mut send_counter: u8 = 2; // conn-confirm was 0, our introduction 1
    let mut rbuf = [0u8; 2048];
    let mut last_activity = Instant::now();

    loop {
        tokio::select! {
            _ = ctk.cancelled() => {
                debug!("{INNER_NAME}: weave: superseded by a new connection, closing");
                break;
            }
            _ = tokio::time::sleep(LIVENESS_TICK) => {
                // A phone that walks off, or whose link hits the supervision
                // timeout, sends neither ERROR nor DISCONNECTION. BlueZ still
                // calls StopNotify when the link drops, so watch for that:
                // without it the session would sit here forever holding the
                // weave socket, and every later connection would be left
                // hanging in "Connecting" until the app was restarted.
                if notifier.is_stopped() {
                    debug!("{INNER_NAME}: weave: notifications stopped, closing");
                    break;
                }
                if last_activity.elapsed() >= IDLE_LIMIT {
                    debug!("{INNER_NAME}: weave: idle for {IDLE_LIMIT:?}, closing");
                    break;
                }
            }
            maybe_pkt = rx.recv() => {
                let Some((addr, pkt)) = maybe_pkt else { break; };
                if addr != peer {
                    debug!("{INNER_NAME}: weave: ignoring a packet from {addr}, this session is {peer}'s");
                    continue;
                }
                last_activity = Instant::now();
                if pkt.is_empty() { continue; }
                let hdr = pkt[0];
                debug!("{INNER_NAME}: rx pkt hdr={hdr:#04x} plen={}", pkt.len());
                if hdr & WEAVE_CONTROL != 0 {
                    if hdr & WEAVE_CMD_MASK == WEAVE_CMD_ERROR {
                        debug!("{INNER_NAME}: weave: peer sent ERROR, closing");
                        break;
                    }
                    continue;
                }
                // Data weave packet: reassemble first..last into a message.
                reasm.extend_from_slice(&pkt[1..]);
                if hdr & WEAVE_LAST_BIT == 0 {
                    continue;
                }
                let msg = std::mem::take(&mut reasm);
                if msg.len() < 3 {
                    continue;
                }
                debug!("{INNER_NAME}: rx msg {} bytes: {}", msg.len(), hex::encode(&msg[..msg.len().min(24)]));
                if msg[0..3] == [0, 0, 0] {
                    // BLE control packet (SocketControlFrame): 00 00 00 08 <type> ...
                    let ctrl_type = if msg.len() >= 5 && msg[3] == 0x08 { msg[4] } else { 0 };
                    match ctrl_type {
                        SOCKET_CTRL_INTRODUCTION => debug!("{INNER_NAME}: weave: INTRODUCTION"),
                        SOCKET_CTRL_DISCONNECTION => {
                            debug!("{INNER_NAME}: weave: DISCONNECTION");
                            break;
                        }
                        _ => debug!("{INNER_NAME}: weave: control {}", hex::encode(&msg)),
                    }
                    continue;
                }
                // Data packet: strip 3-byte hash, forward [len][frame] to inbound.
                let inner = &msg[3..];
                let len_field = if inner.len() >= 4 {
                    u32::from_be_bytes([inner[0], inner[1], inner[2], inner[3]])
                } else {
                    0
                };
                debug!(
                    "{INNER_NAME}: -> inbound {} bytes, len_field={len_field} hash={}",
                    inner.len(),
                    hex::encode(&msg[..3])
                );
                if weave_wr.write_all(inner).await.is_err() {
                    break;
                }
            }
            r = weave_rd.read(&mut rbuf) => {
                match r {
                    Ok(0) => break, // inbound closed
                    Ok(n) => {
                        last_activity = Instant::now();
                        out_buf.extend_from_slice(&rbuf[..n]);
                        // Emit each complete [len(4)][frame] as a BLE data message.
                        loop {
                            if out_buf.len() < 4 { break; }
                            let len = u32::from_be_bytes([out_buf[0], out_buf[1], out_buf[2], out_buf[3]]) as usize;
                            if out_buf.len() < 4 + len { break; }
                            let entry: Vec<u8> = out_buf.drain(0..4 + len).collect();
                            let mut message = Vec::with_capacity(3 + entry.len());
                            message.extend_from_slice(&QS_SVC_HASH);
                            message.extend_from_slice(&entry);
                            // Fragment into weave data packets.
                            let total = message.len();
                            let mut off = 0;
                            while off < total {
                                let end = (off + max_payload).min(total);
                                let first = off == 0;
                                let last = end == total;
                                let mut wp = Vec::with_capacity(1 + (end - off));
                                let mut h = (send_counter & 0x07) << 4;
                                if first { h |= WEAVE_FIRST_BIT; }
                                if last { h |= WEAVE_LAST_BIT; }
                                let _ = WEAVE_COUNTER_MASK; // documents counter field
                                wp.push(h);
                                wp.extend_from_slice(&message[off..end]);
                                send_counter = send_counter.wrapping_add(1);
                                if let Err(e) = notifier.notify(wp).await {
                                    error!("{INNER_NAME}: weave: notify failed: {e}");
                                    return Some(peer);
                                }
                                off = end;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }

    Some(peer)
}
