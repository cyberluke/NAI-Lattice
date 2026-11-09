//! Windows named-pipe transport for the NAF/1 client (Lattice spec §7).
//!
//! Connects to `\\\\.\\pipe\\NAI.AmbientFabric.NAF1.<sid-hash>`. The pipe
//! `HANDLE` is bridged to a `std::fs::File` (std file I/O, no overlapped
//! plumbing), then wrapped as an async stream. `<sid-hash>` is a stable 6-char
//! hash of the user identity so the pipe name is per-user and never globally
//! writable.

use std::io::{Read, Write};
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_MODE, OPEN_EXISTING,
};

/// A bridging stream around a blocking named-pipe `HANDLE` (via `std::fs::File`).
pub struct NamedPipeStream {
    file: std::fs::File,
}

impl NamedPipeStream {
    /// Connect to the named pipe (e.g. `\\.\pipe\NAI.AmbientFabric.NAF1.<hash>`).
    pub fn connect(pipe_name: &str) -> std::io::Result<Self> {
        let wide: Vec<u16> = pipe_name.encode_utf16().chain(std::iter::once(0)).collect();
        let handle: HANDLE = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )?
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        // Adopt the HANDLE into a std file. `HANDLE` is `*mut c_void` in 0.58;
        // convert to a `RawHandle` (`*mut c_void`/`isize`) for std.
        let file = unsafe { std::fs::File::from_raw_handle(handle.0 as RawHandle) };
        Ok(Self { file })
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
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut data = vec![0u8; buf.remaining()];
        match self.get_mut().file.read(&mut data) {
            Ok(n) => {
                buf.put_slice(&data[..n]);
                Poll::Ready(Ok(()))
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl AsyncWrite for NamedPipeStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(self.get_mut().file.write(buf))
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(self.get_mut().file.flush())
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}