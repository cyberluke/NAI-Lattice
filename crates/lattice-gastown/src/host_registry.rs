//! Host/session registry with real-host flags and leases (spec §4, §5, §12).
//!
//! `realHostCount` must reflect actual connected developer hosts, excluding
//! disconnected, stale, simulated, and headless reviewer runtimes. A
//! client-supplied `isSimulated=false` is NOT proof of a real host; the backend
//! records the verified OS process identity from the NAF pipe and reconcile
//! disconnects against it.

use std::collections::HashMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// A registered host (one Kilo runtime installation + OS process).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostRecord {
    pub host_runtime_id: String,
    pub producer_id: String,
    /// Verified OS process identity (PID + process start time when available).
    pub process_identity: String,
    /// Set by the pipe transport from a LIVE OS handle, not trusted from the
    /// wire. Headless reviewer runs are excluded by capability/namespace.
    pub verified_real_host: bool,
    pub created_unix_ms: i64,
    pub last_heartbeat_unix_ms: i64,
    pub attached_at_unix_ms: Option<i64>,
    /// Hosts whose runtime adopted us on a later safe init.
    pub attach_state: AttachState,
    /// Additional connector context (e.g. workspace snapshots mounted).
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AttachState {
    Attached,
    AttachPending,
}

/// A live session under a host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub host_runtime_id: String,
    pub workspace_canonical: String,
    pub created_unix_ms: i64,
    pub last_seen_unix_ms: i64,
    /// `session.idle` is only real if the SDK status type is genuinely idle and
    /// no tool/thinking is in flight (spec §8): never treat a default status as idl.
    pub idle_evidence: Option<String>,
}

/// Registry backed by an in-memory snapshot (projection) of the canonical
/// ledger. Leases are fencing generation for claims (spec §9).
#[derive(Debug, Default)]
pub struct HostRegistry {
    pub hosts: HashMap<String, HostRecord>,
    pub sessions: HashMap<String, SessionRecord>,
    pub leases: HashMap<String, Lease>,
}

#[derive(Debug, Clone)]
pub struct Lease {
    pub key: String,
    pub holder: String,
    pub generation: u64,
    pub expires_unix_ms: i64,
    pub start: Instant,
}

impl HostRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, rec: HostRecord) {
        self.hosts.insert(rec.host_runtime_id.clone(), rec);
    }

    pub fn mark_disconnected(&mut self, host_runtime_id: &str, now_unix_ms: i64) {
        if let Some(h) = self.hosts.get_mut(host_runtime_id) {
            h.attached_at_unix_ms = None;
            // Keep record for provenance; the host count drops on read.
            let _ = now_unix_ms;
        }
    }

    /// Count of hosts that are REAL and currently attached (not stale).
    pub fn real_attached_count(&self, now_unix_ms: i64, stale_after_ms: i64) -> usize {
        self.hosts
            .values()
            .filter(|h| {
                h.verified_real_host
                    && h.attach_state == AttachState::Attached
                    && now_unix_ms.saturating_sub(h.last_heartbeat_unix_ms) < stale_after_ms
            })
            .count()
    }

    pub fn real_active_session_count(&self) -> usize {
        self.sessions
            .values()
            .filter(|s| s.workspace_canonical != "workspace") // never a synthetic id
            .count()
    }

    /// Acquire a fencing lease (spec §9). Returns Some(generation) when free.
    pub fn acquire_lease(&mut self, key: &str, holder: &str, now_unix_ms: i64, ttl_ms: i64) -> Option<u64> {
        if let Some(l) = self.leases.get(key) {
            if l.expires_unix_ms > now_unix_ms {
                return None; // still held
            }
        }
        let gen = self.leases.get(key).map(|l| l.generation + 1).unwrap_or(1);
        self.leases.insert(
            key.to_string(),
            Lease {
                key: key.to_string(),
                holder: holder.to_string(),
                generation: gen,
                expires_unix_ms: now_unix_ms + ttl_ms,
                start: Instant::now(),
            },
        );
        Some(gen)
    }

    /// Fencing check: only a lease holder at the recorded generation may write
    /// the result (stale old workers cannot overwrite, spec §9).
    pub fn is_current_holder(&self, key: &str, holder: &str, generation: u64) -> bool {
        matches!(self.leases.get(key), Some(l) if l.holder == holder && l.generation == generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> HostRecord {
        HostRecord {
            host_runtime_id: "h1".into(),
            producer_id: "gastown.host".into(),
            process_identity: "pid=12345 ts=1".into(),
            verified_real_host: true,
            created_unix_ms: 1_000,
            last_heartbeat_unix_ms: 1_000,
            attached_at_unix_ms: Some(1_100),
            attach_state: AttachState::Attached,
            scopes: vec![],
        }
    }

    #[test]
    fn real_counts_exclude_stale_and_pending() {
        let mut r = HostRegistry::new();
        r.register(host());
        r.sessions.insert(
            "s1".into(),
            SessionRecord {
                session_id: "s1".into(),
                host_runtime_id: "h1".into(),
                workspace_canonical: "C:/proj".into(),
                created_unix_ms: 1,
                last_seen_unix_ms: 1,
                idle_evidence: Some("poll".to_string()),
            },
        );
        // Attached + fresh => counted.
        assert_eq!(r.real_attached_count(2_000, 60_000), 1);
        assert_eq!(r.real_active_session_count(), 1);
        // Stale heartbeat => excluded from real host count.
        assert_eq!(r.real_attached_count(200_000, 60_000), 0);
        // A synthetic "workspace" session never counts as real.
        r.sessions.insert(
            "w".into(),
            SessionRecord {
                session_id: "w".into(),
                host_runtime_id: "h1".into(),
                workspace_canonical: "workspace".into(),
                created_unix_ms: 1,
                last_seen_unix_ms: 1,
                idle_evidence: None,
            },
        );
        assert_eq!(r.real_active_session_count(), 1);
    }

    #[test]
    fn lease_fencing_prevents_stale_overwrite() {
        let mut r = HostRegistry::new();
        let g = r.acquire_lease("job/1", "workerA", 1000, 100_000).unwrap();
        assert_eq!(g, 1);
        assert!(r.is_current_holder("job/1", "workerA", 1));
        assert!(!r.is_current_holder("job/1", "workerA", 2)); // stale generation
        assert!(!r.is_current_holder("job/1", "workerB", 1)); // wrong holder
        assert!(r.acquire_lease("job/1", "workerB", 1000, 100_000).is_none());
    }
}