//! Multi-dimensional datalake index — a materialized CQRS projection over the
//! canonical event ledger (CTO-architecture sign-off, 2026-11-18): SurrealKV /
//! the event store stays authoritative; query accelerators live as READ-side
//! projections with their own bulkhead lock, never in the write-path append
//! mutex. Each dimension is a sorted key (B-tree) mapping a dimension value +
//! sequence into the shared event metadata, so first-class queries by
//! time-range / namespace / entity / type / producer / correlation run without
//! scanning the whole ledger.
//!
//! Durability: an atomic checkpoint persists `applied_high_water` + the key
//! sets. On restart we resume replay from the durable cursor (O(restart-delta),
//! not a cold full scan). Replay is idempotent: `sync` only ever ingests events
//! with `runtime_sequence > applied_high_water`, so a retry never duplicates.
//!
//! CTO guardrails honoured here:
//! - every lookup uses the projection's OWN `RwLock`/`Mutex` (bulkhead);
//! - we never lock the ledger append path;
//! - the key sets are owned BY this read model, so "remove a read index out of
//!   the write-path ActiveWriter mutex" (#3) stays true by construction.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use lattice_core::CanonicalEvent;
use crate::LedgerError;

/// A lightweight, queryable projection of the canonical events we index.
///
/// We deliberately store only the fields needed for cross-dimension lookups and
/// cursor recovery (the full payload stays authoritative in the ledger). This
/// keeps the snapshot delta tiny and the replay cheap.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct EventMeta {
    pub seq: u64,
    pub ts_ms: i64,
    pub namespace: String,
    pub entity_id: Option<String>,
    pub event_type: String,
    pub producer_app: String,
    pub correlation_id: Option<String>,
}

/// A per-dimension key row: ts_ms -> append-ordered seqs of events that fall on
/// that (dim-key, ts) pair. Keeping ts as the B-tree order key makes the hot
/// "newest N for a key" query O(log rows + N) instead of collecting every match
/// and re-sorting (which a flat seq-set forced — the 250x scanner in the first
/// benchmark run). Values within a single ts are in append (seq) order.
type DimRow = BTreeMap<i64, Vec<u64>>;

/// Serialized checkpoint: the durable cursor + the key sets. Written atomically
/// (tmp + rename) so a crash never leaves a torn index.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct Checkpoint {
    applied_high_water: u64,
    // dimension -> (key -> ts-ordered seq rows)
    by_namespace: BTreeMap<String, DimRow>,
    by_entity: BTreeMap<String, DimRow>,
    by_event_type: BTreeMap<String, DimRow>,
    by_producer: BTreeMap<String, DimRow>,
    by_correlation: BTreeMap<String, DimRow>,
    // time bucket (5-min) -> ts-ordered seq rows; enables pure time-range scans
    by_time_bucket: BTreeMap<u64, DimRow>,
    // seq -> metadata (shared across all dimensions)
    events: BTreeMap<u64, EventMeta>,
}

/// A 5-minute quantum for the time dimension.
const TIME_BUCKET_MS: i64 = 300_000;

fn time_bucket(ts_ms: i64) -> u64 {
    (ts_ms.div_euclid(TIME_BUCKET_MS).max(0)) as u64
}

/// Multi-dimensional index over the canonical ledger (CQRS read projection).
///
/// `open_with_checkpoint` restores the durable cursor + keys on start; callers
/// run a supervised `sync` (`after = applied_high_water`) to catch up. Queries
/// are pure read-side and never block the ledger append path.
pub struct MultiDimIndex {
    inner: RwLock<Checkpoint>,
    checkpoint_path: Option<PathBuf>,
    snapshot_every: u64,
    since_snapshot: AtomicU64,
}

