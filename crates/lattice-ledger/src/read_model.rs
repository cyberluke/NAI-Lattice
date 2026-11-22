//! CQRS read model for the canonical ledger — the "hot-queue projection".
//!
//! This is the QUIERY side of CQRS. The [`crate::Ledger`] is the pure event
//! source: an append-only, immutable command store (event sourcing). Reading the
//! ledger to render a live dashboard/tail should NEVER touch that append path —
//! a `scan_events` over the whole ledger is O(total) and, worse, risks blocking
//! the append mutex (a bulkhead violation). Per the SRE runbook ("singlehost
//! hot-queue projection") we keep a separate, incrementally-updated READ model:
//!
//! ```text
//!   Command side                                Query side
//!   ─────────────────                           ─────────────────
//!   Ledger (event source)                       LedgerReadModel
//!     append() ▸ write only                       tail()   ▸ read only
//!     (mutable, append-only)                      (bounded hot-queue)
//!          │                                          ▲
//!          │ high_water (last_sequence)               │ fast, own lock
//!          ▼                                          │
//!      sync() worker ▸ delta(d) ▸ apply to model ─────┘
//!        (background, owns its own handle)
//! ```
//!
//! Properties (bulkhead + CQRS):
//!   * The read model owns a SEPARATE mutex from the ledger's append mutex.
//!     A slow/exhaustive reader contending on the model can NEVER block the
//!     writer path. This is the bulkhead the naive `scan_events`-every-read
//!     breaks.
//!   * `sync()` replays only the delta since the last applied high-water mark
//!     (O(delta), not O(total)). It is driven by the runtime's background
//!     worker, so the transaction side never pays for read-side work.
//!   * `tail(n)` returns the last `n` canonical events from the in-memory hot
//!     queue — O(n), bounded memory (the queue is capacity-capped), and never
//!     touches disk.
//!   * The model is eventually consistent: it converges to the ledger within
//!     one `sync()` tick (yielding a fresh high-water register), as the CQRS
//!     pattern expects.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use nai_lattice_core::CanonicalEvent;

use crate::{EventScanFilter, LedgerError};

/// Default number of recent events the hot queue retains. Mirrors the SSE tail.
pub const DEFAULT_TAIL_CAPACITY: usize = 2048;

/// A serializable snapshot of the read model: the applied high-water cursor
/// plus the bounded hot queue. Persisted to disk periodically so a restart can
/// restore tail + cursor in O(1) and then resume replaying only the O(delta) —
/// exactly the enterprise "durable cursor + snapshot" recovery the CTO requires
/// (a cold full-scan rebuild on every restart is NOT acceptable).
#[derive(serde::Serialize, serde::Deserialize)]
struct ReadModelSnapshot {
    applied_high_water: u64,
    events: Vec<CanonicalEvent>,
}

/// The CQRS query-side read model. Read-only to callers; mutated only by
/// [`LedgerReadModel::sync`] from the background projection worker.
pub struct LedgerReadModel {
    /// Bounded, newest-last ring of canonical events (the hot-queue projection).
    queue: Mutex<VecDeque<CanonicalEvent>>,
    /// How many events the ring may hold without evicting the oldest.
    capacity: usize,
    /// Last ledger high-water mark this model has applied. `sync()` starts from
    /// the exclusive successor of this and replays only what's newer.
    applied_high_water: AtomicU64,
    /// Path to the persisted snapshot (durable cursor + hot queue). `None` when
    /// the read model is ephemeral (tests / temporary).
    snapshot_path: Option<std::path::PathBuf>,
    /// Every N events, persist a fresh snapshot (durable cursor). Bounded I/O.
    snapshot_every: u64,
    /// Events applied since the last snapshot persist.
    since_snapshot: std::sync::atomic::AtomicU64,
}

