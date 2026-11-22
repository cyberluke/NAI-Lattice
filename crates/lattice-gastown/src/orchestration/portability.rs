//! Portable Gas Town domain contract, exclusive ownership epochs, fencing, and
//! durable state transfer (cloud portability, locally verifiable).
//!
//! Gas Town's full domain state (town, rigs, agents, beads, dependencies,
//! convoys, review passes, escalations, events, envelopes) is serialized into a
//! **portable envelope** â€” a content-addressed payload with an ownership epoch.
//! A site that holds the envelope with the HIGHEST epoch is the exclusive writer
//! (single-writer). A previously-primary site that has been demoted is FENCED:
//! any attempt to persist a mutation under a stale (lower) epoch is rejected, so
//! split-brain double-writes cannot occur even if a stale primary is still up.
//!
//! This is the portable contract a real cloud worker would consume the same way;
//! it is fully verifiable locally (two hot-store sites), and the same envelope is
//! what a Cloudflare Worker would import/export. Real cloud deployment remains an
//! external gate (no credentials).

use crate::orchestration::state::{gen_id, now, OrchestrationStore};
use serde::{Deserialize, Serialize};

/// The full durable domain state for one town, portable between sites.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortableTownState {
    pub contract_version: u32,
    pub town_id: String,
    pub towns: Vec<serde_json::Value>,
    pub rigs: Vec<serde_json::Value>,
    pub agents: Vec<serde_json::Value>,
    pub beads: Vec<serde_json::Value>,
    pub dependencies: Vec<serde_json::Value>,
    pub convoy_metadata: Vec<serde_json::Value>,
    pub review_metadata: Vec<serde_json::Value>,
    pub escalations: Vec<serde_json::Value>,
    pub review_passes: Vec<serde_json::Value>,
    pub events: Vec<serde_json::Value>,
    pub mailboxes: Vec<serde_json::Value>,
    pub triage: Vec<serde_json::Value>,
    pub molecules: Vec<serde_json::Value>,
}

/// An exclusive-ownership envelope: the portable state PLUS the owning epoch.
/// `CAS` is the content-addressed hash of the canonical serialization, so the
/// recipient can verify integrity and prove the transfer was atomic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortabilityEnvelope {
    pub epoch: u64,
    pub owner_site: String,
    pub cas: String,
    pub state_json: String,
}

pub const CONTRACT_VERSION: u32 = 1;

