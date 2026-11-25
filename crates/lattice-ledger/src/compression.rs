//! Sealed-segment compression envelope (S03 / NAIOS-011/012).
//!
//! # Why
//!
//! The ledger's sealed segments are immutable after `SegmentWriter::seal`.
//! Compressing them is a pure storage win; it must not change the plain format
//! for active segments or for any segment an older reader can still encounter.
//!
//! # Envelope (version 1)
//!
//! ```text
//! HEADER (174 bytes, little-endian):
//!   magic            [u8;4] = b"NAIZ"
//!   version          u32    = 1
//!   codec            u8     = 0 none | 1 zstd
//!   reserved         u8     = 0
//!   dictionary_id    u32    = 0 when no dictionary
//!   dictionary_blake3[u8;32]= zeros when no dictionary
//!   block_count      u32
//!   frame_count      u64
//!   first_sequence   u64
//!   last_sequence    u64
//!   first_timestamp  i64
//!   last_timestamp   i64
//!   event_count      u64    (same meaning as the plain footer)
//!   byte_count       u64    (sum of encoded frame bodies)
//!   aggregate_blake3 [u8;32](same meaning as the plain footer)
//!   segment_hash     [u8;32](same meaning as the plain footer)
//!   source_format    u32    (plain footer format_version that was sealed)
//!
//! BLOCKS: block payloads (zstd frames or raw block bytes), ~256 KiB raw each,
//!         always aligned to whole `u32le len | pb::Frame` records.
//!
//! BLOCK TABLE (block_count × 80 bytes):
//!   payload_offset   u64    (absolute file offset)
//!   compressed_len   u32
//!   raw_len          u32
//!   first_sequence   u64
//!   last_sequence    u64
//!   first_timestamp  i64
//!   last_timestamp   i64
//!   raw_block_blake3 [u8;32]
//!
//! TAIL (52 bytes):
//!   table_offset     u64
//!   table_len        u32    (block_count × 80)
//!   frame_count      u64    (repeat, detects a torn table/header)
//!   whole_blake3     [u8;32](BLAKE3 of every preceding byte, i.e. the whole
//!                            envelope content before this digest)
//! ```
//!
//! The header carries the original plain footer manifest so
//! `SegmentWriter::read_footer` (and therefore `verify-chain`/`hwm` scans) can
//! treat a compressed segment like a sealed plain one. The reader verifies the
//! header manifest against the decompressed frames, so a header edit cannot
//! hide a record.
//!
//! # Reader rules
//!
//! * Plain segments keep the existing byte layout and reader path unchanged.
//! * `NAIZ` is detected from the first four bytes. Unknown versions/codecs,
//!   a bad whole-file digest, a bad block digest, an out-of-bounds table entry,
//!   a dictionary mismatch, a frame hash mismatch, or a broken hash chain is a
//!   hard error. Nothing is silently skipped or truncated.
//! * Blocks are decompressed and checked independently, then the plain frame
//!   validation (crc32c + frame BLAKE3 + previous-hash chain) runs over the
//!   reconstructed stream.
//!
//! `maintenance compress --sealed` writes a temporary envelope, fsyncs it,
//! reads it back and compares the frames + footer manifest with the source,
//! and only then atomically renames it over the plain segment. The active
//! segment is never eligible.

use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};

use nai_lattice_protocol::pb;
use prost::Message;

use crate::segment::{
    decode_event, footer_from_bytes, RecoveryError, SegmentFooter, SEGMENT_FORMAT_VERSION,
};

/// Envelope magic.
pub const ENVELOPE_MAGIC: [u8; 4] = *b"NAIZ";
/// Envelope version written by this module.
pub const ENVELOPE_VERSION: u32 = 1;
/// `NAIZ` header length.
pub const HEADER_LEN: usize = 174;
/// One block-table entry.
pub const BLOCK_ENTRY_LEN: usize = 80;
/// Tail length before the whole-file digest.
pub const TAIL_LEN: usize = 52;
/// Default raw block size used by the CLI (≈256 KiB).
pub const DEFAULT_BLOCK_BYTES: usize = 256 * 1024;
/// Per-base dictionary file (next to the runtime root, like `active.json`).
pub const COMPRESSION_DICT_FILE: &str = "compression.dict";
/// Sanity bound for one raw block; protects the reader from a malicious header.
const MAX_RAW_BLOCK_BYTES: usize = 256 * 1024 * 1024;
/// Sanity bound for the decompressed stream of one segment.
const MAX_TOTAL_RAW_BYTES: usize = 1024 * 1024 * 1024;
const DICT_HASH_ZERO: [u8; 32] = [0u8; 32];

/// Block codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealedCodec {
    /// Raw block bytes (no compression). Useful for format tests only.
    None,
    /// zstd frames.
    Zstd,
}

impl SealedCodec {
    fn tag(self) -> u8 {
        match self {
            SealedCodec::None => 0,
            SealedCodec::Zstd => 1,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(SealedCodec::None),
            1 => Some(SealedCodec::Zstd),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SealedCodec::None => "none",
            SealedCodec::Zstd => "zstd",
        }
    }
}

/// How to build a new envelope.
#[derive(Clone, Debug)]
pub struct CompressionOptions {
    pub codec: SealedCodec,
    /// Target raw bytes per block (frames are never split).
    pub block_bytes: usize,
    /// zstd level for [`SealedCodec::Zstd`].
    pub level: i32,
    /// Optional trained dictionary. Its BLAKE3 is stored in the header.
    pub dictionary: Option<Vec<u8>>,
}

impl Default for CompressionOptions {
    fn default() -> Self {
        Self {
            codec: SealedCodec::Zstd,
            block_bytes: DEFAULT_BLOCK_BYTES,
            level: 3,
            dictionary: None,
        }
    }
}

/// Result of compressing one sealed segment.
#[derive(Clone, Debug, serde::Serialize)]
pub struct CompressionReport {
    pub frames: u64,
    pub blocks: u64,
    pub source_bytes: u64,
    pub envelope_bytes: u64,
    pub raw_block_bytes: u64,
    pub codec: String,
    pub dictionary_bytes: usize,
    pub dictionary_id: Option<u32>,
}

/// One decompressed + verified envelope.
#[derive(Debug)]
pub struct DecodedEnvelope {
    pub frames: Vec<pb::Frame>,
    pub tail_hash: Option<String>,
    pub last_sequence: u64,
    pub footer: SegmentFooter,
    pub codec: SealedCodec,
    pub block_count: u64,
    pub frame_count: u64,
    pub dictionary_used: bool,
}

struct Header {
    codec: SealedCodec,
    dict_id: u32,
    dict_hash: [u8; 32],
    block_count: u32,
    frame_count: u64,
    first_sequence: u64,
    last_sequence: u64,
    first_timestamp: i64,
    last_timestamp: i64,
    event_count: u64,
    byte_count: u64,
    aggregate_blake3: [u8; 32],
    segment_hash: [u8; 32],
    source_format: u32,
}

struct RawFrame {
    /// `u32le body_len | body` exactly as in the plain segment.
    bytes: Vec<u8>,
    sequence: u64,
    timestamp: i64,
    frame: pb::Frame,
}

/// True when these bytes start with the sealed-segment envelope magic.
pub fn has_envelope_magic(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && bytes[..4] == ENVELOPE_MAGIC
}