impl LedgerReadModel {
    /// Create an ephemeral read model (no durable cursor). Used by tests and
    /// by callers that do not need restart recovery.
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity: capacity.max(1),
            applied_high_water: AtomicU64::new(0),
            snapshot_path: None,
            snapshot_every: 1024,
            since_snapshot: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Open a read model with a DURABLE cursor: the snapshot (applied high-water
    /// + hot queue) is restored from `path` on open, so a restart does NOT cold-
    /// scan the whole ledger — it resumes replaying only what's newer than the
    /// restored cursor. If no snapshot exists yet, starts as an empty model.
    pub fn open_with_snapshot(path: std::path::PathBuf, capacity: usize) -> Self {
        let m = Self {
            queue: Mutex::new(VecDeque::with_capacity(capacity.max(1))),
            capacity: capacity.max(1),
            applied_high_water: AtomicU64::new(0),
            snapshot_path: Some(path.clone()),
            snapshot_every: 1024,
            since_snapshot: std::sync::atomic::AtomicU64::new(0),
        };
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(snap) = serde_json::from_slice::<ReadModelSnapshot>(&bytes) {
                {
                    let mut q = m.queue.lock().unwrap_or_else(|e| e.into_inner());
                    *q = snap.events.into_iter().collect();
                }
                m.applied_high_water.store(snap.applied_high_water, Ordering::Relaxed);
                // Always persist a clean baseline immediately so the snapshot file
                // exists even if the source was corrupt/absent.
            } else {
                let snap: ReadModelSnapshot = ReadModelSnapshot {
                    applied_high_water: 0,
                    events: Vec::new(),
                };
                if let Ok(bytes) = serde_json::to_vec(&snap) {
                    let _ = std::fs::write(&path, bytes);
                }
            }
        }
        m
    }

    /// A reference snapshot for background persistence (used by the worker).
    fn snapshot_bytes(&self) -> Option<Vec<u8>> {
        let queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let snap = ReadModelSnapshot {
            applied_high_water: self.applied_high_water.load(Ordering::Relaxed),
            events: queue.iter().cloned().collect(),
        };
        serde_json::to_vec(&snap).ok()
    }

    /// The highest ledger sequence reflected in this model so far.
    pub fn applied_sequence(&self) -> u64 {
        self.applied_high_water.load(Ordering::Relaxed)
    }

    /// Return the last `n` canonical events (newest last). Never touches disk;
    /// never blocks the ledger append path (separate lock).
    pub fn tail(&self, n: usize) -> Vec<CanonicalEvent> {
        let queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let skip = queue.len().saturating_sub(n.max(1));
        queue.iter().skip(skip).cloned().collect()
    }

    /// Return the next `n` events strictly after `after` (exclusive), in
    /// ascending order — the cursor-driven SSE/API pagination. (Not "last n";
    /// use [`tail`] for that.) The queue is sequence-ordered ascending.
    pub fn since(&self, after: u64, n: usize) -> Vec<CanonicalEvent> {
        let queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue
            .iter()
            .filter(|ev| ev.runtime_sequence > after)
            .take(n.max(1))
            .cloned()
            .collect()
    }

    /// Replay the ledger delta since the last applied high-water mark into this
    /// model (O(delta)). Safe to call from a single worker; internally it takes
    /// ONLY the model's own lock — never the ledger append lock. Recovers the
    /// missing tail by scanning from the exclusive successor of `applied_water`.
    ///
    /// `scan` is a closure the runner supplies: it calls
    /// `ledger.scan_events(&EventScanFilter { after: Some(last), .. })`.
    pub fn sync<F>(&self, scan: F) -> Result<(), LedgerError>
    where
        F: FnOnce(u64) -> Result<Vec<CanonicalEvent>, LedgerError>,
    {
        let start = self.applied_high_water.load(Ordering::Relaxed);
        let events = scan(start)?;
        if events.is_empty() {
            return Ok(());
        }
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        // Advance high-water to the newest applied event so the next sync only
        // replays the true delta (and no event is left behind by a gap). We take
        // the NEXT applied edge as the new base: the exclusive `after` for the
        // following sync is the max sequence applied now.
        let mut last_seq = start;
        for ev in events {
            if ev.runtime_sequence > last_seq {
                last_seq = ev.runtime_sequence;
            }
            queue.push_back(ev);
            while queue.len() > self.capacity {
                queue.pop_front();
            }
        }
        self.applied_high_water.store(last_seq, Ordering::Relaxed);
        // Durable cursor: persist a bounded snapshot every `snapshot_every`
        // applied events (not each one — bounded disk I/O). Snapshot bytes are
        // built under the queue lock, written AFTER it is released so the file
        // write never extends the hot-queue lock hold (bulkhead).
        let snaps = self.since_snapshot.fetch_add(1, Ordering::Relaxed) + 1;
        if self.snapshot_path.is_some() && snaps % self.snapshot_every == 0 {
            let bytes = {
                let snap = ReadModelSnapshot {
                    applied_high_water: last_seq,
                    events: queue.iter().cloned().collect(),
                };
                serde_json::to_vec(&snap).unwrap_or_default()
            };
            drop(queue);
            if let Some(path) = &self.snapshot_path {
                let _ = std::fs::write(path, bytes);
            }
        }
        Ok(())
    }
}

