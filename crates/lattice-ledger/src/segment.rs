//! Ledger segment file format, footer, hash chain, and crash recovery
//! (Lattice spec §9).
//!
//! On-disk layout per frame (length-prefixed NAF/1 `Frame`):
//! ```text
//! u32le wire_len | frame_bytes (pb::Frame)
//! ```
//! A sealed segment ends with a footer:
//! ```text
//! footer_magic: u32le = 0x4E414630 "NAF0"
//! first_sequence u64le, last_sequence u64le
//! first_timestamp i64le, last_timestamp i64le
//! event_count u64le, byte_count u64le
//! aggregate_blake3 [u8;32], segment_hash [u8;32]
//! format_version u32le
//! footer_len u32le, footer_blake3 [u8;32]
//! ```

use std::io::{Read, Write};
use std::path::Path;

use nai_lattice_protocol::pb;
use prost::Message;

use super::LedgerError;

/// Footer magic "NAF0".
pub const FOOTER_MAGIC: u32 = 0x4E41_4630;
pub const SEGMENT_FORMAT_VERSION: u32 = 1;
/// Number of footer payload bytes preceding the self-hash trailer.
const FOOTER_PAYLOAD_LEN: usize = 4 + 8 * 5 + 32 + 32 + 4; // magic + 5 u64 + 2 hashes + version

/// Recovery error with the offset where corruption was found.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("corrupt frame at offset {0}: {1}")]
    Corrupt(u64, String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("prost encode: {0}")]
    Encode(#[from] prost::EncodeError),
    #[error("prost decode: {0}")]
    Decode(#[from] prost::DecodeError),
}

/// Sealed-segment footer (self-describing).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SegmentFooter {
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub first_timestamp: i64,
    pub last_timestamp: i64,
    pub event_count: u64,
    pub byte_count: u64,
    pub aggregate_blake3: [u8; 32],
    pub segment_hash: [u8; 32],
    pub format_version: u32,
}

/// Append writer for one segment file.
pub struct SegmentWriter {
    file: std::fs::File,
    event_count: u64,
    byte_count: u64,
    first_sequence: Option<u64>,
    last_sequence: u64,
    first_timestamp: i64,
    last_timestamp: i64,
    aggregate: blake3::Hasher,
}

