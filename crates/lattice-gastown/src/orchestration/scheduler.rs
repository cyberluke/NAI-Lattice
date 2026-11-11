//! Durable Gas Town scheduler + action dispatcher (`orchestration::scheduler`).
//!
//! Runs the reconciler on the durable orchestration state and applies the
//! resulting actions. A background loop (owned by the host) ticks the scheduler
//! with generation fencing (reusing `GastownService::scheduler_generation`) so
//! a restarted host never double-applies: each tick reads the current durable
//! state, reconciles, applies actions (each idempotent), and advances a durable
//! tick watermark. Crash recovery is inherent — after any crash, the next tick
//! reconciles from durable state and re-applies whatever is still needed.
//!
//! The real-model agent spawn (polecat/refinery/mayor) is a host-provided
//! callback so the crate stays substrate-agnostic; the host supervises the real
//! `gastown-agent` worker subprocesses (Job Object + timeout), like the
//! existing review workers.

use std::sync::Arc;
use std::path::PathBuf;

use crate::orchestration::reconciler::{self, Action, ReconcileCtx};
use crate::orchestration::state::{
    self, Agent, AgentRole, AgentStatus, Bead, BeadStatus, BeadType, DependencyKind,
    OrchestrationStore, Town, gen_id, now,
};

/// Result of a single tick's action application.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TickReport {
    pub tick: u64,
    pub generated: usize,
    pub applied: usize,
    pub dispatched: usize,
    pub landed: Vec<String>,
    pub error: Option<String>,
}

impl TickReport {
    /// A JSON-friendly view for the host control surface.
    pub fn to_jsonish(&self) -> serde_json::Value {
        serde_json::json!({
            "tick": self.tick,
            "generated": self.generated,
            "applied": self.applied,
            "dispatched": self.dispatched,
            "landed": self.landed,
            "error": self.error,
        })
    }
}

/// What the apply layer needs to actually run a real-model agent.
pub trait AgentRunner: Send + Sync {
    /// Run one agent (polecat/refinery) against a bead in its worktree.
    fn run_agent(&self, ctx: &DispatchCtx) -> Result<DispatchResult, String>;
}

/// Parameters to run one real agent.
#[derive(Clone)]
pub struct DispatchCtx<'a> {
    pub town: &'a Town,
    pub store: &'a OrchestrationStore,
    pub agent: &'a Agent,
    pub bead: &'a Bead,
    pub rig_id: String,
    pub default_branch: String,
    pub worktree_root: std::path::PathBuf,
    pub branch: String,
    /// The git branch this dispatch should merge INTO when done (for refinery /
    /// landing merges). For a convoy-tracked source, this is the convoy feature
    /// branch; for a landing MR it is the rig default branch.
    pub target_branch: String,
    pub pr_url: Option<String>,
    /// When this dispatch is a per-bead REVIEW PASS, the pass id + role so the
    /// worker knows to return a review decision+findings instead of done/pr_url.
    pub review_pass_id: Option<String>,
    pub review_role: Option<crate::orchestration::state::ReviewPassRole>,
    /// The concrete model for this review pass (per-role identity).
    pub review_model: Option<String>,
}

/// What a real agent run produced (drives the follow-up state mutations).
#[derive(Debug, Clone, Default)]
pub struct DispatchResult {
    pub done: bool,
    pub pr_url: Option<String>,
    pub summary: Option<String>,
    /// When a REVIEW-PASS agent runs, it returns a review decision + findings
    /// instead of done/pr_url. The adversarial policy consumes this.
    pub review: Option<ReviewerOutcome>,
}

/// The reviewer worker's output (decision + anchored findings).
#[derive(Debug, Clone)]
pub struct ReviewerOutcome {
    pub decision: crate::orchestration::state::ReviewDecision,
    pub findings: Vec<crate::orchestration::state::Finding>,
}

/// The durable scheduler (state + tick watermark + reconciler + apply).
#[derive(Clone)]
pub struct Orchestrator {
    store: OrchestrationStore,
    town_cache: Arc<std::sync::RwLock<Option<Town>>>,
    tick: Arc<std::sync::atomic::AtomicU64>,
}

/// The orchestrator's own generated actions for external inspection.
pub use reconciler::Action as OrtoAction;