/// True when the file exists and starts with `NAIZ`.
pub fn is_compressed_file(path: &Path) -> std::io::Result<bool> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut magic = [0u8; 4];
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(magic == ENVELOPE_MAGIC),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error),
    }
}

/// Extract the source plain-footer manifest stored in the envelope header.
///
/// Used by [`crate::segment::SegmentWriter::read_footer`] so audit/high-water
/// scans stay transparent over compressed segments. This only parses the
/// header; full integrity is established by [`decode_envelope`].
pub fn read_source_footer(bytes: &[u8]) -> Result<SegmentFooter, RecoveryError> {
    let header = header_from_bytes(bytes)?;
    Ok(SegmentFooter {
        first_sequence: header.first_sequence,
        last_sequence: header.last_sequence,
        first_timestamp: header.first_timestamp,
        last_timestamp: header.last_timestamp,
        event_count: header.event_count,
        byte_count: header.byte_count,
        aggregate_blake3: header.aggregate_blake3,
        segment_hash: header.segment_hash,
        format_version: header.source_format,
    })
}

/// Verified high-water mark of an envelope without decompressing blocks.
///
/// The whole-envelope digest and the table/tail bounds are checked, so a
/// cursor scan can trust the metadata it uses to skip whole segments. Block
/// payloads are still fully verified by [`decode_envelope`] whenever the
/// segment is actually read.
pub fn envelope_hwm(bytes: &[u8]) -> Result<u64, RecoveryError> {
    let header = parse_header(bytes)?;
    let n = bytes.len();
    let computed = blake3::hash(&bytes[..n - 32]);
    if computed.as_bytes() != &bytes[n - 32..] {
        return Err(RecoveryError::Corrupt(
            0,
            "whole-envelope blake3 mismatch".into(),
        ));
    }
    let tail_start = n - TAIL_LEN;
    let table_offset = read_u64(bytes, tail_start);
    let table_len = read_u32(bytes, tail_start + 8) as usize;
    let tail_frame_count = read_u64(bytes, tail_start + 12);
    let expected_table_len = (header.block_count as usize)
        .checked_mul(BLOCK_ENTRY_LEN)
        .ok_or_else(|| RecoveryError::Corrupt(0, "block table length overflow".into()))?;
    if tail_frame_count != header.frame_count
        || table_len != expected_table_len
        || table_offset < HEADER_LEN as u64
        || (table_offset as usize)
            .checked_add(table_len)
            .and_then(|end| end.checked_add(TAIL_LEN))
            != Some(n)
    {
        return Err(RecoveryError::Corrupt(
            0,
            "envelope table/tail metadata is inconsistent".into(),
        ));
    }
    if header.block_count == 0 {
        if header.frame_count != 0 {
            return Err(RecoveryError::Corrupt(
                0,
                "envelope has frames but no blocks".into(),
            ));
        }
        return Ok(header.last_sequence);
    }
    let last_entry = table_offset as usize + (header.block_count as usize - 1) * BLOCK_ENTRY_LEN;
    let payload_offset = read_u64(bytes, last_entry);
    let compressed_len = read_u32(bytes, last_entry + 8) as usize;
    let raw_len = read_u32(bytes, last_entry + 12) as usize;
    if raw_len == 0
        || raw_len > MAX_RAW_BLOCK_BYTES
        || payload_offset < HEADER_LEN as u64
        || (payload_offset as usize)
            .checked_add(compressed_len)
            .map(|end| end > table_offset as usize)
            .unwrap_or(true)
    {
        return Err(RecoveryError::Corrupt(
            0,
            "envelope last block is outside the block region".into(),
        ));
    }
    let last_sequence = read_u64(bytes, last_entry + 24);
    if last_sequence != header.last_sequence {
        return Err(RecoveryError::Corrupt(
            0,
            "envelope last block sequence does not match its header".into(),
        ));
    }
    Ok(last_sequence)
}

/// Decode + verify an envelope for a segment path, loading
/// `<base>/compression.dict` when the header declares a dictionary.
pub fn decode_envelope_for_path(
    path: &Path,
    bytes: &[u8],
) -> Result<DecodedEnvelope, RecoveryError> {
    let header = header_from_bytes(bytes)?;
    if header.dict_hash == DICT_HASH_ZERO {
        return decode_envelope(bytes, None);
    }
    let dict_path = dictionary_path_for_segment(path).ok_or_else(|| {
        RecoveryError::Corrupt(
            0,
            format!(
                "segment {} declares a compression dictionary but is not under \
                 <base>/ledger and no {COMPRESSION_DICT_FILE} can be located",
                path.display()
            ),
        )
    })?;
    let dictionary = std::fs::read(&dict_path).map_err(|error| {
        RecoveryError::Corrupt(
            0,
            format!(
                "segment {} requires the compression dictionary {}: {error}",
                path.display(),
                dict_path.display()
            ),
        )
    })?;
    decode_envelope(bytes, Some(&dictionary))
}

/// `<base>/compression.dict` for a canonical segment path.
///
/// The canonical layout is `<base>/v1/ledger/sealed/<Y>/<M>/<D>/<file>.segment`
/// (or `<base>/v1/ledger/active/...`), so the `ledger` ancestor's parent is the
/// runtime root and its parent is the lifecycle base.
pub fn dictionary_path_for_segment(path: &Path) -> Option<PathBuf> {
    for ancestor in path.ancestors() {
        if ancestor.file_name().map(|name| name == "ledger").unwrap_or(false) {
            let root = ancestor.parent()?;
            let base = root.parent().unwrap_or(root);
            return Some(base.join(COMPRESSION_DICT_FILE));
        }
    }
    None
}

