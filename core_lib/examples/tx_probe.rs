// Stage-0 probe for BLE-initiated *sending* (PC -> phone).
//
// Scans for a phone advertising itself as a Quick Share receiver on 0xFEF3,
// decodes the advertisement (endpoint id, device name, L2CAP PSM), then acts
// as the Nearby Connections initiator: opens an LE CoC to that PSM and runs
// the first BLE-socket handshake frames, printing everything the phone sends.
//
//   tx_probe                 scan, connect to the first receiver with a PSM
//   tx_probe AA:BB:CC:DD:EE:FF   only consider that address
//   PROBE_SCAN_SECS=60 tx_probe
//
// Throwaway diagnostic: nothing here is used by the library.
use std::collections::HashSet;
use std::time::Duration;

use bluer::l2cap::{Security, SecurityLevel, Socket, SocketAddr, Stream};
use bluer::{
    AdapterEvent, Address, AddressType, DiscoveryFilter, DiscoveryTransport, Uuid, UuidExt,
};
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant;

// SHA-256("NearbySharing")[..3]
const SVC_HASH: [u8; 3] = [0xfc, 0x9f, 0x5e];

// BleL2capPacket commands (u32-framed, GmsCore dialect).
const CMD_REQUEST_DATA_CONNECTION: u8 = 3;
const CMD_RESPONSE_DATA_CONNECTION_READY: u8 = 23;

#[derive(Debug)]
struct Advert {
    form: &'static str,
    endpoint_id: Option<String>,
    device_type: Option<u8>,
    name: Option<String>,
    bt_mac: Option<String>,
    psm: Option<u16>,
    extra_mask: Option<u8>,
    /// endpoint_info visibility bit clear == "Everyone" (visible to us).
    visible: Option<bool>,
}

/// Inverse of blea.rs `receiver_service_data` / `build_advertisement_header`.
fn decode(sd: &[u8]) -> Option<Advert> {
    if sd.is_empty() {
        return None;
    }
    let version = sd[0] >> 5;

    // 15/17-byte legacy header: [ver|ext|slots][bloom 10][hash 4][psm 2]?
    if sd.len() <= 17 && version == 2 {
        let psm = if sd.len() >= 17 {
            Some(u16::from_be_bytes([sd[15], sd[16]])).filter(|p| *p != 0)
        } else {
            None
        };
        return Some(Advert {
            form: if sd[0] & 0x10 != 0 {
                "header (ext bit set)"
            } else {
                "header"
            },
            endpoint_id: None,
            device_type: None,
            name: None,
            bt_mac: None,
            psm,
            extra_mask: None,
            visible: None,
        });
    }

    if version != 2 {
        return None;
    }
    // Bit 1 of the mediums header: "fast advertisement" -- the background
    // beacon (contacts-only visibility): [0x4a][u8 len][data][token 2], where
    // data omits the service-id hash and MAC and truncates endpoint_info to
    // 17 bytes (no name), so it never carries a PSM.
    let fast = sd[0] & 0x02 != 0;
    let (data, trailer): (&[u8], &[u8]) = if fast {
        let len = *sd.get(1)? as usize;
        (sd.get(2..2 + len)?, &sd[2 + len..])
    } else {
        // Full mediums advertisement: [0x48][svc hash 3][u32 len][data][token 2][mask][psm 2]?
        if sd.len() < 8 || sd[1..4] != SVC_HASH {
            return None;
        }
        let len = u32::from_be_bytes([sd[4], sd[5], sd[6], sd[7]]) as usize;
        (sd.get(8..8 + len)?, &sd[8 + len..])
    };

    // data: [0x23][svc hash 3][endpoint_id 4][einfo len][einfo][mac 6][extra 2]
    // (fast: [0x23][endpoint_id 4][einfo len][einfo])
    let mut endpoint_id = None;
    let mut device_type = None;
    let mut name = None;
    let mut bt_mac = None;
    let mut visible = None;
    let body_ok = if fast {
        data.len() >= 6
    } else {
        data.len() >= 9 && data[1..4] == SVC_HASH
    };
    if body_ok {
        let eid_at = if fast { 1 } else { 4 };
        endpoint_id = Some(String::from_utf8_lossy(&data[eid_at..eid_at + 4]).into_owned());
        let elen = data[eid_at + 4] as usize;
        let einfo_at = eid_at + 5;
        if let Some(einfo) = data.get(einfo_at..einfo_at + elen) {
            // einfo: [ver 3b|vis 1b|type 3b|res][identity 16][name len][name]
            if !einfo.is_empty() {
                device_type = Some((einfo[0] >> 1) & 0x7);
                visible = Some(einfo[0] & 0x10 == 0);
            }
            if einfo.len() >= 18 {
                let nlen = einfo[17] as usize;
                if let Some(n) = einfo.get(18..18 + nlen) {
                    name = Some(String::from_utf8_lossy(n).into_owned());
                }
            }
        }
        let mac_at = einfo_at + elen;
        if let Some(mac) = (!fast).then(|| data.get(mac_at..mac_at + 6)).flatten() {
            bt_mac = Some(
                mac.iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(":"),
            );
        }
    }

    let mut psm = None;
    let mut extra_mask = None;
    if !fast && trailer.len() >= 3 {
        let mask = trailer[2];
        extra_mask = Some(mask);
        if mask & 0x01 != 0 && trailer.len() >= 5 {
            psm = Some(u16::from_be_bytes([trailer[3], trailer[4]])).filter(|p| *p != 0);
        }
    }

    Some(Advert {
        form: if fast {
            "fast (background beacon, contacts-only)"
        } else {
            "full"
        },
        visible,
        endpoint_id,
        device_type,
        name,
        bt_mac,
        psm,
        extra_mask,
    })
}

