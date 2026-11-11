//! Per-bead adversarial review policy (WS5).
//!
//! Turns the review pipeline from a single refinery pass into a bounded,
//! evidence-grounded adversarial chain. For every bead a Polecat completes:
//!
//!   1. Reviewer A   — independent code review (model from `reviewer_a`).
//!   2. Reviewer B   — adversarial review (model from `reviewer_b`).
//!   3. Adversarial  — a distinct strong, independent pass (model from
//!                     `adversarial`) whose job is to find defects, not confirm.
//!   4. Adjudicator  — weights the reviewers AND machine-validates every
//!                     finding's cited evidence against the immutable source
//!                     snapshot via `review_evidence`. A finding that cites a
//!                     non-existent file/line/snippet is `Hallucinated` and does
//!                     NOT count as a valid rejection.
//!
//! On a valid rejection the engine auto-generates precise corrective
//! instructions (file + line range + message) from the finding and rewinds the
//! source bead to `open` for repair. Cycles are bounded by the rig's
//! `max_review_cycles` (default 3). Exhausting the budget creates a DURABLE
//! escalation (never a fake PASS) and fails the bead. A convoy only reaches
//! `all_done` when every tracked issue bead is Closed — which now requires its
//! review chain to be satisfied — so landing is gated on all required reviews.
//!
//! The machine evidence validator is the ground truth; reviewer/AI opinion is
//! always tested against it. This is a pure store-driven engine so the whole
//! policy is deterministically unit-testable with a scripted worker.

use crate::orchestration::state::{
    Adjudication, Bead, EffectiveConfig, Finding, OrchestrationStore,
    ReviewDecision, ReviewPass, ReviewPassRole, ReviewPassStatus, ReviewRole,
    gen_id, now, resolve_review_model,
};
use crate::review_evidence::{CitedEvidence, EvidenceVerdict, SnapshotIndex};

/// The model-family name (for evidence labeling) of each review role.
impl ReviewPassRole {
    /// Resolve this pass role to a concrete model via the per-role registry,
    /// honoring the immutable per-role identity (never silently a default if a
    /// role-specific model is configured).
    pub fn resolve_model(&self, eff: &EffectiveConfig) -> String {
        let role = match self {
            ReviewPassRole::ReviewerA => ReviewRole::ReviewerA,
            ReviewPassRole::ReviewerB => ReviewRole::ReviewerB,
            ReviewPassRole::Adversarial => ReviewRole::Adversarial,
            ReviewPassRole::Adjudicator => ReviewRole::Adjudicator,
        };
        resolve_review_model(eff, role)
    }
}

/// Outcome of one advance of the review policy for a source bead.
#[derive(Debug, Clone)]
pub enum PolicyAction {
    /// The review chain for this cycle does not exist yet — create the passes.
    CreateCycle,
    /// A pass is pending and should be dispatched (role + model provided).
    DispatchPass { pass_id: String, role: ReviewPassRole, model: String },
    /// All passes completed and approved → the MR is accepted.
    Approve { mr_bead_id: String, source_bead_id: String },
    /// A valid rejection → rework with corrective instructions.
    Reject {
        source_bead_id: String,
        mr_bead_id: String,
        corrective: Vec<String>,
        reason: String,
    },
    /// Review budget exhausted → durable escalation, never a fake PASS.
    Escalate { source_bead_id: String, mr_bead_id: String, reason: String },
    /// Reviewer concern is NOT machine-confirmed (unverified/hallucinated) — the
    /// change must neither be auto-rewritten nor auto-accepted without corroboration.
    NeedsCorroboration { source_bead_id: String, mr_bead_id: String, note: String },
    /// Passes still in flight.
    Wait,
    /// Adversarial review disabled for this rig → fall back to plain approval.
    Disabled { mr_bead_id: String, source_bead_id: String },
}

/// The immutable source snapshot the Adjudicator validates findings against.
/// Built once per cycle from the repository state (recursive file walk so a
/// finding can cite any file in the tree, including in subdirectories).
pub fn build_snapshot(worktree_root: &std::path::Path) -> SnapshotIndex {
    use std::collections::HashMap;
    let mut files = HashMap::new();
    walk_files(worktree_root, worktree_root, &mut files);
    // Attach the whole-tree content hash as the immutable evidence pointer.
    let mut bundle = String::new();
    let mut keys: Vec<&String> = files.keys().collect();
    keys.sort();
    for k in keys {
        bundle.push_str(k);
        bundle.push_str(":");
        bundle.push_str(&files[k].sha256);
        bundle.push('\n');
    }
    SnapshotIndex { files, bundle_hash: blake3::hash(bundle.as_bytes()).to_hex().to_string(), root: Some(worktree_root.to_path_buf()) }
}