/// Decode + verify an envelope with an explicit dictionary.
pub fn decode_envelope(
    bytes: &[u8],
    dictionary: Option<&[u8]>,
) -> Result<DecodedEnvelope, RecoveryError> {
    let header = parse_header(bytes)?;
    let n = bytes.len();
    let tail_start = n - TAIL_LEN;

    // 1) Whole-envelope digest.
    let stored_whole = &bytes[n - 32..];
    let computed_whole = blake3::hash(&bytes[..n - 32]);
    if computed_whole.as_bytes() != stored_whole {
        return Err(RecoveryError::Corrupt(
            0,
            "whole-envelope blake3 mismatch".into(),
        ));
    }

    let table_offset = read_u64(bytes, tail_start);
    let table_len = read_u32(bytes, tail_start + 8) as usize;
    let tail_frame_count = read_u64(bytes, tail_start + 12);
    if tail_frame_count != header.frame_count {
        return Err(RecoveryError::Corrupt(
            0,
            format!(
                "frame_count mismatch: header {}, tail {}",
                header.frame_count, tail_frame_count
            ),
        ));
    }
    let expected_table_len = (header.block_count as usize)
        .checked_mul(BLOCK_ENTRY_LEN)
        .ok_or_else(|| RecoveryError::Corrupt(0, "block table length overflow".into()))?;
    if table_len != expected_table_len {
        return Err(RecoveryError::Corrupt(
            0,
            format!("block table length {table_len} != {expected_table_len}"),
        ));
    }
    if table_offset < HEADER_LEN as u64
        || (table_offset as usize)
            .checked_add(table_len)
            .and_then(|end| end.checked_add(TAIL_LEN))
            != Some(n)
    {
        return Err(RecoveryError::Corrupt(
            0,
            "block table does not fit the envelope".into(),
        ));
    }

    // 2) Dictionary policy.
    let mut dictionary_used = false;
    let dictionary_bytes = if header.dict_hash == DICT_HASH_ZERO {
        if header.dict_id != 0 {
            return Err(RecoveryError::Corrupt(
                0,
                "dictionary id is set but dictionary blake3 is zero".into(),
            ));
        }
        None
    } else {
        if header.codec != SealedCodec::Zstd {
            return Err(RecoveryError::Corrupt(
                0,
                "dictionary is declared for a non-zstd codec".into(),
            ));
        }
        let Some(dictionary) = dictionary else {
            return Err(RecoveryError::Corrupt(
                0,
                "segment requires a compression dictionary that was not provided".into(),
            ));
        };
        if blake3::hash(dictionary).as_bytes() != &header.dict_hash {
            return Err(RecoveryError::Corrupt(
                0,
                "compression dictionary blake3 mismatch".into(),
            ));
        }
        dictionary_used = true;
        Some(dictionary)
    };

    // 3) Blocks: decompress, verify the raw-block digest, parse frames strictly.
    let mut decompressor = match header.codec {
        SealedCodec::Zstd => Some(match dictionary_bytes {
            Some(dictionary) => zstd::bulk::Decompressor::with_dictionary(dictionary)?,
            None => zstd::bulk::Decompressor::new()?,
        }),
        SealedCodec::None => None,
    };
    let mut frames = Vec::with_capacity(header.frame_count.min(1_000_000) as usize);
    let mut aggregate = blake3::Hasher::new();
    let mut byte_count = 0u64;
    let mut event_count = 0u64;
    let mut previous_hash: Option<String> = None;
    let mut tail_hash: Option<String> = None;
    let mut last_sequence = 0u64;
    let mut last_block_seq: Option<u64> = None;

    for index in 0..header.block_count as usize {
        let entry_offset = table_offset as usize + index * BLOCK_ENTRY_LEN;
        let payload_offset = read_u64(bytes, entry_offset);
        let compressed_len = read_u32(bytes, entry_offset + 8) as usize;
        let raw_len = read_u32(bytes, entry_offset + 12) as usize;
        let first_seq = read_u64(bytes, entry_offset + 16);
        let last_seq = read_u64(bytes, entry_offset + 24);
        let first_ts = read_u64(bytes, entry_offset + 32) as i64;
        let last_ts = read_u64(bytes, entry_offset + 40) as i64;
        let raw_hash: [u8; 32] = bytes[entry_offset + 48..entry_offset + 80]
            .try_into()
            .expect("32 bytes");

        if raw_len == 0 || raw_len > MAX_RAW_BLOCK_BYTES {
            return Err(RecoveryError::Corrupt(
                payload_offset,
                format!("block {index}: invalid raw length {raw_len}"),
            ));
        }
        let payload_end = (payload_offset as usize).checked_add(compressed_len);
        if payload_offset < HEADER_LEN as u64
            || payload_end.is_none()
            || payload_end.expect("checked") > table_offset as usize
        {
            return Err(RecoveryError::Corrupt(
                payload_offset,
                format!("block {index}: payload is outside the block region"),
            ));
        }
        let payload = &bytes[payload_offset as usize..payload_end.expect("checked")];
        let raw = match header.codec {
            SealedCodec::None => {
                if compressed_len != raw_len {
                    return Err(RecoveryError::Corrupt(
                        payload_offset,
                        format!("block {index}: raw codec length mismatch"),
                    ));
                }
                payload.to_vec()
            }
            SealedCodec::Zstd => {
                let decompressor = decompressor.as_mut().expect("zstd decompressor");
                let raw = decompressor.decompress(payload, raw_len).map_err(|error| {
                    RecoveryError::Corrupt(
                        payload_offset,
                        format!("block {index}: zstd decompress: {error}"),
                    )
                })?;
                if raw.len() != raw_len {
                    return Err(RecoveryError::Corrupt(
                        payload_offset,
                        format!(
                            "block {index}: decompressed {} bytes, table says {raw_len}",
                            raw.len()
                        ),
                    ));
                }
                raw
            }
        };
        if blake3::hash(&raw).as_bytes() != &raw_hash {
            return Err(RecoveryError::Corrupt(
                payload_offset,
                format!("block {index}: raw block blake3 mismatch"),
            ));
        }

        let parsed = parse_block_frames(&raw, payload_offset)?;
        let Some(first) = parsed.first() else {
            return Err(RecoveryError::Corrupt(
                payload_offset,
                format!("block {index}: no frames"),
            ));
        };
        let last = parsed.last().expect("non-empty");
        if first.sequence != first_seq
            || last.sequence != last_seq
            || first.timestamp != first_ts
            || last.timestamp != last_ts
        {
            return Err(RecoveryError::Corrupt(
                payload_offset,
                format!("block {index}: sequence/timestamp table mismatch"),
            ));
        }

        for RawFrame {
            sequence,
            timestamp: _,
            frame,
            ..
        } in parsed
        {
            if let Some(previous) = last_block_seq {
                if sequence <= previous {
                    return Err(RecoveryError::Corrupt(
                        payload_offset,
                        format!("frame order broken: sequence {sequence} after {previous}"),
                    ));
                }
            }
            last_block_seq = Some(sequence);
            // `segment::recover` records the physical tail hash for EVERY frame
            // (even one whose event payload is unknown), but only advances the
            // chain parent for decodable events. Mirror that exactly.
            let own_hash = normalized_frame_hash(&frame)?;
            if let Ok(Some(event)) = decode_event(&frame) {
                if let Some(parent) = &previous_hash {
                    if !event.previous_hash.is_empty() && &event.previous_hash != parent {
                        return Err(RecoveryError::Corrupt(
                            payload_offset,
                            format!("hash chain break at sequence {sequence}"),
                        ));
                    }
                }
                previous_hash = Some(own_hash.clone());
                last_sequence = event.runtime_sequence;
            }
            tail_hash = Some(own_hash);
            aggregate.update(&frame.protobuf_event);
            byte_count = byte_count.saturating_add(frame.encoded_len() as u64);
            event_count = event_count.saturating_add(1);
            frames.push(frame);
            if byte_count > MAX_TOTAL_RAW_BYTES as u64 {
                return Err(RecoveryError::Corrupt(
                    payload_offset,
                    "decompressed segment exceeds the raw size sanity bound".into(),
                ));
            }
        }
    }

    // 4) Header/footer manifest must describe these exact frames.
    if header.frame_count != frames.len() as u64 {
        return Err(RecoveryError::Corrupt(
            0,
            format!(
                "frame_count mismatch: header {}, decoded {}",
                header.frame_count,
                frames.len()
            ),
        ));
    }
    let first_sequence = frames.first().map(|frame| frame.runtime_sequence).unwrap_or(0);
    let first_timestamp = frames.first().map(ts_of).unwrap_or(0);
    let last_timestamp = frames.last().map(ts_of).unwrap_or(0);
    let aggregate = aggregate.finalize();
    let mut segment_hasher = blake3::Hasher::new();
    segment_hasher.update(aggregate.as_bytes());
    segment_hasher.update(&last_sequence.to_le_bytes());
    segment_hasher.update(&byte_count.to_le_bytes());
    let segment_hash = segment_hasher.finalize();

    if header.source_format != SEGMENT_FORMAT_VERSION
        || header.event_count != event_count
        || header.first_sequence != first_sequence
        || header.last_sequence != last_sequence
        || header.first_timestamp != first_timestamp
        || header.last_timestamp != last_timestamp
        || header.byte_count != byte_count
        || header.aggregate_blake3 != *aggregate.as_bytes()
        || header.segment_hash != *segment_hash.as_bytes()
    {
        return Err(RecoveryError::Corrupt(
            0,
            "envelope header manifest does not match the decoded frames".into(),
        ));
    }

    let footer = SegmentFooter {
        first_sequence,
        last_sequence,
        first_timestamp,
        last_timestamp,
        event_count,
        byte_count,
        aggregate_blake3: *aggregate.as_bytes(),
        segment_hash: *segment_hash.as_bytes(),
        format_version: header.source_format,
    };

    Ok(DecodedEnvelope {
        frames,
        tail_hash,
        last_sequence,
        footer,
        codec: header.codec,
        block_count: header.block_count as u64,
        frame_count: header.frame_count,
        dictionary_used,
    })
}

