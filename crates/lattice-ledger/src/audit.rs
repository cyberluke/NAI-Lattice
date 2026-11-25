//! Strictly read-only ledger chain audit (Lattice spec §9).
//!
//! [`crate::Ledger::open`] runs crash recovery and **truncates** a torn or
//! chain-broken tail. An audit must never do that: it reports the damage (the
//! offset where the verified chain stops, plus the number of hidden bytes) and
//! leaves every byte on disk untouched.
//!
//! The audit walks exactly the layout the writer produces:
//!
//! ```text
//! <root>/ledger/active/active.segment                                # no footer
//! <root>/ledger/sealed/<YYYY>/<MM>/<DD>/<first_seq>-<hash>.segment   # + footer
//! ```
//!
//! The sealed footer **is** the segment manifest for this format (the ledger has
//! no separate manifest file): its format version, sequence range, event count,
//! byte count, aggregate BLAKE3 and segment hash are re-derived from the frames
//! actually present and compared. `segment::recover` does not know about
//! footers, so it reports the footer start as an incomplete "torn tail" on every
//! healthy sealed segment; the audit normalizes that case back to a healthy
//! segment instead of raising a false alarm.
//!
//! `hidden_bytes` counts payload bytes that are not part of a verified chain:
//! everything after the first torn/corrupt/chain-broken frame in a segment, plus
//! every byte of a segment whose first frame does not link to its predecessor
//! (or whose continuity cannot be proven at all). A footer that does not match
//! its frames is reported as `broken` but hides no frame bytes.
//!
//! The active segment is read without taking the writer's lock (the audit never
//! opens the ledger). A concurrent append can therefore look like a torn tail in
//! one snapshot; re-running the audit distinguishes an in-flight write from
//! durable damage.

use std::path::{Path, PathBuf};

use nai_lattice_protocol::pb;
use prost::Message;
use serde::Serialize;

use crate::segment::{
    self, decode_event, RecoveryError, SegmentFooter, SegmentWriter, SEGMENT_FORMAT_VERSION,
};
use crate::LedgerError;

/// Schema version of the [`ChainAudit`] payload emitted by `verify-chain --json`.
pub const AUDIT_SCHEMA: u32 = 1;

/// Maximum directory depth walked under `<root>/ledger/sealed` (the writer uses
/// `YYYY/MM/DD`). Bounds the audit on a corrupted/junctioned tree.
const MAX_SEALED_DEPTH: usize = 8;

/// Per-segment audit record. Field names are deliberately stable: they are the
/// `verify-chain --json` contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SegmentAudit {
    /// Path of the segment file.
    pub path: String,
    /// Complete frames `segment::recover` could read.
    pub frames: usize,
    /// First runtime sequence physically present (`None` when no frame is readable).
    pub first_seq: Option<u64>,
    /// Last runtime sequence physically present (`None` when no frame is readable).
    pub last_seq: Option<u64>,
    /// Offset where the verified chain stops, or `None` for a healthy segment.
    /// Reported only: the audit never truncates. For a sealed segment the footer
    /// start is *not* a truncated tail and is reported as `None`.
    pub truncate_offset: Option<u64>,
    /// File size on disk (footer included).
    pub bytes: u64,
    /// True when the segment cannot be replayed as part of an intact chain.
    pub broken: bool,
}

/// Whole-ledger audit payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChainAudit {
    /// Always [`AUDIT_SCHEMA`].
    pub schema: u32,
    /// Data root that was audited (`.../ledger` is resolved below it).
    pub root: String,
    /// Segments in chain order: sealed layout order (oldest first), then active.
    pub segments: Vec<SegmentAudit>,
    /// True when every segment and every segment-to-segment link is intact.
    pub ok: bool,
    /// Bytes that are not part of a verified chain (see the module docs).
    pub hidden_bytes: u64,
}

/// One recovered segment plus the cross-segment facts the chain pass needs.
struct Inspected {
    audit: SegmentAudit,
    /// Hash of the last physically written frame (recovery's tail).
    tail_hash: Option<String>,
    /// `previous_hash` carried by the first frame (empty at genesis).
    first_prev: Option<String>,
}

