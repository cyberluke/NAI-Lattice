//! `GastownService` — the domain facade the host drives (spec §4, §13).
//!
//! Owns the rebuildable projection, the review policy, host/session leases and
//! the `gastown.*` method dispatch surface. It does NOT own an event store; it
//! consumes committed canonical signals (the host passes what it already
//! committed) and produces typed NAF method responses. It never schedules model
//! workers here — those go through the shared scheduler/supervisor.

use serde_json::json;

use crate::capabilities;
use crate::host_registry::HostRecord;
use crate::migration::ImportLedger;
use crate::projection::{CommittedSignal, GastownProjection};
use crate::review::{JobState as JS, ReviewJob, ReviewPolicy};

/// Mutable service state (behind the host's single dispatch lock).
pub struct GastownService {
    pub projection: GastownProjection,
    pub policy: ReviewPolicy,
    pub import: ImportLedger,
    started_unix_ms: i64,
    flags: crate::GastownFlags,
    /// Fencing for the shared scheduler (keyed) — spec §9.
    scheduler_generation: u64,
}

impl GastownService {
    pub fn new(started_unix_ms: i64) -> Self {
        Self {
            projection: GastownProjection::new(),
            policy: ReviewPolicy::default(),
            import: ImportLedger::new(json!({"source": "legacy-gastown", "schema": "v7"})),
            started_unix_ms,
            flags: crate::GastownFlags::default(),
            scheduler_generation: 0,
        }
    }

    pub fn capabilities(&self) -> Vec<capabilities::GastownCapability> {
        capabilities::capabilities()
    }

    /// Replay committed signals (host calls this with every committed event).
    pub fn replay(&mut self, sig: CommittedSignal) {
        self.projection.apply(sig);
    }

    /// Register a host as real (verified by the pipe transport from a live OS
    /// process handle — never trusted from the wire). Returns ack JSON.
    pub fn host_register(&mut self, host: HostRecord) -> serde_json::Value {
        self.projection.hosts.register(host.clone());
        json!({"accepted": true, "host_runtime_id": host.host_runtime_id, "state": "attached"})
    }

    pub fn host_heartbeat(&mut self, host_id: &str, now_unix_ms: i64) -> serde_json::Value {
        if let Some(h) = self.projection.hosts.hosts.get_mut(host_id) {
            h.last_heartbeat_unix_ms = now_unix_ms;
        }
        json!({"accepted": true, "host_runtime_id": host_id})
    }

    pub fn host_disconnect(&mut self, host_id: &str, now_unix_ms: i64) -> serde_json::Value {
        self.projection.hosts.mark_disconnected(host_id, now_unix_ms);
        json!({"accepted": true, "host_runtime_id": host_id, "state": "disconnected"})
    }

    /// Health + metrics (admin).
    pub fn admin_health(&self, now_unix_ms: i64) -> serde_json::Value {
        json!({
            "service_pid": std::process::id(),
            "version": env!("CARGO_PKG_VERSION"),
            "service_stated_ms": self.started_unix_ms,
            "service_alive": true,
            "real_connected_hosts": self.projection.hosts.real_attached_count(now_unix_ms, 60_000),
            "real_active_sessions": self.projection.hosts.real_active_session_count(),
            "queued_jobs": self.projection.all_jobs(Some(JS::Queued)).len(),
            "running_jobs": self.projection.all_jobs(Some(JS::ReviewerA)).len() + self.projection.all_jobs(Some(JS::ReviewerB)).len(),
            "adjudicating_jobs": self.projection.all_jobs(Some(JS::Adjudicating)).len(),
            "watermark": self.projection.watermark,
            "flags": &self.flags,
        })
    }

    pub fn metrics(&self, now_unix_ms: i64) -> serde_json::Value {
        self.projection.metrics(now_unix_ms)
    }

    /// Enqueue a review job (idempotent by job idempotency key). The host is
    /// responsible for committing this decision to the ledger first; this only
    /// updates the projection + returns the job body.
    pub fn enqueue_review(&mut self, job: ReviewJob) -> serde_json::Value {
        // Same idempotency key => same job; no duplicate.
        let existing = self
            .projection
            .jobs
            .values()
            .find(|j| j.idempotency_key() == job.idempotency_key());
        if let Some(e) = existing {
            return json!({"job_id": e.job_id, "duplicate": true, "state": format!("{:?}", e.state)});
        }
        let id = job.job_id.clone();
        self.projection.upsert_job(job);
        json!({"job_id": id, "duplicate": false, "state": "Queued"})
    }

    pub fn job_status(&self, job_id: &str) -> serde_json::Value {
        match self.projection.jobs.get(job_id) {
            Some(j) => json!({"job_id": job_id, "state": format!("{:?}", j.state), "attempts": j.attempts, "report": j.artifacts.final_report_cas_ref}),
            None => json!({"job_id": job_id, "state": "unknown"}),
        }
    }