impl Orchestrator {
    pub fn new(store: OrchestrationStore) -> Self {
        Self {
            store,
            town_cache: Arc::new(std::sync::RwLock::new(None)),
            tick: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    pub fn store(&self) -> &OrchestrationStore {
        &self.store
    }

    pub fn set_town(&self, town: Town) {
        self.store.put_town(&town);
        *self.town_cache.write().unwrap() = Some(town);
    }

    pub fn town(&self) -> Option<Town> {
        // Bound the read guard's lifetime: drop it BEFORE the .or_else path may
        // take a write lock (otherwise read+write on the same RwLock deadlocks).
        let cached = { self.town_cache.read().unwrap().clone() };
        cached.or_else(|| {
            let t = self.store.get_town(&self.primary_town_id());
            if let Some(t) = &t {
                *self.town_cache.write().unwrap() = Some(t.clone());
            }
            t
        })
    }

    pub fn primary_town_id(&self) -> String {
        // If a town index exists, use the first; otherwise default to a known id.
        let towns = self.store.list_towns();
        towns
            .first()
            .map(|t| t.town_id.clone())
            .unwrap_or_else(|| "town-default".to_string())
    }

    /// Advance the durable tick counter.
    fn next_tick(&self) -> u64 {
        let t = self.tick.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if let Err(e) = self.store.hot_put(format!("gastown/scheduler/tick"), &t.to_string()) {
            tracing::warn!(%e, "failed to persist scheduler tick");
        }
        t
    }

    /// Last persisted/current tick (`0` if never run).
    pub fn last_tick(&self) -> u64 {
        let persisted = self
            .store
            .hot_get(format!("gastown/scheduler/tick"))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        persisted.max(self.tick.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// Durable store sizes (crash-recovery evidence).
    pub fn store_watermark(&self) -> serde_json::Value {
        serde_json::json!({
            "towns": self.store.list_towns().len(),
            "rigs": self.store.list_rigs().len(),
            "agents": self.store.list_agents().len(),
            "beads": self.store.list_beads().len(),
        })
    }

    /// Run one reconcile + apply pass. Returns the tick report. Crash-safe:
    /// starting from durable state, applies what the reconciler says is needed.
    pub fn tick(&self, runner: &dyn AgentRunner) -> TickReport {
        let town = match self.town() {
            Some(t) => t,
            None => return TickReport { tick: 0, generated: 0, applied: 0, dispatched: 0, landed: vec![], error: Some("no town configured".into()) },
        };
        let tick = self.next_tick();
        let ctx = ReconcileCtx { now_ms: state::now_unix_ms(), town: &town };
        let actions = reconciler::reconcile(&ctx, &self.store, &town);
        let generated = actions.len();
        let mut applied = 0usize;
        let mut dispatched = 0usize;
        let mut landed = Vec::new();
        let mut error: Option<String> = None;

        // Run the per-bead adversarial review policy BEFORE applying the base
        // reconciler actions, so a review decision can transition the MR/source
        // and suppress a conflicting refinery dispatch in the same tick.
        let review_outcome = self.reconcile_review_passes(&town, runner, &mut dispatched);

        for action in &actions {
            match self.apply(&ctx, action, runner, &mut landed) {
                ApplyOutcome::Applied => applied += 1,
                ApplyOutcome::Dispatched => {
                    applied += 1;
                    dispatched += 1;
                }
                ApplyOutcome::Skipped => {}
                ApplyOutcome::Error(e) => {
                    error = Some(e);
                }
            }
        }
        // Surface any review-policy error in the tick report.
        if let Err(e) = review_outcome {
            error = Some(format!("review policy: {e}"));
        }
        TickReport { tick, generated, applied, dispatched, landed, error }
    }

    /// Run the per-bead adversarial review policy for every issue bead currently
    /// `in_review` behind a non-terminal MR. Returns Err only on a hard policy
    /// error (e.g. worker misbehaving); normally returns Ok(()).
    ///
    /// This is where the WS5 chain actually executes on the live worker path:
    /// creates review passes per cycle, dispatches each reviewer/adjudicator
    /// through `runner` (returning a review decision + findings), machine-
    /// validates findings, and on reject rewinds the source to `open` for
    /// repair with corrective instructions; on exhausted cycles FAILS the MR and
    /// creates a durable escalation (never a fake PASS); on approve merges.
    fn reconcile_review_passes(
        &self,
        town: &Town,
        runner: &dyn AgentRunner,
        dispatched: &mut usize,
    ) -> Result<(), String> {
        use crate::orchestration::adversarial::{self, PolicyAction};
        use crate::orchestration::state::{resolve_rig_config};

        let candidates: Vec<(Bead, Bead)> = self
            .store
            .list_beads()
            .into_iter()
            .filter(|b| b.bead_type == BeadType::Issue && b.status == BeadStatus::InReview)
            .filter_map(|src| {
                // The non-terminal MR tracking this source.
                let mr = self
                    .store
                    .dependents_on(&src.bead_id)
                    .into_iter()
                    .filter(|d| d.kind == DependencyKind::Tracks)
                    .filter_map(|d| self.store.get_bead(&d.bead_id))
                    .find(|m| m.bead_type == BeadType::MergeRequest && !m.status.is_terminal());
                mr.map(|m| (src, m))
            })
            .collect();

        for (source, mr) in candidates {
            let rig_id = mr.rig_id.clone().unwrap_or_default();
            let eff = resolve_rig_config(town, self.store.get_rig(&rig_id).as_ref());
            // Build the immutable evidence snapshot from the rig's worktree.
            let rig_root = self.store.get_rig(&rig_id).map(|r| r.repo_base(&self.rig_truth_root(&rig_id).to_string_lossy().into_owned()));
            let truth_root = rig_root.map(std::path::PathBuf::from).unwrap_or_else(|| self.rig_truth_root(&rig_id));
            // The Adjudicator validates against the polecat's CHANGE WORKTREE (the
            // branch the MR tracks), so finding paths are repo-relative to where the
            // polecat actually edits. Fall back to the truth root when no branch.
            let branch = self.store.get_review_metadata(&mr.bead_id).map(|m| m.branch.clone()).filter(|b| !b.is_empty());
            let workroot = branch.map(|b| truth_root.join(b)).unwrap_or_else(|| truth_root.clone());
            let snapshot = adversarial::build_snapshot(&workroot);

            let action = adversarial::advance(&self.store, &source, &mr, &eff, &snapshot, &[]);
            match action {
                PolicyAction::Disabled { mr_bead_id, source_bead_id } => {
                    // Not adversarial for this rig: the base reconciler refinery
                    // path handles merge (Approve), so just let it flow.
                    let _ = (mr_bead_id, source_bead_id);
                }
                PolicyAction::NeedsCorroboration { source_bead_id, mr_bead_id, note } => {
                    // Conservative block: a reviewer raised a concern but NOTHING
                    // is machine-confirmed (unverified/hallucinated evidence). We
                    // must neither auto-rewrite (unconfirmed) nor auto-accept
                    // (uncorroborated). Record a durable escalation so a human/
                    // operator can corroborate; the source stays OPEN (blocked),
                    // NOT Closed, so landing never advances on an unverified
                    // concern.
                    self.store.emit_event(&state::BeadEvent::new(
                        "review.needs_corroboration",
                        Some(source_bead_id.clone()),
                        None,
                        Some(town.town_id.clone()),
                        Some(rig_id.clone()),
                        serde_json::json!({"mr_bead_id": mr_bead_id.clone(), "note": note.clone()}),
                    ));
                    let stat = state::TriageRequestStat {
                        triage_type: state::TriageType::Escalation,
                        agent_bead_id: None,
                        hooked_bead_id: Some(source_bead_id.clone()),
                        title: format!("Review needs corroboration: {note}"),
                        context: serde_json::json!({"source_bead_id": source_bead_id.clone(), "mr_bead_id": mr_bead_id.clone(), "note": note.clone()}),
                        rig_id: Some(rig_id.clone()),
                    };
                    self.store.create_triage_request(&town.town_id, &stat, vec!["RESTART".into(), "PROVIDE_GUIDANCE".into(), "CLOSE_BEAD".into()]);
                }
                PolicyAction::CreateCycle => {
                    let cycle = self.store.review_cycle_state(&source.bead_id).cycles_used;
                    adversarial::create_cycle(&self.store, &source.bead_id, &mr.bead_id, cycle, &eff);
                    self.store.emit_event(&state::BeadEvent::new(
                        "review.passes_created",
                        Some(source.bead_id.clone()),
                        None,
                        Some(town.town_id.clone()),
                        Some(rig_id.clone()),
                        serde_json::json!({"cycle": cycle}),
                    ));
                }
                PolicyAction::DispatchPass { pass_id, role, model } => {
                    // Dispatch a review-pass agent. We reuse the refinery role
                    // but tag the pass + role + model so the worker returns a
                    // review outcome (the runner inspects DispatchCtx.review_role).
                    let agent = self.get_or_create_review_agent(&town.town_id, &rig_id, role)?;
                    self.store.hook_agent(agent.bead_id.clone(), pass_id.clone());
                    self.store.update_agent_status(agent.bead_id.clone(), state::AgentStatus::Working, None);
                    let mut p = match self.store.get_review_pass(&pass_id) {
                        Some(p) => p,
                        None => return Err(format!("review pass {pass_id} vanished before dispatch")),
                    };
                    p.status = state::ReviewPassStatus::Running;
                    p.agent_id = Some(agent.bead_id.clone());
                    p.model = Some(model.clone());
                    p.updated_at = state::now();
                    self.store.put_review_pass(&p);

                    // Build a DispatchCtx against the MR + worktree.
                    let workroot = self.store.get_rig(&rig_id).map(|r| r.repo_base(&self.rig_truth_root(&rig_id).to_string_lossy().into_owned()))
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|| self.rig_truth_root(&rig_id));
                    let branch = self.store.get_review_metadata(&mr.bead_id).map(|m| m.branch.clone()).unwrap_or_else(|| format!("mr/{}", mr.bead_id.split('-').next().unwrap_or("review")));
                    let dc = DispatchCtx {
                        town,
                        store: &self.store,
                        agent: &agent,
                        bead: &mr,
                        rig_id: rig_id.clone(),
                        default_branch: mr.metadata["target_branch"].as_str().unwrap_or("main").to_string(),
                        worktree_root: workroot.join(&branch),
                        branch: branch.clone(),
                        target_branch: mr.metadata["target_branch"].as_str().unwrap_or("main").to_string(),
                        pr_url: self.store.get_review_metadata(&mr.bead_id).and_then(|m| m.pr_url),
                        review_pass_id: Some(pass_id.clone()),
                        review_role: Some(role),
                        review_model: Some(model.clone()),
                    };
                    match runner.run_agent(&dc) {
                        Ok(res) => {
                            if let Some(review) = res.review {
                                let decision = match review.decision {
                                    state::ReviewDecision::Approved => "approved",
                                    state::ReviewDecision::ChangesRequested => "changes_requested",
                                };
                                adversarial::record_pass_result(&self.store, &pass_id, decision, review.findings);
                                *dispatched += 1;
                            } else {
                                // Worker didn't return a review outcome — fail it.
                                adversarial::record_pass_result(&self.store, &pass_id, "changes_requested", vec![
                                    state::Finding {
                                        finding_id: state::gen_id(),
                                        severity: "critical".into(),
                                        message: "reviewer worker did not return a decision".into(),
                                        evidence: None,
                                    },
                                ]);
                            }
                        }
                        Err(e) => return Err(format!("review pass {role:?} worker failed: {e}")),
                    }
                }
                PolicyAction::Wait => {}
                PolicyAction::Approve { mr_bead_id, source_bead_id } => {
                    // Merge the review-approved MR and close the source.
                    if let Some(mut mrb) = self.store.get_bead(&mr_bead_id) {
                        mrb.status = BeadStatus::Closed;
                        mrb.closed_at = Some(state::now());
                        mrb.updated_at = state::now();
                        self.store.put_bead(&mrb);
                    }
                    self.store.complete_review(mr_bead_id.clone(), None);
                    if let Some(mut src) = self.store.get_bead(&source_bead_id) {
                        if !src.status.is_terminal() {
                            src.status = BeadStatus::Closed;
                            src.closed_at = Some(state::now());
                            src.updated_at = state::now();
                            self.store.put_bead(&src);
                            // Genuine VERIFIED_10_10: the review-approved MR merged,
                            // so this closure (unlike a manual set_status) carries a
                            // durable verification stamp bound to the current epoch.
                            // Downstream `blocks` edges open only on this stamp while
                            // the gate's source_revision still matches.
                            let epoch = self.store.hot_get("gastown/portability/epoch").unwrap_or_default();
                            self.store.mark_bead_verified(source_bead_id.as_str(), &epoch, "COMPLETE", 8, 8);
                        }
                    }
                    self.store.emit_event(&state::BeadEvent::new(
                        "review.approved",
                        Some(source_bead_id.clone()),
                        None,
                        Some(town.town_id.clone()),
                        Some(rig_id.clone()),
                        serde_json::json!({"mr_bead_id": mr_bead_id.clone()}),
                    ));
                }
                PolicyAction::Reject { source_bead_id, mr_bead_id, corrective, reason } => {
                    // Rewind the source for repair + attach corrective instructions.
                    if let Some(mut src) = self.store.get_bead(&source_bead_id) {
                        src.status = BeadStatus::Open;
                        src.assignee_agent_bead_id = None;
                        // A valid rejection is a deliberate repair instruction, not a
                        // dispatch failure — reset the cooldown so the repair is prompt.
                        src.dispatch_attempts = 0;
                        src.last_dispatch_attempt_at = None;
                        src.updated_at = state::now();
                        // clear the assignee hook
                        for a in self.store.list_agents() {
                            if a.current_hook_bead_id.as_deref() == Some(src.bead_id.as_str()) || a.current_hook_bead_id.as_deref() == Some(mr_bead_id.as_str()) {
                                self.store.unhook_agent(&a.bead_id);
                            }
                        }
                        self.store.put_bead(&src);
                    }
                    // Leave the MR open so the rework's new submission replaces it;
                    // also detach it from the source (the reconciler abandons stale
                    // MRs; we reopen the source so a fresh polecat repair re-slings).
                    if let Some(mut mrb) = self.store.get_bead(&mr_bead_id) {
                        mrb.status = BeadStatus::Failed; // superseded by corrective rework
                        mrb.updated_at = state::now();
                        self.store.put_bead(&mrb);
                    }
                    self.store.emit_event(&state::BeadEvent::new(
                        "review.rejected",
                        Some(source_bead_id.clone()),
                        None,
                        Some(town.town_id.clone()),
                        Some(rig_id.clone()),
                        serde_json::json!({"mr_bead_id": mr_bead_id.clone(), "corrective": corrective.clone(), "reason": reason.clone()}),
                    ));
                }
                PolicyAction::Escalate { source_bead_id, mr_bead_id, reason } => {
                    // Durable escalation + fail the MR. NEVER a fake PASS.
                    if let Some(mut mrb) = self.store.get_bead(&mr_bead_id) {
                        mrb.status = BeadStatus::Failed;
                        mrb.metadata["failureReason"] = serde_json::json!("review_cycles_exhausted");
                        mrb.metadata["failureMessage"] = serde_json::json!(reason.clone());
                        mrb.updated_at = state::now();
                        self.store.put_bead(&mrb);
                    }
                    if let Some(mut src) = self.store.get_bead(&source_bead_id) {
                        src.status = BeadStatus::Failed;
                        src.updated_at = state::now();
                        self.store.put_bead(&src);
                    }
                    // Create a durable triage escalation bead.
                    let stat = state::TriageRequestStat {
                        triage_type: state::TriageType::Escalation,
                        agent_bead_id: None,
                        hooked_bead_id: Some(source_bead_id.clone()),
                        title: format!("Review cycles exhausted: {reason}"),
                        context: serde_json::json!({"source_bead_id": source_bead_id.clone(), "mr_bead_id": mr_bead_id.clone(), "reason": reason.clone()}),
                        rig_id: Some(rig_id.clone()),
                    };
                    self.store.create_triage_request(&town.town_id, &stat, vec!["RESTART".into(), "ESCALATE_TO_MAYOR".into(), "CLOSE_BEAD".into()]);
                    self.store.emit_event(&state::BeadEvent::new(
                        "review.cycles_exhausted",
                        Some(source_bead_id.clone()),
                        None,
                        Some(town.town_id.clone()),
                        Some(rig_id.clone()),
                        serde_json::json!({"reason": reason.clone(), "mr_bead_id": mr_bead_id.clone()}),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Disk/worktree budget guard: true when a rig has a configured worktree
    /// budget and its isolated worktrees already consume it. Blocks dispatch so
    /// a huge repo isn't materialized past the limit.
    fn rig_over_disk_budget(&self, rig_id: &str) -> bool {
        let Some(rig) = self.store.get_rig(rig_id) else { return false };
        let Some(budget_mib) = rig.worktree_disk_budget_mib else { return false };
        if budget_mib == 0 {
            return false;
        }
        let root = PathBuf::from(rig.repo_base(&self.rig_truth_root(rig_id).to_string_lossy().into_owned()));
        crate::orchestration::git::disk_budget_exceeded(&root, budget_mib)
    }

    /// Get-or-create a review-pass agent (one per rig + role). Reuses the
    /// refinery role but records its review role in the agent checkpoint so the
    /// worker returns a review decision.
    fn get_or_create_review_agent(&self, town_id: &str, rig_id: &str, role: state::ReviewPassRole) -> Result<Agent, String> {
        let name = match role {
            state::ReviewPassRole::ReviewerA => "ReviewerA",
            state::ReviewPassRole::ReviewerB => "ReviewerB",
            state::ReviewPassRole::Adversarial => "Adversarial",
            state::ReviewPassRole::Adjudicator => "Adjudicator",
        };
        // Try to reuse an idle reviewer of the same role+rig.
        if let Some(a) = self.store.list_agents().into_iter().find(|a| {
            a.role == AgentRole::Refinery
                && a.name == name
                && a.rig_id.as_deref() == Some(rig_id)
                && a.status == AgentStatus::Idle
                && a.current_hook_bead_id.is_none()
        }) {
            return Ok(a);
        }
        let agent = Agent {
            bead_id: state::gen_id(),
            role: AgentRole::Refinery,
            name: name.to_string(),
            identity: format!("{name}-{}-{}@gastown", rig_id.chars().take(8).collect::<String>(), town_id.chars().take(8).collect::<String>()),
            status: AgentStatus::Idle,
            rig_id: Some(rig_id.to_string()),
            town_id: town_id.to_string(),
            current_hook_bead_id: None,
            dispatch_attempts: 0,
            checkpoint: Some(serde_json::json!({"review_role": role.as_str()})),
            last_activity_at: Some(state::now()),
            last_event_type: None,
            last_event_at: None,
            active_tools: vec![],
            stalled_at: None,
            agent_status_message: None,
            created_at: state::now(),
        };
        self.store.put_agent(&agent);
        Ok(agent)
    }

    fn apply(
        &self,
        ctx: &ReconcileCtx,
        action: &Action,
        runner: &dyn AgentRunner,
        landed: &mut Vec<String>,
    ) -> ApplyOutcome {
        match action {
            Action::TransitionBead { bead_id, to, reason, actor, .. } => {
                if self.store.get_bead(bead_id).is_some() {
                    let becomes_closed = *to == BeadStatus::Closed;
                    self.store.update_bead_status(bead_id.clone(), *to, actor.clone(), Some(reason.clone()));
                    // A reconciler ORDERED closure (e.g. "MR merged (reconciler
                    // safety net)") is a genuine verified closure as long as the MR
                    // actually merged. Stamp a durable VERIFIED_10_10 so downstream
                    // `blocks` edges open — but nothing else (manual set_status and
                    // Failed/Open transitions never reach here).
                    if becomes_closed {
                        if self.store.is_bead_verified_current(bead_id) {
                            // already verified → leave intact
                        } else {
                            let epoch = self.store.hot_get("gastown/portability/epoch").unwrap_or_default();
                            self.store.mark_bead_verified(bead_id, &epoch, "COMPLETE", 8, 8);
                        }
                    }
                }
                ApplyOutcome::Applied
            }
            Action::ClearBeadAssignee { bead_id } => {
                if let Some(mut b) = self.store.get_bead(bead_id) {
                    b.assignee_agent_bead_id = None;
                    b.updated_at = now();
                    self.store.put_bead(&b);
                    // unhook all agents hooked to it
                    for a in self.store.list_agents() {
                        if a.current_hook_bead_id.as_deref() == Some(bead_id) {
                            self.store.unhook_agent(&a.bead_id);
                        }
                    }
                }
                ApplyOutcome::Applied
            }
            Action::AssignBead { bead_id, agent_id } => {
                if let Some(mut b) = self.store.get_bead(bead_id) {
                    b.assignee_agent_bead_id = Some(agent_id.clone());
                    b.updated_at = now();
                    self.store.put_bead(&b);
                }
                ApplyOutcome::Applied
            }
            Action::TransitionAgent { agent_id, to, reason, .. } => {
                self.store.update_agent_status(agent_id.clone(), *to, Some(reason.clone()));
                ApplyOutcome::Applied
            }
            Action::ResetAgentDispatchAttempts { agent_id } => {
                self.store.reset_agent_dispatch_attempts(agent_id.clone());
                ApplyOutcome::Applied
            }
            Action::HookAgent { agent_id, bead_id } => {
                // Reject unhookable bead types (upstream hookBead guard).
                if let Some(bead) = self.store.get_bead(bead_id) {
                    if matches!(
                        bead.bead_type,
                        BeadType::Escalation | BeadType::Convoy | BeadType::Agent | BeadType::Message
                    ) || bead.has_label("gt:triage-request")
                    {
                        return ApplyOutcome::Skipped;
                    }
                }
                self.store.hook_agent(agent_id.clone(), bead_id.clone());
                ApplyOutcome::Applied
            }
            Action::UnhookAgent { agent_id, .. } => {
                self.store.unhook_agent(agent_id);
                ApplyOutcome::Applied
            }
            Action::ClearAgentCheckpoint { agent_id } => {
                self.store.clear_checkpoint(agent_id.clone());
                ApplyOutcome::Applied
            }
            Action::DeleteAgent { agent_id, .. } => {
                self.store.reap_agent(agent_id);
                ApplyOutcome::Applied
            }
            Action::UpdateConvoyProgress { convoy_id, closed_beads } => {
                if let Some(mut cm) = self.store.get_convoy_metadata(convoy_id) {
                    cm.closed_beads = *closed_beads;
                    self.store.put_convoy_metadata(&cm);
                }
                ApplyOutcome::Applied
            }
            Action::SetConvoyReadyToLand { convoy_id } => {
                if let Some(mut b) = self.store.get_bead(convoy_id) {
                    b.metadata["ready_to_land"] = serde_json::json!(true);
                    b.updated_at = now();
                    self.store.put_bead(&b);
                }
                ApplyOutcome::Applied
            }
            Action::CloseConvoy { convoy_id } => {
                self.store.close_convoy(convoy_id.clone());
                landed.push(convoy_id.clone());
                ApplyOutcome::Applied
            }
            Action::FailConvoy { convoy_id, reason } => {
                self.store.fail_convoy(convoy_id.clone(), reason.clone());
                ApplyOutcome::Applied
            }
            Action::DispatchAgent { agent_id, bead_id, rig_id } => {
                let outcome = self.dispatch(ctx, agent_id, bead_id, rig_id, runner);
                match outcome {
                    Ok(true) => ApplyOutcome::Dispatched,
                    Ok(false) => ApplyOutcome::Skipped,
                    Err(e) => ApplyOutcome::Error(e),
                }
            }
            Action::StopAgent { agent_id, .. } => {
                // The host kills the supervised worker on the next schedule; mark
                // status stalled/idle so the reconciler doesn't re-dispatch.
                if let Some(a) = self.store.get_agent(agent_id) {
                    if a.status == AgentStatus::Working {
                        self.store.update_agent_status(agent_id.clone(), AgentStatus::Idle, None);
                    }
                }
                ApplyOutcome::Applied
            }
            Action::CreateMrBead { source_bead_id, agent_id, rig_id, branch, target_branch, pr_url, summary } => {
                self.store.submit_to_review_queue(
                    source_bead_id.clone(),
                    agent_id.clone(),
                    rig_id.clone(),
                    branch.clone(),
                    target_branch.clone(),
                    pr_url.clone(),
                    summary.clone(),
                );
                ApplyOutcome::Applied
            }
            Action::CreateLandingMr { convoy_id, rig_id, feature_branch, target_branch } => {
                self.store.create_landing_mr(convoy_id.clone(), rig_id.clone(), feature_branch.clone(), target_branch.clone());
                ApplyOutcome::Applied
            }
            Action::PollPr { bead_id, pr_url } => {
                // A no-op hook for the host to run real `gh pr view` polls via a
                // supervised worker; the host handles the async poll. Here we
                // just touch last_poll_at so the bead doesn't look stale.
                if let Some(mut b) = self.store.get_bead(bead_id) {
                    b.metadata["last_poll_at"] = serde_json::json!(now());
                    b.updated_at = now();
                    self.store.put_bead(&b);
                }
                let _ = pr_url;
                ApplyOutcome::Applied
            }
            Action::MergePr { bead_id, .. } => {
                let _ = bead_id;
                ApplyOutcome::Applied
            }
            Action::SendNudge { agent_id, message, tier } => {
                self.store.put_nudge(agent_id, message, "immediate", "urgent", &format!("reconciler:{tier}"));
                ApplyOutcome::Applied
            }
            Action::CreateTriageRequest { agent_id, .. } => {
                if let Some(a) = self.store.get_agent(agent_id) {
                    if a.role != AgentRole::Mayor && a.status == AgentStatus::Stalled {
                        self.store.update_agent_status(agent_id.clone(), AgentStatus::Idle, None);
                    }
                }
                ApplyOutcome::Applied
            }
            Action::NotifyMayor { .. } => ApplyOutcome::Applied,
            Action::EmitEvent { .. } => ApplyOutcome::Applied,
            Action::SetReviewPrUrl { bead_id, pr_url } => {
                self.store.set_review_pr_url(bead_id.clone(), pr_url.clone());
                ApplyOutcome::Applied
            }
            Action::CloseSiblingMrs { source_bead_id, exclude_mr_id } => {
                let source = source_bead_id.clone();
                let exclude = exclude_mr_id.clone();
                // Close all other non-terminal MR beads tracking the source.
                let mrs: Vec<String> = self
                    .store
                    .list_beads()
                    .into_iter()
                    .filter(|b| {
                        b.bead_type == BeadType::MergeRequest
                            && b.bead_id != exclude
                            && b.parent_bead_id.as_deref() == Some(source.as_str())
                            && !b.status.is_terminal()
                    })
                    .map(|b| b.bead_id)
                    .collect();
                for mid in mrs {
                    self.store.update_bead_status(mid, BeadStatus::Closed, "system".into(), None);
                }
                ApplyOutcome::Applied
            }
        }
    }

    /// Dispatch an agent to a bead → run the real model worker.
    fn dispatch(
        &self,
        ctx: &ReconcileCtx,
        agent_id: &str,
        bead_id: &str,
        rig_id: &str,
        runner: &dyn AgentRunner,
    ) -> Result<bool, String> {
        let Some(bead) = self.store.get_bead(bead_id) else { return Ok(false) };
        // Disk/worktree budget guard for filesystem-first rigs: if the rig has a
        // configured worktree budget and its isolated worktrees already consume it,
        // BLOCK dispatch instead of materializing another huge checkout.
        if self.rig_over_disk_budget(rig_id) {
            return Ok(false);
        }
        // Resolve (or create) the agent by role.
        let role = if bead.bead_type == BeadType::MergeRequest { AgentRole::Refinery } else { AgentRole::Polecat };
        let agent = if agent_id.is_empty() {
            self.get_or_create_agent(&ctx.town.town_id, rig_id, role)?
        } else {
            match self.store.get_agent(agent_id) {
                Some(a) => a,
                None => return Ok(false),
            }
        };
        // Hook + mark in_progress synchronously (upstream applyAction).
        self.store.hook_agent(agent.bead_id.clone(), bead.bead_id.clone());
        self.store.update_agent_status(agent.bead_id.clone(), AgentStatus::Working, None);
        if let Some(mut b) = self.store.get_bead(&bead.bead_id) {
            b.status = BeadStatus::InProgress;
            b.assignee_agent_bead_id = Some(agent.bead_id.clone());
            b.dispatch_attempts += 1;
            b.last_dispatch_attempt_at = Some(now());
            b.updated_at = now();
            self.store.put_bead(&b);
        }

        let rig = self.store.get_rig(rig_id);
        let default_branch = rig.as_ref().map(|r| r.default_branch.clone()).unwrap_or_else(|| "main".to_string());
        let rig_id_owned = rig_id.to_string();
        // For an MR being reviewed, merge the POLEcat's SOURCE branch (the one
        // that holds the real committed work), which the MR tracks. For a normal
        // issue/dispatch, derive the isolated worktree branch from the bead.
        let branch = if bead.bead_type == BeadType::MergeRequest {
            self.store
                .get_review_metadata(&bead.bead_id)
                .map(|m| m.branch.clone())
                .filter(|b| !b.is_empty())
                .unwrap_or_else(|| format!("bead/{}.feature", bead.bead_id.split('-').next().unwrap_or(bead.bead_id.as_str())))
        } else {
            format!("bead/{}.feature", bead.bead_id.split('-').next().unwrap_or(bead.bead_id.as_str()))
        };
        // The git branch to merge INTO. For a refinery reviewing a convoy-tracked
        // MR, that is the convoy's feature branch (review-then-land aggregation);
        // for a landing MR it is the rig default branch; otherwise main/default.
        let target_branch = if bead.bead_type == BeadType::MergeRequest {
            self.convoy_feature_for_mr(&bead.bead_id).unwrap_or_else(|| default_branch.clone())
        } else {
            default_branch.clone()
        };
        // isolated worktree root under the rig's truth clone.
        let truth = rig.as_ref().map(|r| self.rig_truth_root(&r.rig_id));
        let worktree_root = match truth {
            Some(t) => t.join(&branch),
            None => std::path::PathBuf::from("."),
        };
        let dc = DispatchCtx {
            town: ctx.town,
            store: &self.store,
            agent: &agent,
            bead: &bead,
            rig_id: rig_id_owned,
            default_branch,
            worktree_root,
            branch: branch.clone(),
            target_branch,
            pr_url: self.store.get_review_metadata(&bead.bead_id).and_then(|m| m.pr_url),
            review_pass_id: None,
            review_role: None,
            review_model: None,
        };
        match runner.run_agent(&dc) {
            Ok(res) => {
                if res.done {
                    // Polecat done → submit to review queue (source bead in_review).
                    // Refinery done → complete the review.
                    if role == AgentRole::Polecat {
                        self.polecat_done(&agent, &bead, &dc, res);
                    } else {
                        self.refinery_done(&agent, &bead, &dc, res);
                    }
                } else {
                    // Agent not done (needs rework / couldn't finish): rewind the
                    // bead to Open and free the agent so the reconciler retries it
                    // on a later tick (bounded by dispatch-attempt cooldown).
                    self.reopen_for_retry(bead_id, &agent.bead_id);
                }
                Ok(true)
            }
            Err(e) => {
                tracing::error!(agent=%agent.bead_id, bead=%bead.bead_id, err=%e, "gastown agent run failed");
                // Reopen so the reconciler does not dead-end on an in_progress
                // bead with no working agent (otherwise it stalls 5 min).
                self.reopen_for_retry(bead_id, &agent.bead_id);
                Ok(false)
            }
        }
    }

    /// Rewind a bead that was dispatched but not completed: set it back to Open,
    /// clear its assignee, and free the (now-idle) agent. Bounded re-dispatch is
    /// governed by the reconciler's dispatch-attempt cooldown, so a hot-spinning
    /// model that never finishes cannot burn the CPU — attempts still accrue.
    fn reopen_for_retry(&self, bead_id: &str, agent_id: &str) {
        self.store.unhook_agent(agent_id);
        self.store.update_agent_status(agent_id.to_string(), AgentStatus::Idle, None);
        if let Some(mut b) = self.store.get_bead(bead_id) {
            if !b.status.is_terminal() {
                // Rewind BOTH issue beads (for polecat rework) and merge_request
                // MR beads (for refinery rework): reopen to Open + clear the
                // assignee/hook so the reconciler retries them next dispatch
                // cycle instead of dead-ending on an in_progress MR.
                b.status = BeadStatus::Open;
                b.assignee_agent_bead_id = None;
                b.updated_at = now();
                // Record the rework so it is auditably visible.
                let n = b.metadata["rework_count"].as_u64().unwrap_or(0) + 1;
                b.metadata["rework_count"] = serde_json::json!(n);
                b.metadata["last_rework_at"] = serde_json::json!(now());
                self.store.put_bead(&b);
            }
        }
    }

    /// Polecat finished a bead → submit an MR to the review queue, source → in_review.
    fn polecat_done(&self, agent: &Agent, bead: &Bead, dc: &DispatchCtx, res: DispatchResult) {
        let rig = self.store.get_rig(&dc.rig_id);
        let default_branch = rig.as_ref().map(|r| r.default_branch.clone()).unwrap_or_else(|| "main".to_string());
        let target = self.store
            .get_review_metadata(&bead.bead_id)
            .map(|m| m.target_branch.clone())
            .unwrap_or(default_branch.clone());
        self.store.submit_to_review_queue(
            bead.bead_id.clone(),
            agent.bead_id.clone(),
            dc.rig_id.clone(),
            dc.branch.clone(),
            target,
            res.pr_url.clone(),
            res.summary.clone(),
        );
        // source → in_review
        if let Some(mut b) = self.store.get_bead(&bead.bead_id) {
            b.status = BeadStatus::InReview;
            b.updated_at = now();
            self.store.put_bead(&b);
        }
        self.store.unhook_agent(&agent.bead_id);
        self.store.update_agent_status(agent.bead_id.clone(), AgentStatus::Idle, None);
    }

    /// Refinery finished an MR review → complete the review (merged), close source.
    fn refinery_done(&self, agent: &Agent, bead: &Bead, _dc: &DispatchCtx, res: DispatchResult) {
        let source_bead_id = bead.parent_bead_id.clone();
        self.store.complete_review(bead.bead_id.clone(), res.pr_url.clone());
        if let Some(src) = source_bead_id {
            if let Some(mut b) = self.store.get_bead(&src) {
                if !b.status.is_terminal() {
                    b.status = BeadStatus::Closed;
                    b.closed_at = Some(now());
                    b.updated_at = now();
                    self.store.put_bead(&b);
                    // Verified closure: Refinery reviewed + merged the MR. Stamp a
                    // durable VERIFIED_10_10 bound to the current epoch so this
                    // release (unlike a bare manual close) unlocks downstream.
                    let epoch = self.store.hot_get("gastown/portability/epoch").unwrap_or_default();
                    self.store.mark_bead_verified(&src, &epoch, "COMPLETE", 8, 8);
                }
            }
        }
        self.store.unhook_agent(&agent.bead_id);
        self.store.update_agent_status(agent.bead_id.clone(), AgentStatus::Idle, None);
    }

    fn get_or_create_agent(&self, town_id: &str, rig_id: &str, role: AgentRole) -> Result<Agent, String> {
        if role == AgentRole::Mayor {
            let existing = self.store.list_agents().into_iter().find(|a| a.role == AgentRole::Mayor);
            if let Some(a) = existing {
                return Ok(a);
            }
        } else if role == AgentRole::Refinery {
            let existing = self
                .store
                .list_agents()
                .into_iter()
                .find(|a| a.role == AgentRole::Refinery && a.rig_id.as_deref() == Some(rig_id));
            if let Some(a) = existing {
                return Ok(a);
            }
        } else {
            let existing = self
                .store
                .list_agents()
                .into_iter()
                .find(|a| {
                    a.role == AgentRole::Polecat
                        && a.status == AgentStatus::Idle
                        && a.current_hook_bead_id.is_none()
                        && a.rig_id.as_deref() == Some(rig_id)
                });
            if let Some(a) = existing {
                return Ok(a);
            }
        }
        let name = if role == AgentRole::Mayor { "Mayor" } else if role == AgentRole::Refinery { "Refinery" } else { "Polecat" };
        let identity = format!("{name}-{}-{}@{}", role.as_str(), rig_id.chars().take(8).collect::<String>(), town_id.chars().take(8).collect::<String>());
        let agent = Agent {
            bead_id: gen_id(),
            role,
            name: name.to_string(),
            identity: identity.clone(),
            status: AgentStatus::Idle,
            rig_id: Some(rig_id.to_string()),
            town_id: town_id.to_string(),
            current_hook_bead_id: None,
            dispatch_attempts: 0,
            checkpoint: None,
            last_activity_at: Some(now()),
            last_event_type: None,
            last_event_at: None,
            active_tools: Vec::new(),
            stalled_at: None,
            agent_status_message: None,
            created_at: now(),
        };
        // Persist immediately so future dispatch/failure logic can find it.
        self.store.put_agent(&agent);
        Ok(agent)
    }

    /// Default truth-clone root for a rig's repo (host may override via env/UI).
    pub fn rig_truth_root(&self, rig_id: &str) -> std::path::PathBuf {
        let base = std::env::var_os("GASTOWN_WORK_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("LOCALAPPDATA")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join("NAI").join("Gastown").join("repos")
            });
        base.join(rig_id)
    }

    /// If an MR reviews a source bead that belongs to a convoy, return that
    /// convoy's feature branch so the refinery aggregates the polecat work there
    /// (review-then-land). Landing MRs (source = convoy) get the default branch.
    fn convoy_feature_for_mr(&self, mr_bead_id: &str) -> Option<String> {
        // The MR's source bead (via the Tracks edge).
        let source_id = self
            .store
            .list_dependencies()
            .into_iter()
            .find(|d| d.kind == DependencyKind::Tracks && d.bead_id == mr_bead_id)
            .map(|d| d.depends_on_bead_id)?;
        if let Some(source) = self.store.get_bead(&source_id) {
            if source.bead_type == BeadType::Convoy {
                // Landing MR → merge into the rig default branch (main).
                return None;
            }
            // The source bead's convoy (Tracks edge pointing TO a convoy).
            if let Some(convoy) = self
                .store
                .list_dependencies()
                .into_iter()
                .find(|d| d.kind == DependencyKind::Tracks && d.bead_id == source_id)
            {
                return self.store.get_convoy_metadata(&convoy.depends_on_bead_id).and_then(|m| m.feature_branch);
            }
        }
        None
    }
}

enum ApplyOutcome {
    Applied,
    Dispatched,
    Skipped,
    Error(String),
}

/// Dedicated convenience re-export so callers can build an orchestrator quickly.
pub fn orchestrator_for(store: OrchestrationStore) -> Orchestrator {
    Orchestrator::new(store)
}
