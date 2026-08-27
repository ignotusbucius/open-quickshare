// Stage 1+2 end-to-end test: SEND a file to a phone over BLE, no Wi-Fi.
//
//   tx_send <file> [<file> ...]
//   TARGET_ADDR=AA:BB:CC:DD:EE:FF tx_send <file>     pick a specific phone
//   PACKET_SEND_MEDIUMS=10,5 tx_send <file>          override advertised mediums
//
// Scans 0xFEF3 for a receiver (phone on its "Everyone" screen), opens the LE
// CoC to its PSM, and drives the real `OutboundRequest` handshake over that
// stream -- the same code path the Wi-Fi send uses, only the transport differs.
#[macro_use]
extern crate log;

use std::time::Duration;

use rqs_lib::channel::{ChannelMessage, Message};
use rqs_lib::hdl::{OutboundPayload, OutboundRequest, TransferState, dial, scan_once};
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
        eprintln!("usage: tx_send <file> [<file> ...]");
        std::process::exit(2);
    }
    for f in &files {
        if !std::path::Path::new(f).is_file() {
            eprintln!("not a file: {f}");
            std::process::exit(2);
        }
    }
    // TARGET_NAME picks a specific phone by its device name (needed when several
    // phones are in receive mode at once).
    let target_name = std::env::var("TARGET_NAME").ok();
    // WIFI_LAN(5) + BLE_L2CAP(10): advertise Wi-Fi so the phone can offer an upgrade.
    let mediums: Vec<i32> = std::env::var("PACKET_SEND_MEDIUMS")
        .ok()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![5, 10]);

    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;
    info!("scanning for a receiver on {} ...", adapter.name());

    let mut target = None;
    for attempt in 1..=6 {
        let found = scan_once(&adapter, Duration::from_secs(20), target_name.as_deref()).await?;
        target = found.into_iter().next();
        if target.is_some() {
            break;
        }
        info!("no receiver yet (attempt {attempt}/6); put the phone on Quick Share 'Everyone'");
    }
    let Some(target) = target else {
        eprintln!("no receiver found - is the phone on its Quick Share receive screen?");
        std::process::exit(1);
    };
    info!(
        "sending to {} ({:?}) over BLE psm {}",
        target.addr, target.rdi.name, target.psm
    );

    let stream = dial(&adapter, &target).await?;

    let (sender, mut watch) = broadcast::channel::<ChannelMessage>(50);
    let id = target.addr.to_string();
    tokio::spawn(async move {
        while let Ok(cm) = watch.recv().await {
            if let Message::Client(mc) = &cm.msg {
                if let Some(state) = &mc.state {
                    info!("[state] {:?}", state);
                }
                if let Some(md) = &mc.metadata {
                    if let Some(pin) = &md.pin_code {
                        info!("[pin] confirm this matches the phone: {pin}");
                    }
                }
            }
        }
    });

    let endpoint_id: [u8; 4] = rand::random();
    let mut or = OutboundRequest::new(
        endpoint_id,
        stream,
        id.clone(),
        sender,
        OutboundPayload::Files(files),
        target.rdi.clone(),
    );
    or.set_mediums(mediums);

    or.send_connection_request().await?;
    or.send_ukey2_client_init().await?;

    loop {
        match or.handle().await {
            Ok(()) => {
                if matches!(
                    or.state.state,
                    TransferState::Finished | TransferState::Cancelled | TransferState::Rejected
                ) {
                    break;
                }
            }
            Err(e) => {
                if e.to_string().contains("NotAnError") {
                    break;
                }
                error!("send failed in state {:?}: {e}", or.state.state);
                std::process::exit(1);
            }
        }
    }

    info!("done: final state {:?}", or.state.state);
    Ok(())
}