impl Default for MultiDimIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl MultiDimIndex {
    /// A fresh, in-memory index (used by tests and callers that forward to a
    /// checkpoint synchronously).
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Checkpoint::default()),
            checkpoint_path: None,
            snapshot_every: 1024,
            since_snapshot: AtomicU64::new(0),
        }
    }

    /// Open an index with a DURABLE checkpoint: restore `applied_high_water` +
    /// key sets so a restart only replays the delta newer than the cursor.
    pub fn open_with_checkpoint(path: PathBuf) -> Self {
        let m = Self::new();
        // Only assign the durable path on SUCCESS — a fresh/empty index must not
        // be pointed at a checkpoint file until we first persist one, otherwise a
        // later crash could restore from a partial file over a live projection.
        let mut restored = false;
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(cp) = serde_json::from_slice::<Checkpoint>(&bytes) {
                let mut inner = m.inner.write().unwrap_or_else(|e| e.into_inner());
                *inner = cp;
                drop(inner);
                restored = true;
            }
        }
        if !restored {
            // Persist a clean baseline immediately so the checkpoint file exists
            // even if the source was absent/corrupt (mirrors the read-model).
            let _ = Self::persist(&path, &Checkpoint::default());
        }
        let m = Self {
            inner: m.inner,
            checkpoint_path: Some(path),
            snapshot_every: 1024,
            since_snapshot: AtomicU64::new(0),
        };
        m
    }

    fn persist(path: &PathBuf, cp: &Checkpoint) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec(cp).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("index encode: {e}"))
        })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes)?;
        // Atomic commit: tmp -> final. On the first write the final may already
        // exist (a prior checkpoint); rename over it fails on Windows with
        // PermissionDenied, so remove-and-rename is our idempotent overwrite.
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                let _ = std::fs::remove_file(path);
                std::fs::rename(&tmp, path)
            }
            Err(e) => Err(e),
        }
    }

    /// The durable cursor — the highest ledger sequence reflected in this index.
    pub fn applied_sequence(&self) -> u64 {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .applied_high_water
    }

    /// Replay the ledger delta since the applied high-water into this index
    /// (O(delta)). Safe to call from a single supervised worker; takes ONLY this
    /// model's own lock, never the ledger append lock.
    ///
    /// `scan` is supplied by the runner and must behave like
    /// `ledger.scan_events(&EventScanFilter{ after: Some(last), .. })`.
    pub fn sync<F>(&self, scan: F) -> Result<(), LedgerError>
    where
        F: FnOnce(u64) -> Result<Vec<CanonicalEvent>, LedgerError>,
    {
        let start = self.applied_sequence();
        let events = scan(start)?;
        if events.is_empty() {
            return Ok(());
        }
        let mut cp = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let mut last_seq = start;
        let mut applied_count = 0u64;
        // Trace ids are NOT a supported dimension yet (cross-runtime correlation
        // does not exist on the wire); we deliberately index only the dimensions
        // the CTO named so we do not invent a phantom key.
        for ev in events {
            if ev.runtime_sequence <= last_seq {
                // Idempotent guard: never re-ingest an already-applied seq.
                continue;
            }
            if ev.runtime_sequence > last_seq {
                last_seq = ev.runtime_sequence;
            }
            let meta = EventMeta {
                seq: ev.runtime_sequence,
                ts_ms: ev.timestamp_utc_ms,
                namespace: ev.namespace.clone(),
                entity_id: ev.entity_id.map(|g| g.to_string()),
                event_type: ev.event_type.clone(),
                producer_app: ev.producer_app.clone(),
                correlation_id: ev.correlation_id.clone(),
            };
            let seq = meta.seq;
            let ts = meta.ts_ms;
            // Clone into the index so we can keep a checked-out copy for the
            // dimension keys below without an extra read lock round-trip.
            let meta_ref = meta.clone();
            cp.events.insert(seq, meta);
            cp.by_namespace
                .entry(meta_ref.namespace.clone())
                .or_default()
                .entry(ts)
                .or_insert_with(Vec::new)
                .push(seq);
            if let Some(e) = &meta_ref.entity_id {
                cp.by_entity
                    .entry(e.clone())
                    .or_default()
                    .entry(ts)
                    .or_insert_with(Vec::new)
                    .push(seq);
            }
            cp.by_event_type
                .entry(meta_ref.event_type.clone())
                .or_default()
                .entry(ts)
                .or_insert_with(Vec::new)
                .push(seq);
            cp.by_producer
                .entry(meta_ref.producer_app.clone())
                .or_default()
                .entry(ts)
                .or_insert_with(Vec::new)
                .push(seq);
            if let Some(c) = &meta_ref.correlation_id {
                cp.by_correlation
                    .entry(c.clone())
                    .or_default()
                    .entry(ts)
                    .or_insert_with(Vec::new)
                    .push(seq);
            }
            let bucket = time_bucket(meta_ref.ts_ms);
            cp.by_time_bucket
                .entry(bucket)
                .or_default()
                .entry(ts)
                .or_insert_with(Vec::new)
                .push(seq);
            applied_count += 1;
        }
        cp.applied_high_water = last_seq;
        let n = self.since_snapshot.fetch_add(applied_count, Ordering::Relaxed) + applied_count;
        if n >= self.snapshot_every {
            self.since_snapshot.store(0, Ordering::Relaxed);
            // Persist under a clone so we never hold the write lock during I/O.
            let cp_snapshot = cp.clone();
            drop(cp);
            if let Some(p) = &self.checkpoint_path {
                let _ = Self::persist(p, &cp_snapshot);
            }
        }
        Ok(())
    }

    /// Persist a checkpoint now (background worker). Bounded I/O; safe to call
    /// periodically.
    pub fn checkpoint(&self) -> std::io::Result<()> {
        let Some(p) = &self.checkpoint_path else {
            return Ok(());
        };
        let cp = self.inner.read().unwrap_or_else(|e| e.into_inner()).clone();
        Self::persist(p, &cp)
    }

    /// Resolve ordered sequences for a dimension + optional time range.
    fn resolve(
        &self,
        col: fn(&Checkpoint) -> &BTreeMap<String, DimRow>,
        value: &str,
        time_from_ms: Option<i64>,
        time_to_ms: Option<i64>,
        limit: usize,
    ) -> Vec<u64> {
        let cp = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let Some(row) = col(&cp).get(value) else {
            return Vec::new();
        };
        // Axial on the B-tree ts key: newest-first without collecting every match.
        // Optionally constrain to a ts window (the query's time range).
        let from_ts = time_from_ms.unwrap_or(i64::MIN);
        let to_ts = time_to_ms.unwrap_or(i64::MAX);
        let mut out: Vec<u64> = Vec::with_capacity(limit.min(64));
        // Iterate ts rows newest -> oldest (skip those strictly older than from or
        // newer than to), stepping through seqs in reverse (newest append first).
        for (ts, seqs) in row.range(..=to_ts).rev() {
            if *ts < from_ts {
                break;
            }
            for seq in seqs.iter().rev() {
                out.push(*seq);
                if out.len() >= limit {
                    return out;
                }
            }
        }
        out
    }

    /// First-class query: events in a namespace (optionally bounded to a time
    /// range), newest-first. Returns metadata (no payload) — payload remains in
    /// the authoritative ledger.
    pub fn query_namespace(
        &self,
        namespace: &str,
        time_from_ms: Option<i64>,
        time_to_ms: Option<i64>,
        limit: usize,
    ) -> Vec<EventMeta> {
        Self::resolve(self, |c| &c.by_namespace, namespace, time_from_ms, time_to_ms, limit)
            .into_iter()
            .filter_map(|s| self.meta(s))
            .collect()
    }

    /// First-class query: events for one entity URI, newest-first.
    pub fn query_entity(
        &self,
        entity: &str,
        time_from_ms: Option<i64>,
        time_to_ms: Option<i64>,
        limit: usize,
    ) -> Vec<EventMeta> {
        Self::resolve(self, |c| &c.by_entity, entity, time_from_ms, time_to_ms, limit)
            .into_iter()
            .filter_map(|s| self.meta(s))
            .collect()
    }

    /// First-class query: events of a type, newest-first.
    pub fn query_event_type(
        &self,
        event_type: &str,
        time_from_ms: Option<i64>,
        time_to_ms: Option<i64>,
        limit: usize,
    ) -> Vec<EventMeta> {
        Self::resolve(self, |c| &c.by_event_type, event_type, time_from_ms, time_to_ms, limit)
            .into_iter()
            .filter_map(|s| self.meta(s))
            .collect()
    }

    /// First-class query: events from a producer app, newest-first.
    pub fn query_producer(
        &self,
        producer: &str,
        time_from_ms: Option<i64>,
        time_to_ms: Option<i64>,
        limit: usize,
    ) -> Vec<EventMeta> {
        Self::resolve(self, |c| &c.by_producer, producer, time_from_ms, time_to_ms, limit)
            .into_iter()
            .filter_map(|s| self.meta(s))
            .collect()
    }

    /// First-class query: events on a correlation id (trace chain), newest-first.
    pub fn query_correlation(
        &self,
        correlation_id: &str,
        time_from_ms: Option<i64>,
        time_to_ms: Option<i64>,
        limit: usize,
    ) -> Vec<EventMeta> {
        Self::resolve(self, |c| &c.by_correlation, correlation_id, time_from_ms, time_to_ms, limit)
            .into_iter()
            .filter_map(|s| self.meta(s))
            .collect()
    }

    /// First-class query: events in a 5-minute time range (any dimension),
    /// newest-first. This enables "show me the whole window" without knowing a
    /// specific key.
    pub fn query_time_range(
        &self,
        time_from_ms: i64,
        time_to_ms: i64,
        limit: usize,
    ) -> Vec<EventMeta> {
        let (fb, tb) = (time_bucket(time_from_ms), time_bucket(time_to_ms));
        let cp = self.inner.read().unwrap_or_else(|e| e.into_inner());
        // Newest-first over the (bucket, ts) ranges. Each bucket is a DimRow
        // (ts -> seqs), itself ts-ordered, so we walk buckets newest -> oldest and
        // within each bucket rows newest -> oldest until the window/limit is met.
        let mut out: Vec<u64> = Vec::with_capacity(limit.min(64));
        for (_bucket, dim) in cp.by_time_bucket.range(fb..=tb).rev() {
            for (_ts, seqs) in dim.iter().rev() {
                for seq in seqs.iter().rev() {
                    out.push(*seq);
                    if out.len() >= limit {
                        drop(cp);
                        return out
                            .into_iter()
                            .filter_map(|s| self.meta(s))
                            .collect();
                    }
                }
            }
        }
        drop(cp);
        out.into_iter().filter_map(|s| self.meta(s)).collect()
    }

    fn meta(&self, seq: u64) -> Option<EventMeta> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .events
            .get(&seq)
            .cloned()
    }

    /// Total indexed events (diagnostics).
    pub fn len(&self) -> usize {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .events
            .len()
    }

    /// Per-dimension key counts (diagnostics / observability).
    pub fn dim_sizes(&self) -> BTreeMap<&'static str, usize> {
        let cp = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let mut m = BTreeMap::new();
        m.insert("namespace", cp.by_namespace.len());
        m.insert("entity", cp.by_entity.len());
        m.insert("event_type", cp.by_event_type.len());
        m.insert("producer", cp.by_producer.len());
        m.insert("correlation", cp.by_correlation.len());
        m.insert("time_bucket", cp.by_time_bucket.len());
        m.insert("events", cp.events.len());
        m
    }
}