fn parse_header(bytes: &[u8]) -> Result<Header, RecoveryError> {
    if bytes.len() < HEADER_LEN + TAIL_LEN {
        return Err(RecoveryError::Corrupt(0, "file too small for NAIZ envelope".into()));
    }
    header_from_bytes(bytes)
}

fn header_from_bytes(bytes: &[u8]) -> Result<Header, RecoveryError> {
    if bytes.len() < HEADER_LEN {
        return Err(RecoveryError::Corrupt(0, "file too small for NAIZ header".into()));
    }
    if !has_envelope_magic(bytes) {
        return Err(RecoveryError::Corrupt(0, "not a NAIZ envelope".into()));
    }
    let version = read_u32(bytes, 4);
    if version != ENVELOPE_VERSION {
        return Err(RecoveryError::Corrupt(
            0,
            format!("unsupported NAIZ envelope version {version}"),
        ));
    }
    let codec = SealedCodec::from_tag(bytes[8]).ok_or_else(|| {
        RecoveryError::Corrupt(0, format!("unknown NAIZ codec tag {}", bytes[8]))
    })?;
    Ok(Header {
        codec,
        dict_id: read_u32(bytes, 10),
        dict_hash: bytes[14..46].try_into().expect("32"),
        block_count: read_u32(bytes, 46),
        frame_count: read_u64(bytes, 50),
        first_sequence: read_u64(bytes, 58),
        last_sequence: read_u64(bytes, 66),
        first_timestamp: read_u64(bytes, 74) as i64,
        last_timestamp: read_u64(bytes, 82) as i64,
        event_count: read_u64(bytes, 90),
        byte_count: read_u64(bytes, 98),
        aggregate_blake3: bytes[106..138].try_into().expect("32"),
        segment_hash: bytes[138..170].try_into().expect("32"),
        source_format: read_u32(bytes, 170),
    })
}

/// Parse exactly one block's raw bytes into validated frames.
fn parse_block_frames(raw: &[u8], block_offset: u64) -> Result<Vec<RawFrame>, RecoveryError> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset < raw.len() {
        if offset + 4 > raw.len() {
            return Err(RecoveryError::Corrupt(
                block_offset + offset as u64,
                "truncated frame length".into(),
            ));
        }
        let len = read_u32(raw, offset) as usize;
        let start = offset + 4;
        let end = start.checked_add(len).ok_or_else(|| {
            RecoveryError::Corrupt(block_offset + offset as u64, "frame length overflow".into())
        })?;
        if end > raw.len() {
            return Err(RecoveryError::Corrupt(
                block_offset + offset as u64,
                "truncated frame body".into(),
            ));
        }
        let mut bytes = Vec::with_capacity(len + 4);
        bytes.extend_from_slice(&(len as u32).to_le_bytes());
        bytes.extend_from_slice(&raw[start..end]);
        let frame = validate_frame_body(&raw[start..end], block_offset + start as u64)?;
        out.push(RawFrame {
            bytes,
            sequence: frame.runtime_sequence,
            timestamp: ts_of(&frame),
            frame,
        });
        offset = end;
    }
    Ok(out)
}

fn validate_frame_body(body: &[u8], offset: u64) -> Result<pb::Frame, RecoveryError> {
    let frame = pb::Frame::decode(body)
        .map_err(|error| RecoveryError::Corrupt(offset, error.to_string()))?;
    if frame.magic != nai_lattice_protocol::FRAME_MAGIC {
        return Err(RecoveryError::Corrupt(offset, "bad frame magic".into()));
    }
    let mut bare = frame.clone();
    bare.crc32c = 0;
    bare.frame_blake3 = Vec::new();
    let mut encoded = Vec::new();
    bare.encode(&mut encoded)?;
    if crc32c::crc32c(&encoded) != frame.crc32c {
        return Err(RecoveryError::Corrupt(offset, "crc32c mismatch".into()));
    }
    if blake3::hash(&encoded).as_bytes() != frame.frame_blake3.as_slice() {
        return Err(RecoveryError::Corrupt(offset, "frame blake3 mismatch".into()));
    }
    Ok(frame)
}

/// Frame hash exactly as `segment::recover` computes it.
fn normalized_frame_hash(frame: &pb::Frame) -> Result<String, RecoveryError> {
    let mut bare = frame.clone();
    bare.crc32c = 0;
    bare.frame_blake3 = Vec::new();
    let mut encoded = Vec::new();
    bare.encode(&mut encoded)?;
    Ok(blake3::hash(&encoded).to_hex().to_string())
}

fn ts_of(frame: &pb::Frame) -> i64 {
    match pb::Event::decode(frame.protobuf_event.as_slice()) {
        Ok(event) => event.timestamp_utc_ms,
        Err(_) => 0,
    }
}

/// Compress one sealed plain segment in place.
///
/// The source is parsed strictly (frames + footer manifest). A temporary
/// envelope in the same directory is written, fsynced, read back, and compared
/// with the source frames + footer before an atomic rename replaces the plain
/// file. Any failure removes the temp file and leaves the original untouched.
pub fn compress_sealed_in_place(
    path: &Path,
    options: &CompressionOptions,
) -> Result<CompressionReport, RecoveryError> {
    if is_active_segment_path(path) {
        return Err(RecoveryError::Corrupt(
            0,
            format!(
                "refusing to compress {}: it is the active segment",
                path.display()
            ),
        ));
    }
    if path.extension().map(|ext| ext != "segment").unwrap_or(true) {
        return Err(RecoveryError::Corrupt(
            0,
            format!(
                "refusing to compress {}: not a .segment file",
                path.display()
            ),
        ));
    }
    let source = std::fs::read(path)?;
    if has_envelope_magic(&source) {
        return Err(RecoveryError::Corrupt(
            0,
            format!("{} is already a NAIZ envelope", path.display()),
        ));
    }
    let (frames, footer) = parse_plain_sealed(&source)?;
    let (envelope, raw_block_bytes, block_count) =
        build_envelope(&frames, &footer, options)?;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "segment".into());
    let temp = parent.join(format!(
        ".{file_name}.compress-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0)
    ));
    let result = (|| -> Result<(), RecoveryError> {
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(&envelope)?;
            file.sync_all()?;
        }
        let written = std::fs::read(&temp)?;
        let decoded = decode_envelope(&written, options.dictionary.as_deref())?;
        let same_frames = decoded.frames.len() == frames.len()
            && decoded
                .frames
                .iter()
                .zip(frames.iter())
                .all(|(decoded_frame, source_frame)| decoded_frame == &source_frame.frame);
        if !same_frames
            || decoded.footer != footer
            || decoded.frame_count != frames.len() as u64
        {
            return Err(RecoveryError::Corrupt(
                0,
                "envelope read-back did not reproduce the source frames/footer".into(),
            ));
        }
        std::fs::rename(&temp, path)?;
        sync_parent(path);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result?;

    Ok(CompressionReport {
        frames: frames.len() as u64,
        blocks: block_count,
        source_bytes: source.len() as u64,
        envelope_bytes: envelope.len() as u64,
        raw_block_bytes,
        codec: options.codec.as_str().to_string(),
        dictionary_bytes: options.dictionary.as_ref().map(Vec::len).unwrap_or(0),
        dictionary_id: options.dictionary.as_deref().map(dictionary_id),
    })
}

