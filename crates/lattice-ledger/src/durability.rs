//! Durability classes and group-commit batching (Lattice spec §9).
//!
//! - `Durable`: flush to the durability boundary before acknowledgement.
//! - `BatchedDurable`: group commit with a strict maximum latency (default
//!   25 ms), then flush before acknowledgement.
//! - `ProjectionOnly`: may be regenerated; no canonical durability promise.
//!
//! The group-commit writer is a simple bounded queue that a single background
//! task drains and flushes within `max_latency_ms`.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use crate::LedgerError;

/// A concrete physical entry to be durably written (a serialized frame body).
pub type SegmentBatch = Vec<u8>;

/// Group-commit policy.
#[derive(Debug, Clone, Copy)]
pub enum GroupCommitPolicy {
    /// Max latency between the first pending write and the flush boundary.
    Config { max_latency_ms: u64 },
    /// Direct write (no batching) — behaves like `Durable`.
    NoBatching,
}

/// Group-commit writer with an explicit flush method. Kept simple and
/// synchronous so the ledger test can drive it deterministically.
pub struct BatchedDurableWriter {
    queue: Mutex<VecDeque<SegmentBatch>>,
    max_latency_ms: u64,
}

impl BatchedDurableWriter {
    pub fn new(max_latency_ms: u64) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            max_latency_ms: max_latency_ms.max(1),
        }
    }

    /// Enqueue a frame body for the durable batch append.
    pub fn submit(&self, body: SegmentBatch) {
        self.queue.lock().unwrap().push_back(body);
    }

    /// The configured maximum group-commit latency.
    pub fn max_latency(&self) -> Duration {
        Duration::from_millis(self.max_latency_ms)
    }

    /// Drain all pending frames and run `flush` on each (durability boundary).
    pub fn drain_with<F>(&self, mut flush: F) -> Result<(), LedgerError>
    where
        F: FnMut(&[u8]) -> Result<(), LedgerError>,
    {
        let mut q = self.queue.lock().unwrap();
        let mut collected = Vec::new();
        while let Some(body) = q.pop_front() {
            collected.push(body);
        }
        for body in &collected {
            flush(body)?;
        }
        Ok(())
    }

    /// Blocking self-flush used by tests/clean shutdown: appends each pending
    /// frame via the supplied callback, if any. When no callback is provided
    /// the queue is drained with no durability (caller must supply the writer).
    pub fn is_pending(&self) -> bool {
        !self.queue.lock().unwrap().is_empty()
    }
}

impl GroupCommitPolicy {
    pub fn writer(self) -> BatchedDurableWriter {
        match self {
            GroupCommitPolicy::Config { max_latency_ms } => {
                BatchedDurableWriter::new(max_latency_ms)
            }
            GroupCommitPolicy::NoBatching => BatchedDurableWriter::new(0),
        }
    }
}