impl SegmentWriter {
    /// Open an existing segment for append (creates if absent). Rebuilds
    /// counters by scanning complete frames so sealing accounts correctly.
    pub fn open(path: &Path) -> Result<Self, LedgerError> {
        if !path.exists() {
            return Self::create_empty(path);
        }
        // A sealed NAIZ envelope is immutable by construction: appending to it
        // would invalidate the block table and whole-file hash.
        if crate::compression::is_compressed_file(path)? {
            return Err(RecoveryError::Corrupt(
                0,
                "compressed sealed segment cannot be appended".into(),
            )
            .into());
        }
        // Rebuild counters from current complete frames (idempotent).
        let (frames, _tail, _seq, _trunc) = recover(path)?;
        let mut aggregate = blake3::Hasher::new();
        let mut byte_count = 0u64;
        let mut event_count = 0u64;
        let mut first_seq = None;
        let mut last_seq = 0u64;
        let mut first_ts = 0i64;
        let mut last_ts = 0i64;
        for fr in &frames {
            event_count += 1;
            byte_count += fr.encoded_len() as u64;
            aggregate.update(canonical_bytes(fr).as_slice());
            if first_seq.is_none() {
                first_seq = Some(fr.runtime_sequence);
                first_ts = ts_of(fr);
            }
            last_seq = fr.runtime_sequence;
            last_ts = ts_of(fr);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            file,
            event_count,
            byte_count,
            first_sequence: first_seq,
            last_sequence: last_seq,
            first_timestamp: first_ts,
            last_timestamp: last_ts,
            aggregate,
        })
    }

    pub fn create_empty(path: &Path) -> Result<Self, LedgerError> {
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)?;
        Self::open(path)
    }

    /// Append one frame (length-prefixed). Returns its runtime sequence.
    pub fn append(&mut self, frame: &pb::Frame) -> Result<u64, LedgerError> {
        let body = frame.encode_to_vec();
        let len = body.len() as u32;
        self.file.write_all(&len.to_le_bytes())?;
        self.file.write_all(&body)?;

        self.event_count += 1;
        self.byte_count += body.len() as u64;
        if self.first_sequence.is_none() {
            self.first_sequence = Some(frame.runtime_sequence);
            self.first_timestamp = ts_of(frame);
        }
        self.last_sequence = frame.runtime_sequence;
        self.last_timestamp = ts_of(frame);
        self.aggregate.update(canonical_bytes(frame).as_slice());
        Ok(frame.runtime_sequence)
    }

    /// Last runtime sequence in the segment (0 when empty).
    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    /// Event count in the segment (0 when empty).
    pub fn event_count(&self) -> u64 {
        self.event_count
    }

    /// Flush to OS + durability boundary (fsync / FlushFileBuffers equivalent).
    pub fn flush_durable(&mut self) -> Result<(), LedgerError> {
        self.file.flush()?;
        self.file.sync_all()?;
        Ok(())
    }

    /// Flush to OS only.
    pub fn flush(&mut self) -> Result<(), LedgerError> {
        self.file.flush()?;
        Ok(())
    }

    /// Finalize by appending the footer (sealed/self-describing segment).
    pub fn seal(mut self) -> Result<SegmentFooter, LedgerError> {
        let aggregate = self.aggregate.finalize();
        let mut seg_hasher = blake3::Hasher::new();
        seg_hasher.update(aggregate.as_bytes());
        seg_hasher.update(&self.last_sequence.to_le_bytes());
        seg_hasher.update(&self.byte_count.to_le_bytes());
        let segment_hash = seg_hasher.finalize();

        let footer = SegmentFooter {
            first_sequence: self.first_sequence.unwrap_or(0),
            last_sequence: self.last_sequence,
            first_timestamp: self.first_timestamp,
            last_timestamp: self.last_timestamp,
            event_count: self.event_count,
            byte_count: self.byte_count,
            aggregate_blake3: *aggregate.as_bytes(),
            segment_hash: *segment_hash.as_bytes(),
            format_version: SEGMENT_FORMAT_VERSION,
        };

        let mut out = Vec::with_capacity(FOOTER_PAYLOAD_LEN);
        out.extend_from_slice(&FOOTER_MAGIC.to_le_bytes());
        out.extend_from_slice(&footer.first_sequence.to_le_bytes());
        out.extend_from_slice(&footer.last_sequence.to_le_bytes());
        out.extend_from_slice(&footer.first_timestamp.to_le_bytes());
        out.extend_from_slice(&footer.last_timestamp.to_le_bytes());
        out.extend_from_slice(&footer.event_count.to_le_bytes());
        out.extend_from_slice(&footer.byte_count.to_le_bytes());
        out.extend_from_slice(&footer.aggregate_blake3);
        out.extend_from_slice(&footer.segment_hash);
        out.extend_from_slice(&footer.format_version.to_le_bytes());
        let footer_hash = blake3::hash(&out);
        out.extend_from_slice(&(out.len() as u32).to_le_bytes());
        out.extend_from_slice(footer_hash.as_bytes());
        self.file.write_all(&out)?;
        self.file.sync_all()?;
        Ok(footer)
    }

    /// Read a sealed segment's footer, verifying its self-blake3.
    ///
    /// A `NAIZ` sealed-segment envelope (crate::compression) keeps the source
    /// footer fields in its header, so this returns the same manifest for a
    /// compressed segment. That keeps `verify-chain`/`read_footer_hwm`
    /// transparent over both formats.
    pub fn read_footer(path: &Path) -> Result<SegmentFooter, LedgerError> {
        let mut f = std::fs::File::open(path)?;
        let mut all = Vec::new();
        f.read_to_end(&mut all)?;
        if crate::compression::has_envelope_magic(&all) {
            return crate::compression::read_source_footer(&all).map_err(Into::into);
        }
        Ok(footer_from_bytes(&all)?.0)
    }
}

