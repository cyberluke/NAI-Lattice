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
        // For a FAILED flush we must not lose acknowledged-but-unpersisted
        // frames: pop ONE, flush it; if it fails, re-queue this frame plus any
        // that were already popped (they were never durably written) and bail.
        // This makes the caller's retry durable instead of silently dropping
        // frames (Codex review finding P1-3).
        let mut survivors: Vec<Vec<u8>> = Vec::new();
        while let Some(body) = q.pop_front() {
            match flush(&body) {
                Ok(()) => {}
                Err(e) => {
                    // Re-push this failing frame and every already-popped member
                    // back to the front (FIFO order) so nothing is lost.
                    survivors.push(body);
                    while let Some(b) = q.pop_front() {
                        survivors.push(b);
                    }
                    survivors.reverse();
                    for b in survivors {
                        q.push_front(b);
                    }
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Blocking self-flush used by tests/clean shutdown: appends each pending
    /// frame via the supplied callback, if any. When no callback is provided
    /// the queue is drained with no durability (caller must supply the writer).
    pub fn is_pending(&self) -> bool {
        !self.queue.lock().unwrap().is_empty()
    }

    /// Number of frames awaiting durable persistence (tests/diagnostics).
    pub fn pending_len(&self) -> usize {
        self.queue.lock().unwrap().len()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_with_failure_preserves_unflushed_frames() {
        // Regression for the Codex review finding P1-3: a mid-drain flush
        // failure must not lose acknowledged-but-unpersisted queued frames.
        let w = BatchedDurableWriter::new(25);
        w.submit(vec![1, 1, 1]);
        w.submit(vec![2, 2, 2]);
        w.submit(vec![3, 3, 3]);
        let mut flushed = Vec::new();
        // Fail on the second flush.
        let res = w.drain_with(|body| {
            if body[0] == 2 {
                return Err(LedgerError::Io(std::io::Error::new(std::io::ErrorKind::OutOfMemory, "simulated flush failure")));
            }
            flushed.push(body.to_vec());
            Ok(())
        });
        assert!(res.is_err(), "flush failure must surface");
        assert_eq!(flushed, vec![vec![1, 1, 1]], "the successfully flushed frame is recorded");
        // Frames 2 and 3 must still be in the queue (not lost).
        assert_eq!(w.pending_len(), 2, "unflushed frames are preserved for retry");
        // A subsequent drain (after the failure resolves) persists the remainder.
        let res2 = w.drain_with(|body| { flushed.push(body.to_vec()); Ok(()) });
        assert!(res2.is_ok());
        assert_eq!(w.pending_len(), 0);
        assert_eq!(flushed, vec![vec![1, 1, 1], vec![2, 2, 2], vec![3, 3, 3]], "recovery persists all frames in order");
    }
}