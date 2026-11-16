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

    /// Per-user NAF/1 pipe name.
    ///
    /// Discovery order (single-fabric coordination):
    ///   1. The host-published canonical endpoint manifest
    ///      (`%LOCALAPPDATA%\NAI\Lattice\v1\endpoint.json`, written atomically by the
    ///      host and carrying `canonical_pipe` = `NAF1.<blake3(SID)[..6]>` + a
    ///      `legacy_alias`). Reading it guarantees ALL fabric clients resolve the
    ///      SAME canonical SID pipe — no client fabricates a divergent name.
    ///   2. Fallback to the legacy lowercase-username alias (`NAF1.<blake3(lower(USERNAME))[..6]>`),
    ///      which the host also serves from the same singleton.
    /// The OLD behavior (blake3 of the RAW case-preserved `USERNAME`) produced a
    /// third name the host NEVER exposes, so clients using it could not reach the
    /// shared fabric at all.
    pub fn pipe_name() -> String {
        // 1. Canonical manifest (authoritative SID endpoint).
        if let Some(endpoint) = std::env::var("NAI_FABRIC_PIPE").ok().filter(|s| !s.is_empty()) {
            return endpoint;
        }
        let local = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from);
        let manifest = local
            .as_deref()
            .map(|p| p.join("NAI").join("Lattice").join("v1").join("endpoint.json"));
        if let Some(path) = manifest {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(p) = v.get("canonical_pipe").and_then(|x| x.as_str()) {
                        if !p.is_empty() {
                            return p.to_string();
                        }
                    }
                }
            }
        }
        // 2. Legacy lowercase-username alias (host serves it from the SAME host).
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into()).to_lowercase();
        let user_hash = quick_hash(user.as_bytes());
        format!(r"\\.\pipe\NAI.AmbientFabric.NAF1.{user_hash}")
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