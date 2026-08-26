use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};
use tokio::net::TcpStream;

/// A stream that can be swapped between the BLE weave channel and a TCP socket,
/// used for the Wi-Fi bandwidth upgrade: the receive handshake reads/writes
/// through this, so migrating the transport (BLE → TCP) keeps all crypto/sequence
/// state intact — only the underlying socket changes.
#[derive(Debug)]
pub enum MigratableStream {
    /// BLE weave data socket (one half of an in-memory duplex).
    Ble(DuplexStream),
    /// BLE L2CAP connection-oriented channel.
    L2cap(bluer::l2cap::Stream),
    /// Wi-Fi-LAN TCP socket (after a bandwidth upgrade).
    Tcp(TcpStream),
}

impl crate::hdl::WifiUpgradable for MigratableStream {
    fn is_low_bandwidth(&self) -> bool {
        // TCP (Wi-Fi) is fast; the BLE variants are slow.
        !matches!(self, MigratableStream::Tcp(_))
    }
    fn upgrade_to_tcp(&mut self, tcp: TcpStream) -> bool {
        *self = MigratableStream::Tcp(tcp);
        true
    }
}

impl AsyncRead for MigratableStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MigratableStream::Ble(s) => Pin::new(s).poll_read(cx, buf),
            MigratableStream::L2cap(s) => Pin::new(s).poll_read(cx, buf),
            MigratableStream::Tcp(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MigratableStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            MigratableStream::Ble(s) => Pin::new(s).poll_write(cx, buf),
            MigratableStream::L2cap(s) => Pin::new(s).poll_write(cx, buf),
            MigratableStream::Tcp(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MigratableStream::Ble(s) => Pin::new(s).poll_flush(cx),
            MigratableStream::L2cap(s) => Pin::new(s).poll_flush(cx),
            MigratableStream::Tcp(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MigratableStream::Ble(s) => Pin::new(s).poll_shutdown(cx),
            MigratableStream::L2cap(s) => Pin::new(s).poll_shutdown(cx),
            MigratableStream::Tcp(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}
