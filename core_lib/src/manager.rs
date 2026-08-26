use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast::Sender;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;

use crate::channel::{self, ChannelMessage, MessageClient, TransferKind};
use crate::errors::AppError;
use crate::hdl::{InboundRequest, OutboundPayload, OutboundRequest, TransferState};
use crate::utils::RemoteDeviceInfo;

const INNER_NAME: &str = "TcpServer";

#[derive(Debug, Clone)]
pub struct SendInfo {
    pub id: String,
    pub name: String,
    pub addr: String,
    pub ob: OutboundPayload,
    /// When set, send over BLE instead of Wi-Fi/TCP: the recipient is a phone
    /// discovered over BLE. The send path re-scans for it by `name` (its LE
    /// address and PSM rotate) and dials the fresh target. `addr`/`id` may be a
    /// placeholder in this case. Ignored on non-Linux / non-experimental builds.
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
                    if let Err(e) = self.connect(cctk, i).await {
                        error!("{INNER_NAME}: error sending: {}", e.to_string());
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
        let socket = TcpStream::connect(si.addr.clone()).await?;

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

        let targets = scan_once(&adapter, Duration::from_secs(20), Some(&si.name)).await?;
        let target = targets
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("{} is no longer advertising over BLE", si.name))?;

        let stream = dial(&adapter, &target).await?;

        let mut or = OutboundRequest::new(
            self.endpoint_id,
            stream,
            si.id,
            self.sender.clone(),
            si.ob,
            target.rdi,
        );
        // Advertise WIFI_LAN (5) + WIFI_DIRECT (8) + WIFI_HOTSPOT (3) +
        // BLE_L2CAP (10). The phone (advertiser) picks the best and offers it:
        // same LAN → WIFI_LAN (we connect to its ip:port); no shared LAN →
        // WIFI_DIRECT (it hosts its own group, we join it). Either way the
        // payload leaves BLE for Wi-Fi speed.
        or.set_mediums(vec![5, 8, 3, 10]);

        or.send_connection_request().await?;
        or.send_ukey2_client_init().await?;

        self.drive_outbound(ctk, or, si.name).await;
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