fn parse_plain_sealed(bytes: &[u8]) -> Result<(Vec<RawFrame>, SegmentFooter), RecoveryError> {
    let (footer, footer_start) = footer_from_bytes(bytes)?;
    if footer.format_version != SEGMENT_FORMAT_VERSION {
        return Err(RecoveryError::Corrupt(
            0,
            format!(
                "unsupported plain segment format_version {}",
                footer.format_version
            ),
        ));
    }
    let region = &bytes[..footer_start];
    let mut frames = Vec::new();
    let mut offset = 0usize;
    let mut aggregate = blake3::Hasher::new();
    let mut byte_count = 0u64;
    let mut previous_hash: Option<String> = None;
    while offset < region.len() {
        if offset + 4 > region.len() {
            return Err(RecoveryError::Corrupt(
                offset as u64,
                "truncated frame length".into(),
            ));
        }
        let len = read_u32(region, offset) as usize;
        let start = offset + 4;
        let end = start.checked_add(len).ok_or_else(|| {
            RecoveryError::Corrupt(offset as u64, "frame length overflow".into())
        })?;
        if end > region.len() {
            return Err(RecoveryError::Corrupt(
                offset as u64,
                "truncated frame body".into(),
            ));
        }
        let frame = validate_frame_body(&region[start..end], start as u64)?;
        if let Ok(Some(event)) = decode_event(&frame) {
            let own_hash = normalized_frame_hash(&frame)?;
            if let Some(parent) = &previous_hash {
                if !event.previous_hash.is_empty() && &event.previous_hash != parent {
                    return Err(RecoveryError::Corrupt(
                        offset as u64,
                        "hash chain break".into(),
                    ));
                }
            }
            previous_hash = Some(own_hash);
        }
        aggregate.update(&frame.protobuf_event);
        byte_count = byte_count.saturating_add(frame.encoded_len() as u64);
        let mut raw = Vec::with_capacity(len + 4);
        raw.extend_from_slice(&(len as u32).to_le_bytes());
        raw.extend_from_slice(&region[start..end]);
        frames.push(RawFrame {
            bytes: raw,
            sequence: frame.runtime_sequence,
            timestamp: ts_of(&frame),
            frame,
        });
        offset = end;
    }
    if offset != region.len() {
        return Err(RecoveryError::Corrupt(
            offset as u64,
            "trailing bytes before the footer".into(),
        ));
    }

    // Rebuild the manifest from the physical frames and require it to match the
    // footer. A segment whose footer does not describe its bytes is never
    // compressed.
    let aggregate = aggregate.finalize();
    let mut segment_hasher = blake3::Hasher::new();
    segment_hasher.update(aggregate.as_bytes());
    segment_hasher.update(&footer.last_sequence.to_le_bytes());
    segment_hasher.update(&byte_count.to_le_bytes());
    let segment_hash = segment_hasher.finalize();
    let first_sequence = frames.first().map(|frame| frame.sequence).unwrap_or(0);
    let last_sequence = frames.last().map(|frame| frame.sequence).unwrap_or(0);
    let first_timestamp = frames.first().map(|frame| frame.timestamp).unwrap_or(0);
    let last_timestamp = frames.last().map(|frame| frame.timestamp).unwrap_or(0);
    if footer.event_count != frames.len() as u64
        || footer.first_sequence != first_sequence
        || footer.last_sequence != last_sequence
        || footer.first_timestamp != first_timestamp
        || footer.last_timestamp != last_timestamp
        || footer.byte_count != byte_count
        || footer.aggregate_blake3 != *aggregate.as_bytes()
        || footer.segment_hash != *segment_hash.as_bytes()
    {
        return Err(RecoveryError::Corrupt(
            0,
            "plain segment footer does not describe its frames".into(),
        ));
    }
    Ok((frames, footer))
}

fn build_envelope(
    frames: &[RawFrame],
    footer: &SegmentFooter,
    options: &CompressionOptions,
) -> Result<(Vec<u8>, u64, u64), RecoveryError> {
    if options.codec == SealedCodec::None && options.dictionary.is_some() {
        return Err(RecoveryError::Corrupt(
            0,
            "a dictionary requires the zstd codec".into(),
        ));
    }
    let target = options.block_bytes.max(1);
    let mut groups: Vec<&[RawFrame]> = Vec::new();
    let mut start = 0usize;
    let mut size = 0usize;
    for (index, frame) in frames.iter().enumerate() {
        if size != 0 && size + frame.bytes.len() > target {
            groups.push(&frames[start..index]);
            start = index;
            size = 0;
        }
        size += frame.bytes.len();
    }
    if start != frames.len() {
        groups.push(&frames[start..]);
    }

    let mut compressor = match options.codec {
        SealedCodec::Zstd => Some(match options.dictionary.as_deref() {
            Some(dictionary) => zstd::bulk::Compressor::with_dictionary(options.level, dictionary)?,
            None => zstd::bulk::Compressor::new(options.level)?,
        }),
        SealedCodec::None => None,
    };
    let mut blocks: Vec<(Vec<u8>, u32, [u8; 32], u64, u64, i64, i64)> = Vec::new();
    let mut raw_block_bytes = 0u64;
    for group in &groups {
        let mut raw = Vec::new();
        for frame in group.iter() {
            raw.extend_from_slice(&frame.bytes);
        }
        raw_block_bytes = raw_block_bytes.saturating_add(raw.len() as u64);
        let raw_len = raw.len() as u32;
        let digest = *blake3::hash(&raw).as_bytes();
        let payload = match options.codec {
            SealedCodec::None => raw.clone(),
            SealedCodec::Zstd => compressor
                .as_mut()
                .expect("zstd compressor")
                .compress(&raw)?,
        };
        let first = group.first().expect("non-empty block");
        let last = group.last().expect("non-empty block");
        blocks.push((
            payload,
            raw_len,
            digest,
            first.sequence,
            last.sequence,
            first.timestamp,
            last.timestamp,
        ));
    }

    let dict_hash = options
        .dictionary
        .as_deref()
        .map(|dictionary| *blake3::hash(dictionary).as_bytes())
        .unwrap_or(DICT_HASH_ZERO);
    let dict_id = options.dictionary.as_deref().map(dictionary_id).unwrap_or(0);

    let mut out = Vec::with_capacity(HEADER_LEN + raw_block_bytes as usize + 1024);
    out.extend_from_slice(&ENVELOPE_MAGIC);
    out.extend_from_slice(&ENVELOPE_VERSION.to_le_bytes());
    out.push(options.codec.tag());
    out.push(0);
    out.extend_from_slice(&dict_id.to_le_bytes());
    out.extend_from_slice(&dict_hash);
    out.extend_from_slice(&(blocks.len() as u32).to_le_bytes());
    out.extend_from_slice(&(frames.len() as u64).to_le_bytes());
    out.extend_from_slice(&footer.first_sequence.to_le_bytes());
    out.extend_from_slice(&footer.last_sequence.to_le_bytes());
    out.extend_from_slice(&(footer.first_timestamp as u64).to_le_bytes());
    out.extend_from_slice(&(footer.last_timestamp as u64).to_le_bytes());
    out.extend_from_slice(&footer.event_count.to_le_bytes());
    out.extend_from_slice(&footer.byte_count.to_le_bytes());
    out.extend_from_slice(&footer.aggregate_blake3);
    out.extend_from_slice(&footer.segment_hash);
    out.extend_from_slice(&footer.format_version.to_le_bytes());
    debug_assert_eq!(out.len(), HEADER_LEN);

    let mut table = Vec::with_capacity(blocks.len() * BLOCK_ENTRY_LEN);
    for (payload, raw_len, digest, first_seq, last_seq, first_ts, last_ts) in &blocks {
        let offset = out.len() as u64;
        out.extend_from_slice(payload);
        table.extend_from_slice(&offset.to_le_bytes());
        table.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        table.extend_from_slice(&raw_len.to_le_bytes());
        table.extend_from_slice(&first_seq.to_le_bytes());
        table.extend_from_slice(&last_seq.to_le_bytes());
        let first_ts_bits = *first_ts as u64;
        let last_ts_bits = *last_ts as u64;
        table.extend_from_slice(&first_ts_bits.to_le_bytes());
        table.extend_from_slice(&last_ts_bits.to_le_bytes());
        table.extend_from_slice(digest);
    }
    let table_offset = out.len() as u64;
    out.extend_from_slice(&table);
    out.extend_from_slice(&table_offset.to_le_bytes());
    out.extend_from_slice(&(table.len() as u32).to_le_bytes());
    out.extend_from_slice(&(frames.len() as u64).to_le_bytes());
    let whole = blake3::hash(&out);
    out.extend_from_slice(whole.as_bytes());
    Ok((out, raw_block_bytes, blocks.len() as u64))
}

