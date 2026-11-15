//! Windows named-pipe transport for the NAF/1 client (Lattice spec §7).
//!
//! Connects to `\\\\.\\pipe\\NAI.AmbientFabric.NAF1.<sid-hash>`. Uses tokio's
//! native named-pipe support (`tokio::net::windows::named_pipe`), which drives
//! the pipe with real overlapped I/O on the reactor (IOCP). This replaces the
//! previous hand-rolled `std::fs::File` read inside `poll_read`, which BLOCKED
//! a Tokio worker on a stalled pipe (P0-17). `<sid-hash>` is a stable 6-char
//! hash of the user identity so the pipe name is per-user and never globally
//! writable.

use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

/// A thin alias so callers keep the same `NamedPipeStream` type name.
pub struct NamedPipeStream {
    /// tokio named-pipe client = genuinely async (overlapped I/O / IOCP).
    pipe: NamedPipeClient,
}

impl NamedPipeStream {
    /// Connect to the named pipe (e.g. `\\.\pipe\NAI.AmbientFabric.NAF1.<hash>`).
    pub fn connect(pipe_name: &str) -> std::io::Result<Self> {
        // tokio opens the pipe with FILE_FLAG_OVERLAPPED under the hood and
        // integrates it with the reactor; no manual overlapped plumbing needed.
        let pipe = ClientOptions::new()
            .read(true)
            .write(true)
            .open(pipe_name)?;
        Ok(Self { pipe })
    }

    /// Per-user NAF/1 pipe name derived from the current user identity.
    pub fn pipe_name() -> String {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into());
        let sid_hash = quick_hash(user.as_bytes());
        format!(r"\\.\pipe\NAI.AmbientFabric.NAF1.{sid_hash}")
    }
}

fn quick_hash(bytes: &[u8]) -> String {
    let h = blake3::hash(bytes);
    let hex = h.to_hex().to_string();
    hex[..6].to_string()
}

impl AsyncRead for NamedPipeStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.pipe).poll_read(cx, buf)
    }
}

impl AsyncWrite for NamedPipeStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.pipe).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.pipe).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.pipe).poll_shutdown(cx)
    }
}