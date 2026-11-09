//! NAF/1 frame codec over an async stream.
//!
//! Same framing as `lattice_protocol::frame` but transport-level:
//! ```text
//! u32le wire_len | pb::Frame { magic, ..., protobuf_event = pb::Envelope }
//! ```

use bytes::{Buf, BytesMut};
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use lattice_protocol::pb;
use lattice_protocol::{ProtocolError, FRAME_MAGIC};

/// Maximum accepted frame size (128 MiB).
pub const MAX_FRAME: usize = 128 * 1024 * 1024;

/// A decoded frame payload (the `protobuf_event` bytes = a `pb::Envelope`).
pub struct WireFrame {
    pub payload: BytesMut,
    pub runtime_sequence: u64,
    pub event_id: String,
}

/// Encode an envelope into a full length-prefixed NAF frame (crc + blake3).
pub fn encode_frame(env: &pb::Envelope) -> Result<Vec<u8>, ProtocolError> {
    let payload = env.encode_to_vec();
    let mut frame = pb::Frame {
        magic: FRAME_MAGIC,
        format_version: 1,
        frame_length: payload.len() as u64,
        runtime_sequence: 0,
        event_id: String::new(),
        protobuf_event: payload,
        crc32c: 0,
        frame_blake3: Vec::new(),
    };
    let mut buf = Vec::new();
    frame.encode(&mut buf)?;
    let crc = crc32c::crc32c(&buf);
    frame.crc32c = crc;
    let hash = blake3::hash(&buf);
    frame.frame_blake3 = hash.as_bytes().to_vec();
    let mut out = Vec::new();
    frame.encode(&mut out)?;
    let mut wire = Vec::with_capacity(4 + out.len());
    wire.extend_from_slice(&(out.len() as u32).to_le_bytes());
    wire.extend_from_slice(&out);
    Ok(wire)
}

/// Encode a handshake request as a NAF frame (Envelope kind=Request).
pub fn encode_handshake_frame(req: &pb::HandshakeRequest) -> Result<Vec<u8>, ProtocolError> {
    let payload = req.encode_to_vec();
    let env = pb::Envelope {
        request_id: 0,
        request_id_raw: Vec::new(),
        idempotency_key: String::new(),
        producer_id: req.client_app_id.clone(),
        correlation_id: String::new(),
        causation_id: String::new(),
        deadline_ms: 0,
        priority: 0,
        payload_type: "naf.handshake.request".to_string(),
        payload: Vec::new(),
        kind: Some(pb::envelope::Kind::Request(pb::Request {
            method: "__handshake".to_string(),
            args: payload,
        })),
    };
    encode_frame(&env)
}

/// Read exactly one complete frame from a stream. Returns the envelope payload.
pub async fn read_frame<S>(
    stream: &mut S,
    buf: &mut BytesMut,
) -> Result<WireFrame, ProtocolError>
where
    S: AsyncRead + Unpin,
{
    loop {
        // Need at least 4 bytes for the length prefix.
        if buf.len() < 4 {
            let got = stream.read_buf(buf).await?;
            if got == 0 && buf.is_empty() {
                return Err(ProtocolError::Incomplete("stream closed".into()));
            }
            continue;
        }
        let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if len > MAX_FRAME {
            return Err(ProtocolError::FrameTooLarge {
                actual: len as u64,
                max: MAX_FRAME as u64,
            });
        }
        let total = 4 + len;
        if buf.len() < total {
            let mut need = total - buf.len();
            let mut chunk = vec![0u8; need];
            let got = stream.read(&mut chunk).await?;
            if got == 0 {
                return Err(ProtocolError::Incomplete("stream closed mid-frame".into()));
            }
            chunk.truncate(got);
            buf.extend_from_slice(&chunk);
            need -= got;
            if need > 0 {
                continue;
            }
        }
        let body = buf.split_to(total);
        let frame: pb::Frame = pb::Frame::decode(&body[4..])?;
        if frame.magic != FRAME_MAGIC {
            return Err(ProtocolError::BadMagic {
                got: frame.magic,
                expected: FRAME_MAGIC,
            });
        }
        // Integrity: recompute crc + blake3 over zeroed fields.
        let mut v = frame.clone();
        v.crc32c = 0;
        v.frame_blake3 = Vec::new();
        let mut vbuf = Vec::new();
        v.encode(&mut vbuf)?;
        if crc32c::crc32c(&vbuf) != frame.crc32c {
            return Err(ProtocolError::CrcMismatch);
        }
        if blake3::hash(&vbuf).as_bytes() != frame.frame_blake3.as_slice() {
            return Err(ProtocolError::HashMismatch);
        }
        return Ok(WireFrame {
            payload: BytesMut::from(&frame.protobuf_event[..]),
            runtime_sequence: frame.runtime_sequence,
            event_id: frame.event_id,
        });
    }
}

/// Write a frame to the stream (full + flush).
pub async fn write_frame<S>(stream: &mut S, frame: &[u8]) -> Result<(), std::io::Error>
where
    S: AsyncWrite + Unpin,
{
    stream.write_all(frame).await?;
    stream.flush().await?;
    Ok(())
}

/// Helper to let `Buf` trait be referenced (used by `split_to` internally).
#[allow(dead_code)]
fn _buf_ref(b: &mut BytesMut) {
    let _ = b.has_remaining();
}