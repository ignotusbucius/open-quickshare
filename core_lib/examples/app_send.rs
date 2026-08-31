// Headless driver for the APP's real send engine — no reimplementation.
//
// Runs the same `RQS` service object the Tauri app runs — ONCE for a whole
// batch of files, like the long-lived app process — with its discovery
// (sender-side BLE advertising included), the same `SendInfo` channel the
// frontend feeds, and manager.rs's scan/dial ladder and medium selection.
// Starting/stopping the BLE stack per file churns BlueZ (advert register/
// unregister races slow the link until the phone's handshake timer fires),
// which is exactly what a per-file process did — so we don't.
//
//   app_send <file> [<file> ...]
//   STEP=1                     prompt on stdin before each file (batch mode)
//   TARGET_NAME=<substring>    pick the phone by name (case-insensitive)
//   PREFER=wifi|ble            when both mDNS and BLE endpoints show up
//   RQS_DEVICE_NAME=<name>     how the phone sees this PC (default: OQS Interop TX)
//
// stdout carries the interaction/protocol lines (READY/RESULT/pin), flushed
// per line so a pipe sees them immediately; all tracing goes to stderr.
#[macro_use]
extern crate log;

use std::io::Write as _;
use std::time::Duration;

use rqs_lib::channel::Message;
use rqs_lib::{EndpointInfo, OutboundPayload, RQS, SendInfo, TransferState, Visibility};
use tokio::sync::{broadcast, watch};
use tracing_subscriber::EnvFilter;

fn say(s: &str) {
    let mut o = std::io::stdout();
    let _ = writeln!(o, "{s}");
    let _ = o.flush();
}

async fn wait_endpoint(
    ep_rx: &watch::Receiver<Option<EndpointInfo>>,
    max: Duration,
) -> Option<EndpointInfo> {
    let deadline = tokio::time::Instant::now() + max;
    while tokio::time::Instant::now() < deadline {
        if let Some(ei) = ep_rx.borrow().clone() {
            return Some(ei);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    None
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
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
    let step = std::env::var("STEP").as_deref() == Ok("1");
    let target = std::env::var("TARGET_NAME")
        .unwrap_or_default()
        .to_lowercase();
    let prefer = std::env::var("PREFER").unwrap_or_default();

    // The real service — same constructor call the app makes in main.rs.
    let mut rqs = RQS::new(
        Visibility::Visible,
        None,
        None,
        Some(std::env::var("RQS_DEVICE_NAME").unwrap_or_else(|_| "OQS Interop TX".to_string())),
    );
    let (sender_file, _ble_rx) = rqs.run().await?;

    // Discovery — the exact thing the app's send screen starts. It stays up
    // for the whole batch; a background task keeps the freshest matching
    // endpoint in a slot.
    let (etx, mut erx) = broadcast::channel::<EndpointInfo>(20);
    rqs.discovery(etx)?;
    let (ep_tx, ep_rx) = watch::channel::<Option<EndpointInfo>>(None);
    {
        let target = target.clone();
        let prefer = prefer.clone();
        tokio::spawn(async move {
            while let Ok(ei) = erx.recv().await {
                let name = ei.name.clone().unwrap_or_default().to_lowercase();
                if !target.is_empty() && !name.contains(&target) {
                    continue;
                }
                if ei.present == Some(false) {
                    continue;
                }
                let is_ble = ei.ble_addr.is_some();
                if !is_ble && (ei.ip.is_none() || ei.port.is_none()) {
                    continue;
                }
                if prefer == "ble" && !is_ble {
                    continue;
                }
                if prefer == "wifi" && is_ble && ep_tx.borrow().as_ref().is_some_and(|c| c.ble_addr.is_none()) {
                    continue; // keep the Wi-Fi endpoint we already hold
                }
                let _ = ep_tx.send(Some(ei));
            }
        });
    }
    info!(
        "discovering (filter: '{}')",
        if target.is_empty() { "<any>" } else { &target }
    );

    let mut all_ok = true;
    for f in &files {
        if step {
            let base = std::path::Path::new(f)
                .file_name()
                .map(|b| b.to_string_lossy().into_owned())
                .unwrap_or_else(|| f.clone());
            say(&format!(
                ">>> NEXT: {base} — press Enter to send it (s+Enter to skip)"
            ));
            let line = tokio::task::spawn_blocking(|| {
                let mut l = String::new();
                let _ = std::io::stdin().read_line(&mut l);
                l
            })
            .await
            .unwrap_or_default();
            if line.trim() == "s" {
                say(&format!("RESULT|{f}|Skipped"));
                continue;
            }
        }

        let Some(ei) = wait_endpoint(&ep_rx, Duration::from_secs(30)).await else {
            say(&format!("RESULT|{f}|NoReceiver"));
            all_ok = false;
            continue;
        };
        let is_ble = ei.ble_addr.is_some();
        let si = SendInfo {
            id: ei.id.clone(),
            name: ei.name.clone().unwrap_or_default(),
            addr: if is_ble {
                String::new()
            } else {
                format!(
                    "{}:{}",
                    ei.ip.clone().unwrap_or_default(),
                    ei.port.clone().unwrap_or_default()
                )
            },
            ob: OutboundPayload::Files(vec![f.clone()]),
            ble: is_ble,
        };
        let sid = si.id.clone();
        info!("sending through the app engine: {:?}", si);

        // Fresh subscription per file so an earlier transfer's backlog can't
        // bleed into this one's state following.
        let mut rx = rqs.message_sender.subscribe();
        sender_file.send(si).await?;

        let overall = tokio::time::Instant::now() + Duration::from_secs(300);
        let mut final_state: Option<TransferState> = None;
        let mut last_pin: Option<String> = None;
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
                            if last_pin.as_deref() != Some(pin.as_str()) {
                                last_pin = Some(pin.clone());
                                say(&format!("    PIN on both screens should be: {pin}"));
                            }
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

        match final_state {
            Some(TransferState::Finished) => {
                say(&format!("RESULT|{f}|Finished"));
            }
            Some(s) => {
                say(&format!("RESULT|{f}|{s:?}"));
                all_ok = false;
            }
            None => {
                say(&format!("RESULT|{f}|Timeout"));
                all_ok = false;
            }
        }
    }

    // Bounded teardown + hard exit: mdns/bluer shutdown can wedge, and the
    // results are already on stdout.
    rqs.stop_discovery();
    let _ = tokio::time::timeout(Duration::from_secs(8), rqs.stop()).await;
    std::process::exit(if all_ok { 0 } else { 1 });
}
