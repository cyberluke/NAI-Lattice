//! `gastown.*` NAF capabilities registered by the domain service (spec §5).

/// Every `gastown.*` capability the service exposes, gated by the host.
pub const GASTOWN_CAPABILITIES: &[&str] = &[
    "gastown.host.register",
    "gastown.host.heartbeat",
    "gastown.host.disconnect",
    "gastown.session.sync",
    "gastown.event.append_batch",
    "gastown.event.ack_status",
    "gastown.event.replay_cursor",
    "gastown.review.enqueue_explicit",
    "gastown.review.job_status",
    "gastown.review.list_jobs",
    "gastown.review.list_findings",
    "gastown.review.report",
    "gastown.review.cancel",
    "gastown.review.retry",
    "gastown.worker.register",
    "gastown.worker.heartbeat",
    "gastown.worker.result",
    "gastown.admin.health",
    "gastown.admin.metrics",
    "gastown.admin.reconcile",
    "gastown.admin.migration_status",
    "gastown.admin.migrate_legacy",
];

/// A single `gastown.*` capability descriptor.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GastownCapability {
    pub name: &'static str,
    pub version: &'static str,
}

/// The set of capabilities this service version advertises.
pub fn capabilities() -> Vec<GastownCapability> {
    GASTOWN_CAPABILITIES
        .iter()
        .map(|n| GastownCapability { name: n, version: "1" })
        .collect()
}