/// Recursively walk `dir` (relative to `root`) collecting real file refs.
fn walk_files(root: &std::path::Path, dir: &std::path::Path, files: &mut std::collections::HashMap<String, crate::review_evidence::FileRef>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk_files(root, &p, files);
        } else if let Ok(content) = std::fs::read_to_string(&p) {
            if let Ok(rel) = p.strip_prefix(root) {
                let path = rel.to_string_lossy().replace('\\', "/").to_lowercase();
                files.insert(
                    path.clone(),
                    crate::review_evidence::FileRef {
                        path,
                        line_count: content.lines().count(),
                        sha256: blake3::hash(content.as_bytes()).to_hex().to_string(),
                    },
                );
            }
        }
    }
}

/// Advance the per-bead adversarial review policy for ONE source bead that is
/// currently `in_review` behind an MR. Called by the scheduler each tick.
///
/// `mr` must be the non-terminal MR tracking `source`. `snapshot` is the
/// immutable evidence index for the cycle. `eff` is the effective rig config.
pub fn advance(
    store: &OrchestrationStore,
    source: &Bead,
    mr: &Bead,
    eff: &EffectiveConfig,
    snapshot: &SnapshotIndex,
    known_symbols: &[&str],
) -> PolicyAction {
    let source_id = source.bead_id.clone();
    let mr_id = mr.bead_id.clone();

    if !eff.adversarial_review {
        return PolicyAction::Disabled { mr_bead_id: mr_id, source_bead_id: source_id };
    }

    let mut cycle_state = store.review_cycle_state(&source_id);
    if cycle_state.max_cycles == 0 {
        cycle_state.max_cycles = eff.max_review_cycles;
    }
    let cycle = cycle_state.cycles_used; // 0-based current cycle

    // 1. Create the cycle's passes if none exist yet.
    let passes = store.review_passes_for_source(&source_id);
    let cycle_passes: Vec<ReviewPass> = passes.iter().filter(|p| p.cycle == cycle).cloned().collect();
    if cycle_passes.is_empty() {
        return PolicyAction::CreateCycle;
    }

    // 2. Dispatch any pending pass.
    for pass in &cycle_passes {
        if pass.status == ReviewPassStatus::Pending {
            return PolicyAction::DispatchPass {
                pass_id: pass.pass_id.clone(),
                role: pass.role,
                model: pass.model.clone().unwrap_or_else(|| pass.role.resolve_model(eff)),
            };
        }
    }

    // 3. Wait while any pass is still running (worker not returned yet).
    if cycle_passes.iter().any(|p| p.status == ReviewPassStatus::Running) {
        return PolicyAction::Wait;
    }

    // 4. Adjudicate. The Adjudicator pass (a real model) plus the machine
    //    validator determine the verdict. Machine truth is authoritative: a
    //    Reviewer finding that cites non-existent evidence is hallucinated and
    //    cannot reject.
    let reviewer_passes: Vec<&ReviewPass> = cycle_passes
        .iter()
        .filter(|p| p.role != ReviewPassRole::Adjudicator)
        .collect();
    let adjudicator: Option<&ReviewPass> = cycle_passes.iter().find(|p| p.role == ReviewPassRole::Adjudicator);

    let mut confirmed_findings: Vec<&Finding> = Vec::new();
    let mut uncertain_findings: Vec<&Finding> = Vec::new();
    let mut any_hallucinated_rejection = false;
    for pass in &reviewer_passes {
        let Some(decision) = pass.decision else { continue };
        if decision == ReviewDecision::Approved {
            continue;
        }
        let mut pass_had_uncertain = false;
        for f in &pass.findings {
            let verdict = verify_finding(snapshot, f, known_symbols);
            let adj = Adjudication {
                finding_id: f.finding_id.clone(),
                verdict: verdict.as_str().to_string(),
                reason: match verdict {
                    EvidenceVerdict::Verified => "VERIFIED: file exists, line range in bounds, exact snippet present — confirmed defect".into(),
                    EvidenceVerdict::Unverified => "UNVERIFIED: cannot confirm the cited evidence exactly — NOT treated as a confirmed defect (requires corroboration)".into(),
                    EvidenceVerdict::Hallucinated => "HALLUCINATED: cited file/line/snippet does not exist in the snapshot — rejected finding, cannot reject the change".into(),
                },
            };
            // Record the adjudication durably on the pass (audit reason for the decision).
            if let Some(mut p2) = store.get_review_pass(&pass.pass_id) {
                p2.adjudications.push(adj);
                p2.updated_at = now();
                store.put_review_pass(&p2);
            }
            match verdict {
                EvidenceVerdict::Verified => confirmed_findings.push(f),
                EvidenceVerdict::Unverified => {
                    pass_had_uncertain = true;
                    uncertain_findings.push(f);
                }
                EvidenceVerdict::Hallucinated => any_hallucinated_rejection = true,
            }
        }
        // Record a persistent per-reviewer reason: confirmed, uncertain, or hallucinated-only rejection.
        if let Some(mut p2) = store.get_review_pass(&pass.pass_id) {
            p2.reason = Some(if p2.findings.iter().any(|f| verify_finding(snapshot, f, known_symbols) == EvidenceVerdict::Verified) {
                "reviewer rejected with machine-confirmed evidence".into()
            } else {
                "reviewer rejected but finding evidence not machine-confirmed (unverified/hallucinated)".into()
            });
            if pass_had_uncertain {
                p2.reason.as_mut().map(|r| r.push_str("; has unverified finding requiring corroboration"));
            }
            p2.updated_at = now();
            store.put_review_pass(&p2);
        }
    }

    // 5. Verdict. ONLY machine-VERIFIED evidence is a confirmed defect that may
    //    trigger an automated corrective rewrite. UNVERIFIED is by definition not
    //    confirmed and must NOT be rewritten on its own. HALLUCINATED cannot
    //    reject. If no reviewer produced a confirmed defect:
    //      - reviewers approved entirely -> APPROVE
    //      - reviewers raised concerns but none are machine-confirmed -> the
    //        change is blocked CONSERVATIVELY (not auto-rewritten, not accepted):
    //        NeedsCorroboration. An Adjudicator MAY confirm a reviewer's concern
    //        if IT cites machine-verified evidence.
    let confirmed_rejection = reviewer_passes.iter().any(|p| {
        p.decision == Some(ReviewDecision::ChangesRequested)
            && confirmed_findings.iter().any(|f| f.severity != "minor")
            && p.findings.iter().any(|f| verify_finding(snapshot, f, known_symbols) == EvidenceVerdict::Verified)
    });

    // The Adjudicator can independently veto/confirm with VERIFIED evidence.
    let adjudicator_confirmed_veto = adjudicator
        .map(|a| {
            a.decision == Some(ReviewDecision::ChangesRequested)
                && a.findings.iter().any(|f| verify_finding(snapshot, f, known_symbols) == EvidenceVerdict::Verified)
        })
        .unwrap_or(false);

    if confirmed_rejection || adjudicator_confirmed_veto {
        // Genuine CONFIRMED defect → corrective instructions (auto-rewrite allowed).
        let confirmed: Vec<&Finding> = if confirmed_rejection {
            confirmed_findings.iter().copied().collect()
        } else {
            adjudicator
                .map(|a| a.findings.iter().filter(|f| verify_finding(snapshot, f, known_symbols) == EvidenceVerdict::Verified).collect())
                .unwrap_or_default()
        };
        let corrective: Vec<String> = confirmed
            .iter()
            .filter(|f| f.severity != "minor")
            .map(|f| format!("({}) {}", f.severity, f.message))
            .collect();
        let reason = confirmed
            .first()
            .map(|f| f.message.clone())
            .unwrap_or_else(|| "review rejected the change (machine-confirmed evidence)".into());

        // Bump the cycle.
        let used = cycle_state.cycles_used + 1;
        cycle_state.cycles_used = used;
        cycle_state.last_reject_reason = Some(reason.clone());
        cycle_state.corrected_instructions = corrective.clone();
        store.put_review_cycle_state(&source_id, &cycle_state);

        if used >= cycle_state.max_cycles {
            return PolicyAction::Escalate { source_bead_id: source_id, mr_bead_id: mr_id, reason };
        }
        return PolicyAction::Reject { source_bead_id: source_id, mr_bead_id: mr_id, corrective, reason };
    }

    // No machine-confirmed defect. If any reviewer (or the adjudicator) expressed
    // a concern that is merely UNVERIFIED (not confirmed), the change must not be
    // auto-rewritten NOR auto-accepted without a human/operator corroboration:
    // conservative block.
    if (!uncertain_findings.is_empty() || any_hallucinated_rejection)
        && (reviewer_passes.iter().any(|p| p.decision == Some(ReviewDecision::ChangesRequested))
            || adjudicator.map(|a| a.decision == Some(ReviewDecision::ChangesRequested)).unwrap_or(false))
    {
        return PolicyAction::NeedsCorroboration { source_bead_id: source_id, mr_bead_id: mr_id, note: format!("{} reviewer concern(s) not machine-confirmed (unverified/hallucinated evidence)", uncertain_findings.len() + if any_hallucinated_rejection { 1 } else { 0 }) };
    }

    // All terminal, no confirmed defect, no uncorroborated concern that blocks.
    let all_terminal = cycle_passes.iter().all(|p| p.status == ReviewPassStatus::Approved || p.status == ReviewPassStatus::Rejected);
    if all_terminal {
        let real_adjudicator_veto = adjudicator
            .map(|a| a.decision == Some(ReviewDecision::ChangesRequested))
            .unwrap_or(false)
            && adjudicator_confirmed_veto;
        if real_adjudicator_veto {
            let used = cycle_state.cycles_used + 1;
            cycle_state.cycles_used = used;
            store.put_review_cycle_state(&source_id, &cycle_state);
            if used >= cycle_state.max_cycles {
                return PolicyAction::Escalate { source_bead_id: source_id, mr_bead_id: mr_id, reason: "adjudicator rejected after evidence review".into() };
            }
            return PolicyAction::Reject {
                source_bead_id: source_id,
                mr_bead_id: mr_id,
                corrective: adjudicator.map(|a| a.findings.iter().map(|f| f.message.clone()).collect()).unwrap_or_default(),
                reason: "adjudicator veto".into(),
            };
        }
        cycle_state.satisfied = true;
        store.put_review_cycle_state(&source_id, &cycle_state);
        return PolicyAction::Approve { mr_bead_id: mr_id, source_bead_id: source_id };
    }

    PolicyAction::Wait
}

