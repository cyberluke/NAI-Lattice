//! NAF/1 named-pipe server for the host (Lattice spec §7).
//!
//! Owns the per-user pipe name, accepts connections, and bridges each to the
//! synchronous runtime dispatch. Sync frame codec (length-prefixed, magic +
//! crc + blake3) mirrors `lattice_protocol`.

use std::io::{Read, Write};
use std::sync::Arc;

use bytes::{Buf, BytesMut};
use prost::Message;

use lattice_protocol::pb;
use lattice_runtime::LatticeRuntime;

use crate::handle_connection;

/// Per-user NAF/1 pipe name (`\\.\pipe\NAI.AmbientFabric.NAF1.<sid-hash>`).
pub(crate) struct ServerPipe;

impl ServerPipe {
    pub(crate) fn pipe_name() -> String {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into());
        let h = blake3::hash(user.as_bytes()).to_hex().to_string();
        format!(r"\\.\pipe\NAI.AmbientFabric.NAF1.{}", &h[..6])
    }
}

/// Serve connections forever on `pipe_name`.
pub(crate) fn serve(rt: Arc<LatticeRuntime>, pipe_name: &str) {
    #[cfg(windows)]
    {
        loop {
            match accept_pipe_client(pipe_name) {
                Ok(mut stream) => {
                    let rt = rt.clone();
                    std::thread::spawn(move || handle_connection(&rt, &mut stream));
                }
                Err(e) => {
                    tracing::warn!("pipe accept error: {e}");
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (rt, pipe_name);
        panic!("NAF pipe server requires Windows");
    }
}

/// Blocks until a client connects to `pipe_name`, returning a blocking stream.
#[cfg(windows)]
fn accept_pipe_client(pipe_name: &str) -> std::io::Result<PipeClientStream> {
    use windows::Win32::System::Pipes::{
    CreateNamedPipeW, ConnectNamedPipe, PIPE_TYPE_BYTE, PIPE_READMODE_BYTE, PIPE_WAIT,
    PIPE_UNLIMITED_INSTANCES,
};
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::core::PCWSTR;

    let wide: Vec<u16> = pipe_name.encode_utf16().chain(std::iter::once(0)).collect();
    // PIPE_ACCESS_DUPLEX == 0x3 (windows-rs 0.58 exposes access mode as raw).
    let pipe_access_duplex = FILE_FLAGS_AND_ATTRIBUTES(0x3);
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(wide.as_ptr()),
            pipe_access_duplex,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            65536,
            65536,
            0,
            None,
        )
    };
    if handle.is_invalid() {
        return Err(std::io::Error::last_os_error());
    }
    unsafe {
        let _ = ConnectNamedPipe(handle, None);
        // ERROR_PIPE_CONNECTED (0xE7) also counts as a successful connect.
    }
    Ok(PipeClientStream::from_handle(handle))
}

/// A blocking byte stream over a connected named-pipe HANDLE.
#[cfg(windows)]
pub(crate) struct PipeClientStream {
    file: std::fs::File,
}

#[cfg(windows)]
impl PipeClientStream {
    fn from_handle(handle: windows::Win32::Foundation::HANDLE) -> Self {
        use std::os::windows::io::FromRawHandle;
        // Adopt the pipe HANDLE into a std file; the File owns + closes it and
        // is `Send`, so the connection can move to a worker thread.
        let file = unsafe { std::fs::File::from_raw_handle(handle.0) };
        Self { file }
    }
}

#[cfg(windows)]
impl Read for PipeClientStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buf)
    }
}

#[cfg(windows)]
impl Write for PipeClientStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// A fast in-memory/echo type so non-Windows builds still compile the signature.
#[cfg(not(windows))]
pub(crate) type PipeClientStream = std::io::Cursor<Vec<u8>>;

/// Read one length-prefixed NAF frame from a `Read` stream.
pub(crate) fn read_naf_frame(
    stream: &mut impl Read,
    buf: &mut BytesMut,
) -> std::io::Result<WireFrame> {
    loop {
        if buf.len() < 4 {
            let mut hdr = [0u8; 4];
            let n = stream.read(&mut hdr)?;
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof"));
            }
            buf.extend_from_slice(&hdr[..n]);
            continue;
        }
        let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        let total = 4 + len;
        if buf.len() < total {
            let mut chunk = vec![0u8; total - buf.len()];
            let n = stream.read(&mut chunk)?;
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof mid-frame"));
            }
            buf.extend_from_slice(&chunk[..n]);
            continue;
        }
        let body = buf.split_to(total);
        let frame: pb::Frame =
            pb::Frame::decode(&body[4..]).map_err(|e| std::io::Error::other(e.to_string()))?;
        if frame.magic != lattice_protocol::FRAME_MAGIC {
            return Err(std::io::Error::other("bad frame magic"));
        }
        return Ok(WireFrame {
            payload: BytesMut::from(&frame.protobuf_event[..]),
        });
    }
}

/// Encode an envelope as a NAF frame (length-prefixed, magic, crc, blake3).
pub(crate) fn encode_naf(env: &pb::Envelope) -> std::io::Result<Vec<u8>> {
    let payload = env.encode_to_vec();
    let mut frame = pb::Frame {
        magic: lattice_protocol::FRAME_MAGIC,
        format_version: 1,
        frame_length: payload.len() as u64,
        runtime_sequence: 0,
        event_id: String::new(),
        protobuf_event: payload,
        crc32c: 0,
        frame_blake3: Vec::new(),
    };
    let mut inner = Vec::new();
    let _ = frame.encode(&mut inner).map_err(|e| std::io::Error::other(e.to_string()))?;
    frame.crc32c = crc32c::crc32c(&inner);
    let hash = blake3::hash(&inner);
    frame.frame_blake3 = hash.as_bytes().to_vec();
    let mut out = Vec::new();
    let _ = frame.encode(&mut out).map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut wire = Vec::with_capacity(4 + out.len());
    wire.extend_from_slice(&(out.len() as u32).to_le_bytes());
    wire.extend_from_slice(&out);
    Ok(wire)
}

/// A digit span helper for the caller.
#[allow(dead_code)]
fn _has_remaining(b: &BytesMut) -> bool {
    b.has_remaining()
}

/// Decoded frame payload.
pub(crate) struct WireFrame {
    pub payload: BytesMut,
}