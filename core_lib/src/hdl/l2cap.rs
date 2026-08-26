use std::sync::Arc;
use std::time::Duration;

use bluer::l2cap::{Security, SecurityLevel, Socket, SocketAddr, Stream};
use bluer::{Adapter, AddressType};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast::Sender;
use tokio_util::sync::CancellationToken;

use crate::channel::ChannelMessage;
use crate::errors::AppError;
use crate::hdl::{BleScanSuppressor, InboundRequest, cycle_advert_when_peer_gone};

const INNER_NAME: &str = "L2capServer";

// SHA-256("NearbySharing")[..3], same service hash as everywhere else.
pub(crate) const SVC_HASH: [u8; 3] = [0xfc, 0x9f, 0x5e];

// BleL2capPacket commands (google/nearby ble_l2cap_packet.h). Requests 1 and
// 21 are followed by [u16 BE length][payload]; the rest are a bare command
// byte. Spoken by the C++ platforms; GmsCore skips this handshake entirely.
const CMD_REQUEST_ADVERTISEMENT: u8 = 1;
const CMD_REQUEST_ADVERTISEMENT_FINISH: u8 = 2;
pub(crate) const CMD_REQUEST_DATA_CONNECTION: u8 = 3;
const CMD_RESPONSE_ADVERTISEMENT: u8 = 21;
const CMD_RESPONSE_SERVICE_ID_NOT_FOUND: u8 = 22;
pub(crate) const CMD_RESPONSE_DATA_CONNECTION_READY: u8 = 23;

// BLE-socket control frame types (same as the GATT weave socket's).
pub(crate) const SOCKET_CTRL_INTRODUCTION: u8 = 1;
pub(crate) const SOCKET_CTRL_DISCONNECTION: u8 = 2;
const SOCKET_CTRL_PACKET_ACK: u8 = 3;

/// Builds a `[u32 len][00 00 00][SocketControlFrame]` acknowledgement for
/// `acked` received payload bytes. The BLE-socket protocol flow-controls on
/// these: a sender stalls once too many of its bytes go unacknowledged, so a
/// receiver that never acks (us, until now) starves the handshake mid-way.
/// Byte-for-byte the frame the phone itself sends:
/// `08 03` type=PACKET_ACK, `22 len` ack submessage, `0a 03 fc9f5e` service
/// id hash, `10 <varint>` received size.
/// Builds our `[u32 len][00 00 00][SocketControlFrame]` INTRODUCTION.
/// Google's own BleSocket sends one from both sides at socket start, and the
/// phone stalls ~4 seconds waiting for the peer's before proceeding without
/// it -- a pause visible at the head of every session until now.
/// `08 01` type=INTRODUCTION, `12 len` introduction submessage,
/// `0a 03 fc9f5e` service id hash, `10 02` socket version 2.
pub(crate) fn build_intro_frame() -> Vec<u8> {
    let mut msg = vec![0u8, 0, 0, 0x08, SOCKET_CTRL_INTRODUCTION, 0x12, 0x07, 0x0a, 0x03];
    msg.extend_from_slice(&SVC_HASH);
    msg.extend_from_slice(&[0x10, 0x02]);
    let mut frame = Vec::with_capacity(4 + msg.len());
    frame.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    frame.extend_from_slice(&msg);
    frame
}

pub(crate) fn build_ack_frame(acked: usize) -> Vec<u8> {
    let mut varint = Vec::new();
    let mut v = acked as u64;
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            varint.push(byte | 0x80);
        } else {
            varint.push(byte);
            break;
        }
    }
    let sub_len = 5 + 1 + varint.len();
    let mut msg = vec![0u8, 0, 0, 0x08, SOCKET_CTRL_PACKET_ACK, 0x22, sub_len as u8, 0x0a, 0x03];
    msg.extend_from_slice(&SVC_HASH);
    msg.push(0x10);
    msg.extend_from_slice(&varint);
    let mut frame = Vec::with_capacity(4 + msg.len());
    frame.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    frame.extend_from_slice(&msg);
    frame
}

/// Android caps its L2CAP transmit packets at 1024 bytes; receive room beyond
/// that is free robustness.
const RECV_MTU: u16 = 4096;