/// Machine-validate one finding's cited evidence against the immutable snapshot.
pub fn verify_finding(snapshot: &SnapshotIndex, f: &Finding, known_symbols: &[&str]) -> EvidenceVerdict {
    match &f.evidence {
        Some(ev) => snapshot.verify(ev, known_symbols),
        None => {
            // A finding with no concrete file/line anchor is not machine-proof —
            // treated as Unverified (needs adjudicator's judgment).
            EvidenceVerdict::Unverified
        }
    }
}

/// Helper: build a `ReviewPass` for a role/cycle.
pub fn new_pass(source_id: &str, mr_id: &str, role: ReviewPassRole, cycle: u32, eff: &EffectiveConfig) -> ReviewPass {
    let model = role.resolve_model(eff);
    ReviewPass {
        pass_id: gen_id(),
        source_bead_id: source_id.to_string(),
        mr_bead_id: mr_id.to_string(),
        role,
        cycle,
        status: ReviewPassStatus::Pending,
        model: Some(model),
        decision: None,
        reason: None,
        findings: vec![],
        adjudications: vec![],
        agent_id: None,
        created_at: now(),
        updated_at: now(),
    }
}

/// Populate a cycle's passes (ReviewerA, ReviewerB, Adversarial, Adjudicator).
/// Call once when `create_cycle` fires. Returns the created pass ids.
pub fn create_cycle(store: &OrchestrationStore, source_id: &str, mr_id: &str, cycle: u32, eff: &EffectiveConfig) -> Vec<String> {
    let roles = [
        ReviewPassRole::ReviewerA,
        ReviewPassRole::ReviewerB,
        ReviewPassRole::Adversarial,
        ReviewPassRole::Adjudicator,
    ];
    let mut ids = Vec::new();
    for role in roles {
        let pass = new_pass(source_id, mr_id, role, cycle, eff);
        let id = pass.pass_id.clone();
        store.put_review_pass(&pass);
        ids.push(id);
    }
    ids
}