    pub fn list_jobs(&self, _state: Option<&str>) -> serde_json::Value {
        let jobs: Vec<_> = self.projection.all_jobs(None).iter().map(|j| json!({
            "job_id": j.job_id,
            "workspace": j.workspace_canonical,
            "state": format!("{:?}", j.state),
        })).collect();
        json!(jobs)
    }

    /// Fencing generation for the shared scheduler (spec §9).
    pub fn next_scheduler_generation(&mut self) -> u64 {
        self.scheduler_generation += 1;
        self.scheduler_generation
    }

    pub fn scheduler_generation(&self) -> u64 {
        self.scheduler_generation
    }

    /// Auto-enqueue review jobs for completed task epochs (task-completion →
    /// durable job automation). Returns the jobs that were newly enqueued.
    /// Idempotent: a completed epoch's idempotency key already has a job, so
    /// duplicate status events never produce a second job (HOST-05). The job's
    /// workspace comes from the epoch's OWN session record — never a global
    /// "last event" value — so concurrent runtimes in different workspaces are
    /// attributed correctly (spec §7).
    pub fn auto_enqueue_completed_epochs(&mut self) -> Vec<ReviewJob> {
        let epoch_ids: Vec<_> = self
            .projection
            .epochs
            .values()
            .filter(|e| e.state == crate::task_epoch::EpochState::ReviewQueued)
            .map(|e| e.id.clone())
            .collect();
        let mut created = Vec::new();
        for ep in epoch_ids {
            // Resolve this epoch's workspace from its own session record.
            let workspace = self
                .projection
                .hosts
                .sessions
                .get(&ep.session_id)
                .map(|s| s.workspace_canonical.clone())
                .filter(|w| !w.is_empty())
                .unwrap_or_else(|| "workspace".to_string());
            let job: ReviewJob = ReviewJob {
                job_id: format!("auto-{}:{}:{}", ep.host_runtime_id, ep.session_id, ep.turn_id),
                workspace_canonical: workspace,
                epoch: Some(ep.clone()),
                source_manifest_hash: "auto-task-epoch".into(),
                review_policy_version: self.policy.version,
                state: JS::Queued,
                created_unix_ms: now_ms(),
                artifacts: crate::review::Artifacts::default(),
                attempts: 0,
            };
            // A completed epoch's idempotency key already has a job => skip.
            let dup = self
                .projection
                .jobs
                .values()
                .any(|j| j.idempotency_key() == job.idempotency_key());
            if !dup {
                self.projection.upsert_job(job.clone());
                created.push(job);
            }
        }
        created
    }
}

/// JobState parse helper for NAF args.
pub type JobState = JS;