/// Parse and self-verify the plain segment footer in `all`.
///
/// Returns the footer and the offset where it starts (the frame region is
/// `all[..start]`).
pub(crate) fn footer_from_bytes(all: &[u8]) -> Result<(SegmentFooter, usize), RecoveryError> {
    let n = all.len();
    if n < FOOTER_PAYLOAD_LEN + 4 + 32 {
        return Err(RecoveryError::Corrupt(0, "file too small for footer".into()));
    }
    let footer_len =
        u32::from_le_bytes(all[n - 36..n - 32].try_into().expect("4 bytes")) as usize;
    if n < 36 + footer_len {
        return Err(RecoveryError::Corrupt(0, "footer length exceeds file".into()));
    }
    let start = n - 36 - footer_len;
    let stored_hash = &all[n - 32..];
    let body = &all[start..n - 36];
    if blake3::hash(body).as_bytes() != stored_hash {
        return Err(RecoveryError::Corrupt(0, "footer blake3 mismatch".into()));
    }
    if body.len() < FOOTER_PAYLOAD_LEN {
        return Err(RecoveryError::Corrupt(0, "footer too short".into()));
    }
    let magic = u32::from_le_bytes(body[0..4].try_into().expect("4"));
    if magic != FOOTER_MAGIC {
        return Err(RecoveryError::Corrupt(0, "bad footer magic".into()));
    }
    let rd = |o: usize| u64::from_le_bytes(body[o..o + 8].try_into().expect("8"));
    Ok((
        SegmentFooter {
            first_sequence: rd(4),
            last_sequence: rd(12),
            first_timestamp: rd(20) as i64,
            last_timestamp: rd(28) as i64,
            event_count: rd(36),
            byte_count: rd(44),
            aggregate_blake3: body[52..84].try_into().expect("32"),
            segment_hash: body[84..116].try_into().expect("32"),
            format_version: u32::from_le_bytes(body[116..120].try_into().expect("4")),
        },
        start,
    ))
}

/// Cheap high-water mark of a SEALED segment: its footer's `last_sequence`.
/// Returns `None` when the file has no valid reader footer (active/partial/
/// unsealed) or is missing — the caller then falls back to a full recover.
/// This lets a cursor/tail scan SKIP whole sealed segments whose max sequence
/// is already behind the `after` cursor, instead of decoding every frame.
pub fn read_footer_hwm(path: &Path) -> Result<Option<u64>, RecoveryError> {
    if !path.exists() {
        return Ok(None);
    }
    let mut f = std::fs::File::open(path)?;
    let mut all = Vec::new();
    f.read_to_end(&mut all)?;
    if crate::compression::has_envelope_magic(&all) {
        // Trust the metadata only after the whole-envelope digest and table
        // bounds verify; otherwise fall back to a full recover (which reports
        // the hard error instead of silently skipping a corrupt segment).
        return Ok(crate::compression::envelope_hwm(&all).ok());
    }
    Ok(footer_from_bytes(&all)
        .ok()
        .map(|(footer, _start)| footer.last_sequence))
}

