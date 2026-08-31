use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast::Sender;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;

use crate::channel::{self, ChannelMessage, MessageClient, TransferKind};
use crate::errors::AppError;
use crate::hdl::{InboundRequest, OutboundPayload, OutboundRequest, TransferState};
use crate::utils::RemoteDeviceInfo;

const INNER_NAME: &str = "TcpServer";

/// Payloads at or below this ride BLE without a Wi-Fi upgrade (matches the
/// send path's own pure-BLE size guard in `outbound.rs`).
#[cfg(all(feature = "experimental", target_os = "linux"))]
const SMALL_SEND_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SendInfo {
    pub id: String,
    pub name: String,
    pub addr: String,
    pub ob: OutboundPayload,
    /// When set, send over BLE instead of Wi-Fi/TCP: the recipient is a phone
    /// discovered over BLE. The send path re-scans for it by `name` (its LE
    /// address and PSM rotate) and dials the fresh target. `addr`/`id` may be a
    /// placeholder in this case. Ignored on non-Linux / non-experimental builds.
    #[serde(default)]
    pub ble: bool,
}

pub struct TcpServer {
    endpoint_id: [u8; 4],
    tcp_listener: TcpListener,
    sender: Sender<ChannelMessage>,
    connect_receiver: Receiver<SendInfo>,
}

impl TcpServer {
    pub fn new(
        endpoint_id: [u8; 4],
        tcp_listener: TcpListener,
        sender: Sender<ChannelMessage>,
        connect_receiver: Receiver<SendInfo>,
    ) -> Result<Self, anyhow::Error> {
        Ok(Self {
            endpoint_id,
            tcp_listener,
            sender,
            connect_receiver,
        })
    }