impl Default for GastownService {
    fn default() -> Self {
        Self::new(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64,
        )
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::Artifacts;
    use crate::task_epoch::TaskEpochId;

    fn job(id: &str, workspace: &str) -> ReviewJob {
        ReviewJob {
            job_id: id.to_string(),
            workspace_canonical: workspace.to_string(),
            epoch: Some(TaskEpochId { host_runtime_id: "h".into(), session_id: "s".into(), turn_id: id.into() }),
            source_manifest_hash: "m".into(),
            review_policy_version: 1,
            state: JS::Queued,
            created_unix_ms: 1,
            artifacts: Artifacts::default(),
            attempts: 0,
        }
    }

    #[test]
    fn enqueue_is_idempotent() {
        let mut svc = GastownService::default();
        let a = svc.enqueue_review(job("j1", "W"));
        assert_eq!(a["duplicate"], false);
        let b = svc.enqueue_review(job("j1", "W"));
        assert_eq!(b["duplicate"], true);
        assert_eq!(svc.projection.jobs.len(), 1);
    }

    #[test]
    fn hosts_sessions_health() {
        let mut svc = GastownService::default();
        svc.host_register(HostRecord {
            host_runtime_id: "h1".into(),
            producer_id: "g".into(),
            process_identity: "pid=1".into(),
            verified_real_host: true,
            created_unix_ms: 1,
            last_heartbeat_unix_ms: 1,
            attached_at_unix_ms: Some(1),
            attach_state: crate::host_registry::AttachState::Attached,
            scopes: vec![],
        });
        let h = svc.admin_health(2_000);
        assert_eq!(h["real_connected_hosts"], 1);
        svc.host_disconnect("h1", 100_000_000);
        let h2 = svc.admin_health(100_000_001);
        assert_eq!(h2["real_connected_hosts"], 0);
        assert_eq!(svc.capabilities().len(), capabilities::GASTOWN_CAPABILITIES.len());
    }

    #[test]
    fn scheduler_generation_monotonic() {
        let mut svc = GastownService::default();
        assert_eq!(svc.next_scheduler_generation(), 1);
        assert_eq!(svc.next_scheduler_generation(), 2);
        assert_eq!(svc.scheduler_generation(), 2);
    }

    #[test]
    fn auto_enqueue_is_idempotent_per_epoch() {
        let mut svc = GastownService::default();
        // Simulate a completed epoch: chat message then idle with evidence.
        let env = crate::events::GastownEvent {
            schema_version: 1,
            producer_id: "gastown.host".into(),
            producer_epoch: 1,
            producer_sequence: 1,
            event_id: uuid::Uuid::now_v7().to_string(),
            correlation_id: None,
            causation_id: None,
            host_runtime_id: Some("h1".into()),
            host_process_identity: Some("pid=1".into()),
            workspace_id: Some("C:/ws".into()),
            session_id: Some("s1".into()),
            task_epoch_id: None,
            turn_id: Some("t1".into()),
            kind: crate::events::EventKind::ChatMessage,
            event_timestamp_utc_ms: 1_000,
            payload_schema_version: 1,
            payload_hash: "x".into(),
        };
        let p = env.encode().unwrap();
        svc.replay(crate::projection::CommittedSignal { ledger_sequence: 1, producer_app: "gastown".into(), event_type: "chat.message".into(), payload_hash: blake3::hash(&p).to_hex().to_string(), payload: p });
        // Now a validated session idle.
        let idle = crate::events::GastownEvent {
            kind: crate::events::EventKind::SessionIdle,
            event_timestamp_utc_ms: 6_000,
            session_id: Some("s1".into()),
            event_id: uuid::Uuid::now_v7().to_string(),
            ..env.clone()
        };
        let p2 = idle.encode().unwrap();
        svc.replay(crate::projection::CommittedSignal { ledger_sequence: 2, producer_app: "gastown".into(), event_type: "session.idle".into(), payload_hash: blake3::hash(&p2).to_hex().to_string(), payload: p2 });

        // First auto-enqueue creates exactly one job.
        let first = svc.auto_enqueue_completed_epochs();
        assert_eq!(first.len(), 1, "one completed epoch -> one job");
        // Second call (duplicate idle/status) creates none (HOST-05 idempotency).
        let second = svc.auto_enqueue_completed_epochs();
        assert_eq!(second.len(), 0, "duplicate status must not create a second job");
        // A SECOND task in the same session (new turn) must create a new job.
        let env2 = crate::events::GastownEvent {
            turn_id: Some("t2".into()),
            producer_sequence: 3,
            event_id: uuid::Uuid::now_v7().to_string(),
            event_timestamp_utc_ms: 10_000,
            session_id: Some("s1".into()),
            kind: crate::events::EventKind::ChatMessage,
            ..env.clone()
        };
        let p3 = env2.encode().unwrap();
        svc.replay(crate::projection::CommittedSignal { ledger_sequence: 3, producer_app: "gastown".into(), event_type: "chat.message".into(), payload_hash: blake3::hash(&p3).to_hex().to_string(), payload: p3 });
        let idle2 = crate::events::GastownEvent { turn_id: Some("t2".into()), event_timestamp_utc_ms: 12_000, event_id: uuid::Uuid::now_v7().to_string(), kind: crate::events::EventKind::SessionIdle, ..env };
        let p4 = idle2.encode().unwrap();
        svc.replay(crate::projection::CommittedSignal { ledger_sequence: 4, producer_app: "gastown".into(), event_type: "session.idle".into(), payload_hash: blake3::hash(&p4).to_hex().to_string(), payload: p4 });
        let third = svc.auto_enqueue_completed_epochs();
        assert_eq!(third.len(), 1, "a new task in same session produces a new job");
        assert_ne!(third[0].job_id, first[0].job_id);
    }

    #[test]
    fn replay_project_is_watermarked() {
        let mut svc = GastownService::default();
        let g = crate::events::GastownEvent {
            schema_version: 1,
            producer_id: "g".into(),
            producer_epoch: 1,
            producer_sequence: 1,
            event_id: uuid::Uuid::now_v7().to_string(),
            correlation_id: None,
            causation_id: None,
            host_runtime_id: Some("h".into()),
            host_process_identity: Some("pid=9".into()),
            workspace_id: Some("C:/w".into()),
            session_id: Some("s".into()),
            task_epoch_id: None,
            turn_id: Some("t1".into()),
            kind: crate::events::EventKind::ChatMessage,
            event_timestamp_utc_ms: 1,
            payload_schema_version: 1,
            payload_hash: "x".into(),
        };
        let p = g.encode().unwrap();
        let sig = CommittedSignal {
            ledger_sequence: 1,
            producer_app: "gastown".into(),
            event_type: "chat.message".into(),
            payload_hash: blake3::hash(&p).to_hex().to_string(),
            payload: p,
        };
        svc.replay(sig.clone());
        svc.replay(sig.clone()); // no double
        assert_eq!(svc.projection.watermark, 1);
    }
}