/// LE CoC server for the Nearby BLE_L2CAP medium.
///
/// A phone that saw our PSM in the advertisement connects here instead of the
/// GATT socket -- skipping GATT service discovery, which is the slow part of
/// every BLE connect. Two wire dialects exist and the first bytes reveal
/// which one a client speaks:
///
/// * GmsCore opens the mediums *BLE socket* stream directly: u32-BE-length
///   frames whose payload is the same `[service_id_hash][data]` message
///   format the GATT weave socket carries (control frames under an all-zero
///   hash, `[u32 len][OfflineFrame]` entries under the service hash).
/// * The C++ platforms speak the BleL2capPacket command handshake
///   (advertisement request / data-connection request), after which the raw
///   socket becomes the standard endpoint channel.
pub struct L2capServer {
    listener: bluer::l2cap::StreamListener,
    adapter: Arc<Adapter>,
}

impl L2capServer {
    /// Binds an LE CoC server socket on this adapter with a kernel-assigned
    /// dynamic PSM and returns it together with that PSM.
    pub async fn bind() -> Result<(Self, u16), anyhow::Error> {
        let session = bluer::Session::new().await?;
        let adapter = session.default_adapter().await?;

        let socket = Socket::<Stream>::new_stream()?;
        // Android connects with an *insecure* L2CAP channel (no bonding), so
        // requiring encryption here would reject every phone.
        socket.set_security(Security { level: SecurityLevel::Low, key_size: 0 })?;
        socket.set_recv_mtu(RECV_MTU)?;
        socket.bind(SocketAddr::new(
            adapter.address().await?,
            AddressType::LePublic,
            0, // let the kernel pick a dynamic PSM
        ))?;
        let listener = socket.listen(2)?;
        let psm = listener.as_ref().local_addr()?.psm;

        Ok((
            Self {
                listener,
                adapter: Arc::new(adapter),
            },
            psm,
        ))
    }

    pub async fn run(
        self,
        advert: Vec<u8>,
        sender: Sender<ChannelMessage>,
        tcp_port: u16,
        ctk: CancellationToken,
    ) {
        info!("{INNER_NAME}: accepting Quick Share connections");
        loop {
            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("{INNER_NAME}: tracker cancelled, returning");
                    return;
                }
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, peer)) => {
                        info!("{INNER_NAME}: connection from {}", peer.addr);
                        let advert = advert.clone();
                        let sender = sender.clone();
                        let adapter = self.adapter.clone();
                        tokio::spawn(async move {
                            // Scanning steals airtime from the link, exactly
                            // as it does from GATT sessions.
                            let _suppressor = BleScanSuppressor::new();
                            if let Err(e) =
                                serve_connection(stream, &advert, sender, tcp_port).await
                            {
                                debug!("{INNER_NAME}: connection from {} ended: {e}", peer.addr);
                            }
                            // The LE connection carrying this socket consumed
                            // a connectable advertising instance, same as any
                            // other connection.
                            cycle_advert_when_peer_gone(adapter, Some(peer.addr)).await;
                        });
                    }
                    Err(e) => {
                        warn!("{INNER_NAME}: accept failed: {e}");
                        tokio::select! {
                            _ = ctk.cancelled() => return,
                            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                        }
                    }
                },
            }
        }
    }
}

/// Reads into `buf`, draining `leftover` before touching the socket.
async fn read_exact_lo(
    stream: &mut Stream,
    leftover: &mut Vec<u8>,
    buf: &mut [u8],
) -> Result<(), anyhow::Error> {
    let from_leftover = leftover.len().min(buf.len());
    if from_leftover > 0 {
        buf[..from_leftover].copy_from_slice(&leftover[..from_leftover]);
        leftover.drain(..from_leftover);
    }
    if from_leftover < buf.len() {
        stream.read_exact(&mut buf[from_leftover..]).await?;
    }
    Ok(())
}

async fn serve_connection(
    mut stream: Stream,
    advert: &[u8],
    sender: Sender<ChannelMessage>,
    tcp_port: u16,
) -> Result<(), anyhow::Error> {
    // Sniff the dialect from the first four bytes (see the type-level docs).
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?; // clean EOF ends the connection
    debug!("{INNER_NAME}: first bytes {}", hex::encode(head));
    let leftover = head.to_vec();

    if head[0] == 0x00 && head[1] == 0x00 {
        // u32-BE frame lengths: the GmsCore BLE-socket stream.
        debug!("{INNER_NAME}: client speaks u32-framed packets");
        serve_ble_socket(stream, leftover, advert, sender, tcp_port).await
    } else if head[0] == 0x00 {
        // u16-BE packet lengths around the command protocol.
        debug!("{INNER_NAME}: client speaks length-prefixed command packets");
        serve_commands(stream, leftover, true, advert, sender, tcp_port).await
    } else {
        debug!("{INNER_NAME}: client speaks bare command packets");
        serve_commands(stream, leftover, false, advert, sender, tcp_port).await
    }
}