/// Audit the canonical ledger under `root` (the runtime data root containing
/// `ledger/`) without opening it mutably: no recovery, no truncation, no repair.
///
/// Returns `Err` only for a genuine I/O failure (the audit cannot see the
/// bytes); integrity damage is reported inside [`ChainAudit`].
pub fn audit_chain(root: &Path) -> Result<ChainAudit, String> {
    let ledger_dir = root.join("ledger");
    let active_path = ledger_dir.join("active").join("active.segment");

    let mut sealed_paths = Vec::new();
    collect_sealed(&ledger_dir.join("sealed"), 0, &mut sealed_paths)?;
    // The writer names sealed files `<first_seq:020>-<hash>.segment` under the
    // seal date, so layout order is chronological chain order.
    sealed_paths.sort();

    let mut inspected = Vec::with_capacity(sealed_paths.len() + 1);
    for path in &sealed_paths {
        inspected.push(inspect_segment(path, true)?);
    }
    if active_path.exists() {
        inspected.push(inspect_segment(&active_path, false)?);
    }

    let mut ok = true;
    let mut hidden_bytes = 0u64;
    let mut expected_tail: Option<String> = None;
    let mut expected_seq: Option<u64> = None;

    for segment in &mut inspected {
        if segment.audit.frames > 0 {
            let mut linkage_broken = match (expected_tail.as_deref(), segment.first_prev.as_deref()) {
                // A continuation segment must chain to the previous tail.
                (Some(tail), Some(prev)) => prev != tail,
                // The first segment in the layout must be genesis (no parent).
                (None, Some(prev)) => !prev.is_empty(),
                // A segment with frames whose first frame cannot be decoded
                // cannot prove continuity at all.
                (_, None) => true,
            };
            if let (Some(expected), Some(first)) = (expected_seq, segment.audit.first_seq) {
                // Runtime sequences are contiguous across segment boundaries.
                linkage_broken |= first != expected;
            }
            if linkage_broken {
                // Nothing in a segment that does not continue the chain is
                // trustworthy history.
                segment.audit.truncate_offset = Some(0);
            }
        }

        // Keep the earliest offset: a within-segment break wins over any later
        // observation, and the chain pass only ever sets `Some(0)` for a link
        // break (which is always the first byte of the segment).
        if let Some(offset) = segment.audit.truncate_offset {
            segment.audit.broken = true;
            hidden_bytes =
                hidden_bytes.saturating_add(segment.audit.bytes.saturating_sub(offset));
        }
        if segment.audit.broken {
            ok = false;
        }

        if let Some(tail) = &segment.tail_hash {
            expected_tail = Some(tail.clone());
        }
        if let Some(last) = segment.audit.last_seq {
            expected_seq = last.checked_add(1);
        }
    }

    Ok(ChainAudit {
        schema: AUDIT_SCHEMA,
        root: root.display().to_string(),
        segments: inspected.into_iter().map(|s| s.audit).collect(),
        ok,
        hidden_bytes,
    })
}

