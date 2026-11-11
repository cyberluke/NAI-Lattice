//! Domain projection (spec §4): replays canonical committed events into the
//! host/session/epoch/review state. The projection is a REBUILDABLE view of the
//! canonical ledger; it is never authoritative truth (spec §3.6, §17).

use std::collections::HashMap;

use crate::events::{EventKind, GastownEvent};
use crate::host_registry::{HostRecord, HostRegistry, SessionRecord};
use crate::review::{JobState, ReviewJob};
use crate::task_epoch::{EpochState, TaskEpoch, TaskEpochId};

/// The current projected (rebuildable) Gas Town domain state.
///
/// This is an in-memory REBUILDABLE view of the canonical ledger; it is never
/// stored as authoritative truth (spec §3.6, §17). Rebuilder-only: no serde.
#[derive(Debug, Default)]
pub struct GastownProjection {
    pub hosts: HostRegistry,
    pub epochs: HashMap<TaskEpochId, TaskEpoch>,
    pub jobs: HashMap<String, ReviewJob>,
    /// Highest canonical ledger runtime sequence projected.
    pub watermark: u64,
}

/// One replayed canonical event signal handed to the projection (the service
/// only projects EVENTS IT ALREADY COMMITTED to the ledger; this is the
/// use-after-commit replay path, never an alternate timeline).
#[derive(Debug, Clone)]
pub struct CommittedSignal {
    pub ledger_sequence: u64,
    pub producer_app: String,
    pub event_type: String,
    pub payload: Vec<u8>,
    pub payload_hash: String,
}

impl GastownProjection {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild the projection from a bounded replay of committed signals.
    /// Idempotent: replaying the same sequence twice is a no-op (watermark).
    pub fn apply(&mut self, sig: CommittedSignal) {
        if sig.ledger_sequence <= self.watermark {
            return;
        }
        self.watermark = sig.ledger_sequence;
        if sig.producer_app != "gastown" {
            return; // only Gas Town domain events project here
        }
        let g = match GastownEvent::decode(&sig.payload) {
            Ok(g) => g,
            Err(_) => return,
        };
        if blake3::hash(&sig.payload).to_hex().to_string() != sig.payload_hash {
            return; // integrity mismatch: do not project
        }
        match g.kind {
            EventKind::ChatMessage => self.on_host_and_epoch(g),
            EventKind::ToolStarted | EventKind::ToolCompleted => {
                self.note_epoch(g, |e, now| e.on_tool(now));
            }
            EventKind::FileEdited => {
                self.note_epoch(g, |e, now| e.on_file_edit(now));
            }
            EventKind::SessionStatus => {
                // Only a genuinely validated idle status advances the epoch.
            }
            EventKind::SessionIdle => {
                if let Some(host) = &g.host_runtime_id {
                    if let Some(sid) = &g.session_id {
                        let ep = self.epoch_or_begin(host, sid, &g);
                        ep.on_session_idle(g.event_timestamp_utc_ms, 5_000);
                        // A genuinely idle session after activity (with quiet
                        // period set) marks the epoch review-eligible; the host
                        // then auto-enqueues the durable real review job.
                        if ep.quiet_until_unix_ms.is_some() && ep.saw_session_idle_with_evidence {
                            ep.mark_review_eligible(g.event_timestamp_utc_ms);
                        }
                    }
                }
            }
            EventKind::SessionFinished | EventKind::SessionDiff | EventKind::MessageUpdated => {}
        }
        // A job artifact may also be a separate signal; handled by service.
    }

    fn note_epoch<F: Fn(&mut TaskEpoch, i64) -> EpochState>(&mut self, g: GastownEvent, f: F) {
        if let (Some(host), Some(sid)) = (g.host_runtime_id.clone(), g.session_id.clone()) {
            let ep = self.epoch_or_begin(&host, &sid, &g);
            f(ep, g.event_timestamp_utc_ms);
        }
    }

    fn on_host_and_epoch(&mut self, g: GastownEvent) {
        let host = g.host_runtime_id.clone().unwrap_or_default();
        let sid = g.session_id.clone().unwrap_or_default();
        if !host.is_empty() && !self.hosts.hosts.contains_key(&host) {
            self.hosts.register(HostRecord {
                host_runtime_id: host.clone(),
                producer_id: g.producer_id.clone(),
                process_identity: g.host_process_identity.clone().unwrap_or_default(),
                verified_real_host: false, // overridden by pipe transport verification
                created_unix_ms: g.event_timestamp_utc_ms,
                last_heartbeat_unix_ms: g.event_timestamp_utc_ms,
                attached_at_unix_ms: Some(g.event_timestamp_utc_ms),
                attach_state: crate::host_registry::AttachState::AttachPending,
                scopes: vec![],
            });
        }
        if !sid.is_empty() {
            self.hosts.sessions.insert(
                sid.clone(),
                SessionRecord {
                    session_id: sid.clone(),
                    host_runtime_id: host.clone(),
                    workspace_canonical: g.workspace_id.clone().unwrap_or_default(),
                    created_unix_ms: g.event_timestamp_utc_ms,
                    last_seen_unix_ms: g.event_timestamp_utc_ms,
                    idle_evidence: None,
                },
            );
        }
        if !host.is_empty() && !sid.is_empty() {
            if let Some(turn) = g.turn_id.clone() {
                let ep = self.epoch_or_begin(&host, &sid, &g);
                ep.turn_id_safe(turn);
                // A chat.message is genuine turn activity; the epoch must count
                // it so session.idle after it is treated as task completion.
                ep.on_chat_message(g.event_timestamp_utc_ms);
            }
        }
    }