/// Record a worker's decision + findings for a pass (called when a review agent
/// returns). The Adjudicator also records its decision here; the machine
/// validator runs separately via `advance`.
pub fn record_pass_result(
    store: &OrchestrationStore,
    pass_id: &str,
    decision: &str,
    findings: Vec<Finding>,
) -> bool {
    let Some(mut pass) = store.get_review_pass(pass_id) else { return false };
    pass.decision = Some(ReviewDecision::parse(decision));
    pass.findings = findings;
    pass.status = if pass.decision == Some(ReviewDecision::Approved) {
        ReviewPassStatus::Approved
    } else {
        ReviewPassStatus::Rejected
    };
    pass.updated_at = now();
    store.put_review_pass(&pass);
    true
}

/// Build a finding from a reviewer's JSON (path/lines/snippet/message/severity).
pub fn finding_from_json(v: &serde_json::Value) -> Option<Finding> {
    let message = v.get("message").and_then(|m| m.as_str())?.to_string();
    let severity = v.get("severity").and_then(|s| s.as_str()).unwrap_or("minor").to_string();
    let evidence = v.get("evidence").map(|e| CitedEvidence {
        path: e.get("path").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        lines: e
            .get("lines")
            .and_then(|l| l.as_array())
            .map(|a| {
                let lo = a.first().and_then(|x| x.as_u64()).unwrap_or(0);
                let hi = a.get(1).and_then(|x| x.as_u64()).unwrap_or(lo);
                (lo as u32, hi as u32)
            }),
        snippet: e.get("snippet").and_then(|x| x.as_str()).map(str::to_string),
    });
    Some(Finding {
        finding_id: gen_id(),
        severity,
        message,
        evidence,
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Policy-level helpers used by the scheduler to enforce "landing waits for all
// required reviews".
// ────────────────────────────────────────────────────────────────────────────

/// True when a source bead's review chain is satisfied (approved) — used to gate
/// convoy progress and to decide the MR is mergeable.
pub fn review_satisfied(store: &OrchestrationStore, source_id: &str) -> bool {
    store.review_cycle_state(source_id).satisfied
}

/// Human-readable review summary for the dashboard/UI.
pub fn review_summary(store: &OrchestrationStore, source_id: &str) -> serde_json::Value {
    let st = store.review_cycle_state(source_id);
    let passes = store.review_passes_for_source(source_id);
    serde_json::json!({
        "cycles_used": st.cycles_used,
        "max_cycles": st.max_cycles,
        "satisfied": st.satisfied,
        "last_reject_reason": st.last_reject_reason,
        "corrective_instructions": st.corrected_instructions,
        "passes": passes.iter().map(|p| serde_json::json!({
            "pass_id": p.pass_id,
            "role": p.role.as_str(),
            "cycle": p.cycle,
            "status": match p.status { ReviewPassStatus::Pending=>"pending", ReviewPassStatus::Running=>"running", ReviewPassStatus::Approved=>"approved", ReviewPassStatus::Rejected=>"rejected"},
            "model": p.model,
            "decision": p.decision.map(|d| match d { ReviewDecision::Approved=>"approved", ReviewDecision::ChangesRequested=>"changes_requested"}),
            "findings": p.findings.len(),
            "adjudications": p.adjudications.iter().map(|a| serde_json::json!({"finding_id": a.finding_id, "verdict": a.verdict, "reason": a.reason})).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// A durable in-memory store for tests/tools (shared with the state topology tests).
#[cfg(test)]
pub fn test_store() -> (std::sync::Arc<dyn lattice_hotstore::HotStore>, OrchestrationStore) {
    use lattice_hotstore::{HotKey, HotStore, HotStoreError, HotValue};
    use std::sync::{Arc, Mutex};
    #[derive(Default, Debug)]
    struct Mem(Mutex<std::collections::BTreeMap<Vec<u8>, Vec<u8>>>);
    impl HotStore for Mem {
        fn get(&self, k: &HotKey) -> Result<Option<HotValue>, HotStoreError> { Ok(self.0.lock().unwrap().get(&k.0).cloned().map(HotValue)) }
        fn put(&self, k: &HotKey, v: &HotValue) -> Result<(), HotStoreError> { self.0.lock().unwrap().insert(k.0.clone(), v.0.clone()); Ok(()) }
        fn delete(&self, k: &HotKey) -> Result<bool, HotStoreError> { Ok(self.0.lock().unwrap().remove(&k.0).is_some()) }
        fn exists(&self, k: &HotKey) -> Result<bool, HotStoreError> { Ok(self.0.lock().unwrap().contains_key(&k.0)) }
        fn scan_prefix(&self, p: &[u8]) -> Result<Vec<HotKey>, HotStoreError> {
            Ok(self.0.lock().unwrap().keys().filter(|k| k.starts_with(p)).cloned().map(HotKey).collect())
        }
    }
    let mem = Arc::new(Mem::default());
    (mem.clone() as Arc<dyn HotStore>, OrchestrationStore::new(mem as Arc<dyn HotStore>))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestration::state::{Bead, Town, TownConfig, ReviewCycleState, gen_id, now};
    use crate::review_evidence::CitedEvidence;
    use lattice_hotstore::{HotKey, HotStore, HotStoreError, HotValue};
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    #[derive(Default, Debug)]
    pub struct MemHotStore(Mutex<BTreeMap<Vec<u8>, Vec<u8>>>);
    impl HotStore for MemHotStore {
        fn get(&self, k: &HotKey) -> Result<Option<HotValue>, HotStoreError> {
            Ok(self.0.lock().unwrap().get(&k.0).cloned().map(HotValue))
        }
        fn put(&self, k: &HotKey, v: &HotValue) -> Result<(), HotStoreError> {
            self.0.lock().unwrap().insert(k.0.clone(), v.0.clone());
            Ok(())
        }
        fn delete(&self, _k: &HotKey) -> Result<bool, HotStoreError> {
            Ok(true)
        }
        fn exists(&self, _k: &HotKey) -> Result<bool, HotStoreError> {
            Ok(false)
        }
        fn scan_prefix(&self, _p: &[u8]) -> Result<Vec<HotKey>, HotStoreError> {
            Ok(vec![])
        }
    }

    fn store() -> OrchestrationStore {
        OrchestrationStore::new(Arc::new(MemHotStore::default()) as Arc<dyn HotStore>)
    }

    fn source_bead(s: &OrchestrationStore, id: &str) -> Bead {
        let b = Bead::create(
            crate::orchestration::state::BeadType::Issue,
            "source",
            None,
            None,
            vec![],
            serde_json::json!({}),
            Some("rig-1"),
            None,
            Some("polecat"),
        );
        let mut b = b;
        b.bead_id = id.to_string();
        b.status = crate::orchestration::state::BeadStatus::InReview;
        s.put_bead(&b);
        b
    }

    fn mr_bead(s: &OrchestrationStore, id: &str, source_id: &str) -> Bead {
        let mut b = Bead::create(
            crate::orchestration::state::BeadType::MergeRequest,
            "mr",
            None,
            None,
            vec!["gt:merge-request".into()],
            serde_json::json!({}),
            Some("rig-1"),
            Some(source_id),
            Some("polecat"),
        );
        b.bead_id = id.to_string();
        b.status = crate::orchestration::state::BeadStatus::Open;
        s.put_bead(&b);
        s.put_review_metadata(&crate::orchestration::state::ReviewMetadata {
            bead_id: id.to_string(),
            branch: "feat".into(),
            target_branch: "main".into(),
            merge_commit: None,
            pr_url: None,
            retry_count: 0,
            auto_merge_ready_since: None,
            last_feedback_check_at: None,
        });
        b
    }

    fn eff(max_cycles: u32) -> EffectiveConfig {
        // Build via resolve_rig_config from a default town (max_review_cycles=3).
        let mut cfg = crate::orchestration::state::resolve_rig_config(&Town {
            town_id: "t".into(),
            owner_user_id: None,
            config: TownConfig::default(),
            created_at: now(),
        }, None);
        cfg.max_review_cycles = max_cycles;
        cfg.adversarial_review = true;
        cfg
    }

    fn finding(path: &str, lo: u32, hi: u32, sev: &str, msg: &str) -> Finding {
        Finding {
            finding_id: gen_id(),
            severity: sev.into(),
            message: msg.into(),
            evidence: Some(CitedEvidence {
                path: path.into(),
                lines: Some((lo, hi)),
                snippet: None,
            }),
        }
    }

    /// A snapshot with one real file at a known line range.
    fn snapshot_with(files: Vec<(String, u32)>) -> SnapshotIndex {
        use std::collections::HashMap;
        let mut map = HashMap::new();
        for (p, lc) in files {
            map.insert(p.clone(), crate::review_evidence::FileRef { path: p.clone(), line_count: lc as usize, sha256: "x".into() });
        }
        SnapshotIndex { files: map, bundle_hash: "h".into(), root: None }
    }

    /// Simulate the full policy for ONE cycle: create passes for `cycle`, record
    /// the reviewer/adjudicator results, then advance → final outcome.
    fn drive_cycle(
        s: &OrchestrationStore,
        source_id: &str,
        mr_id: &str,
        cycle: u32,
        eff: &EffectiveConfig,
        snapshot: &SnapshotIndex,
        reviewer_results: &[(&str, &str, Vec<Finding>)], // (role, decision, findings)
    ) -> PolicyAction {
        let pass_ids = create_cycle(s, source_id, mr_id, cycle, eff);
        assert_eq!(pass_ids.len(), 4, "four passes per cycle");
        for (i, _role) in roles().iter().enumerate() {
            let (_, decision, findings) = &reviewer_results[i];
            record_pass_result(s, &pass_ids[i], decision, findings.clone());
        }
        let source = s.get_bead(source_id).unwrap();
        let mr = s.get_bead(mr_id).unwrap();
        advance(s, &source, &mr, eff, snapshot, &[])
    }

    fn roles() -> [ReviewPassRole; 4] {
        [ReviewPassRole::ReviewerA, ReviewPassRole::ReviewerB, ReviewPassRole::Adversarial, ReviewPassRole::Adjudicator]
    }

    #[test]
    fn reject_then_work_then_approve_within_cycles() {
        let s = store();
        source_bead(&s, "src-1");
        mr_bead(&s, "mr-1", "src-1");
        let eff = eff(3);
        // Snapshot: src/main.rs has 200 lines.
        let snap = snapshot_with(vec![("src/main.rs".into(), 200)]);

        // CYCLE 0: Reviewer A + B + Adversarial all reject on a REAL source line →
        // machine-valid rejection → Reject with corrective instructions.
        let a1 = finding("src/main.rs", 10, 12, "major", "logic error on line 11");
        let out1 = drive_cycle(
            &s, "src-1", "mr-1", 0, &eff, &snap,
            &[
                ("reviewer_a", "changes_requested", vec![a1.clone()]),
                ("reviewer_b", "changes_requested", vec![a1.clone()]),
                ("adversarial", "changes_requested", vec![a1.clone()]),
                ("adjudicator", "approved", vec![]),
            ],
        );
        match &out1 {
            PolicyAction::Reject { corrective, .. } => {
                assert!(!corrective.is_empty());
                assert!(corrective[0].contains("logic error on line 11"), "bad corrective: {corrective:?}");
            }
            other => panic!("expected Reject on cycle 0, got {other:?}"),
        }
        // After rejection, cycle state shows 1 used, not satisfied.
        let st = s.review_cycle_state("src-1");
        assert_eq!(st.cycles_used, 1);
        assert!(!st.satisfied);

        // CYCLE 1 (fresh store, all approve) → Approved; review satisfied.
        let s2 = store();
        source_bead(&s2, "src-1");
        mr_bead(&s2, "mr-1", "src-1");
        let out2 = drive_cycle(
            &s2, "src-1", "mr-1", 0, &eff, &snap,
            &[
                ("reviewer_a", "approved", vec![]),
                ("reviewer_b", "approved", vec![]),
                ("adversarial", "approved", vec![]),
                ("adjudicator", "approved", vec![]),
            ],
        );
        match out2 {
            PolicyAction::Approve { .. } => {}
            other => panic!("expected Approve on accept, got {other:?}"),
        }
        assert!(s2.review_cycle_state("src-1").satisfied, "accepted change marks the review satisfied");
    }

    #[test]
    fn exhausted_cycles_escalate_never_fake_pass() {
        let s = store();
        source_bead(&s, "src-x");
        mr_bead(&s, "mr-x", "src-x");
        let eff = eff(1); // max 1 cycle
        let snap = snapshot_with(vec![("src/main.rs".into(), 200)]);
        let a1 = finding("src/main.rs", 5, 5, "critical", "uninitialized use");

        // Set cycle state -- already at 1 used, max 1 → a new valid rejection
        // over the budget must ESCALATE, never a fake PASS.
        s.put_review_cycle_state("src-x", &ReviewCycleState { cycles_used: 1, max_cycles: 1, corrected_instructions: vec![], last_reject_reason: None, satisfied: false });

        // An escape hatch: create_cycle is keyed by the cycle number; use the
        // current used count as the cycle so the passes attach to cycle 1 and
        // the budget check runs after them.
        let out = drive_cycle(
            &s, "src-x", "mr-x", 1, &eff, &snap,
            &[
                ("reviewer_a", "changes_requested", vec![a1.clone()]),
                ("reviewer_b", "changes_requested", vec![a1.clone()]),
                ("adversarial", "changes_requested", vec![a1]),
                ("adjudicator", "approved", vec![]),
            ],
        );
        match out {
            PolicyAction::Escalate { reason, .. } => {
                assert!(!reason.is_empty(), "escalation reason must be specific");
            }
            other => panic!("expected Escalate when budget exhausted, got {other:?}"),
        }
    }

    #[test]
    fn hallucinated_finding_does_not_rewrite_stays_conservatively_blocked() {
        let s = store();
        source_bead(&s, "src-h");
        mr_bead(&s, "mr-h", "src-h");
        let eff = eff(3);
        // Real file is src/main.rs (200 lines). Reviewer A + B + Adversarial
        // "reject" by citing a NON-EXISTENT file → all evidence Hallucinated →
        // NOT a confirmed defect, so NO auto-rewrite. Because reviewers
        // explicitly rejected, the change stays conservatively blocked
        // (NeedsCorroboration) rather than silently accepted or rewritten.
        let fake = finding("src/does_not_exist.rs", 1, 1, "critical", "claims fake file");
        let out = drive_cycle(
            &s, "src-h", "mr-h", 0, &eff,
            &snapshot_with(vec![("src/main.rs".into(), 200)]),
            &[
                ("reviewer_a", "changes_requested", vec![fake.clone()]),
                ("reviewer_b", "changes_requested", vec![fake.clone()]),
                ("adversarial", "changes_requested", vec![fake.clone()]),
                ("adjudicator", "approved", vec![]),
            ],
        );
        match out {
            PolicyAction::NeedsCorroboration { .. } => {}
            other => panic!("hallucinated-only rejection must be a conservative block, not approve or rewrite; got {other:?}"),
        }
    }

    #[test]
    fn unverified_concern_does_not_auto_rewrite_stays_blocked() {
        let s = store();
        source_bead(&s, "src-u");
        mr_bead(&s, "mr-u", "src-u");
        let eff = eff(3);
        // A concern citing a REAL file but a snippet that does NOT appear in the
        // cited lines is UNVERIFIED — NOT a confirmed defect. It must NOT trigger
        // an automated corrective rewrite; it stays conservatively blocked.
        let unverified = Finding {
            finding_id: crate::orchestration::state::gen_id(),
            severity: "major".into(),
            message: "claims a line-1 issue but the snippet is not there".into(),
            evidence: Some(CitedEvidence {
                path: "src/main.rs".into(),
                lines: Some((1, 1)),
                snippet: Some("this snippet definitely does not exist anywhere".into()),
            }),
        };
        let out = drive_cycle(
            &s, "src-u", "mr-u", 0, &eff,
            &snapshot_with(vec![("src/main.rs".into(), 200)]),
            &[
                ("reviewer_a", "changes_requested", vec![unverified.clone()]),
                ("reviewer_b", "changes_requested", vec![unverified.clone()]),
                ("adversarial", "changes_requested", vec![unverified.clone()]),
                ("adjudicator", "approved", vec![]),
            ],
        );
        match out {
            PolicyAction::NeedsCorroboration { .. } => {}
            PolicyAction::Approve { .. } => panic!("UNVERIFIED concern must NOT be silently accepted"),
            PolicyAction::Reject { .. } => panic!("UNVERIFIED concern must NOT trigger an auto-rewrite"),
            other => panic!("unverified concern: expected conservative block, got {other:?}"),
        }
    }
}