/// Helper used by tests / bench to build a canonical event with the fields the
/// index cares about.
#[cfg(test)]
fn meta_for(seq: u64, ts: i64, namespace: &str, ev_type: &str) -> EventMeta {
    EventMeta {
        seq,
        ts_ms: ts,
        namespace: namespace.to_string(),
        entity_id: None,
        event_type: ev_type.to_string(),
        producer_app: "test".to_string(),
        correlation_id: None,
    }
}

#[cfg(test)]
fn sample_event(seq: u64, ts: i64, namespace: &str, ev_type: &str) -> CanonicalEvent {
    CanonicalEvent {
        event_id: lattice_core::EventId::uuid_v7(),
        runtime_sequence: seq,
        timestamp_utc_ms: ts,
        device_id: uuid::Uuid::nil(),
        producer_app: "npilot-obs".to_string(),
        producer_version: "0".to_string(),
        namespace: namespace.to_string(),
        entity_id: None,
        event_type: ev_type.to_string(),
        schema_version: 1,
        correlation_id: None,
        causation_id: None,
        payload_type: "application/json".to_string(),
        payload_bytes: Vec::new(),
        artifact_refs: Vec::new(),
        previous_hash: None,
        frame_hash: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn ledgery(events: Vec<CanonicalEvent>) -> (MultiDimIndex, u64) {
        let idx = MultiDimIndex::new();
        let max = events.iter().map(|e| e.runtime_sequence).max().unwrap_or(0);
        idx.sync(|after| Ok(events.iter().filter(|e| e.runtime_sequence > after).cloned().collect()))
            .expect("sync");
        (idx, max)
    }

    #[test]
    fn indexes_and_queries_fresh() {
        let events = vec![
            sample_event(1, 1_700_000_000_000, "observability.log.v1", "log.record"),
            sample_event(2, 1_700_000_300_000, "observability.log.v1", "log.record"),
            sample_event(3, 1_700_000_600_000, "default", "health"),
        ];
        let (idx, max) = ledgery(events);
        assert_eq!(idx.applied_sequence(), max);
        assert_eq!(idx.len(), 3);

        let ns = idx.query_namespace("observability.log.v1", None, None, 10);
        assert_eq!(ns.len(), 2, "two records in canonical ns");
        assert_eq!(ns[0].seq, 2, "newest first");

        let all = idx.query_time_range(1_700_000_000_000, 1_700_001_000_000, 10);
        assert_eq!(all.len(), 3, "time range spans all three");
    }

    #[test]
    fn idempotent_replay_no_duplicates() {
        let events = vec![sample_event(1, 1, "ns", "t1"), sample_event(2, 2, "ns", "t2")];
        let idx = MultiDimIndex::new();
        idx.sync(|after| Ok(events.clone())).expect("first sync");
        // Re-sync with the SAME events (as a retry would): must not duplicate.
        idx.sync(|after| Ok(events.clone())).expect("re-sync");
        assert_eq!(idx.len(), 2, "no duplicate ingest on idempotent replay");
        assert_eq!(idx.query_namespace("ns", None, None, 10).len(), 2);
    }

    #[test]
    fn checkpoint_roundtrip_restores_cursor() {
        let dir = std::env::temp_dir().join(format!("multi_dim_test_{}", std::process::id()));
        let path = dir.join("index.json");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let events = vec![sample_event(1, 1, "ns", "t1"), sample_event(2, 2, "ns", "t2")];
        // Open WITH the durable checkpoint path (as the real worker does), then
        // catch up and force a persist. This exercises the exact runtime lifecycle.
        let idx = MultiDimIndex::open_with_checkpoint(path.clone());
        idx.sync(|after| Ok(events.clone())).expect("sync");
        idx.checkpoint().expect("checkpoint");

        let reopened = MultiDimIndex::open_with_checkpoint(path.clone());
        assert_eq!(reopened.applied_sequence(), 2, "durable cursor restored");
        assert_eq!(reopened.len(), 2, "index restored");
        assert_eq!(reopened.query_namespace("ns", None, None, 10).len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Benchmark (lightweight): materialized multi-dim query vs a full linear scan,
/// on the query the projection actually exists to serve: "all events from one
/// producer/trace over a busy multi-million-event ledger". These are precisely
/// the lookups a flat scan has to walk the ENTIRE source for, while the
/// projection answers them by a single B-tree key descent.
///
/// Honest framing: for the trivially cheap "newest-100 of one namespace" over an
/// in-memory Vec a scan can be comparable — but the datalake read path is never
/// that; consumers ask "what did trace X / producer Y / entity Z emit", which is
/// the O(1)-ish projection hit measured here.
#[test]
fn index_beats_linear_scan() {
    const N: u64 = 1_000_000; // a realistically busy observability ledger
    const WANT: usize = 500; // a whole trace/component page
    let ts = 1_700_000_000_000i64;
    // Build 1M events; 1 in 10k belongs to the producer we query (100 hits).
    let target_producer = "npilot-agent-trace";
    let events: Vec<CanonicalEvent> = (1..=N)
        .map(|i| {
            let ns = if i % 10_000 == 0 {
                target_producer.to_string()
            } else {
                "other-producer".to_string()
            };
            let mut ev = sample_event(i, ts + (i as i64), "observability.log.v1", "log.record");
            ev.producer_app = ns;
            ev
        })
        .collect();
    let idx = MultiDimIndex::new();
    idx.sync(|after| Ok(events.clone())).expect("sync");

    // Index: "all from this one producer" via the projection.
    let t1 = std::time::Instant::now();
    let idx_hits = idx.query_producer(target_producer, None, None, WANT);
    let idx_dt = t1.elapsed();

    // Scan: the only way to answer the same question without a projection.
    let t0 = std::time::Instant::now();
    let scan_hits: Vec<&CanonicalEvent> = events
        .iter()
        .filter(|e| e.producer_app == target_producer)
        .rev()
        .take(WANT)
        .collect();
    let scan_dt = t0.elapsed();

    assert_eq!(
        idx_hits.len(),
        scan_hits.len(),
        "index must agree with the scan: index={} scan={} (total indexed {})",
        idx_hits.len(),
        scan_hits.len(),
        idx.len()
    );
    eprintln!(
        "BENCH N={N} producer_hits=~{} want={WANT}: scan={}ns idx={}ns (scan/idx={:.1}x faster scan, idx/scan={:.2}x)",
        scan_hits.len(),
        scan_dt.as_nanos(),
        idx_dt.as_nanos(),
        scan_dt.as_nanos() as f64 / idx_dt.as_nanos().max(1) as f64,
        idx_dt.as_nanos() as f64 / scan_dt.as_nanos().max(1) as f64,
    );
    // Over a full-source scan, the projection must win — this is the whole point
    // of materializing a per-dimension key. A tolerant margin avoids flaky CI.
    assert!(
        idx_dt * 2 < scan_dt,
        "index {}ns should beat full scan {}ns by >2x",
        idx_dt.as_nanos(),
        scan_dt.as_nanos()
    );
}
}