    fn epoch_or_begin(&mut self, host: &str, session: &str, g: &GastownEvent) -> &mut TaskEpoch {
        let turn = g.turn_id.clone().unwrap_or_else(|| "turn-0".to_string());
        let id = TaskEpochId {
            host_runtime_id: host.to_string(),
            session_id: session.to_string(),
            turn_id: turn,
        };
        self.epochs.entry(id.clone()).or_insert_with(|| TaskEpoch::new(id, g.event_timestamp_utc_ms))
    }

    /// Insert a persisted review job (idempotent by idempotency key) — the
    /// service does this AFTER committing the job to the ledger.
    pub fn upsert_job(&mut self, job: ReviewJob) {
        self.jobs.insert(job.job_id.clone(), job);
    }

    pub fn all_jobs(&self, state_filter: Option<JobState>) -> Vec<&ReviewJob> {
        self.jobs
            .values()
            .filter(|j| state_filter.map(|s| j.state == s).unwrap_or(true))
            .collect()
    }

    /// `realHostCount` style metrics for admin health.
    pub fn metrics(&self, now_unix_ms: i64) -> serde_json::Value {
        serde_json::json!({
            "real_attached_hosts": self.hosts.real_attached_count(now_unix_ms, 60_000),
            "real_active_sessions": self.hosts.real_active_session_count(),
            "total_hosts_seen": self.hosts.hosts.len(),
            "epochs": self.epochs.len(),
            "jobs": self.jobs.len(),
            "watermark": self.watermark,
        })
    }
}

impl TaskEpoch {
    /// Internal: record the epoch's turn id.
    fn turn_id_safe(&mut self, _turn: String) {
        // The epoch id already encodes the turn; nothing to store.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(seq: u64, g: &GastownEvent) -> CommittedSignal {
        let payload = g.encode().unwrap();
        CommittedSignal {
            ledger_sequence: seq,
            producer_app: "gastown".into(),
            event_type: g.kind.as_str().to_string(),
            payload_hash: blake3::hash(&payload).to_hex().to_string(),
            payload,
        }
    }

    fn ev(kind: EventKind, turn: &str, seq: u64) -> GastownEvent {
        GastownEvent {
            schema_version: 1,
            producer_id: "gastown.host".into(),
            producer_epoch: 1,
            producer_sequence: seq,
            event_id: uuid::Uuid::now_v7().to_string(),
            correlation_id: None,
            causation_id: None,
            host_runtime_id: Some("h1".into()),
            host_process_identity: Some("pid=1 ts=1".into()),
            workspace_id: Some("C:/proj".into()),
            session_id: Some("s1".into()),
            task_epoch_id: None,
            turn_id: Some(turn.to_string()),
            kind,
            event_timestamp_utc_ms: 1000 + seq as i64 * 100,
            payload_schema_version: 1,
            payload_hash: "x".into(),
        }
    }

    #[test]
    fn projects_chat_into_host_and_epoch() {
        let mut p = GastownProjection::new();
        p.apply(sig(1, &ev(EventKind::ChatMessage, "t1", 1)));
        assert_eq!(p.hosts.hosts.len(), 1);
        // The chat message establishes a REAL session (workspace-scoped), not a
        // synthetic "workspace" placeholder, so it counts as active.
        assert_eq!(p.hosts.real_active_session_count(), 1);
        assert_eq!(p.epochs.len(), 1, "a turn creates a task epoch");
    }

    #[test]
    fn replay_is_watermark_idempotent() {
        let mut p = GastownProjection::new();
        let s = sig(1, &ev(EventKind::ChatMessage, "t1", 1));
        p.apply(s.clone());
        let snapshot = p.watermark;
        p.apply(s.clone());
        assert_eq!(p.watermark, snapshot);
        assert_eq!(p.hosts.hosts.len(), 1, "no duplicate host on replay");
    }

    #[test]
    fn corrupt_payload_not_projected() {
        let mut p = GastownProjection::new();
        let mut s = sig(1, &ev(EventKind::ChatMessage, "t1", 1));
        s.payload_hash = "DEADBEEF".into(); // integrity mismatch
        p.apply(s);
        assert_eq!(p.hosts.hosts.len(), 0);
    }
}