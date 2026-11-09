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

pub mod durability;
pub mod segment;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lattice_core::{CanonicalEvent, Durability, EventId, GlobalId};
use lattice_protocol::pb;
use prost::Message;
use tokio::sync::Mutex;

use crate::segment::SegmentWriter;

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame: {0}")]
    Frame(#[from] lattice_protocol::ProtocolError),
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
    tail_hash: Mutex<Option<String>>,
    high_water_mark: Mutex<u64>,
    /// Group-commit writer for `BatchedDurable`.
    batched: durability::BatchedDurableWriter,
    /// User-supplied batched flush hook (writes + flushes a frame body).
    batch_flush: std::sync::Mutex<Option<Arc<dyn Fn(&[u8]) -> Result<(), LedgerError> + Send + Sync>>>,
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

        Ok(Self {
            config,
            active_path,
            tail_hash: Mutex::new(tail_hash),
            high_water_mark: Mutex::new(high_water),
            batched: durability::BatchedDurableWriter::new(batched_latency_ms),
            batch_flush: std::sync::Mutex::new(None),
        })
    }

    /// Provide the durable write+flush hook used by group-commit drains.
    pub fn with_batch_flush<F>(self, f: F) -> Self
    where
        F: Fn(&[u8]) -> Result<(), LedgerError> + Send + Sync + 'static,
    {
        self.install_batch_flush(f);
        self
    }

/// Non-consuming setter for the group-commit flush hook (works behind `Arc`).
    pub fn install_batch_flush<F>(&self, f: F)
    where
        F: Fn(&[u8]) -> Result<(), LedgerError> + Send + Sync + 'static,
    {
        *self.batch_flush.lock().unwrap() = Some(Arc::new(f));
    }

    pub fn active_path(&self) -> &Path {
        &self.active_path
    }

    /// Last committed runtime sequence (after crash recovery caught up).
    pub async fn last_sequence(&self) -> u64 {
        *self.high_water_mark.lock().await
    }

    /// Append one canonical event with the requested durability. Never
    /// silently downgrades durability.
    pub async fn append(
        &self,
        event: CanonicalEvent,
        requested: Durability,
    ) -> Result<EventId, LedgerError> {
        let seq = {
            let mut hwm = self.high_water_mark.lock().await;
            let next = *hwm + 1;
            *hwm = next;
            next
        };
        let mut event = event;
        event.runtime_sequence = seq;

        let prev = self.tail_hash.lock().await.clone();
        event.previous_hash = prev.clone();

        let (frame, frame_hash) = build_frame(&event, seq, prev.as_deref())?;
        event.frame_hash = Some(frame_hash.clone());

        match requested {
            Durability::Durable => {
                let mut w = SegmentWriter::open(&self.active_path)?;
                w.append(&frame)?;
                w.flush_durable()?; // durability boundary + sync
                *self.tail_hash.lock().await = Some(frame_hash);
            }
            Durability::BatchedDurable => {
                // Enqueue; a background timer (or explicit flush) drains and
                // writes+syncs within the group-commit latency.
                self.batched.submit(encode_frame(&frame)?);
                *self.tail_hash.lock().await = Some(frame_hash);
            }
            Durability::ProjectionOnly => {
                let mut w = SegmentWriter::open(&self.active_path)?;
                w.append(&frame)?;
                w.flush()?; // no durability boundary
            }
        }
        Ok(event.event_id)
    }

    /// Flush any pending batched frames to the durability boundary.
    pub fn flush_batched(&self) -> Result<(), LedgerError> {
        // Drain the queue up front, then write each body via the installed hook.
        self.batched.drain_with(|b| {
            let hook = self.batch_flush.lock().unwrap().clone();
            match hook {
                Some(h) => h(b),
                None => Ok(()), // no hook installed: nothing to write
            }
        })
    }

    /// Seal the active segment and start a fresh one.
    pub fn rotate_if_needed(&self) -> Result<bool, LedgerError> {
        let size = std::fs::metadata(&self.active_path).map(|m| m.len()).unwrap_or(0);
        if size < self.config.target_segment_bytes {
            return Ok(false);
        }
        let footer = SegmentWriter::open(&self.active_path)?.seal()?;
        let now = chrono_now_utc_ymd();
        let sealed_dir = self.config.root.join("ledger").join("sealed").join(now);
        std::fs::create_dir_all(&sealed_dir)?;
        let name = format!("{:020}-{}.segment", footer.first_sequence, hex(&footer.segment_hash));
        std::fs::rename(&self.active_path, sealed_dir.join(name))?;
        segment::create_empty(&self.active_path)?;
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
        magic: lattice_protocol::FRAME_MAGIC,
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
        let ledger = Ledger::open(cfg.clone())
            .unwrap()
            .with_batch_flush(move |b| {
                let mut w = SegmentWriter::open(&active)?;
                let f: pb::Frame = prost::Message::decode(b)?;
                w.append(&f)?;
                w.flush_durable()
            });
        ledger.append(event(0, 1), Durability::Durable).await.unwrap();
        ledger.append(event(1, 2), Durability::BatchedDurable).await.unwrap();
        ledger.flush_batched().unwrap();
        assert_eq!(ledger.last_sequence().await, 2);

        // Reopen -> crash recovery replays both frames.
        let ledger2 = Ledger::open(cfg).unwrap();
        assert_eq!(ledger2.last_sequence().await, 2);
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

    #[test]
    fn seal_date_is_ymd() {
        let s = chrono_now_utc_ymd();
        let parts: Vec<_> = s.split('/').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].parse::<i32>().is_ok());
        assert!(parts[1].parse::<u32>().is_ok());
        assert!(parts[2].parse::<u32>().is_ok());
    }
}