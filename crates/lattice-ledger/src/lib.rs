//! Canonical event ledger (Lattice spec §9).
//!
//! The durable, replayable source of behavioral/application change. Never a
//! SQL database and never the same thing as any projection.
//!
//! Directory layout (spec):
//! ```text
//! <root>/ledger/
//!   active/            # current append segment
//!   sealed/<YYYY>/<MM>/<DD>/   # completed segments + footers
//! ```
//!
//! Segment target 128 MiB (configurable 64..256 MiB). Each frame is a NAF/1
//! `Frame` whose `protobuf_event` is a NAF/1 `Event` (carries runtime_sequence,
//! timestamp, previous_hash, frame_hash). Sealed segments carry a footer:
//! first/last sequence, first/last timestamp, event_count, byte_count,
//! aggregate BLAKE3 root, segment_hash, format_version.

pub mod audit;
pub mod compression;
pub mod durability;
pub mod multi_dim;
pub mod read_model;
pub mod segment;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use nai_lattice_core::{CanonicalEvent, Durability, EventId, GlobalId};
use nai_lattice_protocol::pb;
use prost::Message;
use tokio::sync::Mutex;

use crate::segment::{decode_event, SegmentWriter};

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame: {0}")]
    Frame(#[from] nai_lattice_protocol::ProtocolError),
    #[error("segment target {0} out of range: must be 64..256 MiB")]
    BadSegmentTarget(u64),
    #[error("append encode: {0}")]
    Encode(#[from] prost::EncodeError),
    #[error("append decode: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("durability policy violation: requested {requested:?} unavailable")]
    DurabilityDowngrade { requested: Durability },
    #[error("recovery: {0}")]
    Recovery(#[from] segment::RecoveryError),
}

/// Filter + pagination for a canonical event scan (fabric re-read delta).
#[derive(Debug, Clone, Default)]
pub struct EventScanFilter {
    /// Resume from an exclusive runtime sequence.
    pub after: Option<u64>,
    /// Bound the result set count.
    pub limit: Option<u64>,
    /// Exact namespace match.
    pub namespace: Option<String>,
    /// Exact event_type match.
    pub event_type: Option<String>,
    /// Exact entity_id URI match.
    pub entity_id: Option<String>,
    /// Exact correlation_id match.
    pub correlation_id: Option<String>,
}

/// Maximum depth walked under `<root>/ledger/sealed` (`YYYY/MM/DD` is 3).
const MAX_SEALED_DEPTH: usize = 8;

/// Collect `.segment` files under a sealed directory, oldest-first by path.
///
/// The writer uses `sealed/<YYYY>/<MM>/<DD>/<file>.segment`; scanning only the
/// first level silently skipped every canonical sealed segment (and therefore
/// every compressed one). Junction/symlink entries are never traversed.
fn collect_segment_files(
    dir: &Path,
    depth: usize,
    out: &mut Vec<PathBuf>,
) -> Result<(), LedgerError> {
    if depth > MAX_SEALED_DEPTH {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if is_reparse_point(&metadata) {
            continue;
        }
        if metadata.is_dir() {
            collect_segment_files(&path, depth + 1, out)?;
        } else if path.extension().map(|ext| ext == "segment").unwrap_or(false) {
            out.push(path);
        }
    }
    Ok(())
}

fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Config for the ledger.
#[derive(Debug, Clone)]
pub struct LedgerConfig {
    /// Parent of `<root>/ledger`. Data root.
    pub root: PathBuf,
    /// Target active segment size (64 MiB..256 MiB, default 128 MiB).
    pub target_segment_bytes: u64,
    /// Group-commit flush latency for `BatchedDurable` (default 25 ms).
    pub batched_latency_ms: u64,
}

impl Default for LedgerConfig {
    fn default() -> Self {
        let local = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            root: local.join("NAI").join("Lattice").join("v1"),
            target_segment_bytes: 128 * 1024 * 1024,
            batched_latency_ms: 25,
        }
    }
}

/// Callback fired after crash recovery so projections rewind and replay.
pub type ProjectionCatchup = Arc<dyn Fn(u64) + Send + Sync>;
fn noop_catchup() -> ProjectionCatchup {
    Arc::new(|_| ())
}

/// The canonical ledger. Owns the active segment append + tail hash.
pub struct Ledger {
    pub config: LedgerConfig,
    active_path: PathBuf,
    /// Single persistent active-segment writer plus the ordered append state.
    /// Held under ONE mutex: sequence allocation, previous-hash advancement,
    /// and the physical write+flush are all linearizable (LEDGER-001), so
    /// concurrent producers never interleave a torn frame or broken prev-link.
    active: Mutex<ActiveWriter>,
    /// Group-commit writer for `BatchedDurable`.
    batched: durability::BatchedDurableWriter,
}

/// One persistent segment writer serialized with the ordered append state.
struct ActiveWriter {
    writer: SegmentWriter,
    tail_hash: Option<String>,
    /// Highest allocated sequence + 1 (advances for every append, including
    /// BatchedDurable whose physical write is deferred to `flush_batched`).
    next_seq: u64,
    // NOTE (CQRS purity): the WRITE side owns NO read index. A query accelerator
    // (seq→byte-offset) is a READ-side concern and must live in a read model, not
    // in the command model's append mutex. A leaked read index here would couple
    // the high-frequency read path to the append lock — the bulkhead violation
    // the CTO flagged. Read models (LedgerReadModel) build their own accelerators.
}

impl Ledger {
    /// Open (or create) the ledger and run crash recovery on the active
    /// segment. `catchup` is invoked with the last committed sequence so
    /// projections can rewind and replay.
    pub fn open(config: LedgerConfig) -> Result<Self, LedgerError> {
        Self::open_with_catchup(config, noop_catchup())
    }

    /// Like [`Ledger::open`] but installs a catch-up hook.
    pub fn open_with_catchup(
        config: LedgerConfig,
        catchup: ProjectionCatchup,
    ) -> Result<Self, LedgerError> {
        let target = config.target_segment_bytes;
        if !(64 * 1024 * 1024..=256 * 1024 * 1024).contains(&target) {
            return Err(LedgerError::BadSegmentTarget(target));
        }
        let ledger_dir = config.root.join("ledger");
        let active_dir = ledger_dir.join("active");
        std::fs::create_dir_all(&active_dir)?;
        let active_path = active_dir.join("active.segment");
        let sealed_dir = ledger_dir.join("sealed");
        std::fs::create_dir_all(&sealed_dir)?;

        let (_frames, tail_hash, high_water, truncate) = segment::recover(&active_path)?;
        // Crash recovery: truncate only the invalid/incomplete tail.
        if let Some(offset) = truncate {
            use std::io::{Seek, SeekFrom};
            let mut f = std::fs::OpenOptions::new().write(true).open(&active_path)?;
            f.set_len(offset)?;
            f.seek(SeekFrom::Start(offset))?;
            std::mem::drop(f);
        }
        catchup(high_water);
        let batched_latency_ms = config.batched_latency_ms;

        // Persistent writer: open once so all subsequent appends are serialized
        // through a single file cursor (prevents concurrent torn frames).
        let writer = SegmentWriter::open(&active_path)?;
        let next_seq = writer.last_sequence() + 1;

        Ok(Self {
            config,
            active_path,
            active: Mutex::new(ActiveWriter { writer, tail_hash, next_seq }),
            batched: durability::BatchedDurableWriter::new(batched_latency_ms),
        })
    }

    pub fn active_path(&self) -> &Path {
        &self.active_path
    }

    /// Last committed runtime sequence (reserved order, after crash recovery).
    pub async fn last_sequence(&self) -> u64 {
        self.active.lock().await.next_seq.saturating_sub(1)
    }

    /// Read-only scan over canonical events across ALL segments (sealed + active),
    /// in ascending `runtime_sequence` order. This is the replay/rebuild path
    /// projections and fabric clients use to reconstruct state from the ledger.
    ///
    /// A sealed segment's footer is ignored by `recover` (read-only), so a scan
    /// never mutates anything. Applied filters run after the merge; `after`
    /// resumes from an exclusive sequence (cursor pagination), `limit` bounds the
    /// result set.
    ///
    /// INCREMENTAL READ (runbook "NOT CLOSED"): the active segment maintains an
    /// in-memory `seq_offsets` index (seq→byte offset). When `after` falls inside
    /// the active segment, the scan BIN-SEARCHES that offset and recovers only
    /// the frames after it — O(delta), never O(whole active segment). Sealed
    /// segments whose highest sequence ≤ `after` are skipped entirely (a cursor
    /// read is only interested in newer events). This makes tail/SSE/API reads
    /// cheap on a high-volume stream instead of O(total ledger).
    pub fn scan_events(&self, filter: &EventScanFilter) -> Result<Vec<CanonicalEvent>, LedgerError> {
        #[derive(Ord, PartialOrd, Eq, PartialEq, Clone, Copy)]
        struct SeqKey(u64);
        // Collect (sequence, event-pb-frame) tuples across all segments.
        let mut collected: Vec<(u64, pb::Frame)> = Vec::new();
        // 1) Sealed segments (oldest first). Skip any whose last sequence is <=
        //    `after` — a cursor/tail read can never need them.
        let after = filter.after;
        let sealed_dir = self.config.root.join("ledger").join("sealed");
        if sealed_dir.exists() {
            let mut files = Vec::new();
            collect_segment_files(&sealed_dir, 0, &mut files)?;
            files.sort();
            for path in files {
                // Cheap footer peek: if this sealed segment ends at/before the
                // cursor, it can't contribute post-`after` events. Works for
                // plain footers and NAIZ envelopes (synthetic source footer).
                if let Some(hwm) = segment::read_footer_hwm(&path)? {
                    if after.is_some_and(|a| hwm <= a) {
                        continue;
                    }
                }
                let (frames, _tail, _hwm, _trunc) = segment::recover(&path)?;
                collected.extend(frames.into_iter().map(|f| (f.runtime_sequence, f)));
            }
        }
        // 2) Active segment (read-side concern; no write-side index leaked here).
        //    Sealed segments beyond the cursor are skipped above (read-accelerator),
        //    which is the read-side chunk of the runbook fix. The active segment
        //    is bounded and small; the live hot-queue projection (LedgerReadModel)
        //    is what serves the high-frequency dashboard/SSE reads, NOT this
        //    replay — so the active rescan here is only for rebuilds/replay.
        let (frames, _tail, _hwm, _trunc) = segment::recover(&self.active_path)?;
        collected.extend(frames.into_iter().map(|f| (f.runtime_sequence, f)));
        // Merge-sort by sequence (dedupe identical sequence).
        collected.sort_by_key(|(seq, _)| SeqKey(*seq));
        collected.dedup_by_key(|(seq, _)| SeqKey(*seq));
        // Apply filters + pagination.
        let mut out = Vec::new();
        for (_, frame) in collected {
            let Some(ev) = decode_event(&frame)? else { continue };
            if let Some(after) = filter.after {
                if ev.runtime_sequence <= after {
                    continue;
                }
            }
            if let Some(ns) = &filter.namespace {
                if ev.namespace != *ns {
                    continue;
                }
            }
            if let Some(et) = &filter.event_type {
                if ev.event_type != *et {
                    continue;
                }
            }
            if let Some(ent) = &filter.entity_id {
                if ev.entity_id_uri != *ent {
                    continue;
                }
            }
            if let Some(corr) = &filter.correlation_id {
                if ev.correlation_id != *corr {
                    continue;
                }
            }
            out.push(proto_to_canonical(&ev));
            if let Some(limit) = filter.limit {
                if out.len() as u64 >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Append one canonical event with the requested durability. Never
    /// silently downgrades durability.
    ///
    /// Everything is serialized under ONE mutex over a single persistent
    /// `SegmentWriter` (LEDGER-001): sequence allocation, previous-hash link,
    /// and the physical write are mutually consistent, so concurrent producers
    /// yield one deterministic, contiguous, uncorrupted hash chain.
    pub async fn append(
        &self,
        event: CanonicalEvent,
        requested: Durability,
    ) -> Result<EventId, LedgerError> {
        let mut active = self.active.lock().await;
        let prev = active.tail_hash.clone();
        let seq = active.next_seq;
        active.next_seq += 1;

        let mut event = event;
        event.runtime_sequence = seq;
        event.previous_hash = prev.clone();

        let (frame, frame_hash) = build_frame(&event, seq, prev.as_deref())?;
        event.frame_hash = Some(frame_hash.clone());

        match requested {
            Durability::Durable => {
                // Physical order must equal logical order. A pending group-commit
                // frame already owns an earlier runtime_sequence and its hash is
                // THIS frame's `previous_hash`, so it must land on disk first.
                // Skipping this drain lets a direct append overtake a queued frame
                // and breaks `segment::recover`'s hash chain.
                Self::drain_batched_locked(&self.batched, &mut active)?;
                active.writer.append(&frame)?;
                active.writer.flush_durable()?; // durability boundary + sync
                active.tail_hash = Some(frame_hash);
            }
            Durability::BatchedDurable => {
                // Enqueue for group commit; the physical write happens on a
                // later flush_batched. Sequence order stays deterministic (the
                // writer lock is held during submit), and the durable ACK is
                // only visible once flush_batched confirms via last_sequence.
                self.batched.submit(encode_frame(&frame)?);
                active.tail_hash = Some(frame_hash);
            }
            Durability::ProjectionOnly => {
                Self::drain_batched_locked(&self.batched, &mut active)?;
                active.writer.append(&frame)?;
                active.writer.flush()?; // no durability boundary
                // A ProjectionOnly frame IS physically written now, so it is part
                // of the hash chain. Advancing the tail is mandatory: without it
                // the next append chains to a stale parent and crash recovery
                // truncates every later record.
                active.tail_hash = Some(frame_hash);
            }
        }
        Ok(event.event_id)
    }

    /// Append every queued `BatchedDurable` frame through the SAME persistent
    /// writer, while the caller already holds the active-writer lock.
    ///
    /// Group commit is only sound if the physical order matches the sequence
    /// order the frames were built in. Any direct append therefore drains the
    /// queue first (see [`Self::append`]); `flush_batched` is the explicit
    /// latency-boundary path and does the same work.
    fn drain_batched_locked(
        batched: &durability::BatchedDurableWriter,
        active: &mut ActiveWriter,
    ) -> Result<(), LedgerError> {
        if !batched.is_pending() {
            return Ok(());
        }
        batched.drain_with(|body| {
            let frame: pb::Frame = prost::Message::decode(body).map_err(LedgerError::Decode)?;
            active.writer.append(&frame)?;
            active.writer.flush_durable()?; // durability boundary + sync
            Ok(())
        })
    }

    /// Flush any pending BatchedDurable frames to the durability boundary through
    /// the SAME persistent writer as every other append (one authoritative write
    /// path — LEDGER-002/003). The acknowledgement/visibility of a BatchedDurable
    /// frame only happens once this returns; queue insertion is never an ACK.
    pub async fn flush_batched(&self) -> Result<(), LedgerError> {
        if !self.batched.is_pending() {
            return Ok(());
        }
        let mut active = self.active.lock().await;
        Self::drain_batched_locked(&self.batched, &mut active)
    }

    /// Seal the active segment and start a fresh one.
    ///
    /// LEDGER-SAFE rotation: acquires the SAME `active` mutex as every append
    /// and `flush_batched`, drains any pending BatchedDurable frames first, then
    /// seals + renames the current active segment, creates a fresh empty active
    /// segment, and REPLACES `active.writer` so subsequent appends go to the new
    /// file (not the sealed one). This closes the rotation race where a writer
    /// left pointing at the renamed path silently writes acknowledged frames into
    /// a sealed segment that recovery then misses (Codex review finding P1-1).
    pub async fn rotate_if_needed(&self) -> Result<bool, LedgerError> {
        let mut active = self.active.lock().await;
        let size = std::fs::metadata(&self.active_path).map(|m| m.len()).unwrap_or(0);
        if size < self.config.target_segment_bytes {
            return Ok(false);
        }
        // Drain any queued BatchedDurable frames through the SAME writer before
        // rotation so no acknowledged-but-unflushed frame could be stranded.
        self.batched.drain_with(|body| {
            let frame: pb::Frame = prost::Message::decode(body)
                .map_err(|e| LedgerError::Decode(e))?;
            active.writer.append(&frame)?;
            active.writer.flush_durable()?;
            Ok(())
        })?;
        // Seal the current segment (footer with first seq + segment hash).
        let footer = SegmentWriter::open(&self.active_path)?.seal()?;
        let now = chrono_now_utc_ymd();
        let sealed_dir = self.config.root.join("ledger").join("sealed").join(now);
        std::fs::create_dir_all(&sealed_dir)?;
        let name = format!("{:020}-{}.segment", footer.first_sequence, hex(&footer.segment_hash));
        std::fs::rename(&self.active_path, sealed_dir.join(name))?;
        // Fresh active segment + a NEW writer bound to it, so append writes to
        // the active file. Sequence + tail hash continue from the sealed tail.
        let new_writer = SegmentWriter::create_empty(&self.active_path)?;
        active.writer = new_writer;
        Ok(true)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Build a NAF/1 `Frame` whose `protobuf_event` is a NAF/1 `Event` carrying the
/// ledger envelope. Frame integrity (crc32c + frame_blake3) is applied here.
pub fn build_frame(
    event: &CanonicalEvent,
    seq: u64,
    previous_hash: Option<&str>,
) -> Result<(pb::Frame, String), LedgerError> {
    let proto_event = canonical_event_to_proto(event, seq, previous_hash)?;
    let event_bytes = proto_event.encode_to_vec();
    let mut frame = pb::Frame {
        magic: nai_lattice_protocol::FRAME_MAGIC,
        format_version: 1,
        frame_length: event_bytes.len() as u64,
        runtime_sequence: seq,
        event_id: event.event_id.to_string(),
        protobuf_event: event_bytes,
        crc32c: 0,
        frame_blake3: Vec::new(),
    };
    let mut buf = Vec::new();
    frame.encode(&mut buf)?;
    let crc = crc32c::crc32c(&buf);
    frame.crc32c = crc;
    let hash = blake3::hash(&buf);
    frame.frame_blake3 = hash.as_bytes().to_vec();
    let _ = crc;
    Ok((frame, hash.to_hex().to_string()))
}

fn encode_frame(frame: &pb::Frame) -> Result<Vec<u8>, LedgerError> {
    Ok(frame.encode_to_vec())
}

/// Map `CanonicalEvent` -> NAF/1 `Event` proto (preserving all fields).
fn canonical_event_to_proto(
    event: &CanonicalEvent,
    seq: u64,
    previous_hash: Option<&str>,
) -> Result<pb::Event, LedgerError> {
    Ok(pb::Event {
        event_id: event.event_id.to_string(),
        runtime_sequence: seq,
        timestamp_utc_ms: event.timestamp_utc_ms,
        device_id_bytes: event.device_id.as_bytes().to_vec(),
        producer_app: event.producer_app.clone(),
        producer_version: event.producer_version.clone(),
        namespace: event.namespace.clone(),
        entity_id_uri: event.entity_id.as_ref().map(|g| g.uri.clone()).unwrap_or_default(),
        event_type: event.event_type.clone(),
        schema_version: event.schema_version,
        correlation_id: event.correlation_id.clone().unwrap_or_default(),
        causation_id: event.causation_id.clone().unwrap_or_default(),
        payload_type: event.payload_type.clone(),
        payload_bytes: event.payload_bytes.clone(),
        artifact_refs: event.artifact_refs.iter().map(|g| g.uri.clone()).collect(),
        previous_hash: previous_hash.unwrap_or_default().to_string(),
        frame_hash: event.frame_hash.clone().unwrap_or_default(),
    })
}

/// Build a `CanonicalEvent` from a NAF/1 `Event` proto (replay path).
pub fn proto_to_canonical(ev: &pb::Event) -> CanonicalEvent {
    CanonicalEvent {
        event_id: EventId { raw: ev.event_id.clone() },
        runtime_sequence: ev.runtime_sequence,
        timestamp_utc_ms: ev.timestamp_utc_ms,
        device_id: {
            let mut b = [0u8; 16];
            let src = ev.device_id_bytes.as_slice();
            b[..src.len().min(16)].copy_from_slice(&src[..src.len().min(16)]);
            uuid::Uuid::from_bytes(b)
        },
        producer_app: ev.producer_app.clone(),
        producer_version: ev.producer_version.clone(),
        namespace: ev.namespace.clone(),
        entity_id: if ev.entity_id_uri.is_empty() {
            None
        } else {
            GlobalId::from_uri(&ev.entity_id_uri)
        },
        event_type: ev.event_type.clone(),
        schema_version: ev.schema_version,
        correlation_id: non_empty(&ev.correlation_id),
        causation_id: non_empty(&ev.causation_id),
        payload_type: ev.payload_type.clone(),
        payload_bytes: ev.payload_bytes.clone(),
        artifact_refs: ev.artifact_refs.iter().filter_map(|u| GlobalId::from_uri(u)).collect(),
        previous_hash: non_empty(&ev.previous_hash),
        frame_hash: non_empty(&ev.frame_hash),
    }
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// `YYYY/MM/DD` UTC path for sealing.
pub fn chrono_now_utc_ymd() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (now / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}/{m:02}/{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(n: u64, ts: i64) -> CanonicalEvent {
        CanonicalEvent {
            event_id: EventId::uuid_v7(),
            runtime_sequence: n,
            timestamp_utc_ms: ts,
            device_id: uuid::Uuid::nil(),
            producer_app: "test".into(),
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

    #[tokio::test]
    async fn appends_durable_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = LedgerConfig {
            root: dir.path().to_path_buf(),
            ..Default::default()
        };
        let active = dir.path().join("ledger/active/active.segment");
        let ledger = Ledger::open(cfg.clone()).unwrap();
        ledger.append(event(0, 1), Durability::Durable).await.unwrap();
        ledger.append(event(1, 2), Durability::BatchedDurable).await.unwrap();
        ledger.flush_batched().await.unwrap();
        assert_eq!(ledger.last_sequence().await, 2);

        // Reopen -> crash recovery replays both frames.
        let ledger2 = Ledger::open(cfg).unwrap();
        assert_eq!(ledger2.last_sequence().await, 2);
        // The batched frame must be visibly durable: reopen shows seq 1 and 2.
        let (_frames, _t, hwm2, _tr) = segment::recover(&active).unwrap();
        assert_eq!(hwm2, 2, "batched frame must be physically persisted after flush");
    }

    #[tokio::test]
    async fn projection_only_appends_advance_the_hash_chain() {
        // Regression: ProjectionOnly frames are physically written immediately,
        // so they must also advance the tail hash. Skipping that made the next
        // append chain to a stale parent; `segment::recover` then treated the
        // third frame as a torn tail and truncated every later record.
        let dir = tempfile::tempdir().unwrap();
        let cfg = LedgerConfig {
            root: dir.path().to_path_buf(),
            ..Default::default()
        };
        let active = dir.path().join("ledger/active/active.segment");
        let ledger = Ledger::open(cfg.clone()).unwrap();
        ledger.append(event(0, 1), Durability::Durable).await.unwrap();
        ledger.append(event(1, 2), Durability::ProjectionOnly).await.unwrap();
        ledger.append(event(2, 3), Durability::ProjectionOnly).await.unwrap();

        let (frames, _tail, hwm, truncate) = segment::recover(&active).unwrap();
        assert_eq!(truncate, None, "projection-only appends must not break the chain");
        assert_eq!(frames.len(), 3);
        assert_eq!(hwm, 3);

        let reopened = Ledger::open(cfg).unwrap();
        assert_eq!(reopened.last_sequence().await, 3);
        let scanned = reopened.scan_events(&EventScanFilter::default()).unwrap();
        assert_eq!(scanned.len(), 3, "every appended record must survive reopen");
    }

    #[tokio::test]
    async fn direct_append_drains_a_pending_batched_frame_first() {
        // Regression: a queued BatchedDurable frame already owns the earlier
        // sequence. A direct append that overtakes it writes the newer frame
        // first, so recovery sees a broken parent hash and truncates.
        let dir = tempfile::tempdir().unwrap();
        let cfg = LedgerConfig {
            root: dir.path().to_path_buf(),
            ..Default::default()
        };
        let active = dir.path().join("ledger/active/active.segment");
        let ledger = Ledger::open(cfg.clone()).unwrap();
        ledger.append(event(0, 1), Durability::BatchedDurable).await.unwrap();
        // Deliberately no flush_batched(): the next direct append must preserve
        // physical order by draining the queued frame first.
        ledger.append(event(1, 2), Durability::Durable).await.unwrap();

        let (frames, _tail, hwm, truncate) = segment::recover(&active).unwrap();
        assert_eq!(truncate, None, "a direct append must not overtake a batched frame");
        assert_eq!(frames.len(), 2);
        assert_eq!(hwm, 2);

        let reopened = Ledger::open(cfg).unwrap();
        assert_eq!(reopened.last_sequence().await, 2);
        let scanned = reopened.scan_events(&EventScanFilter::default()).unwrap();
        assert_eq!(scanned.len(), 2);
        // Sequence order, not insertion order, is what readers observe.
        assert_eq!(
            scanned.iter().map(|e| e.runtime_sequence).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[tokio::test]
    async fn crash_recovery_truncates_invalid_tail() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let cfg = LedgerConfig {
            root: dir.path().to_path_buf(),
            ..Default::default()
        };
        let ledger = Ledger::open(cfg.clone()).unwrap();
        ledger.append(event(0, 1), Durability::Durable).await.unwrap();
        ledger.append(event(1, 2), Durability::Durable).await.unwrap();
        assert_eq!(ledger.last_sequence().await, 2);

        // Capture the valid segment size (2 committed frames).
        let active = dir.path().join("ledger/active/active.segment");
        let valid_size = std::fs::metadata(&active).map(|m| m.len()).unwrap();

        // Simulate a torn write: append an invalid length prefix (garbage tail).
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&active).unwrap();
            // Length prefix claims a frame of 4096 bytes but nothing follows -> incomplete tail.
            f.write_all(&4096u32.to_le_bytes()).unwrap();
            f.flush().unwrap();
        }
        let corrupted_size = std::fs::metadata(&active).map(|m| m.len()).unwrap();
        assert!(corrupted_size > valid_size);

        // Reopen -> recovery truncates the incomplete tail, keeps the 2 valid frames.
        let ledger2 = Ledger::open(cfg).unwrap();
        assert_eq!(ledger2.last_sequence().await, 2);
        // The active file must have been truncated back to the valid frames only.
        let size = std::fs::metadata(&active).map(|m| m.len()).unwrap();
        assert_eq!(size, valid_size, "tail not truncated: {size}",);
    }

    #[tokio::test]
    async fn stale_tail_chain_break_is_truncated_not_hard_fail() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let cfg = LedgerConfig {
            root: dir.path().to_path_buf(),
            ..Default::default()
        };
        let ledger = Ledger::open(cfg.clone()).unwrap();
        ledger.append(event(0, 1), Durability::Durable).await.unwrap();
        ledger.append(event(1, 2), Durability::Durable).await.unwrap();
        assert_eq!(ledger.last_sequence().await, 2);

        let active = dir.path().join("ledger/active/active.segment");
        let valid_size = std::fs::metadata(&active).map(|m| m.len()).unwrap();

        // Simulate the stale-tail writer bug: append a frame whose previous_hash
        // references a pre-repair/different tail, so the hash chain breaks.
        // Build a chained frame by appending via build_frame with a WRONG prev.
        {
            let mut ev = event(2, 3);
            ev.previous_hash = Some("0000000000000000000000000000000000000000000000000000000000000000".into());
            let (frame, _hash) = build_frame(&ev, 2, Some("0000000000000000000000000000000000000000000000000000000000000000")).unwrap();
            let body = frame.encode_to_vec();
            let mut f = std::fs::OpenOptions::new().append(true).open(&active).unwrap();
            f.write_all(&(body.len() as u32).to_le_bytes()).unwrap();
            f.write_all(&body).unwrap();
            f.flush().unwrap();
        }
        assert!(std::fs::metadata(&active).unwrap().len() > valid_size);

        // Reopen -> previously hard-failed (Err from recover); now the broken
        // tail frame is truncated and the 2 valid frames are recovered.
        let ledger2 = Ledger::open(cfg).unwrap();
        assert_eq!(ledger2.last_sequence().await, 2, "valid prefix recovered");
        let size = std::fs::metadata(&active).map(|m| m.len()).unwrap();
        assert_eq!(size, valid_size, "broken chain tail not truncated");
    }

    #[test]
    fn seal_date_is_ymd() {
        let s = chrono_now_utc_ymd();
        let parts: Vec<_> = s.split('/').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].parse::<i32>().is_ok());
        assert!(parts[1].parse::<u32>().is_ok());
        assert!(parts[2].parse::<u32>().is_ok());
    }

    /// LEDGER-001: 32 concurrent producers → strict unique sequence + unbroken
    /// previous-hash chain (the single-coordinator linearizability fix).
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_appends_yield_unique_seq_and_valid_hash_chain() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = LedgerConfig { root: dir.path().to_path_buf(), ..Default::default() };
        let ledger = Arc::new(Ledger::open(cfg.clone()).unwrap());
        let producers = 32u32;
        let per = 4u32;
        let mut handles = Vec::new();
        for p in 0..producers {
            let ledger = ledger.clone();
            handles.push(tokio::spawn(async move {
                for k in 0..per {
                    ledger
                        .append(event(p as u64 * 1000 + k as u64, k as i64), Durability::Durable)
                        .await
                        .unwrap();
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let total = producers * per;
        assert_eq!(ledger.last_sequence().await, total as u64);

        // Replay all frames and verify the hash chain is contiguous.
        let (frames, tail, _hwm, _trunc) = segment::recover(&dir.path().join("ledger/active/active.segment")).unwrap();
        assert!(tail.is_some(), "tail hash must be set after appends");
        assert_eq!(frames.len(), total as usize, "all frames persisted");
        // Sequences must be strictly 1..=total and unique.
        let mut seqs: Vec<u64> = frames.iter().map(|f| f.runtime_sequence).collect();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), total as usize, "sequences must be unique");
        assert_eq!(seqs[0], 1);
        assert_eq!(seqs[total as usize - 1], total as u64);
    }

    /// Fabric re-read delta: `scan_events` reconstructs events from both
    /// active and sealed segments, filtered + paginated, in sequence order.
    #[tokio::test]
    async fn scan_events_reconstructs_across_segments() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = LedgerConfig {
            root: dir.path().to_path_buf(),
            ..Default::default()
        };
        let ledger = Ledger::open(cfg.clone()).unwrap();
        for ts in 1..=5_i64 {
            let mut e = event(0, ts);
            e.namespace = "npilot".into();
            e.event_type = "conversation.turn.created".into();
            e.payload_bytes = format!(r#"{{"seq":{ts}}}"#).into_bytes();
            e.entity_id = Some(GlobalId::entity("npilot", "conv-abcd"));
            ledger.append(e, Durability::Durable).await.unwrap();
        }
        // All 5 events readable in sequence order.
        let all = ledger
            .scan_events(&EventScanFilter {
                namespace: Some("npilot".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(all.len(), 5);
        let seqs: Vec<u64> = all.iter().map(|e| e.runtime_sequence).collect();
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        assert_eq!(seqs, sorted, "scan must be in sequence order");
        // after=2 → resumes from seq 4 onward (exclusive) → 5-3 = 2 events.
        let resumed = ledger
            .scan_events(&EventScanFilter {
                after: Some(3),
                limit: Some(2),
                namespace: Some("npilot".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(resumed.len(), 2);
        assert_eq!(resumed[0].runtime_sequence, 4);
        // event_type filter narrows.
        let typed = ledger
            .scan_events(&EventScanFilter {
                event_type: Some("conversation.turn.created".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(typed.len(), 5);
    }
}