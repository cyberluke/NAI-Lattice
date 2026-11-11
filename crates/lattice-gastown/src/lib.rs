//! `lattice-gastown` — the Gas Town domain module inside NAI-Lattice (spec §4).
//!
//! This crate contains ONLY Gas Town domain logic: the host/session registry,
//! server-owned task epochs, the review graph, Git snapshot coordination, and
//! projection/migration mappings. It owns no second event store, no second CAS,
//! no pipe, and no second service; the canonical LEDGER is the single source of
//! truth and `nai-fabric-host.exe` is the sole control plane.
//!
//! Everything generic (durable scheduler, process supervision, IPC client,
//! telemetry, CAS) lives in shared NAI-Lattice crates (`lattice-supervisor`,
//! `lattice-cas`, `lattice-ledger`, ...) and is imported, not duplicated here.

pub mod capabilities;
pub mod events;
pub mod host_registry;
pub mod task_epoch;
pub mod git_observer;
pub mod review;
pub mod projection;
pub mod service;
pub mod migration;
pub mod review_evidence;
pub mod orchestration;

pub use service::GastownService;
pub use task_epoch::{TaskEpoch, TaskEpochId};
pub use host_registry::{HostRegistry, HostRecord, SessionRecord, AttachState, Lease};
pub use review::{Role, Severity, JobState, ReviewJob, ReviewFinding, ReviewPolicy, Artifacts, EvidenceRef, ModelProvenance, RoleOutput};
pub use events::{EventKind, GastownEvent, AckStatus, ReplayCursor};
pub use migration::legacy::{LegacyMigration, LegacyRow, ImportCounts, ArtifactStore};
pub use migration::{ImportKind, ImportLedger, IMPORT_ORDER};

use serde::{Deserialize, Serialize};

/// Stable produced identity for a Gas Town host installation (per §5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ProducerIdentity {
    /// Stable per Kilo runtime installation.
    pub producer_id: String,
    /// New on process restart.
    pub producer_epoch: u64,
}

/// A workspace's canonical identity (worktree-normalized).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceId {
    pub canonical: String,
}

/// Gas Town feature flags (spec §15), single source, no duplicated config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GastownFlags {
    pub native_enabled: bool,
    pub native_shadow: bool,
    pub legacy_accept_new_jobs: bool,
    pub review_real_model_only: bool,
}

impl Default for GastownFlags {
    fn default() -> Self {
        Self {
            native_enabled: true,
            native_shadow: false,
            legacy_accept_new_jobs: false,
            review_real_model_only: true,
        }
    }
}