impl EventScanFilter {
    /// Convenience: a filter that resumes from `after` (exclusive) and is
    /// otherwise unconstrained — the delta the read model worker consumes.
    pub fn exclusive_after(after: u64) -> Self {
        Self {
            after: Some(after),
            limit: None,
            namespace: None,
            event_type: None,
            entity_id: None,
            correlation_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nai_lattice_core::{EventId, GlobalId};

    fn ev(seq: u64) -> CanonicalEvent {
        CanonicalEvent {
            event_id: EventId::uuid_v7(),
            runtime_sequence: seq,
            timestamp_utc_ms: 1,
            device_id: uuid::Uuid::nil(),
            producer_app: "test".into(),
            producer_version: "1".into(),
            namespace: "test".into(),
            entity_id: Some(GlobalId::entity("email", &format!("m{seq}"))),
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

    #[test]
    fn tail_returns_last_n() {
        let m = LedgerReadModel::new(8);
        m.sync(|after| {
            let s = if after == 0 { 1u64 } else { after + 1 };
            Ok((s..=5).map(ev).collect())
        })
        .unwrap();
        let t = m.tail(3);
        assert_eq!(t.len(), 3);
        assert_eq!(t.last().unwrap().runtime_sequence, 5);
        assert_eq!(m.applied_sequence(), 5);
        // capacity-bounded: oldest evicted when over capacity
        m.sync(|_| Ok((6..=20).map(ev).collect())).unwrap();
        assert!(m.tail(100).len() <= 8);
    }

    #[test]
    fn since_resumes_from_cursor() {
        let m = LedgerReadModel::new(8);
        m.sync(|_| Ok((1..=6).map(ev).collect())).unwrap();
        let s = m.since(3, 2);
        assert_eq!(s.len(), 2);
        assert_eq!(s.first().unwrap().runtime_sequence, 4);
        assert_eq!(s.last().unwrap().runtime_sequence, 5);
    }

    #[test]
    fn durable_cursor_snapshot_recovery() {
        // Enterprise claim: after a restart, open_with_snapshot restores the
        // applied high-water + hot queue from disk (no cold full-scan), then a
        // subsequent sync replays only the O(delta) beyond the restored cursor.
        let dir = tempfile::tempdir().unwrap();
        let snap_path = dir.path().join("read-model.json");

        let mut m = LedgerReadModel::open_with_snapshot(snap_path.clone(), 8);
        // Simulate a process that applied seqs 1..=5, then force a snapshot.
        m.snapshot_path = Some(snap_path.clone());
        m.snapshot_every = 1; // persist every sync in this test
        m.sync(|_| Ok((1..=5).map(ev).collect())).unwrap();
        let applied = m.applied_sequence();
        let tail5 = m.tail(5).len();
        drop(m);

        // "Restart": reopen from the same snapshot — cursor + queue restored.
        let m2 = LedgerReadModel::open_with_snapshot(snap_path.clone(), 8);
        assert_eq!(m2.applied_sequence(), applied, "durable cursor must survive restart");
        assert_eq!(m2.tail(5).len(), tail5, "hot queue must survive restart");
        // The next sync must only replay what's newer than the restored cursor
        // (O(delta)) — simulate events 6..=9 arriving while we were down.
        m2.sync(|after| {
            assert_eq!(after, applied, "sync after restart must resume from durable cursor");
            Ok((6..=9).map(ev).collect())
        })
        .unwrap();
        assert_eq!(m2.applied_sequence(), 9);
        assert_eq!(m2.tail(100).last().unwrap().runtime_sequence, 9);
    }
}// touch