/// The GmsCore dialect: u32-BE-length frames. The first frame reveals the
/// sub-dialect: a tiny frame whose payload is a BleL2capPacket command means
/// the command handshake (framed, unlike the C++ bare version) -- after the
/// data-connection request is acknowledged the socket carries the standard
/// `[u32 len][OfflineFrame]` endpoint channel, which the inbound handler
/// reads natively. A frame starting with a 3-byte service-hash slot instead
/// carries weave-style messages, bridged like the GATT socket.
async fn serve_ble_socket(
    mut stream: Stream,
    mut leftover: Vec<u8>,
    advert: &[u8],
    sender: Sender<ChannelMessage>,
    tcp_port: u16,
) -> Result<(), anyhow::Error> {
    loop {
        let Some(msg) = read_frame(&mut stream, &mut leftover).await? else {
            return Ok(());
        };
        debug!(
            "{INNER_NAME}: rx frame {}",
            hex::encode(&msg[..msg.len().min(24)])
        );

        // Weave-style messages: [00 00 00|service_hash][data].
        if msg.len() >= 3 && (msg[..3] == [0, 0, 0] || msg[..3] == SVC_HASH) {
            return serve_weave_messages(stream, leftover, Some(msg), sender, tcp_port).await;
        }

        // Otherwise: a u32-framed BleL2capPacket command.
        match msg.first().copied() {
            Some(CMD_REQUEST_ADVERTISEMENT) => {
                let hash = msg.get(3..).unwrap_or_default();
                if hash.get(..3) == Some(&SVC_HASH[..]) {
                    debug!(
                        "{INNER_NAME}: advertisement requested; sending {} bytes",
                        advert.len()
                    );
                    let mut packet = Vec::with_capacity(3 + advert.len());
                    packet.push(CMD_RESPONSE_ADVERTISEMENT);
                    packet.extend_from_slice(&(advert.len() as u16).to_be_bytes());
                    packet.extend_from_slice(advert);
                    send_frame(&mut stream, &packet).await?;
                } else {
                    debug!(
                        "{INNER_NAME}: advertisement requested for unknown service {}",
                        hex::encode(hash)
                    );
                    send_frame(&mut stream, &[CMD_RESPONSE_SERVICE_ID_NOT_FOUND]).await?;
                }
            }
            Some(CMD_REQUEST_ADVERTISEMENT_FINISH) => {
                debug!("{INNER_NAME}: advertisement fetch finished");
            }
            Some(CMD_REQUEST_DATA_CONNECTION) => {
                info!("{INNER_NAME}: data connection requested; bridging to inbound");
                send_frame(&mut stream, &[CMD_RESPONSE_DATA_CONNECTION_READY]).await?;
                // The data phase keeps the same u32 framing, carrying the
                // weave-style messages (INTRODUCTION control frame first, then
                // service-hash-tagged entries) -- not the bare endpoint
                // channel.
                return serve_weave_messages(stream, leftover, None, sender, tcp_port).await;
            }
            other => anyhow::bail!("unsupported framed command {other:?}"),
        }
    }
}

/// Sends one `[u32 BE length][packet]` frame.
pub(crate) async fn send_frame(stream: &mut Stream, packet: &[u8]) -> Result<(), anyhow::Error> {
    let mut framed = Vec::with_capacity(4 + packet.len());
    framed.extend_from_slice(&(packet.len() as u32).to_be_bytes());
    framed.extend_from_slice(packet);
    stream.write_all(&framed).await?;
    Ok(())
}