/// Collect `.segment` files under the sealed tree without following links.
fn collect_sealed(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> Result<(), String> {
    if depth > MAX_SEALED_DEPTH {
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("read {}: {e}", dir.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("read {}: {e}", dir.display()))?;
        let path = entry.path();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("stat {}: {e}", path.display())),
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_sealed(&path, depth + 1, out)?;
        } else if path
            .extension()
            .map(|ext| ext == "segment")
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Recover one segment read-only and verify its internal chain and (for sealed
/// segments) its footer manifest.
fn inspect_segment(path: &Path, sealed: bool) -> Result<Inspected, String> {
    let bytes = std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|e| format!("stat {}: {e}", path.display()))?;

    let mut audit = SegmentAudit {
        path: path.display().to_string(),
        frames: 0,
        first_seq: None,
        last_seq: None,
        truncate_offset: None,
        bytes,
        broken: false,
    };

    // The sealed footer is this format's segment manifest. Read it first: on a
    // healthy sealed segment `recover` stops exactly at the footer and reports
    // it as a "torn tail", which must not be mistaken for damage.
    let footer = if sealed {
        match SegmentWriter::read_footer(path) {
            Ok(footer) => Some(footer),
            Err(LedgerError::Io(e)) => {
                return Err(format!("read footer {}: {e}", path.display()));
            }
            // Missing/invalid footer: flagged below, frames still audited.
            Err(_) => None,
        }
    } else {
        None
    };
    let footer_start = match &footer {
        Some(_) => sealed_footer_start(path, bytes)?,
        None => None,
    };

    let mut tail_hash = None;
    let mut first_prev = None;

    match segment::recover(path) {
        Ok((frames, tail, _high_water, truncate)) => {
            tail_hash = tail;
            audit.frames = frames.len();
            audit.first_seq = frames.first().map(|frame| frame.runtime_sequence);
            audit.last_seq = frames.last().map(|frame| frame.runtime_sequence);
            audit.truncate_offset = normalize_truncate(truncate, footer_start);

            // Strict chain walk. `segment::recover` tolerates an EMPTY
            // `previous_hash` after the first frame; an audit must not: every
            // frame after the first has to chain to its physical parent.
            let mut expected: Option<String> = None;
            let mut offset = 0u64;
            for (index, frame) in frames.iter().enumerate() {
                let Some(own_hash) = frame_hash(frame) else {
                    if audit.truncate_offset.is_none() {
                        audit.truncate_offset = Some(offset);
                    }
                    break;
                };
                if let Ok(Some(event)) = decode_event(frame) {
                    if index == 0 {
                        first_prev = Some(event.previous_hash.clone());
                    } else if expected.as_deref() != Some(event.previous_hash.as_str()) {
                        if audit.truncate_offset.is_none() {
                            audit.truncate_offset = Some(offset);
                        }
                    }
                }
                expected = Some(own_hash);
                offset = offset
                    .saturating_add(4)
                    .saturating_add(frame.encoded_len() as u64);
            }

            if let Some(footer) = &footer {
                verify_footer_manifest(&mut audit, footer, &frames);
            }
        }
        Err(RecoveryError::Io(e)) => {
            return Err(format!("read {}: {e}", path.display()));
        }
        Err(error) => {
            // The whole segment is unusable to replay; report the failure point.
            audit.broken = true;
            audit.truncate_offset = Some(match error {
                RecoveryError::Corrupt(offset, _) => offset,
                _ => 0,
            });
        }
    }

    if sealed && footer.is_none() {
        // A sealed segment without a valid footer manifest is unverifiable.
        audit.broken = true;
    }

    Ok(Inspected {
        audit,
        tail_hash,
        first_prev,
    })
}

/// `Some(footer_start)` only when the trailing footer framing is well formed;
/// `None` means the footer length cannot be trusted (flagged as broken).
fn sealed_footer_start(path: &Path, file_len: u64) -> Result<Option<u64>, String> {
    const TRAILER_LEN: u64 = 36; // footer_len u32le + footer_blake3 [u8; 32]
    if file_len < TRAILER_LEN {
        return Ok(None);
    }
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    file.seek(SeekFrom::End(-(TRAILER_LEN as i64)))
        .map_err(|e| format!("seek {}: {e}", path.display()))?;
    let mut trailer = [0u8; TRAILER_LEN as usize];
    file.read_exact(&mut trailer)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let footer_len = u32::from_le_bytes(trailer[0..4].try_into().expect("4 bytes")) as u64;
    if footer_len.saturating_add(TRAILER_LEN) > file_len {
        return Ok(None);
    }
    Ok(Some(file_len - TRAILER_LEN - footer_len))
}

/// A sealed footer start is normal, not a torn tail.
fn normalize_truncate(truncate: Option<u64>, footer_start: Option<u64>) -> Option<u64> {
    match (truncate, footer_start) {
        (Some(offset), Some(start)) if offset == start => None,
        (offset, _) => offset,
    }
}

/// Compare a sealed segment's manifest against the frames actually present.
fn verify_footer_manifest(audit: &mut SegmentAudit, footer: &SegmentFooter, frames: &[pb::Frame]) {
    let mut aggregate = blake3::Hasher::new();
    let mut byte_count = 0u64;
    for frame in frames {
        aggregate.update(&frame.protobuf_event);
        byte_count = byte_count.saturating_add(frame.encoded_len() as u64);
    }
    let aggregate = aggregate.finalize();

    let mut segment_hasher = blake3::Hasher::new();
    segment_hasher.update(aggregate.as_bytes());
    segment_hasher.update(&footer.last_sequence.to_le_bytes());
    segment_hasher.update(&footer.byte_count.to_le_bytes());
    let segment_hash = segment_hasher.finalize();

    if footer.format_version != SEGMENT_FORMAT_VERSION
        || footer.event_count != audit.frames as u64
        || footer.first_sequence != audit.first_seq.unwrap_or(0)
        || footer.last_sequence != audit.last_seq.unwrap_or(0)
        || footer.byte_count != byte_count
        || footer.aggregate_blake3 != *aggregate.as_bytes()
        || footer.segment_hash != *segment_hash.as_bytes()
    {
        // Manifest mismatch: frames may still be intact, but the segment no
        // longer proves which bytes were sealed.
        audit.broken = true;
    }
}

/// Frame hash exactly as `segment::recover` computes it (crc + own hash zeroed).
fn frame_hash(frame: &pb::Frame) -> Option<String> {
    let mut bare = frame.clone();
    bare.crc32c = 0;
    bare.frame_blake3 = Vec::new();
    let mut buf = Vec::new();
    bare.encode(&mut buf).ok()?;
    Some(blake3::hash(&buf).to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Write as _;

    use nai_lattice_core::{CanonicalEvent, Durability, EventId, GlobalId};

    use crate::segment::SegmentWriter;
    use crate::{build_frame, Ledger, LedgerConfig};

    fn event(n: u64, ts: i64) -> CanonicalEvent {
        CanonicalEvent {
            event_id: EventId::uuid_v7(),
            runtime_sequence: n,
            timestamp_utc_ms: ts,
            device_id: uuid::Uuid::nil(),
            producer_app: "audit-test".into(),
            producer_version: "1".into(),
            namespace: "test".into(),
            entity_id: Some(GlobalId::entity("email", &format!("m{n}"))),
            event_type: "email.message.ingested".into(),
            schema_version: 1,
            correlation_id: None,
            causation_id: None,
            payload_type: "application/json".into(),
            payload_bytes: b"{}".to_vec(),
            artifact_refs: vec![],
            previous_hash: None,
            frame_hash: None,
        }
    }

    /// Every file under `root` with its exact contents, for the read-only proof.
    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                if metadata.is_dir() {
                    walk(&path, out);
                } else {
                    out.insert(path.clone(), std::fs::read(&path).unwrap_or_default());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, &mut out);
        out
    }

    #[tokio::test]
    async fn healthy_ledger_with_mixed_durability_audits_ok_and_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let config = LedgerConfig {
            root: dir.path().to_path_buf(),
            ..Default::default()
        };
        let ledger = Ledger::open(config).unwrap();
        ledger.append(event(0, 1), Durability::Durable).await.unwrap();
        ledger
            .append(event(1, 2), Durability::ProjectionOnly)
            .await
            .unwrap();
        ledger
            .append(event(2, 3), Durability::ProjectionOnly)
            .await
            .unwrap();
        drop(ledger);

        let before = snapshot(dir.path());
        let audit = audit_chain(dir.path()).unwrap();
        assert!(audit.ok, "healthy ledger must audit OK: {audit:?}");
        assert_eq!(audit.schema, AUDIT_SCHEMA);
        assert_eq!(audit.hidden_bytes, 0);
        assert_eq!(audit.segments.len(), 1, "only the active segment exists");
        let segment = &audit.segments[0];
        assert_eq!(segment.frames, 3);
        assert_eq!(segment.first_seq, Some(1));
        assert_eq!(segment.last_seq, Some(3));
        assert_eq!(segment.truncate_offset, None);
        assert!(!segment.broken);
        assert!(segment.bytes > 0);
        assert!(segment.path.ends_with("active.segment"));

        assert_eq!(
            snapshot(dir.path()),
            before,
            "the audit must not add, remove or rewrite a single byte"
        );
    }

    #[tokio::test]
    async fn torn_tail_is_reported_with_offset_and_hidden_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let config = LedgerConfig {
            root: dir.path().to_path_buf(),
            ..Default::default()
        };
        let ledger = Ledger::open(config).unwrap();
        ledger.append(event(0, 1), Durability::Durable).await.unwrap();
        ledger.append(event(1, 2), Durability::Durable).await.unwrap();
        drop(ledger);

        let active = dir.path().join("ledger/active/active.segment");
        let valid_size = std::fs::metadata(&active).unwrap().len();
        {
            // Incomplete length prefix: claims 4096 bytes, nothing follows.
            let mut file = std::fs::OpenOptions::new().append(true).open(&active).unwrap();
            file.write_all(&4096u32.to_le_bytes()).unwrap();
            file.flush().unwrap();
        }
        let torn_size = std::fs::metadata(&active).unwrap().len();
        assert!(torn_size > valid_size);

        let before = snapshot(dir.path());
        let audit = audit_chain(dir.path()).unwrap();
        assert!(!audit.ok, "a torn tail is not OK: {audit:?}");
        let segment = &audit.segments[0];
        assert!(segment.broken);
        assert_eq!(segment.frames, 2, "the valid prefix is still visible");
        assert_eq!(segment.first_seq, Some(1));
        assert_eq!(segment.last_seq, Some(2));
        assert_eq!(
            segment.truncate_offset,
            Some(valid_size),
            "audit reports where recovery would truncate"
        );
        assert_eq!(audit.hidden_bytes, torn_size - valid_size);
        assert_eq!(
            snapshot(dir.path()),
            before,
            "a torn tail must be reported, never truncated by the audit"
        );
        assert_eq!(std::fs::metadata(&active).unwrap().len(), torn_size);
    }

    #[tokio::test]
    async fn chain_break_is_reported_with_hidden_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let config = LedgerConfig {
            root: dir.path().to_path_buf(),
            ..Default::default()
        };
        let ledger = Ledger::open(config).unwrap();
        ledger.append(event(0, 1), Durability::Durable).await.unwrap();
        ledger.append(event(1, 2), Durability::Durable).await.unwrap();
        drop(ledger);

        let active = dir.path().join("ledger/active/active.segment");
        let valid_size = std::fs::metadata(&active).unwrap().len();
        {
            // The regression fixture: a well-formed frame whose previous_hash
            // references a foreign tail, so the chain no longer links.
            let wrong = "0000000000000000000000000000000000000000000000000000000000000000";
            let (frame, _hash) = build_frame(&event(2, 3), 2, Some(wrong)).unwrap();
            let body = frame.encode_to_vec();
            let mut file = std::fs::OpenOptions::new().append(true).open(&active).unwrap();
            file.write_all(&(body.len() as u32).to_le_bytes()).unwrap();
            file.write_all(&body).unwrap();
            file.flush().unwrap();
        }
        let broken_size = std::fs::metadata(&active).unwrap().len();
        assert!(broken_size > valid_size);

        let before = snapshot(dir.path());
        let audit = audit_chain(dir.path()).unwrap();
        assert!(!audit.ok, "a broken chain is not OK: {audit:?}");
        let segment = &audit.segments[0];
        assert!(segment.broken);
        assert_eq!(segment.frames, 2);
        assert_eq!(segment.last_seq, Some(2));
        assert_eq!(segment.truncate_offset, Some(valid_size));
        assert!(
            audit.hidden_bytes >= broken_size - valid_size,
            "hidden bytes must cover the unverifiable tail: {audit:?}"
        );
        assert!(audit.hidden_bytes > 0);
        assert_eq!(
            snapshot(dir.path()),
            before,
            "the broken tail must survive the audit untouched"
        );
        assert_eq!(std::fs::metadata(&active).unwrap().len(), broken_size);
    }

    #[tokio::test]
    async fn sealed_segment_footer_is_verified_and_not_a_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let sealed = dir
            .path()
            .join("ledger/sealed/2026/11/24");
        std::fs::create_dir_all(&sealed).unwrap();
        let path = sealed.join("00000000000000000001-test.segment");

        let mut writer = SegmentWriter::create_empty(&path).unwrap();
        let (first, first_hash) = build_frame(&event(0, 1), 1, None).unwrap();
        writer.append(&first).unwrap();
        let (second, _second_hash) =
            build_frame(&event(1, 2), 2, Some(first_hash.as_str())).unwrap();
        writer.append(&second).unwrap();
        writer.seal().unwrap();

        let before = snapshot(dir.path());
        let audit = audit_chain(dir.path()).unwrap();
        assert!(audit.ok, "a well-sealed segment is healthy: {audit:?}");
        assert_eq!(audit.segments.len(), 1);
        let segment = &audit.segments[0];
        assert_eq!(segment.frames, 2);
        assert_eq!(segment.first_seq, Some(1));
        assert_eq!(segment.last_seq, Some(2));
        assert_eq!(
            segment.truncate_offset, None,
            "the footer start is not a torn tail"
        );
        assert!(!segment.broken);
        assert_eq!(segment.bytes, std::fs::metadata(&path).unwrap().len());
        assert_eq!(snapshot(dir.path()), before);
    }

    #[tokio::test]
    async fn segment_boundaries_are_chain_checked() {
        let dir = tempfile::tempdir().unwrap();
        let sealed = dir.path().join("ledger/sealed/2026/11/24");
        std::fs::create_dir_all(&sealed).unwrap();

        fn write_sealed(dir: &Path, name: &str, frames: &[pb::Frame]) {
            let path = dir.join(name);
            let mut writer = SegmentWriter::create_empty(&path).unwrap();
            for frame in frames {
                writer.append(frame).unwrap();
            }
            writer.seal().unwrap();
        }

        let (first, first_hash) = build_frame(&event(0, 1), 1, None).unwrap();
        let (second, second_hash) =
            build_frame(&event(1, 2), 2, Some(first_hash.as_str())).unwrap();
        write_sealed(
            &sealed,
            "00000000000000000001-segment-a.segment",
            &[first, second],
        );
        let (third, third_hash) =
            build_frame(&event(2, 3), 3, Some(second_hash.as_str())).unwrap();
        let (fourth, _fourth_hash) =
            build_frame(&event(3, 4), 4, Some(third_hash.as_str())).unwrap();
        write_sealed(
            &sealed,
            "00000000000000000003-segment-b.segment",
            &[third, fourth],
        );

        let audit = audit_chain(dir.path()).unwrap();
        assert!(audit.ok, "a linked two-segment chain is healthy: {audit:?}");
        assert_eq!(audit.segments.len(), 2);
        assert!(audit.segments.iter().all(|segment| !segment.broken));
        assert_eq!(audit.hidden_bytes, 0);

        // A third segment whose first frame names a foreign parent breaks the
        // boundary link: it is reported at offset 0 with all its bytes hidden.
        let (orphan, _orphan_hash) = build_frame(
            &event(4, 5),
            5,
            Some("0000000000000000000000000000000000000000000000000000000000000000"),
        )
        .unwrap();
        write_sealed(
            &sealed,
            "00000000000000000005-segment-c.segment",
            &[orphan],
        );

        let before = snapshot(dir.path());
        let audit = audit_chain(dir.path()).unwrap();
        assert!(!audit.ok, "a broken boundary is not OK: {audit:?}");
        assert_eq!(audit.segments.len(), 3);
        assert!(
            audit.segments[..2].iter().all(|segment| !segment.broken),
            "segments before the break stay healthy"
        );
        let broken = &audit.segments[2];
        assert!(broken.broken);
        assert_eq!(broken.truncate_offset, Some(0));
        assert_eq!(audit.hidden_bytes, broken.bytes);
        assert_eq!(
            snapshot(dir.path()),
            before,
            "the audit must not repair a segment boundary"
        );
    }
}
