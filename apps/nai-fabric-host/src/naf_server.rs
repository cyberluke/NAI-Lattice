//! NAF/1 named-pipe server for the host (Lattice spec §7).
//!
//! Owns the per-user pipe name, accepts connections, and bridges each to the
//! synchronous runtime dispatch. Sync frame codec (length-prefixed, magic +
//! crc + blake3) mirrors `nai_lattice_protocol`.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use bytes::{Buf, BytesMut};
use prost::Message;

use nai_lattice_gastown::GastownService;
use nai_lattice_protocol::pb;
use nai_lattice_runtime::LatticeRuntime;

use crate::handle_connection;

/// Per-user NAF/1 pipe name (`\\.\pipe\NAI.AmbientFabric.NAF1.<sid-hash>`),
/// owned by the current user SID (§5: SID-based identity, not USERNAME text).
pub(crate) struct ServerPipe;

impl ServerPipe {
    pub(crate) fn pipe_name() -> String {
        let sid = crate::win_acl::current_user_sid_string()
            .unwrap_or_else(|| "unknown".into());
        let h = blake3::hash(sid.as_bytes()).to_hex().to_string();
        format!(r"\\.\pipe\NAI.AmbientFabric.NAF1.{}", &h[..6])
    }
}

/// Serve connections forever on every endpoint name (canonical SID pipe + legacy
/// alias), each in its own accept thread, all routing to the SAME dispatcher and
/// domain state. Exactly one persistent singleton owns all names.
pub(crate) fn serve(
    rt: Arc<LatticeRuntime>,
    gastown: Arc<Mutex<GastownService>>,
    pipe_names: &[String],
) {
    #[cfg(windows)]
    {
        let mut handles = Vec::new();
        for name in pipe_names.iter().cloned() {
            let rt = rt.clone();
            let gastown = gastown.clone();
            handles.push(std::thread::spawn(move || {
                loop {
                    match accept_pipe_client(&name) {
                        Ok(mut stream) => {
                            let rt = rt.clone();
                            let gastown = gastown.clone();
                            std::thread::spawn(move || handle_connection(&rt, &gastown, &mut stream));
                        }
                        Err(e) => {
                            tracing::warn!("pipe accept error ({name}): {e}");
                            std::thread::sleep(std::time::Duration::from_millis(200));
                        }
                    }
                }
            }));
        }
        for h in handles { let _ = h.join(); }
    }
    #[cfg(not(windows))]
    {
        let _ = (rt, pipe_names, gastown);
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
    // Current-user-only pipe DACL (Lattice §5): only the owner SID can connect.
    let mut owner_sd: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut attrs: Option<windows::Win32::Security::SECURITY_ATTRIBUTES> = None;
    if let Some(sid) = crate::win_acl::current_user_sid_string() {
        if let Some(sd) = crate::win_acl::current_user_only_descriptor(&sid) {
            owner_sd = sd;
            attrs = crate::win_acl::security_attributes_for(sd);
        }
    }
    let lp_sa: Option<*const windows::Win32::Security::SECURITY_ATTRIBUTES> =
        attrs.as_ref().map(|a| a as *const _);
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(wide.as_ptr()),
            pipe_access_duplex,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            65536,
            65536,
            0,
            lp_sa,
        )
    };
    if !owner_sd.is_null() {
        unsafe { crate::win_acl::free_security_descriptor(owner_sd); }
    }
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
///
/// Routes every server frame through the CANONICAL `nai_lattice_protocol::frame`
/// decoder, which validates BLAKE3 + CRC32C and enforces `MAX_FRAME_BYTES`
/// before any allocation (NAF-004). This removes the previous duplicate decoder
/// that skipped integrity/hash checks and allocated from an unvalidated length.
pub(crate) fn read_naf_frame(
    stream: &mut impl Read,
    buf: &mut BytesMut,
) -> std::io::Result<WireFrame> {
    loop {
        // Pull the length header (4 bytes).
        if buf.len() < 4 {
            let mut hdr = [0u8; 4];
            let n = stream.read(&mut hdr)?;
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof before frame header"));
            }
            buf.extend_from_slice(&hdr[..n]);
            continue;
        }
        let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as u64;
        // Bounded: reject absurd lengths BEFORE reading/allocation.
        if len > nai_lattice_protocol::frame::MAX_FRAME_BYTES {
            return Err(std::io::Error::other(format!(
                "frame too large: {len} > {}",
                nai_lattice_protocol::frame::MAX_FRAME_BYTES
            )));
        }
        let total = 4 + len as usize;
        if buf.len() < total {
            let need = total - buf.len();
            let mut chunk = vec![0u8; need]; // bounded by MAX_FRAME_BYTES check above
            let n = stream.read(&mut chunk)?;
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof mid-frame"));
            }
            buf.extend_from_slice(&chunk[..n]);
            continue;
        }
        // Use the canonical incremental decoder for full-frame validation
        // (magic, CRC32C, BLAKE3, size). Errors reject the frame; caller breaks.
        let mut frames = Vec::new();
        nai_lattice_protocol::frame::decode_frames(buf, &mut frames).map_err(|e| {
            std::io::Error::other(format!("frame rejected by canonical codec: {e:?}"))
        })?;
        if frames.is_empty() {
            return Err(std::io::Error::other("invalid frame (no complete frame decoded)"));
        }
        let frame = frames.remove(0);
        return Ok(WireFrame { payload: BytesMut::from(&frame.payload[..]) });
    }
}

/// Encode an envelope as a NAF frame via the CANONICAL codec (same framing,
/// integrity and limits in both directions).
pub(crate) fn encode_naf(env: &pb::Envelope) -> std::io::Result<Vec<u8>> {
    let payload = env.encode_to_vec();
    if payload.len() as u64 > nai_lattice_protocol::frame::MAX_FRAME_BYTES {
        return Err(std::io::Error::other("envelope exceeds max frame bytes"));
    }
    nai_lattice_protocol::frame::encode_frame(1, 0, "", &payload)
        .map_err(|e| std::io::Error::other(format!("encode failed: {e:?}")))
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