/// Train a zstd dictionary. Returns a validated dictionary or an explanation.
pub fn train_dictionary(samples: &[Vec<u8>], max_size: usize) -> Result<Vec<u8>, String> {
    if samples.len() < 8 {
        return Err(format!(
            "need at least 8 samples to train a dictionary, got {}",
            samples.len()
        ));
    }
    let mut data = Vec::new();
    let mut sizes = Vec::with_capacity(samples.len());
    for sample in samples {
        if sample.is_empty() {
            continue;
        }
        data.extend_from_slice(sample);
        sizes.push(sample.len());
    }
    if sizes.len() < 8 {
        return Err("not enough non-empty samples to train a dictionary".into());
    }
    let dictionary = zstd::dict::from_continuous(&data, &sizes, max_size)
        .map_err(|error| format!("zstd dictionary training failed: {error}"))?;
    if !dictionary_is_valid(&dictionary) {
        return Err("trained dictionary did not round-trip".into());
    }
    Ok(dictionary)
}

/// True when `dictionary` can actually be used to compress + decompress.
pub fn dictionary_is_valid(dictionary: &[u8]) -> bool {
    if dictionary.is_empty() {
        return false;
    }
    let sample = b"nai-lattice dictionary verification sample";
    let Ok(mut compressor) = zstd::bulk::Compressor::with_dictionary(3, dictionary) else {
        return false;
    };
    let Ok(mut decompressor) = zstd::bulk::Decompressor::with_dictionary(dictionary) else {
        return false;
    };
    let Ok(compressed) = compressor.compress(sample) else {
        return false;
    };
    decompressor
        .decompress(&compressed, sample.len())
        .map(|decoded| decoded == sample)
        .unwrap_or(false)
}

/// Stable numeric identity for a dictionary (the BLAKE3 prefix, forced nonzero).
pub fn dictionary_id(dictionary: &[u8]) -> u32 {
    let digest = blake3::hash(dictionary);
    let id = u32::from_le_bytes(digest.as_bytes()[..4].try_into().expect("4"));
    id | 1
}

/// Load `<base>/compression.dict` when it exists and validates.
pub fn load_dictionary(base: &Path) -> Option<Vec<u8>> {
    let bytes = std::fs::read(base.join(COMPRESSION_DICT_FILE)).ok()?;
    if dictionary_is_valid(&bytes) {
        Some(bytes)
    } else {
        None
    }
}

/// Write a dictionary atomically, only when no dictionary exists yet.
///
/// Existing dictionaries are never replaced: segments already reference the
/// old dictionary by BLAKE3, so rotating it silently would make them unreadable.
/// Returns `true` when this call created the file.
pub fn write_dictionary_if_absent(base: &Path, dictionary: &[u8]) -> Result<bool, String> {
    if !dictionary_is_valid(dictionary) {
        return Err("refusing to write an invalid dictionary".into());
    }
    let path = base.join(COMPRESSION_DICT_FILE);
    if path.exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(base).map_err(|error| format!("create {}: {error}", base.display()))?;
    let temp = base.join(format!(
        ".{COMPRESSION_DICT_FILE}.tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0)
    ));
    let result = (|| -> Result<(), String> {
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(|error| format!("create {}: {error}", temp.display()))?;
            file.write_all(dictionary)
                .map_err(|error| format!("write {}: {error}", temp.display()))?;
            file.sync_all()
                .map_err(|error| format!("sync {}: {error}", temp.display()))?;
        }
        let read_back =
            std::fs::read(&temp).map_err(|error| format!("read back {}: {error}", temp.display()))?;
        if blake3::hash(&read_back).as_bytes() != blake3::hash(dictionary).as_bytes() {
            return Err("dictionary read-back hash mismatch".into());
        }
        std::fs::rename(&temp, &path)
            .map_err(|error| format!("replace {}: {error}", path.display()))?;
        sync_parent(&path);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result?;
    Ok(true)
}

fn is_active_segment_path(path: &Path) -> bool {
    if path.file_name().map(|name| name == "active.segment").unwrap_or(false) {
        return true;
    }
    for ancestor in path.ancestors() {
        if ancestor.file_name().map(|name| name == "ledger").unwrap_or(false) {
            if let Ok(relative) = path.strip_prefix(ancestor) {
                return relative
                    .components()
                    .next()
                    .map(|component| component.as_os_str() == "active")
                    .unwrap_or(false);
            }
        }
    }
    false
}

#[cfg(unix)]
fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(directory) = std::fs::File::open(parent) {
            let _ = directory.sync_all();
        }
    }
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) {}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("4 bytes"))
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("8 bytes"))
}

