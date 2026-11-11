//! Gas Town orchestration engine (`gastown.*` · `gt_*`).
//!
//! This module implements the REAL Gas Town orchestration system — towns,
//! rigs, agents, beads, dependencies, convoys, the reconciler, durable
//! scheduling, the review queue, merging/landing, and the mayor/polecat/
//! refinery role semantics — inside NAI-Lattice, faithful to the hosted
//! Gas Town product (`Kilo-Org/cloud` at `services/gastown/`, commit
//! `322138a19`). Substrate specifics are adapted: the Cloudflare Worker +
//! Durable Object + container runtime become the NAI-Lattice host, its
//! canonical ledger/CAS/hot-store, and `lattice-supervisor`-driven real-model
//! agent processes in isolated git worktrees.
//!
//! NAI-Lattice remains the singleton data fabric and control plane. This
//! module is Gas Town *domain logic*, not a second fabric.

pub mod state;
pub mod git;
pub mod reconciler;
pub mod agent;
pub mod control;
pub mod scheduler;
pub mod adversarial;
pub mod portability;
#[cfg(test)]
pub mod integration;

pub use state::{OrchestrationStore, Town, Rig, Agent, Bead, Dependency, ReviewQueueEntry, Convoy};
pub use scheduler::Orchestrator;