/// Weave-style messages over u32 frames, bridged to the inbound handshake
/// through an in-memory duplex exactly like the GATT weave socket.
async fn serve_weave_messages(
    mut stream: Stream,
    mut leftover: Vec<u8>,
    first_msg: Option<Vec<u8>>,
    sender: Sender<ChannelMessage>,
    tcp_port: u16,
) -> Result<(), anyhow::Error> {
    let (inbound_side, local) = tokio::io::duplex(64 * 1024);
    let (mut local_rd, mut local_wr) = tokio::io::split(local);
    tokio::spawn(run_inbound(
        crate::hdl::MigratableStream::Ble(inbound_side),
        sender,
        tcp_port,
    ));

    // Introduce ourselves right away; the phone waits ~4s for this.
    stream.write_all(&build_intro_frame()).await?;

    let mut out_buf: Vec<u8> = Vec::new();
    let mut rbuf = [0u8; 2048];
    let mut pending_msg = first_msg;

    loop {
        // Process a message from the phone (the first one arrives pre-read).
        if let Some(msg) = pending_msg.take() {
            if msg.len() < 3 {
                continue;
            }
            if msg[..3] == [0, 0, 0] {
                let ctrl = if msg.len() >= 5 && msg[3] == 0x08 { msg[4] } else { 0 };
                match ctrl {
                    SOCKET_CTRL_INTRODUCTION => debug!("{INNER_NAME}: INTRODUCTION"),
                    SOCKET_CTRL_DISCONNECTION => {
                        debug!("{INNER_NAME}: DISCONNECTION");
                        break;
                    }
                    _ => debug!("{INNER_NAME}: control {}", hex::encode(&msg)),
                }
            } else if msg[..3] == SVC_HASH {
                // The payload is the [u32 len][OfflineFrame] entry the
                // inbound handler reads natively.
                if local_wr.write_all(&msg[3..]).await.is_err() {
                    break;
                }
                // Acknowledge the received bytes or the phone's sender stalls.
                stream.write_all(&build_ack_frame(msg.len() - 3)).await?;
            } else {
                debug!(
                    "{INNER_NAME}: frame for foreign service {}",
                    hex::encode(&msg[..3])
                );
            }
            continue;
        }

        tokio::select! {
            frame = read_frame(&mut stream, &mut leftover) => {
                match frame? {
                    Some(msg) => pending_msg = Some(msg),
                    None => break,
                }
            }
            r = local_rd.read(&mut rbuf) => {
                match r {
                    Ok(0) => break,
                    Ok(n) => {
                        out_buf.extend_from_slice(&rbuf[..n]);
                        loop {
                            if out_buf.len() < 4 { break; }
                            let len = u32::from_be_bytes(out_buf[..4].try_into().unwrap()) as usize;
                            if out_buf.len() < 4 + len { break; }
                            let entry: Vec<u8> = out_buf.drain(0..4 + len).collect();
                            let mut frame = Vec::with_capacity(4 + 3 + entry.len());
                            frame.extend_from_slice(&((3 + entry.len()) as u32).to_be_bytes());
                            frame.extend_from_slice(&SVC_HASH);
                            frame.extend_from_slice(&entry);
                            stream.write_all(&frame).await?;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }
    info!("{INNER_NAME}: BLE-socket session ended");
    Ok(())
}

/// Reads one `[u32 BE length][payload]` frame; `Ok(None)` on clean EOF.
pub(crate) async fn read_frame(
    stream: &mut Stream,
    leftover: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>, anyhow::Error> {
    let mut len_bytes = [0u8; 4];
    if leftover.is_empty() {
        // Distinguish clean EOF from a mid-frame error.
        match stream.read_exact(&mut len_bytes).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    } else {
        read_exact_lo(stream, leftover, &mut len_bytes).await?;
    }
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len == 0 || len > 1 << 20 {
        anyhow::bail!("implausible frame length {len}");
    }
    let mut msg = vec![0u8; len];
    read_exact_lo(stream, leftover, &mut msg).await?;
    Ok(Some(msg))
}

/// The C++ dialect: BleL2capPacket commands, bare or u16-length-prefixed.
async fn serve_commands(
    mut stream: Stream,
    mut leftover: Vec<u8>,
    length_prefixed: bool,
    advert: &[u8],
    sender: Sender<ChannelMessage>,
    tcp_port: u16,
) -> Result<(), anyhow::Error> {
    loop {
        let packet: Vec<u8> = if length_prefixed {
            let mut len_bytes = [0u8; 2];
            read_exact_lo(&mut stream, &mut leftover, &mut len_bytes).await?;
            let len = u16::from_be_bytes(len_bytes) as usize;
            if len == 0 || len > 512 {
                anyhow::bail!("implausible packet length {len}");
            }
            let mut buf = vec![0u8; len];
            read_exact_lo(&mut stream, &mut leftover, &mut buf).await?;
            buf
        } else {
            let mut cmd = [0u8; 1];
            read_exact_lo(&mut stream, &mut leftover, &mut cmd).await?;
            let mut packet = vec![cmd[0]];
            if cmd[0] == CMD_REQUEST_ADVERTISEMENT || cmd[0] == CMD_RESPONSE_ADVERTISEMENT {
                let mut len_bytes = [0u8; 2];
                read_exact_lo(&mut stream, &mut leftover, &mut len_bytes).await?;
                let len = u16::from_be_bytes(len_bytes) as usize;
                if len == 0 || len > 512 {
                    anyhow::bail!("implausible payload length {len}");
                }
                packet.extend_from_slice(&len_bytes);
                let start = packet.len();
                packet.resize(start + len, 0);
                read_exact_lo(&mut stream, &mut leftover, &mut packet[start..]).await?;
            }
            packet
        };
        debug!(
            "{INNER_NAME}: rx packet {}",
            hex::encode(&packet[..packet.len().min(24)])
        );

        match packet[0] {
            CMD_REQUEST_ADVERTISEMENT => {
                let hash = packet.get(3..).unwrap_or_default();
                if hash.get(..3) == Some(&SVC_HASH[..]) {
                    debug!(
                        "{INNER_NAME}: advertisement requested; sending {} bytes",
                        advert.len()
                    );
                    let mut resp = Vec::with_capacity(3 + advert.len());
                    resp.push(CMD_RESPONSE_ADVERTISEMENT);
                    resp.extend_from_slice(&(advert.len() as u16).to_be_bytes());
                    resp.extend_from_slice(advert);
                    send_packet(&mut stream, length_prefixed, &resp).await?;
                } else {
                    debug!(
                        "{INNER_NAME}: advertisement requested for unknown service {}",
                        hex::encode(hash)
                    );
                    send_packet(&mut stream, length_prefixed, &[CMD_RESPONSE_SERVICE_ID_NOT_FOUND])
                        .await?;
                }
            }
            CMD_REQUEST_ADVERTISEMENT_FINISH => {
                debug!("{INNER_NAME}: advertisement fetch finished");
            }
            CMD_REQUEST_DATA_CONNECTION => {
                info!("{INNER_NAME}: data connection requested; bridging to inbound");
                send_packet(&mut stream, length_prefixed, &[CMD_RESPONSE_DATA_CONNECTION_READY])
                    .await?;
                if !leftover.is_empty() {
                    anyhow::bail!("unexpected {} buffered bytes at data-connection start", leftover.len());
                }
                run_inbound(
                    crate::hdl::MigratableStream::L2cap(stream),
                    sender,
                    tcp_port,
                )
                .await;
                return Ok(());
            }
            other => anyhow::bail!("unsupported command {other}"),
        }
    }
}

async fn send_packet(
    stream: &mut Stream,
    length_prefixed: bool,
    packet: &[u8],
) -> Result<(), anyhow::Error> {
    if length_prefixed {
        let mut framed = Vec::with_capacity(2 + packet.len());
        framed.extend_from_slice(&(packet.len() as u16).to_be_bytes());
        framed.extend_from_slice(packet);
        stream.write_all(&framed).await?;
    } else {
        stream.write_all(packet).await?;
    }
    Ok(())
}

/// Runs the transport-generic inbound handshake; the Wi-Fi bandwidth upgrade
/// works the same as from the GATT weave path.
async fn run_inbound(
    socket: crate::hdl::MigratableStream,
    sender: Sender<ChannelMessage>,
    tcp_port: u16,
) {
    let mut ir = InboundRequest::new(socket, "ble-l2cap".to_string(), sender);
    ir.set_bwu_tcp_port(tcp_port);
    loop {
        if let Err(e) = ir.handle().await {
            if !matches!(e.downcast_ref(), Some(AppError::NotAnError)) {
                debug!("{INNER_NAME}: inbound ended: {e}");
            }
            break;
        }
        if ir.take_bwu_pending() || ir.bwu_retry_due() {
            if let Err(e) = ir.do_bwu().await {
                warn!("{INNER_NAME}: BWU failed, staying on L2CAP: {e}");
            }
        }
    }
}
