//! Durability classes for the canonical ledger (spec §9 "Durability").
//!
//! An event is acknowledged only according to the durability class its
//! producer requested. Never silently downgrade durability.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Durability {
    /// Acknowledge only after the frame is written **and** the durability
    /// boundary (FlushFileBuffers) completes. Used for email mutations, user
    /// preferences, user-created chat messages and agent checkpoints.
    Durable,
    /// Group commit with a strict maximum latency (default 25ms), then flush
    /// before acknowledgement. Bounded batched durability.
    BatchedDurable,
    /// May be regenerated; no canonical durability promise. Used for
    /// behavioral/analytics events.
    ProjectionOnly,
}

impl Durability {
    pub fn is_durable(&self) -> bool {
        !matches!(self, Durability::ProjectionOnly)
    }
    pub fn batch_latency_ms(&self) -> Option<u64> {
        match self {
            Durability::BatchedDurable => Some(25),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn durable_semantics() {
        assert!(Durability::Durable.is_durable());
        assert!(Durability::BatchedDurable.is_durable());
        assert!(!Durability::ProjectionOnly.is_durable());
        assert_eq!(Durability::BatchedDurable.batch_latency_ms(), Some(25));
    }
}