struct Found {
    addr: Address,
    addr_type: AddressType,
    psm: u16,
}

async fn send_frame(stream: &mut Stream, packet: &[u8]) -> anyhow::Result<()> {
    let mut framed = Vec::with_capacity(4 + packet.len());
    framed.extend_from_slice(&(packet.len() as u32).to_be_bytes());
    framed.extend_from_slice(packet);
    stream.write_all(&framed).await?;
    println!("  tx frame {}", hex::encode(packet));
    Ok(())
}

/// [u32 len][00 00 00][08 01 12 07 0a 03 fc9f5e 10 02]  (SocketControlFrame INTRODUCTION)
fn intro_frame() -> Vec<u8> {
    let mut msg = vec![0u8, 0, 0, 0x08, 0x01, 0x12, 0x07, 0x0a, 0x03];
    msg.extend_from_slice(&SVC_HASH);
    msg.extend_from_slice(&[0x10, 0x02]);
    let mut frame = Vec::with_capacity(4 + msg.len());
    frame.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    frame.extend_from_slice(&msg);
    frame
}

async fn read_exact_lo(
    stream: &mut Stream,
    leftover: &mut Vec<u8>,
    buf: &mut [u8],
) -> anyhow::Result<()> {
    let n = leftover.len().min(buf.len());
    if n > 0 {
        buf[..n].copy_from_slice(&leftover[..n]);
        leftover.drain(..n);
    }
    if n < buf.len() {
        stream.read_exact(&mut buf[n..]).await?;
    }
    Ok(())
}

/// One [u32 len][payload] frame, or None on clean EOF.
async fn read_frame(
    stream: &mut Stream,
    leftover: &mut Vec<u8>,
) -> anyhow::Result<Option<Vec<u8>>> {
    let mut len_bytes = [0u8; 4];
    match read_exact_lo(stream, leftover, &mut len_bytes).await {
        Ok(()) => {}
        Err(e) => {
            if let Some(io) = e.downcast_ref::<std::io::Error>() {
                if io.kind() == std::io::ErrorKind::UnexpectedEof {
                    return Ok(None);
                }
            }
            return Err(e);
        }
    }
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len == 0 || len > 1 << 20 {
        anyhow::bail!(
            "implausible frame length {len} (raw {})",
            hex::encode(len_bytes)
        );
    }
    let mut msg = vec![0u8; len];
    read_exact_lo(stream, leftover, &mut msg).await?;
    Ok(Some(msg))
}