/// Test helper (module-private): build a plain sealed segment from frames.
#[cfg(test)]
fn plain_bytes_from_frames(frames: &[(pb::Frame, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (frame, _) in frames {
        let body = frame.encode_to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
    }
    // Footer requires a writer; tests use SegmentWriter to seal. This helper is
    // reserved for diagnostics.
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::SegmentWriter;
    use crate::{build_frame, Ledger, LedgerConfig};
    use nai_lattice_core::{CanonicalEvent, Durability, EventId, GlobalId};

    fn event(seq: u64, ts: i64, payload: &[u8]) -> CanonicalEvent {
        CanonicalEvent {
            event_id: EventId::uuid_v7(),
            runtime_sequence: seq,
            timestamp_utc_ms: ts,
            device_id: uuid::Uuid::nil(),
            producer_app: "compression-test".into(),
            producer_version: "1".into(),
            namespace: "test".into(),
            entity_id: Some(GlobalId::entity("email", &format!("m{seq}"))),
            event_type: "email.message.ingested".into(),
            schema_version: 1,
            correlation_id: None,
            causation_id: None,
            payload_type: "application/octet-stream".into(),
            payload_bytes: payload.to_vec(),
            artifact_refs: vec![],
            previous_hash: None,
            frame_hash: None,
        }
    }

    fn layout() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().to_path_buf();
        let root = base.join("v1");
        (dir, base, root)
    }

    fn sealed_file(root: &Path, name: &str) -> PathBuf {
        let path = root
            .join("ledger")
            .join("sealed")
            .join("2026")
            .join("11")
            .join("25")
            .join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        path
    }

    fn write_plain(
        path: &Path,
        count: u64,
        payload: impl Fn(u64) -> Vec<u8>,
    ) -> (Vec<pb::Frame>, SegmentFooter) {
        let mut writer = SegmentWriter::create_empty(path).unwrap();
        let mut prev: Option<String> = None;
        let mut frames = Vec::new();
        for i in 1..=count {
            let (frame, hash) =
                build_frame(&event(i, i as i64, &payload(i)), i, prev.as_deref()).unwrap();
            writer.append(&frame).unwrap();
            prev = Some(hash);
            frames.push(frame);
        }
        let footer = writer.seal().unwrap();
        (frames, footer)
    }

    #[test]
    fn roundtrip_preserves_frames_sequences_and_footer() {
        let (_dir, _base, root) = layout();
        let path = sealed_file(&root, "roundtrip.segment");
        let (frames, footer) =
            write_plain(&path, 64, |i| format!("repetitive-payload-{i:02}").into_bytes());
        // Plain reading still works before compression. A sealed plain segment
        // reports its footer start as a "truncation" (existing reader contract);
        // the audit layer normalizes that, so only the frames/hwm matter here.
        let (plain_frames, _tail, hwm, _truncate) = crate::segment::recover(&path).unwrap();
        assert_eq!(hwm, 64);
        assert_eq!(plain_frames, frames);

        let report = compress_sealed_in_place(&path, &CompressionOptions::default()).unwrap();
        assert_eq!(report.frames, 64);
        assert_eq!(report.codec, "zstd");
        assert!(report.envelope_bytes > 0);

        let bytes = std::fs::read(&path).unwrap();
        assert!(has_envelope_magic(&bytes));
        let decoded = decode_envelope(&bytes, None).unwrap();
        assert_eq!(decoded.frames, frames);
        assert_eq!(decoded.footer, footer);
        assert_eq!(decoded.last_sequence, 64);
        assert_eq!(
            decoded
                .frames
                .iter()
                .map(|frame| frame.runtime_sequence)
                .collect::<Vec<_>>(),
            (1..=64).collect::<Vec<_>>()
        );

        let (recovered, _tail, recovered_hwm, truncate) =
            crate::segment::recover(&path).unwrap();
        assert_eq!(truncate, None, "a verified envelope never reports a truncation");
        assert_eq!(recovered, frames);
        assert_eq!(recovered_hwm, 64);
        assert_eq!(
            SegmentWriter::read_footer(&path).unwrap(),
            footer,
            "audit/hwm footer reads stay transparent"
        );
        assert_eq!(crate::segment::read_footer_hwm(&path).unwrap(), Some(64));

        // `verify-chain` must see a healthy compressed segment, not a broken
        // one with a missing footer.
        let audit = crate::audit::audit_chain(&root).unwrap();
        assert!(audit.ok, "{audit:?}");
        assert_eq!(audit.segments.len(), 1);
        assert_eq!(audit.segments[0].frames, 64);
        assert!(!audit.segments[0].broken);
        assert_eq!(audit.hidden_bytes, 0);
    }

    #[test]
    fn compression_ratio_on_repetitive_data() {
        let (_dir, _base, root) = layout();
        let path = sealed_file(&root, "ratio.segment");
        let payload = vec![b'A'; 4096];
        write_plain(&path, 200, |_| payload.clone());
        let report = compress_sealed_in_place(&path, &CompressionOptions::default()).unwrap();
        assert!(
            report.envelope_bytes * 2 < report.source_bytes,
            "expected >2x compression, source {} envelope {}",
            report.source_bytes,
            report.envelope_bytes
        );
        assert_eq!(crate::segment::recover(&path).unwrap().0.len(), 200);
    }

    #[test]
    fn roundtrip_with_trained_dictionary_and_missing_dictionary_is_fatal() {
        let (_dir, base, root) = layout();
        let path = sealed_file(&root, "dict.segment");
        let (frames, footer) = write_plain(&path, 120, |i| {
            format!("prefix-common-block-{i:04}-suffix-common-block").into_bytes()
        });
        let samples: Vec<Vec<u8>> = frames
            .iter()
            .map(|frame| frame.protobuf_event.clone())
            .collect();
        let dictionary = train_dictionary(&samples, 16 * 1024).expect("train dictionary");
        assert!(dictionary_is_valid(&dictionary));
        assert!(write_dictionary_if_absent(&base, &dictionary).unwrap());
        // Existing dictionaries are never silently replaced.
        assert!(!write_dictionary_if_absent(&base, &dictionary).unwrap());

        let options = CompressionOptions {
            dictionary: Some(dictionary),
            ..Default::default()
        };
        compress_sealed_in_place(&path, &options).unwrap();
        let decoded = crate::segment::recover(&path).unwrap();
        assert_eq!(decoded.0, frames);
        assert_eq!(SegmentWriter::read_footer(&path).unwrap(), footer);

        // Without the per-base dictionary the reader must refuse, not skip.
        std::fs::remove_file(base.join(COMPRESSION_DICT_FILE)).unwrap();
        let error = crate::segment::recover(&path).expect_err("missing dictionary is fatal");
        assert!(
            error.to_string().contains("compression dictionary"),
            "{error}"
        );
    }

    #[test]
    fn corrupted_block_is_rejected_even_with_a_recomputed_whole_hash() {
        let (_dir, _base, root) = layout();
        let path = sealed_file(&root, "corrupt-block.segment");
        let (frames, _footer) = write_plain(&path, 40, |i| vec![b'b' + (i as u8 % 4); 512]);
        compress_sealed_in_place(&path, &CompressionOptions::default()).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        // Flip one payload byte, then repair the whole-file digest so the block
        // digest itself is what must catch the corruption.
        let payload_offset = HEADER_LEN + 16;
        bytes[payload_offset] ^= 0xFF;
        let whole = blake3::hash(&bytes[..bytes.len() - 32]);
        let len = bytes.len();
        bytes[len - 32..].copy_from_slice(whole.as_bytes());
        let error = decode_envelope(&bytes, None).expect_err("corrupt block must be rejected");
        assert!(error.to_string().contains("block"), "{error}");
        // A repaired digest still cannot make the frames equal the source.
        assert_ne!(
            decode_envelope(&bytes, None)
                .ok()
                .map(|decoded| decoded.frames)
                .unwrap_or_default(),
            frames
        );
    }

    #[test]
    fn unknown_codec_is_rejected() {
        let (_dir, _base, root) = layout();
        let path = sealed_file(&root, "codec.segment");
        write_plain(&path, 8, |_| b"payload".to_vec());
        compress_sealed_in_place(&path, &CompressionOptions::default()).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[8] = 9;
        let error = decode_envelope(&bytes, None).expect_err("unknown codec must be rejected");
        assert!(error.to_string().contains("codec"), "{error}");
        std::fs::write(&path, &bytes).unwrap();
        assert!(crate::segment::recover(&path).is_err());
    }

    #[test]
    fn tampered_table_is_rejected() {
        let (_dir, _base, root) = layout();
        let path = sealed_file(&root, "table.segment");
        write_plain(&path, 12, |_| b"table-tamper".to_vec());
        compress_sealed_in_place(&path, &CompressionOptions::default()).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        let table_offset = read_u64(&bytes, bytes.len() - TAIL_LEN) as usize;
        // First block's last_sequence lives at entry+24.
        bytes[table_offset + 24] ^= 0x01;
        let whole = blake3::hash(&bytes[..bytes.len() - 32]);
        let len = bytes.len();
        bytes[len - 32..].copy_from_slice(whole.as_bytes());
        let error = decode_envelope(&bytes, None).expect_err("table tamper must be rejected");
        assert!(error.to_string().contains("mismatch"), "{error}");
    }

    #[test]
    fn atomic_failure_leaves_the_original_untouched() {
        let (_dir, _base, root) = layout();
        let path = sealed_file(&root, "atomic.segment");
        write_plain(&path, 16, |_| b"atomic-original".to_vec());
        // Corrupt the footer: compression must fail before replacing anything.
        let mut bytes = std::fs::read(&path).unwrap();
        let len = bytes.len();
        bytes[len - 1] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();
        let error = compress_sealed_in_place(&path, &CompressionOptions::default())
            .expect_err("a broken source must never be compressed");
        assert!(error.to_string().contains("footer"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let temps: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(".compress-"))
            .collect();
        assert!(temps.is_empty(), "no staging file may be left behind");
    }

    #[test]
    fn active_segment_is_refused() {
        let (_dir, _base, root) = layout();
        let active = root.join("ledger").join("active").join("active.segment");
        std::fs::create_dir_all(active.parent().unwrap()).unwrap();
        write_plain(&active, 4, |_| b"active".to_vec());
        let before = std::fs::read(&active).unwrap();
        let error = compress_sealed_in_place(&active, &CompressionOptions::default())
            .expect_err("the active segment must be refused");
        assert!(error.to_string().contains("active segment"), "{error}");
        assert_eq!(std::fs::read(&active).unwrap(), before);
        assert!(!has_envelope_magic(&before));
    }

    #[test]
    fn raw_codec_roundtrips_without_zstd() {
        let (_dir, _base, root) = layout();
        let path = sealed_file(&root, "raw.segment");
        let (frames, _footer) = write_plain(&path, 10, |_| b"raw-codec-payload".to_vec());
        let options = CompressionOptions {
            codec: SealedCodec::None,
            ..Default::default()
        };
        let report = compress_sealed_in_place(&path, &options).unwrap();
        assert_eq!(report.codec, "none");
        assert_eq!(crate::segment::recover(&path).unwrap().0, frames);
    }

    #[tokio::test]
    async fn durable_and_projection_only_records_survive_compression() {
        let (_dir, _base, root) = layout();
        let cfg = LedgerConfig {
            root: root.clone(),
            ..Default::default()
        };
        let ledger = Ledger::open(cfg.clone()).unwrap();
        ledger
            .append(event(0, 1, b"durable"), Durability::Durable)
            .await
            .unwrap();
        ledger
            .append(event(0, 2, b"projection-only"), Durability::ProjectionOnly)
            .await
            .unwrap();
        ledger
            .append(event(0, 3, b"batched"), Durability::BatchedDurable)
            .await
            .unwrap();
        ledger.flush_batched().await.unwrap();
        drop(ledger);

        let active = root.join("ledger").join("active").join("active.segment");
        let footer = SegmentWriter::open(&active).unwrap().seal().unwrap();
        let sealed = sealed_file(&root, "mixed-durability.segment");
        std::fs::rename(&active, &sealed).unwrap();
        compress_sealed_in_place(&sealed, &CompressionOptions::default()).unwrap();

        let reopened = Ledger::open(cfg).unwrap();
        let scanned = reopened
            .scan_events(&crate::EventScanFilter::default())
            .unwrap();
        assert_eq!(scanned.len(), 3, "every durability class must survive");
        assert_eq!(
            scanned
                .iter()
                .map(|event| event.runtime_sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(footer.last_sequence, 3);
    }

    #[test]
    fn empty_sealed_segment_roundtrips() {
        let (_dir, _base, root) = layout();
        let path = sealed_file(&root, "empty.segment");
        let writer = SegmentWriter::create_empty(&path).unwrap();
        let footer = writer.seal().unwrap();
        assert_eq!(footer.event_count, 0);
        compress_sealed_in_place(&path, &CompressionOptions::default()).unwrap();
        let (frames, _tail, hwm, truncate) = crate::segment::recover(&path).unwrap();
        assert!(frames.is_empty());
        assert_eq!(hwm, 0);
        assert_eq!(truncate, None);
        assert_eq!(SegmentWriter::read_footer(&path).unwrap(), footer);
    }

    #[test]
    fn whole_file_digest_covers_the_envelope() {
        let (_dir, _base, root) = layout();
        let path = sealed_file(&root, "whole.segment");
        write_plain(&path, 20, |_| b"whole-file-digest".to_vec());
        compress_sealed_in_place(&path, &CompressionOptions::default()).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        // Flip a header byte without repairing the digest.
        bytes[58] ^= 0xFF;
        let error = decode_envelope(&bytes, None).expect_err("header tamper must be rejected");
        assert!(
            error.to_string().contains("whole-envelope") || error.to_string().contains("manifest"),
            "{error}"
        );
    }

    #[test]
    fn already_compressed_file_is_not_recompressed() {
        let (_dir, _base, root) = layout();
        let path = sealed_file(&root, "twice.segment");
        write_plain(&path, 8, |_| b"once".to_vec());
        compress_sealed_in_place(&path, &CompressionOptions::default()).unwrap();
        let before = std::fs::read(&path).unwrap();
        let error = compress_sealed_in_place(&path, &CompressionOptions::default())
            .expect_err("already compressed");
        assert!(error.to_string().contains("already"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn dictionary_id_is_stable_and_nonzero() {
        let dictionary = vec![7u8; 1024];
        assert_eq!(dictionary_id(&dictionary), dictionary_id(&dictionary));
        assert_ne!(dictionary_id(&dictionary), 0);
        assert_ne!(
            dictionary_id(&dictionary),
            dictionary_id(&vec![8u8; 1024])
        );
    }

    #[test]
    fn test_helper_plain_bytes_is_not_an_envelope() {
        let (frame, _) = build_frame(&event(1, 1, b"x"), 1, None).unwrap();
        let bytes = plain_bytes_from_frames(&[(frame, String::new())]);
        assert!(!has_envelope_magic(&bytes));
    }
}
