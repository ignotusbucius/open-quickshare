// Headless driver for the APP's real send engine — no reimplementation.
//
// Runs the same `RQS` service object the Tauri app runs, starts discovery
// (which also advertises us as a sender over BLE, exactly like the app's send
// screen), waits for the target endpoint to be discovered, then feeds the
// manager the same `SendInfo` the frontend sends and follows the transfer's
// state machine to a terminal state. Everything between — scan/dial ladder,
// medium selection by payload size, identity, upgrades — is the app's own
// code in `manager.rs`/`outbound.rs`, untouched.
//
//   app_send <file> [<file> ...]
//   TARGET_NAME=<substring>    pick the phone by name (case-insensitive)
//   PREFER=wifi|ble            when both mDNS and BLE endpoints show up
//   RQS_DEVICE_NAME=<name>     how the phone sees this PC (default: OQS Interop TX)
#[macro_use]
extern crate log;

use std::time::Duration;

use rqs_lib::channel::Message;
use rqs_lib::{EndpointInfo, OutboundPayload, RQS, SendInfo, TransferState, Visibility};
use tokio::sync::broadcast;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    tracing_subscriber::fmt()
        .with_env_filter(if std::env::var("RUST_LOG").is_ok() {
            EnvFilter::builder().from_env_lossy()
        } else {
            EnvFilter::builder().parse_lossy(
                "info,rqs_lib=debug,mdns_sd=error,polling=error,neli=error,bluez_async=error,btleplug=error",
            )
        })
        .init();

    let files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        eprintln!("usage: app_send <file> [<file> ...]");
        std::process::exit(2);
    }
    for f in &files {
        if !std::path::Path::new(f).is_file() {
            eprintln!("not a file: {f}");
            std::process::exit(2);
        }
    }
    let target = std::env::var("TARGET_NAME")
        .unwrap_or_default()
        .to_lowercase();
    let prefer = std::env::var("PREFER").unwrap_or_default();
    let prefer_wifi = prefer == "wifi";
    let prefer_ble = prefer == "ble";

    // The real service — same constructor call the app makes in main.rs.
    let mut rqs = RQS::new(
        Visibility::Visible,
        None,
        None,
        Some(std::env::var("RQS_DEVICE_NAME").unwrap_or_else(|_| "OQS Interop TX".to_string())),
    );
    let (sender_file, _ble_rx) = rqs.run().await?;

    // Discovery — the exact thing the app's send screen starts.
    let (etx, mut erx) = broadcast::channel::<EndpointInfo>(20);
    rqs.discovery(etx)?;
    info!(
        "discovering (filter: '{}') — put the phone on its Quick Share receive screen",
        if target.is_empty() { "<any>" } else { &target }
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let mut chosen: Option<EndpointInfo> = None;
    let mut fallback: Option<EndpointInfo> = None;
    while tokio::time::Instant::now() < deadline {
        let ei = match tokio::time::timeout(Duration::from_secs(1), erx.recv()).await {
            Ok(Ok(ei)) => ei,
            _ => continue,
        };
        let name = ei.name.clone().unwrap_or_default();
        if !target.is_empty() && !name.to_lowercase().contains(&target) {
            continue;
        }
        if ei.present == Some(false) {
            continue;
        }
        let is_ble = ei.ble_addr.is_some();
        if !is_ble && (ei.ip.is_none() || ei.port.is_none()) {
            continue;
        }
        if prefer_ble && !is_ble {
            continue;
        }
        if prefer_wifi && is_ble {
            // hold as fallback, keep waiting for an mDNS endpoint
            if fallback.is_none() {
                fallback = Some(ei);
            }
            continue;
        }
        chosen = Some(ei);
        break;
    }
    let ei = match chosen.or(fallback) {
        Some(e) => e,
        None => {
            error!("no matching receiver found — is the phone on its receive screen?");
            rqs.stop_discovery();
            rqs.stop().await;
            std::process::exit(1);
        }
    };

    let is_ble = ei.ble_addr.is_some();
    let si = SendInfo {
        id: ei.id.clone(),
        name: ei.name.clone().unwrap_or_default(),
        addr: if is_ble {
            String::new()
        } else {
            format!("{}:{}", ei.ip.clone().unwrap(), ei.port.clone().unwrap())
        },
        ob: OutboundPayload::Files(files),
        ble: is_ble,
    };
    let sid = si.id.clone();
    info!("sending through the app engine: {:?}", si);

    let mut rx = rqs.message_sender.subscribe();
    sender_file.send(si).await?;

    let overall = tokio::time::Instant::now() + Duration::from_secs(300);
    let mut final_state: Option<TransferState> = None;
    while tokio::time::Instant::now() < overall {
        let cm = match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Ok(cm)) => cm,
            Ok(Err(_)) => break,
            Err(_) => continue,
        };
        if cm.id != sid {
            continue;
        }
        if let Message::Client(mc) = &cm.msg {
            if let Some(st) = &mc.state {
                info!("[state] {:?}", st);
                if let Some(md) = &mc.metadata {
                    if let Some(pin) = &md.pin_code {
                        info!("[pin] confirm this matches the phone: {pin}");
                    }
                }
                if matches!(
                    st,
                    TransferState::Finished
                        | TransferState::Cancelled
                        | TransferState::Rejected
                        | TransferState::Disconnected
                ) {
                    final_state = Some(st.clone());
                    break;
                }
            }
        }
    }

    // Verdict FIRST — service teardown (mdns/bluer) can wedge, and the caller
    // needs the result line even if cleanup stalls. Then bounded cleanup and a
    // hard exit so lingering non-tokio threads can't keep the process alive.
    let code = match final_state {
        Some(TransferState::Finished) => {
            info!("final state Finished");
            0
        }
        Some(s) => {
            error!("final state {:?}", s);
            1
        }
        None => {
            error!("timed out without reaching a terminal state");
            1
        }
    };
    rqs.stop_discovery();
    let _ = tokio::time::timeout(Duration::from_secs(8), rqs.stop()).await;
    std::process::exit(code);
}