/// Crash recovery over a segment file. Returns `(frames, tail_hash,
/// tail_sequence, truncate_offset)`. `truncate_offset` is `Some` when a
/// partial/corrupt tail must be removed.
pub fn recover(
    path: &Path,
) -> Result<(Vec<pb::Frame>, Option<String>, u64, Option<u64>), RecoveryError> {
    if !path.exists() {
        return Ok((Vec::new(), None, 0, None));
    }
    let mut f = std::fs::File::open(path)?;
    let mut all = Vec::new();
    f.read_to_end(&mut all)?;

    // Sealed-segment envelope (NAIZ): the transparent reader decodes and
    // verifies every block before the plain frame/chain path is reused. A
    // corrupt envelope is a hard error, never a silent skip.
    if crate::compression::has_envelope_magic(&all) {
        let decoded = crate::compression::decode_envelope_for_path(path, &all)?;
        return Ok((decoded.frames, decoded.tail_hash, decoded.last_sequence, None));
    }

    let mut frames = Vec::new();
    let mut offset = 0usize;
    let mut tail_hash: Option<String> = None;
    let mut last_seq = 0u64;
    let mut previous_hash: Option<String> = None;

    while offset + 4 <= all.len() {
        let len = u32::from_le_bytes(all[offset..offset + 4].try_into().expect("4")) as usize;
        let body_start = offset + 4;
        if body_start + len > all.len() {
            return Ok((frames, tail_hash, last_seq, Some(offset as u64)));
        }
        let frame = match pb::Frame::decode(&all[body_start..body_start + len]) {
            Ok(f) => f,
            Err(e) => return Err(RecoveryError::Corrupt(offset as u64, e.to_string())),
        };
        if frame.magic != nai_lattice_protocol::FRAME_MAGIC {
            return Err(RecoveryError::Corrupt(offset as u64, "bad frame magic".into()));
        }
        // Integrity: crc32c + frame_blake3 computed over the frame with crc +
        // hash zeroed (matches builder).
        let mut v = frame.clone();
        v.crc32c = 0;
        v.frame_blake3 = Vec::new();
        let mut vbuf = Vec::new();
        v.encode(&mut vbuf)?;
        if crc32c::crc32c(&vbuf) != frame.crc32c {
            return Err(RecoveryError::Corrupt(offset as u64, "crc32c mismatch".into()));
        }
        if blake3::hash(&vbuf).as_bytes() != frame.frame_blake3.as_slice() {
            return Err(RecoveryError::Corrupt(offset as u64, "frame blake3 mismatch".into()));
        }
        // Hash-chain: the event inside carries previous_hash; must match the
        // prior frame's actual hash (the parent tail), not its stored prev.
        if let Some(ev) = decode_event(&frame)? {
            let own_hash = blake3::hash(&vbuf).to_hex().to_string();
            if let Some(prev) = &previous_hash {
                if !ev.previous_hash.is_empty() && &ev.previous_hash != prev {
                    // CRASH-RECOVERY (stale-tail writer): a frame whose
                    // previous_hash does not chain to the last VALID frame marks
                    // a broken frame that a crashed/pre-repair writer appended.
                    // Treat it like a torn tail: keep all frames validated
                    // BEFORE this offset, truncate at the broken frame, and let
                    // the caller (open_with_catchup) drop it. This makes the
                    // ledger self-heal instead of hard-failing on the whole
                    // store, and the returned tail_hash stays the last VALID
                    // frame's hash so the next append chains correctly.
                    return Ok((frames, tail_hash, last_seq, Some(offset as u64)));
                }
            }
            // Advance the tracked parent hash to THIS frame's own hash so the
            // next frame's previous_hash is validated against its true parent.
            previous_hash = Some(own_hash);
            last_seq = ev.runtime_sequence;
        }
        tail_hash = Some(blake3::hash(&vbuf).to_hex().to_string());
        frames.push(frame);
        offset = body_start + len;
    }
    if offset < all.len() {
        return Ok((frames, tail_hash, last_seq, Some(offset as u64)));
    }
    Ok((frames, tail_hash, last_seq, None))
}

/// Extract the embedded `Event` from a segment frame, if decodable.
pub fn decode_event(frame: &pb::Frame) -> Result<Option<pb::Event>, RecoveryError> {
    match pb::Event::decode(frame.protobuf_event.as_slice()) {
        Ok(ev) => Ok(Some(ev)),
        Err(_) => Ok(None), // unknown payload preserved; not a corruption by itself
    }
}

/// The canonical byte set fed to the aggregate / frame hash: the encoded event.
fn canonical_bytes(frame: &pb::Frame) -> Vec<u8> {
    frame.protobuf_event.clone()
}

/// Timestamp of a frame's embedded event (falls back to 0).
fn ts_of(frame: &pb::Frame) -> i64 {
    if let Ok(ev) = pb::Event::decode(frame.protobuf_event.as_slice()) {
        ev.timestamp_utc_ms
    } else {
        0
    }
}

/// Create an empty segment file (used after sealing).
pub fn create_empty(path: &Path) -> Result<(), LedgerError> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    Ok(())
}