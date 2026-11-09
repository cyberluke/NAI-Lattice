//! Async transport abstraction for the NAF/1 client.
//!
//! The client is generic over `S: AsyncRead + AsyncWrite`. This module boxes
//! two concrete transports so higher-level code can hold a single `Box<dyn
//! NafTransport>`: the Windows named pipe (production) and an in-memory duplex
//! (tests / in-process).

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// A boxed async byte stream usable by the NAF client.
pub type NafTransport = Box<dyn NafTransportTrait>;

/// Object-safe transport trait (async read/write without generics at the call site).
pub trait NafTransportTrait: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> NafTransportTrait for T {}

/// Convenience constructor for tests: build an in-memory duplex pair.
pub fn duplex() -> (tokio::io::DuplexStream, tokio::io::DuplexStream) {
    tokio::io::duplex(64 * 1024)
}

/// A TCP transport (used only in non-Windows/dev harnesses; NAF/1 forbids TCP
/// in production but it is convenient for local smoke tests).
pub async fn tcp_connect(addr: &str) -> std::io::Result<TcpStream> {
    TcpStream::connect(addr).await
}

/// Read a bounded amount (unused by the codec; kept for completeness).
#[allow(dead_code)]
async fn _drain(mut s: impl AsyncRead + Unpin, mut out: &mut Vec<u8>) -> std::io::Result<usize> {
    s.read_to_end(&mut out).await
}

#[allow(dead_code)]
async fn _flush(mut s: impl AsyncWrite + Unpin) -> std::io::Result<()> {
    s.flush().await
}