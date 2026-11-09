//! Projection contract (spec §28) and projection metadata.
//!
//! Every projection implements the same lifecycle. Rebuild is generation-based:
//! build B, verify B, atomically swap A→B; never destroy the only usable index
//! before a replacement is verified.

use serde::{Deserialize, Serialize};

use crate::ledger::CanonicalEvent;

/// A ledger sequence (u64 counter). All projection application is idempotent on
/// `last_applied_sequence`.
pub type Sequence = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectionState {
    Healthy,
    CatchingUp,
    Rebuilding,
    Degraded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectionHealth {
    pub state: ProjectionState,
    pub last_applied_sequence: Sequence,
    pub last_verify_ms: Option<i64>,
    pub detail: Option<String>,
}

/// The common lifecycle every projection implementor provides.
#[async_trait::async_trait]
pub trait Projection: Send + Sync + 'static {
    fn id(&self) -> &'static str;
    fn schema_version(&self) -> u32;
    fn engine(&self) -> &'static str;
    async fn checkpoint(&self) -> Result<Sequence, crate::SystemError>;
    async fn apply_batch(&self, events: &[CanonicalEvent]) -> Result<(), crate::SystemError>;
    async fn verify(&self) -> Result<ProjectionHealth, crate::SystemError>;
    /// Generation-based rebuild draining `source` until caught up.
    async fn rebuild(&self) -> Result<(), crate::SystemError>;
    async fn compact(&self) -> Result<(), crate::SystemError>;
}

/// Event stream source used by `Projection::rebuild` (e.g. the ledger replay).
#[derive(Debug, Clone, Copy)]
pub enum ReplaySource {
    Ledger,
    HotState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_json_roundtrips() {
        let s = ProjectionState::CatchingUp;
        assert_eq!(serde_json::from_str::<ProjectionState>(&serde_json::to_string(&s).unwrap()).unwrap(), s);
    }
}