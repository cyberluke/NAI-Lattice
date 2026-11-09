//! NAF/1 length-prefixed frame codec (Lattice spec §8, §9 framing).
//!
//! Wire layout (over a stream, e.g. a named pipe):
//! ```text
//! length_prefix:u32le  len = length of serialized Frame
//! Frame { magic, format_version, frame_length, runtime_sequence, event_id,
//!         protobuf_event, crc32c, frame_blake3 }
//! ```
//! The `crc32c` covers the serialized Frame body excluding itself and the
//! `frame_blake3` field is over the canonicalized frame fields (spec §9).

use bytes::BytesMut;
use prost::Message;

use super::{ProtocolError, FRAME_MAGIC};
use crate::pb;

/// Maximum accepted serialized frame size in one header (128 MiB default cap).
pub const MAX_FRAME_BYTES: u64 = 128 * 1024 * 1024;

/// A decoded NAF/1 frame payload. `payload` is the raw `protobuf_event` bytes;
/// interpretation is the caller's responsibility (preserve unknown types).
pub struct FrameRead {
    pub magic: u32,
    pub format_version: u32,
    pub runtime_sequence: u64,
    pub event_id: String,
    pub payload: BytesMut,
    pub crc32c: u32,
    pub frame_blake3: [u8; 32],
}

/// Encode a `Frame` into a length-prefixed byte stream, filling crc32c and
/// BLAKE3 over the serialized body.
pub fn encode_frame(
    format_version: u32,
    runtime_sequence: u64,
    event_id: &str,
    protobuf_event: &[u8],
) -> Result<Vec<u8>, ProtocolError> {
    let mut frame = pb::Frame {
        magic: FRAME_MAGIC,
        format_version,
        frame_length: protobuf_event.len() as u64,
        runtime_sequence,
        event_id: event_id.to_string(),
        protobuf_event: protobuf_event.to_vec(),
        crc32c: 0,
        frame_blake3: Vec::new(),
    };
    // CRC32C over the serialized frame (excluding crc + blake3 placeholders).
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

/// Incrementally decode length-prefixed frames from `buf`, appending any
/// complete frames to `out` and leaving the remainder in `buf`.
pub fn decode_frames(buf: &mut BytesMut, out: &mut Vec<FrameRead>) -> Result<(), ProtocolError> {
    loop {
        if buf.len() < 4 {
            return Ok(());
        }
        let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as u64;
        if len > MAX_FRAME_BYTES {
            return Err(ProtocolError::FrameTooLarge { actual: len, max: MAX_FRAME_BYTES });
        }
        let total = 4 + len as usize;
        if buf.len() < total {
            return Ok(());
        }
        let body = buf.split_to(total);
        let frame: pb::Frame = pb::Frame::decode(&body[4..])?;
        if frame.magic != FRAME_MAGIC {
            return Err(ProtocolError::BadMagic {
                got: frame.magic,
                expected: FRAME_MAGIC,
            });
        }
        // Verify CRC32C over the serialized frame (crc field zeroed for compute).
        let mut verify = frame.clone();
        verify.crc32c = 0;
        verify.frame_blake3 = Vec::new();
        let mut vbuf = Vec::with_capacity(frame.encoded_len());
        verify.encode(&mut vbuf)?;
        let got_crc = crc32c::crc32c(&vbuf);
        if got_crc != frame.crc32c {
            return Err(ProtocolError::CrcMismatch);
        }
        let got_hash = blake3::hash(&vbuf);
        if got_hash.as_bytes() != frame.frame_blake3.as_slice() {
            return Err(ProtocolError::HashMismatch);
        }
        out.push(FrameRead {
            magic: frame.magic,
            format_version: frame.format_version,
            runtime_sequence: frame.runtime_sequence,
            event_id: frame.event_id,
            payload: BytesMut::from(&frame.protobuf_event[..]),
            crc32c: frame.crc32c,
            frame_blake3: *got_hash.as_bytes(),
        });
    }
}

/// Convenience: turn a `FrameRead` back into a `pb::Frame` value.
pub fn to_frame(read: &FrameRead) -> pb::Frame {
    pb::Frame {
        magic: read.magic,
        format_version: read.format_version,
        frame_length: read.payload.len() as u64,
        runtime_sequence: read.runtime_sequence,
        event_id: read.event_id.clone(),
        protobuf_event: read.payload.to_vec(),
        crc32c: read.crc32c,
        frame_blake3: read.frame_blake3.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_frame_with_integrity() {
        let encoded = encode_frame(1, 42, "evt-1", b"{\"k\":\"v\"}").unwrap();
        let mut buf = BytesMut::from(&encoded[..]);
        let mut frames = Vec::new();
        decode_frames(&mut buf, &mut frames).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].runtime_sequence, 42);
        assert_eq!(frames[0].event_id, "evt-1");
        assert_eq!(&frames[0].payload[..], b"{\"k\":\"v\"}");
        assert!(buf.is_empty());
    }

    #[test]
    fn version_selection() {
        assert_eq!(crate::select_version(1, 1, 1, 1), Some(1));
        assert_eq!(crate::select_version(1, 3, 2, 5), Some(3));
        assert_eq!(crate::select_version(3, 4, 1, 2), None);
    }
}