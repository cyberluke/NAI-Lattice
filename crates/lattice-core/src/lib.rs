//! NAI Lattice core domain types.
//!
//! These are the versioned, engine-independent primitives that constitute the
//! **canonical substrate** (the architectural gate in the spec):
//!
//! ```text
//! EVENTS + ARTIFACTS + GLOBAL IDENTITY  ->  REBUILDABLE VIEWS
//! ```
//!
//! No engine (SurrealKV/DuckDB/Tantivy/LanceDB/Grafeo) appears here. Everything
//! in this crate is the durable, replayable, identity-stable contract that the
//! engines only ever project from.

use std::sync::atomic::{AtomicU64, Ordering};

pub mod id;
pub mod ledger;
pub mod cas;
pub mod durability;
pub mod projection;

pub use id::*;
pub use ledger::*;
pub use cas::*;
pub use durability::*;
pub use projection::*;

/// Canonical, engine-independent error type shared by the Lattice substrate.
#[derive(Debug, thiserror::Error)]
pub enum SystemError {
    #[error("ledger error: {0}")]
    Ledger(String),
    #[error("cas error: {0}")]
    Cas(String),
    #[error("hot-store error: {0}")]
    HotStore(String),
    #[error("projection {0}: {1}")]
    Projection(&'static str, String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("corruption: {0}")]
    Corruption(String),
    #[error("invalid: {0}")]
    Invalid(String),
}

impl SystemError {
    pub fn invalid<T: Into<String>>(msg: T) -> Self {
        Self::Invalid(msg.into())
    }
}

/// A process-local monotonically increasing sequence used to order ledger
/// frames. The authoritative ordering is the per-device `runtime_sequence` in
/// the ledger; this is the fast-path source for the append writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RuntimeSequence(u64);

impl RuntimeSequence {
    pub const ZERO: RuntimeSequence = RuntimeSequence(0);
    pub fn new(v: u64) -> Self {
        Self(v)
    }
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// A small allocator that yields strictly increasing runtime sequences under
/// contention (the append writer is the only producer in practice, but a
/// `fetch_add` keeps it cheap and panic-free).
#[derive(Debug, Default)]
pub struct SequenceAllocator {
    next: AtomicU64,
}

impl SequenceAllocator {
    pub fn new(start: u64) -> Self {
        Self {
            next: AtomicU64::new(start),
        }
    }
    pub fn allocate(&self) -> RuntimeSequence {
        RuntimeSequence::new(self.next.fetch_add(1, Ordering::Relaxed))
    }
}