fn describe(msg: &[u8]) -> String {
    if msg.len() >= 5 && msg[..3] == [0, 0, 0] && msg[3] == 0x08 {
        let t = match msg[4] {
            1 => "INTRODUCTION",
            2 => "DISCONNECTION",
            3 => "PACKET_ACK",
            _ => "control?",
        };
        return format!("control {t}");
    }
    if msg.len() >= 3 && msg[..3] == SVC_HASH {
        return format!("data for NearbySharing ({} bytes payload)", msg.len() - 3);
    }
    if msg.len() == 1 {
        return match msg[0] {
            CMD_RESPONSE_DATA_CONNECTION_READY => "cmd DATA_CONNECTION_READY".into(),
            22 => "cmd SERVICE_ID_NOT_FOUND".into(),
            c => format!("cmd {c}"),
        };
    }
    "?".into()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;
    let local = adapter.address().await?;
    println!("adapter {} @ {local}", adapter.name());

    let target: Option<Address> = match std::env::args().nth(1) {
        Some(s) => Some(s.parse()?),
        None => None,
    };
    let scan_secs: u64 = std::env::var("PROBE_SCAN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);

    adapter
        .set_discovery_filter(DiscoveryFilter {
            transport: DiscoveryTransport::Le,
            duplicate_data: true,
            ..Default::default()
        })
        .await?;
    let mut events = adapter.discover_devices_with_changes().await?;
    println!(
        "scanning {scan_secs}s for 0xFEF3 receivers -- put the phone in Quick Share receive mode ('Everyone')"
    );

    let deadline = Instant::now() + Duration::from_secs(scan_secs);
    let mut found: Option<Found> = None;
    let mut seen: HashSet<(Address, Vec<u8>)> = HashSet::new();
    while found.is_none() {
        let ev = tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            ev = events.next() => match ev { Some(ev) => ev, None => break },
        };
        let AdapterEvent::DeviceAdded(addr) = ev else {
            continue;
        };
        if let Some(t) = target {
            if addr != t {
                continue;
            }
        }
        let Ok(dev) = adapter.device(addr) else {
            continue;
        };
        let Ok(Some(sd)) = dev.service_data().await else {
            continue;
        };
        let Some(bytes) = sd.get(&Uuid::from_u16(0xFEF3)) else {
            continue;
        };
        if !seen.insert((addr, bytes.clone())) {
            continue;
        }
        let addr_type = dev.address_type().await.unwrap_or(AddressType::LeRandom);
        let name = dev.name().await.ok().flatten().unwrap_or_default();
        let rssi = dev.rssi().await.ok().flatten();
        println!(
            "\n{addr} ({addr_type:?}) bluez-name={name:?} rssi={rssi:?}: FEF3 {} bytes\n  {}",
            bytes.len(),
            hex::encode(bytes)
        );
        match decode(bytes) {
            Some(adv) => {
                println!("  decoded: {adv:?}");
                match adv.psm {
                    Some(psm) => {
                        found = Some(Found {
                            addr,
                            addr_type,
                            psm,
                        })
                    }
                    None => println!(
                        "  -> no PSM in this advertisement (would need a GATT slot-0 read)"
                    ),
                }
            }
            None => println!("  -> unrecognised layout"),
        }
    }
    drop(events);

    let Some(f) = found else {
        println!("\nNO-GO: no 0xFEF3 receiver advertising a PSM seen within {scan_secs}s");
        return Ok(());
    };
    // Let BlueZ wind the scan down before we ask for a connection.
    tokio::time::sleep(Duration::from_millis(500)).await;

    println!(
        "\nconnecting LE CoC to {} ({:?}) psm {} ...",
        f.addr, f.addr_type, f.psm
    );
    let socket = Socket::<Stream>::new_stream()?;
    // Android accepts insecure (unbonded) CoC; asking for more would fail.
    socket.set_security(Security {
        level: SecurityLevel::Low,
        key_size: 0,
    })?;
    socket.set_recv_mtu(4096)?;
    socket.bind(SocketAddr::new(local, AddressType::LePublic, 0))?;
    let t0 = Instant::now();
    let mut stream = match tokio::time::timeout(
        Duration::from_secs(20),
        socket.connect(SocketAddr::new(f.addr, f.addr_type, f.psm)),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            println!("NO-GO: connect failed after {:?}: {e}", t0.elapsed());
            return Ok(());
        }
        Err(_) => {
            println!("NO-GO: connect timed out after 20s");
            return Ok(());
        }
    };
    println!(
        "CONNECTED in {:?}: send_mtu={:?} recv_mtu={:?} security={:?}",
        t0.elapsed(),
        stream.as_ref().send_mtu(),
        stream.as_ref().recv_mtu(),
        stream.as_ref().security()
    );

    // As the connecting client we speak first -- exactly as the Pixel did when
    // it connected to us: a u32-framed RequestDataConnection command. (Reading
    // first got us "connection reset by peer": the phone-as-server drops a
    // client that goes silent after the CoC opens.)
    let mut leftover: Vec<u8> = Vec::new();
    send_frame(&mut stream, &[CMD_REQUEST_DATA_CONNECTION]).await?;
    let mut ready = false;
    let handshake_deadline = Instant::now() + Duration::from_secs(10);
    while !ready {
        let msg = tokio::select! {
            _ = tokio::time::sleep_until(handshake_deadline) => {
                println!("  no DATA_CONNECTION_READY within 10s");
                break;
            }
            r = read_frame(&mut stream, &mut leftover) => match r? {
                Some(m) => m,
                None => { println!("NO-GO: phone closed during command handshake"); return Ok(()); }
            }
        };
        println!(
            "  rx frame {}  [{}]",
            hex::encode(&msg[..msg.len().min(48)]),
            describe(&msg)
        );
        if msg == [CMD_RESPONSE_DATA_CONNECTION_READY] {
            ready = true;
        }
    }

    stream.write_all(&intro_frame()).await?;
    println!("  tx INTRODUCTION");

    println!("  listening 15s for whatever the phone sends next ...");
    let listen_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let msg = tokio::select! {
            _ = tokio::time::sleep_until(listen_deadline) => break,
            r = read_frame(&mut stream, &mut leftover) => match r {
                Ok(Some(m)) => m,
                Ok(None) => { println!("  phone closed the channel"); break; }
                Err(e) => { println!("  read error: {e}"); break; }
            }
        };
        println!(
            "  rx frame {}  [{}]",
            hex::encode(&msg[..msg.len().min(64)]),
            describe(&msg)
        );
    }

    println!(
        "\nRESULT: {}",
        if ready {
            "GO -- CoC connect + data-connection handshake accepted by the phone"
        } else {
            "PARTIAL -- CoC connected but the phone did not acknowledge the data-connection request (dialect?)"
        }
    );
    Ok(())
}