    pub async fn run(&mut self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        info!("{INNER_NAME}: service starting");

        loop {
            let cctk = ctk.clone();

            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("{INNER_NAME}: tracker cancelled, breaking");
                    break;
                }
                Some(i) = self.connect_receiver.recv() => {
                    info!("{INNER_NAME}: connect_receiver: got {:?}", i);
                    let report_id = i.id.clone();
                    if let Err(e) = self.connect(cctk, i).await {
                        error!("{INNER_NAME}: error sending: {}", e.to_string());
                        // Surface the failure -- a click that only dies in the
                        // log looks like a dead button in the UI.
                        let _ = self.sender.send(ChannelMessage {
                            id: report_id,
                            msg: channel::Message::Client(MessageClient {
                                kind: TransferKind::Outbound,
                                state: Some(TransferState::Disconnected),
                                metadata: Default::default(),
                            }),
                        });
                    }
                }
                r = self.tcp_listener.accept() => {
                    match r {
                        Ok((socket, remote_addr)) => {
                            trace!("{INNER_NAME}: new client: {remote_addr}");
                            let esender = self.sender.clone();
                            let csender = self.sender.clone();

                            tokio::spawn(async move {
                                // Detect a Wi-Fi bandwidth-upgrade CLIENT_INTRODUCTION so it can be
                                // routed to its in-flight BLE session (Milestone A: observe only).
                                #[cfg(all(feature = "experimental", target_os = "linux"))]
                                {
                                    let mut pbuf = [0u8; 512];
                                    if let Ok(n) = socket.peek(&mut pbuf).await {
                                        if let Some(eid) = crate::hdl::peek_client_introduction(&pbuf[..n]) {
                                            info!("{INNER_NAME}: BWU CLIENT_INTRODUCTION from {remote_addr} (endpoint_id={eid}) — routing TODO");
                                            return;
                                        }
                                    }
                                }
                                let mut ir = InboundRequest::new(socket, remote_addr.to_string(), csender);

                                loop {
                                    match ir.handle().await {
                                        Ok(_) => {},
                                        Err(e) => match e.downcast_ref() {
                                            Some(AppError::NotAnError) => break,
                                            None => {
                                                if ir.state.state == TransferState::Initial {
                                                    break;
                                                }

                                                if ir.state.state != TransferState::Finished {
                                                    let _ = esender.send(ChannelMessage {
                                                        id: remote_addr.to_string(),
                                                        msg: channel::Message::Client(MessageClient {
                                                            kind: TransferKind::Inbound,
                                                            state: Some(TransferState::Disconnected),
                                                            metadata: Default::default()
                                                        }),
                                                    });
                                                }
                                                error!("{INNER_NAME}: error while handling client: {e} ({:?})", ir.state.state);
                                                break;
                                            }
                                        },
                                    }
                                }
                            });
                        },
                        Err(err) => {
                            error!("{INNER_NAME}: error accepting: {}", err);
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// To be called inside a separate task if we want to handle concurrency
    pub async fn connect(&self, ctk: CancellationToken, si: SendInfo) -> Result<(), anyhow::Error> {
        #[cfg(all(feature = "experimental", target_os = "linux"))]
        if si.ble {
            return self.connect_ble(ctk, si).await;
        }

        debug!("{INNER_NAME}: Connecting to: {}", si.addr);
        // A stale mDNS endpoint (device left the network) otherwise spins for
        // the OS's full connect timeout (~40s) with the UI stuck on it.
        let socket = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            TcpStream::connect(si.addr.clone()),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "connect to {} timed out — the device may have left the network",
                si.addr
            )
        })??;

        let mut or = OutboundRequest::new(
            self.endpoint_id,
            socket,
            si.id,
            self.sender.clone(),
            si.ob,
            RemoteDeviceInfo {
                device_type: crate::DeviceType::Unknown,
                name: si.name,
            },
        );

        // Send connection request
        or.send_connection_request().await?;
        // Send UKEY init
        or.send_ukey2_client_init().await?;

        self.drive_outbound(ctk, or, si.addr).await;
        Ok(())
    }

    /// Send over BLE to a phone discovered on its receive screen. Re-scans for
    /// it by name (its LE address and PSM rotate), dials the fresh target, then
    /// drives the same outbound handshake over the L2CAP-backed stream.
    #[cfg(all(feature = "experimental", target_os = "linux"))]
    async fn connect_ble(&self, ctk: CancellationToken, si: SendInfo) -> Result<(), anyhow::Error> {
        use crate::hdl::{dial, scan_once};
        use std::time::Duration;

        debug!("{INNER_NAME}: BLE send to {:?}", si.name);
        let session = bluer::Session::new().await?;
        let adapter = session.default_adapter().await?;
        adapter.set_powered(true).await?;

        // Airtime for the scan/connect; also stops our own receiver scanner.
        let _suppressor = crate::hdl::BleScanSuppressor::new();

        // The phone rotates its LE address (and PSM) every few minutes, and a
        // scan can still surface the pre-rotation address from BlueZ's cache.
        // A failed dial retries with a fresh scan. Addresses that already
        // failed are deprioritized but NOT banned: a dial failure is usually
        // transient (BlueZ mid-wind-down after the scan), not proof the
        // address is dead -- the very same address often connects on the
        // next, settled attempt.
        let mut tried: Vec<bluer::Address> = Vec::new();
        let mut connected = None;
        let mut last_err: Option<anyhow::Error> = None;
        for round in 1..=3u8 {
            if round > 1 {
                // Give BlueZ time to wind the previous discovery/dial down --
                // starting a new scan immediately fails with "operation
                // already in progress".
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            let targets = match scan_once(&adapter, Duration::from_secs(20), Some(&si.name)).await {
                Ok(t) => t,
                Err(e) => {
                    warn!("{INNER_NAME}: scan round {round} failed ({e}); retrying");
                    last_err = Some(e);
                    continue;
                }
            };
            let (fresh, retry): (Vec<_>, Vec<_>) =
                targets.into_iter().partition(|t| !tried.contains(&t.addr));
            let Some(target) = fresh
                .into_iter()
                .next()
                .or_else(|| retry.into_iter().next())
            else {
                last_err = Some(anyhow::anyhow!(
                    "{} is no longer advertising over BLE",
                    si.name
                ));
                continue;
            };
            match dial(&adapter, &target).await {
                Ok(stream) => {
                    connected = Some((stream, target.rdi));
                    break;
                }
                Err(e) => {
                    warn!(
                        "{INNER_NAME}: dial {} failed on round {round} ({e}); re-scanning",
                        target.addr
                    );
                    tried.push(target.addr);
                    last_err = Some(e);
                }
            }
        }
        let Some((stream, rdi)) = connected else {
            return Err(last_err.unwrap_or_else(|| anyhow::anyhow!("BLE connect failed")));
        };

        // A payload that fits comfortably over BLE never needs a Wi-Fi
        // upgrade: a few bytes of pasted text shouldn't cost a multi-second
        // Wi-Fi Direct join that also drops this machine off its own network.
        let OutboundPayload::Files(files) = &si.ob;
        let total_bytes: u64 = files
            .iter()
            .filter_map(|f| std::fs::metadata(f).ok().map(|m| m.len()))
            .sum();

        // The UI knows this transfer by `si.id` (the `ble://<name>` endpoint
        // id) -- a Disconnected report under any other id renders as a
        // detached "Unknown" card.
        let report_id = si.id.clone();
        let mut or = OutboundRequest::new(
            self.endpoint_id,
            stream,
            si.id,
            self.sender.clone(),
            si.ob,
            rdi,
        );
        if total_bytes <= SMALL_SEND_BYTES {
            // BLE_L2CAP (10) only: the phone (advertiser) has no Wi-Fi medium
            // in common with us, so it never offers an upgrade.
            info!("{INNER_NAME}: {total_bytes}-byte payload; BLE only, no Wi-Fi upgrade");
            or.set_mediums(vec![10]);
        } else {
            // Advertise WIFI_LAN (5) + WIFI_DIRECT (8) + WIFI_HOTSPOT (3) +
            // BLE_L2CAP (10). The phone (advertiser) picks the best and offers it:
            // same LAN → WIFI_LAN (we connect to its ip:port); no shared LAN →
            // WIFI_DIRECT (it hosts its own group, we join it). Either way the
            // payload leaves BLE for Wi-Fi speed.
            or.set_mediums(vec![5, 8, 3, 10]);
        }

        or.send_connection_request().await?;
        or.send_ukey2_client_init().await?;

        self.drive_outbound(ctk, or, report_id).await;
        Ok(())
    }

    /// Drives an outbound transfer to completion, reporting a Disconnected state
    /// on an unexpected error. Generic over the transport (`TcpStream` for
    /// Wi-Fi, the BLE-backed stream for L2CAP).
    async fn drive_outbound<S>(
        &self,
        ctk: CancellationToken,
        mut or: OutboundRequest<S>,
        report_id: String,
    ) where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + crate::hdl::WifiUpgradable,
    {
        loop {
            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("{INNER_NAME}: tracker cancelled, breaking");
                    break;
                },
                r = or.handle() => {
                    if let Err(e) = r {
                        match e.downcast_ref() {
                            Some(AppError::NotAnError) => break,
                            None => {
                                if or.state.state == TransferState::Initial {
                                    break;
                                }

                                if or.state.state != TransferState::Finished && or.state.state != TransferState::Cancelled {
                                    let _ = self.sender.clone().send(ChannelMessage {
                                        id: report_id.clone(),
                                        msg: channel::Message::Client(MessageClient {
                                            kind: TransferKind::Outbound,
                                            state: Some(TransferState::Disconnected),
                                            metadata: Default::default()
                                        }),
                                    });
                                }
                                error!("{INNER_NAME}: error while handling client: {e} ({:?})", or.state.state);
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
}