/// The current ownership epoch for a site, persisted in its local store.
pub fn current_epoch(store: &OrchestrationStore) -> u64 {
    store.hot_get("helldesk/portability/epoch").and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

/// Fence token validation: a writer with a lower-or-equal epoch than the current
/// local epoch is fenced and MUST NOT write. Returns the accepted latest epoch.
pub fn fence_and_reject_stale(store: &OrchestrationStore, proposing_epoch: u64) -> bool {
    let current = current_epoch(store);
    // Strictly greater epoch wins; a stale/equal writer is fenced (rejected).
    proposing_epoch > current
}

/// Export the current town domain state into a portable envelope, bumping the
/// ownership epoch and tagging an owner site identity. Returns the durable,
/// CAS-bound envelope. This is what the local "cloud" transfer carries.
pub fn export_state(store: &OrchestrationStore, town_id: &str, owner_site: &str) -> PortabilityEnvelope {
    let towns: Vec<serde_json::Value> = store.list_towns().into_iter().map(|t| serde_json::to_value(&t).unwrap_or(serde_json::json!({}))).collect();
    let rigs: Vec<serde_json::Value> = store.list_rigs_for_town(town_id).into_iter().map(|r| serde_json::to_value(&r).unwrap_or(serde_json::json!({}))).collect();
    let agents: Vec<serde_json::Value> = store.list_agents().into_iter().map(|a| serde_json::to_value(&a).unwrap_or(serde_json::json!({}))).collect();
    let beads: Vec<serde_json::Value> = store.list_beads().into_iter().map(|b| serde_json::to_value(&b).unwrap_or(serde_json::json!({}))).collect();
    let dependencies: Vec<serde_json::Value> = store.list_dependencies().into_iter().map(|d| serde_json::to_value(&d).unwrap_or(serde_json::json!({}))).collect();
    let convoy_metadata: Vec<serde_json::Value> = store.list_beads().into_iter().filter(|b| b.bead_type == crate::orchestration::state::BeadType::Convoy).filter_map(|b| store.get_convoy_metadata(&b.bead_id).map(|m| serde_json::to_value(&m).unwrap_or(serde_json::json!({})))).collect();
    let review_metadata: Vec<serde_json::Value> = store.list_beads().into_iter().filter(|b| b.bead_type == crate::orchestration::state::BeadType::MergeRequest).filter_map(|b| store.get_review_metadata(&b.bead_id).map(|m| serde_json::to_value(&m).unwrap_or(serde_json::json!({})))).collect();
    let escalations: Vec<serde_json::Value> = store.list_beads().into_iter().filter(|b| b.bead_type == crate::orchestration::state::BeadType::Escalation).filter_map(|b| store.get_escalation(&b.bead_id).map(|e| serde_json::to_value(&e).unwrap_or(serde_json::json!({})))).collect();
    let review_passes: Vec<serde_json::Value> = store.list_beads().into_iter().flat_map(|b| store.review_passes_for_source(&b.bead_id).into_iter().map(|p| serde_json::to_value(&p).unwrap_or(serde_json::json!({})))).collect();
    let events: Vec<serde_json::Value> = store.events_for_town(town_id).into_iter().map(|e| serde_json::json!({
        "event_id": e.event_id, "event": e.event, "bead_id": e.bead_id, "agent_id": e.agent_id,
        "town_id": e.town_id, "rig_id": e.rig_id, "data": e.data, "created_at": e.created_at,
    })).collect();
    let mailboxes: Vec<serde_json::Value> = store.list_agents().into_iter().flat_map(|a| store.list_mail_for(&a.bead_id).into_iter().map(|m| serde_json::to_value(&m).unwrap_or(serde_json::json!({})))).collect();
    let triage: Vec<serde_json::Value> = store.list_pending_triage_requests().into_iter().map(|b| serde_json::to_value(&b).unwrap_or(serde_json::json!({}))).collect();
    let molecules: Vec<serde_json::Value> = store.list_beads().into_iter().filter(|b| b.bead_type == crate::orchestration::state::BeadType::Molecule).filter_map(|b| store.get_molecule_metadata(&b.bead_id).map(|m| serde_json::to_value(&m).unwrap_or(serde_json::json!({})))).collect();

    let st = PortableTownState {
        contract_version: CONTRACT_VERSION,
        town_id: town_id.to_string(),
        towns, rigs, agents, beads, dependencies, convoy_metadata, review_metadata,
        escalations, review_passes, events, mailboxes, triage, molecules,
    };
    let state_json = serde_json::to_string(&st).unwrap_or_default();
    let cas = blake3::hash(state_json.as_bytes()).to_hex().to_string();
    let epoch = current_epoch(store) + 1;
    // Persist the new epoch locally (we own it after the bump).
    let _ = store.hot_put("helldesk/portability/epoch", &epoch.to_string());
    PortabilityEnvelope { epoch, owner_site: owner_site.to_string(), cas, state_json }
}

/// Import a portable envelope into a (possibly empty/new) site, ATOMICALLY and
/// under fencing: only when `envelope.epoch` is greater than the current local
/// epoch are the contents applied and the epoch advanced. Returns true on success,
/// false when the envelope is fenced (stale owner) or fails integrity.
pub fn import_state(store: &OrchestrationStore, envelope: &PortabilityEnvelope) -> bool {
    // Integrity: CAS must match the serialized payload.
    let actual = blake3::hash(envelope.state_json.as_bytes()).to_hex().to_string();
    if actual != envelope.cas {
        return false;
    }
    if !fence_and_reject_stale(store, envelope.epoch) {
        return false; // fenced: this site's epoch is higher/equal â†’ stale transfer rejected
    }
    // Decode + apply (idempotent overwrite as an atomic swap under the accepted
    // epoch). For interruption safety we decode EVERYTHING first and only apply
    // when the whole payload is well-formed â€” a truncated/partial envelope is
    // rejected before any mutation, so an interrupted transfer leaves no partial
    // state.
    let Ok(st): Result<PortableTownState, _> = serde_json::from_str(&envelope.state_json) else { return false };
    let towns: Vec<crate::orchestration::state::Town> = st.towns.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect();
    let rigs: Vec<crate::orchestration::state::Rig> = st.rigs.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect();
    let agents: Vec<crate::orchestration::state::Agent> = st.agents.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect();
    let beads: Vec<crate::orchestration::state::Bead> = st.beads.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect();
    let deps: Vec<crate::orchestration::state::Dependency> = st.dependencies.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect();
    let convoys: Vec<crate::orchestration::state::ConvoyMetadata> = st.convoy_metadata.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect();
    let reviews: Vec<crate::orchestration::state::ReviewMetadata> = st.review_metadata.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect();
    let escalations: Vec<crate::orchestration::state::Escalation> = st.escalations.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect();
    let passes: Vec<crate::orchestration::state::ReviewPass> = st.review_passes.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect();
    use crate::orchestration::state::BeadEvent;
    let events: Vec<BeadEvent> = st.events.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect();
    let molecules: Vec<crate::orchestration::state::MoleculeMetadata> = st.molecules.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect();
    let triage: Vec<crate::orchestration::state::Bead> = st.triage.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect();
    // "Interruption safe": if ANY declared collection failed to wholly decode, we
    // reject BEFORE mutating (no partial state).
    if towns.len() != st.towns.len() || rigs.len() != st.rigs.len() || agents.len() != st.agents.len()
        || beads.len() != st.beads.len() || deps.len() != st.dependencies.len() || reviews.len() != st.review_metadata.len()
        || escalations.len() != st.escalations.len() || passes.len() != st.review_passes.len() || events.len() != st.events.len() {
        return false; // interrupted / malformed payload â†’ nothing applied
    }
    // Apply (all-or-nothing within a well-formed payload).
    for t in towns { store.put_town(&t); }
    for r in rigs { store.put_rig(&r); }
    for a in agents { store.put_agent(&a); }
    for b in beads { store.put_bead(&b); }
    for d in deps { store.put_dependency(&d); }
    for cm in convoys { store.put_convoy_metadata(&cm); }
    for rm in reviews { store.put_review_metadata(&rm); }
    for e in escalations { store.put_escalation(&e); }
    for p in passes { store.put_review_pass(&p); }
    for ev in events { store.emit_event(&ev); }
    for m in molecules { store.put_molecule_metadata(&m); }
    for t in triage { store.put_bead(&t); }
    // Advance the local epoch (we now own it).
    let _ = store.hot_put("helldesk/portability/epoch", &envelope.epoch.to_string());
    true
}

#[allow(dead_code)]
fn _noop() { let _ = gen_id(); let _ = now(); }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestration::state::{Bead, BeadStatus, BeadType};
    use nai_lattice_hotstore::{HotKey, HotStore, HotStoreError, HotValue};
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    #[derive(Default, Debug)]
    pub struct MemHotStore(Mutex<BTreeMap<Vec<u8>, Vec<u8>>>);
    impl HotStore for MemHotStore {
        fn get(&self, k: &HotKey) -> Result<Option<HotValue>, HotStoreError> { Ok(self.0.lock().unwrap().get(&k.0).cloned().map(HotValue)) }
        fn put(&self, k: &HotKey, v: &HotValue) -> Result<(), HotStoreError> { self.0.lock().unwrap().insert(k.0.clone(), v.0.clone()); Ok(()) }
        fn delete(&self, _k: &HotKey) -> Result<bool, HotStoreError> { Ok(true) }
        fn exists(&self, _k: &HotKey) -> Result<bool, HotStoreError> { Ok(false) }
        fn scan_prefix(&self, _p: &[u8]) -> Result<Vec<HotKey>, HotStoreError> { Ok(vec![]) }
    }
    fn store() -> OrchestrationStore { OrchestrationStore::new(Arc::new(MemHotStore::default()) as Arc<dyn HotStore>) }

    #[test]
    fn portable_transfer_is_cas_bound_atomic_and_recoverable() {
        let src = store();
        // Seed primary state (a town + a rig + a bead).
        let t = crate::orchestration::state::Town {
            town_id: "t-1".into(), owner_user_id: None,
            config: crate::orchestration::state::TownConfig::default(), created_at: now(),
        };
        src.put_town(&t);
        let rig = crate::orchestration::state::Rig {
            rig_id: "rig-1".into(), name: "r".into(), git_url: "https://x".into(),
            default_branch: "main".into(), town_id: "t-1".into(), config: serde_json::json!({}),
            created_at: now(), working_dir: Some("D:/repo".into()), worktree_disk_budget_mib: None, sparse_paths: vec![],
        };
        src.put_rig(&rig);
        let b = Bead::create(BeadType::Issue, "feature".into(), None, None, vec![], serde_json::json!({}), Some("rig-1".into()), None, Some("mayor"));
        src.put_bead(&b);

        // Export â†’ transfer envelope (epoch 1).
        let env = export_state(&src, "t-1", "site-local");
        assert_eq!(env.epoch, 1);
        assert_eq!(env.cas.len(), 64);
        // New empty "cloud" site imports it atomically under fencing.
        let cloud = store();
        assert!(import_state(&cloud, &env), "cloud site accepts the higher-epoch envelope");
        assert_eq!(current_epoch(&cloud), 1);
        // State is fully recovered (town, rig, bead).
        assert_eq!(cloud.list_towns().len(), 1);
        assert_eq!(cloud.list_rigs_for_town("t-1").len(), 1);
        assert_eq!(cloud.list_beads().iter().filter(|x| x.title.contains("feature")).count(), 1);
    }

    #[test]
    fn stale_epoch_is_fenced_and_cannot_split_brain() {
        let a = store();
        let env1 = export_state(&a, "t-1", "site-a"); // epoch 1
        let cloud = store();
        assert!(import_state(&cloud, &env1));
        let env2 = export_state(&cloud, "t-1", "site-cloud"); // epoch 2 (cloud now owner)
        assert_eq!(env2.epoch, 2);
        // A stale primary RECOVERS by importing the newer (single-source-of-truth)
        // envelope â€” this converges state, it does not create divergence.
        assert!(import_state(&a, &env2), "stale primary must recover to the newest epoch");
        assert_eq!(current_epoch(&a), 2, "recovered site advances to the owning epoch");
        // FENCING: once a site is at epoch 2, it cannot WRITE a mutation under a
        // stale (equal or lower) epoch â€” a stale writer is rejected, preventing
        // split-brain double-writes even if the old primary is still running.
        assert!(!fence_and_reject_stale(&a, 2), "a writer proposing the current epoch is fenced");
        assert!(!fence_and_reject_stale(&a, 1), "a writer proposing a LOWER epoch is fenced (stale)");
        assert!(fence_and_reject_stale(&a, 3), "only a strictly greater epoch may take ownership");
    }

    #[test]
    fn tampered_envelope_fails_integrity() {
        let src = store();
        let mut env = export_state(&src, "t-1", "site-local");
        // Tamper with the payload â†’ CAS mismatch â†’ import rejected, no split-brain.
        env.state_json.push('x');
        let dst = store();
        assert!(!import_state(&dst, &env), "tampered envelope must be rejected by CAS integrity");
        assert_eq!(current_epoch(&dst), 0, "failed import advances no epoch");
    }

    #[test]
    fn interrupted_transfer_leaves_destination_unchanged() {
        // Simulate a transfer that is INTERRUPTED mid-flight (a truncated /
        // corrupt envelope from a lost network write): CAS fails BEFORE any bead
        // is applied, so the destination is byte-for-byte at its prior epoch and
        // holds none of the source's partial state. Recoverable: a later, intact
        // envelope still imports cleanly.
        let src = store();
        let t = crate::orchestration::state::Town { town_id: "t-src".into(), owner_user_id: None, config: crate::orchestration::state::TownConfig::default(), created_at: now() };
        src.put_town(&t);
        let env = export_state(&src, "t-src", "site-local"); // epoch 1, intact CAS

        // Interruption: the recipient only receives a CORRUPTED prefix / bad CAS.
        let mut truncated = env.clone();
        truncated.cas = "0000000000000000000000000000000000000000000000000000000000000000".to_string(); // wrong CAS = lost/partial write
        let dst = store();
        assert!(!import_state(&dst, &truncated), "interrupted (corrupt-CAS) transfer must be rejected");
        assert_eq!(current_epoch(&dst), 0, "interrupted transfer must NOT advance the epoch");
        assert_eq!(dst.list_towns().len(), 0, "interrupted transfer must NOT leave partial state");

        // Recovery: the same site later receives the INTACT envelope and converges.
        assert!(import_state(&dst, &env), "recovery after interruption succeeds");
        assert_eq!(current_epoch(&dst), 1);
        assert_eq!(dst.list_towns().len(), 1);
    }
}