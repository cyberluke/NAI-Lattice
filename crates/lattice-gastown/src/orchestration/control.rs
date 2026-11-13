//! Gas Town local control API + interface (`orchestration::control`).
//!
//! A usable control surface independent of VS Code: typed JSON queries and
//! mutations over the durable orchestration state. The host exposes these over
//! a local HTTP server (and the CLI over a terminal), so an operator can create
//! towns/rigs/convoys, post user requests to the Mayor, watch bead/convoy
//! status, and inspect the review queue without any editor.

use crate::orchestration::reconciler::{self};
use crate::orchestration::state::{
    Bead, BeadStatus, BeadType, BeadPriority, DependencyKind, OrchestrationStore, Town,
    TransitionCtx, TransitionError, TransitionOrigin, TransitionResult, gen_id,
    now, resolve_rig_config,
};

/// Snapshot of a bead for the interface.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BeadView {
    pub bead_id: String,
    pub bead_type: String,
    pub status: String,
    pub title: String,
    pub body: Option<String>,
    pub rig_id: Option<String>,
    pub parent_bead_id: Option<String>,
    pub assignee_agent_bead_id: Option<String>,
    pub priority: String,
    pub labels: Vec<String>,
    pub dispatch_attempts: u32,
    pub created_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
}

impl BeadView {
    pub fn of(b: &Bead) -> Self {
        BeadView {
            bead_id: b.bead_id.clone(),
            bead_type: b.bead_type.as_str().to_string(),
            status: b.status.as_str().to_string(),
            title: b.title.clone(),
            body: b.body.clone(),
            rig_id: b.rig_id.clone(),
            parent_bead_id: b.parent_bead_id.clone(),
            assignee_agent_bead_id: b.assignee_agent_bead_id.clone(),
            priority: b.priority.as_str().to_string(),
            labels: b.labels.clone(),
            dispatch_attempts: b.dispatch_attempts,
            created_by: b.created_by.clone(),
            created_at: b.created_at.clone(),
            updated_at: b.updated_at.clone(),
            closed_at: b.closed_at.clone(),
        }
    }
}

/// The full control-surface state report (JSON serializable).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Dashboard {
    pub town_id: String,
    pub rigs: Vec<serde_json::Value>,
    pub agents: Vec<serde_json::Value>,
    pub beads: Vec<BeadView>,
    pub convoys: Vec<serde_json::Value>,
    pub review_queue: Vec<serde_json::Value>,
    pub escalations: Vec<serde_json::Value>,
pub events: Vec<serde_json::Value>,
    pub mailboxes: Vec<serde_json::Value>,
    pub triage: Vec<serde_json::Value>,
    pub molecules: Vec<serde_json::Value>,
    /// Dependency edges (blocks / tracks / parent-child) — used to render the DAG.
    pub dependencies: Vec<serde_json::Value>,
    /// All adversarial review passes across beads (role, decision, evidence verdicts).
    pub review_passes: Vec<serde_json::Value>,
    /// Live execution stream: recent events summary sorted newest-first.
    pub live_stream: Vec<serde_json::Value>,
}

/// Local control API over a store.
#[derive(Clone)]
pub struct ControlApi {
    store: OrchestrationStore,
}

impl ControlApi {
    pub fn new(store: OrchestrationStore) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &OrchestrationStore {
        &self.store
    }

    /// Full dashboard snapshot for the town described by the primary town.
    pub fn dashboard(&self, town_id: &str) -> Dashboard {
        let rigs = self.store.list_rigs_for_town(town_id);
        let rig_ids: std::collections::HashSet<String> = rigs.iter().map(|r| r.rig_id.clone()).collect();
        let agents: Vec<serde_json::Value> = self
            .store
            .list_agents()
            .into_iter()
            .map(|a| {
                serde_json::json!({
                    "bead_id": a.bead_id,
                    "role": a.role.as_str(),
                    "name": a.name,
                    "identity": a.identity,
                    "status": a.status.as_str(),
                    "rig_id": a.rig_id,
                    "current_hook_bead_id": a.current_hook_bead_id,
                    "dispatch_attempts": a.dispatch_attempts,
                    "last_activity_at": a.last_activity_at,
                    "town_id": a.town_id,
                })
            })
            .collect();
        let convoys: Vec<serde_json::Value> = self
            .store
            .list_beads()
            .into_iter()
            .filter(|b| b.bead_type == BeadType::Convoy && b.rig_id.as_deref().map(|r| rig_ids.contains(r)).unwrap_or(false))
            .map(|b| {
                let cm = self.store.get_convoy_metadata(&b.bead_id);
                serde_json::json!({
                    "bead_id": b.bead_id,
                    "title": b.title,
                    "status": b.status.as_str(),
                    "total_beads": cm.as_ref().map(|c| c.total_beads).unwrap_or(0),
                    "closed_beads": cm.as_ref().map(|c| c.closed_beads).unwrap_or(0),
                    "feature_branch": cm.as_ref().and_then(|c| c.feature_branch.clone()),
                    "merge_mode": cm.as_ref().map(|c| match c.merge_mode { crate::orchestration::state::MergeMode::ReviewThenLand => "review-then-land", crate::orchestration::state::MergeMode::ReviewAndMerge => "review-and-merge" }).unwrap_or("review-then-land"),
                    "staged": cm.as_ref().map(|c| c.staged).unwrap_or(false),
                    "ready_to_land": b.metadata["ready_to_land"].as_bool().unwrap_or(false),
                    "landed_at": cm.as_ref().and_then(|c| c.landed_at.clone()),
                })
            })
            .collect();
        let review_queue: Vec<serde_json::Value> = self
            .store
            .list_beads()
            .into_iter()
            .filter(|b| b.bead_type == BeadType::MergeRequest)
            .filter_map(|b| {
                let rm = self.store.get_review_metadata(&b.bead_id)?;
                Some(serde_json::json!({
                    "id": b.bead_id,
                    "source_bead": b.parent_bead_id,
                    "rig_id": b.rig_id,
                    "branch": rm.branch,
                    "target_branch": rm.target_branch,
                    "pr_url": rm.pr_url,
                    "status": b.status.as_str(),
                    "updated_at": b.updated_at,
                }))
            })
            .collect();
        let escalations: Vec<serde_json::Value> = self
            .store
            .list_beads()
            .into_iter()
            .filter(|b| b.bead_type == BeadType::Escalation && !b.status.is_terminal())
            .map(|b| {
                serde_json::json!({
                    "bead_id": b.bead_id,
                    "title": b.title,
                    "priority": b.priority.as_str(),
                    "created_at": b.created_at,
                    "labels": b.labels,
                })
            })
            .collect();
        let beads: Vec<BeadView> = self
            .store
            .list_beads()
            .into_iter()
            .filter(|b| {
                b.rig_id.as_deref().map(|r| rig_ids.contains(r)).unwrap_or(false)
                    && b.bead_type != BeadType::Convoy
            })
            .map(|b| BeadView::of(&b))
            .collect();
        let mut sorted = beads;
        sorted.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        // Audit timeline (most recent first) for the town (WS1 parity).
        let mut events: Vec<serde_json::Value> = self
            .store
            .events_for_town(town_id)
            .into_iter()
            .map(|e| serde_json::json!({
                "event_id": e.event_id,
                "event": e.event,
                "bead_id": e.bead_id,
                "agent_id": e.agent_id,
                "rig_id": e.rig_id,
                "data": e.data,
                "created_at": e.created_at,
            }))
            .collect();
        events.sort_by(|a, b| a["created_at"].as_str().cmp(&b["created_at"].as_str()).reverse());
        // Mailboxes: undelivered mail per agent (rig-scoped).
        let mut mailboxes: Vec<serde_json::Value> = Vec::new();
        for a in &agents {
            let a_id = a["bead_id"].as_str().unwrap_or("");
            if a_id.is_empty() {
                continue;
            }
            let undelivered: Vec<serde_json::Value> = self
                .store
                .list_undelivered_mail_for(a_id)
                .into_iter()
                .map(|m| serde_json::json!({
                    "mail_id": m.mail_id,
                    "from_agent_id": m.from_agent_id,
                    "subject": m.subject,
                    "body": m.body,
                    "kind": m.kind,
                    "created_at": m.created_at,
                }))
                .collect();
            if !undelivered.is_empty() {
                mailboxes.push(serde_json::json!({
                    "agent_id": a_id,
                    "role": a["role"],
                    "undelivered": undelivered,
                }));
            }
        }
        // Pending triage requests (patrol surface).
        let triage: Vec<serde_json::Value> = self
            .store
            .list_pending_triage_requests()
            .into_iter()
            .map(|b| serde_json::json!({
                "bead_id": b.bead_id,
                "title": b.title,
                "triage_type": b.metadata["triage_type"],
                "agent_bead_id": b.metadata["agent_bead_id"],
                "options": b.metadata["options"],
                "context": b.metadata["context"],
                "created_at": b.created_at,
            }))
            .collect();
        // Active / not-completed molecules (rig-scoped).
        let molecules: Vec<serde_json::Value> = self
            .store
            .list_beads()
            .into_iter()
            .filter(|b| b.bead_type == BeadType::Molecule && b.rig_id.as_deref().map(|r| rig_ids.contains(r)).unwrap_or(false))
            .filter_map(|b| {
                let meta = self.store.get_molecule_metadata(&b.bead_id)?;
                Some(serde_json::json!({
                    "bead_id": b.bead_id,
                    "source_bead_id": b.metadata["source_bead_id"],
                    "status": format!("{:?}", meta.status).to_lowercase(),
                    "current_step": meta.current_step,
                    "step_count": meta.formula.as_array().map(|a| a.len()).unwrap_or(0),
                    "created_at": b.created_at,
                }))
            })
            .collect();
let live_stream_values: Vec<serde_json::Value> = events.iter().rev().cloned().take(40).collect();
        Dashboard {
            town_id: town_id.to_string(),
            rigs: rigs.iter().map(|r| serde_json::json!({
                "rig_id": r.rig_id, "name": r.name, "git_url": r.git_url,
                "default_branch": r.default_branch, "created_at": r.created_at,
            })).collect(),
            agents,
            beads: sorted,
            convoys,
            review_queue,
            escalations,
            events,
            mailboxes,
            triage,
            molecules,
            dependencies: self.store.list_dependencies().into_iter().map(|d| serde_json::json!({
                "bead_id": d.bead_id, "depends_on_bead_id": d.depends_on_bead_id,
                "kind": match d.kind { DependencyKind::Blocks => "blocks", DependencyKind::Tracks => "tracks", DependencyKind::ParentChild => "parent_child" },
            })).collect(),
            review_passes: {
                let rigs_set: std::collections::HashSet<String> = rigs.iter().map(|r| r.rig_id.clone()).collect();
                let mut all: Vec<serde_json::Value> = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for b in self.store.list_beads() {
                    if b.rig_id.as_deref().map(|r| rigs_set.contains(r)).unwrap_or(false) {
                        for p in self.store.review_passes_for_source(&b.bead_id) {
                            if seen.insert(p.pass_id.clone()) {
                                all.push(serde_json::json!({
                                    "pass_id": p.pass_id, "bead": b.title, "role": p.role.as_str(),
                                    "cycle": p.cycle, "decision": p.decision.map(|d| match d { crate::orchestration::state::ReviewDecision::Approved=>"approved", crate::orchestration::state::ReviewDecision::ChangesRequested=>"changes_requested"}),
                                    "status": match p.status { crate::orchestration::state::ReviewPassStatus::Pending=>"pending", crate::orchestration::state::ReviewPassStatus::Running=>"running", crate::orchestration::state::ReviewPassStatus::Approved=>"approved", crate::orchestration::state::ReviewPassStatus::Rejected=>"rejected"},
                                    "model": p.model, "reason": p.reason,
                                    "adjudications": p.adjudications.iter().map(|a| serde_json::json!({"finding_id": a.finding_id, "verdict": a.verdict, "reason": a.reason})).collect::<Vec<_>>(),
                                }));
                            }
                        }
                    }
                }
                all
            },
            live_stream: live_stream_values,
        }
    }

    /// Create a bare owner town with defaults.
    pub fn ensure_town(&self, town_id: &str) -> Town {
        match self.store.get_town(town_id) {
            Some(t) => t,
            None => {
                let t = Town {
                    town_id: town_id.to_string(),
                    owner_user_id: None,
                    config: Default::default(),
                    created_at: now(),
                };
                self.store.put_town(&t);
                t
            }
        }
    }

    /// Register a rig (git repo) under a town.
    pub fn add_rig(
        &self,
        town_id: &str,
        name: &str,
        git_url: &str,
        default_branch: &str,
        config: serde_json::Value,
    ) -> crate::orchestration::state::Rig {
        let rig = crate::orchestration::state::Rig {
            rig_id: gen_id(),
            name: name.to_string(),
            git_url: git_url.to_string(),
            default_branch: if default_branch.is_empty() { "main".to_string() } else { default_branch.to_string() },
            town_id: town_id.to_string(),
            config,
            created_at: now(),
            working_dir: None,
            worktree_disk_budget_mib: None,
            sparse_paths: vec![],
        };
        self.store.put_rig(&rig);
        rig
    }

    /// Attach an **existing filesystem repository** as a rig without cloning.
    /// The absolute `working_dir` must be a git repo (or be attachable); the
    /// orchestrator shares its `.git` object database via isolated worktrees and
    /// never touches the repo's active/primary working tree. This is the
    /// filesystem-first rig path that supports very large repos without a second
    /// clone or materializing the whole tree in every worktree.
    pub fn attach_rig(
        &self,
        town_id: &str,
        name: &str,
        working_dir: &str,
        default_branch: &str,
        config: serde_json::Value,
    ) -> Result<crate::orchestration::state::Rig, String> {
        let dir = std::path::Path::new(working_dir).canonicalize().map_err(|e| format!("working_dir not resolvable: {e}"))?;
        let dir_str = strip_windows_extended_prefix(&dir.to_string_lossy());
        let dot_git = std::path::Path::new(&dir_str).join(".git");
        if !dot_git.exists() && !dot_git.is_file() {
            return Err(format!("{working_dir} is not a git repository (no .git at {:?})", dir));
        }
        let rig = crate::orchestration::state::Rig {
            rig_id: gen_id(),
            name: name.to_string(),
            git_url: format!("file://{dir_str}"),
            default_branch: if default_branch.is_empty() { "main".to_string() } else { default_branch.to_string() },
            town_id: town_id.to_string(),
            config,
            created_at: now(),
            working_dir: Some(dir_str),
            worktree_disk_budget_mib: None,
            sparse_paths: vec![],
        };
        self.store.put_rig(&rig);
        Ok(rig)
    }

    /// Create a standalone bead (issue) on a rig.
    pub fn create_bead(
        &self,
        town_id: &str,
        rig_id: &str,
        title: &str,
        body: Option<&str>,
        priority: Option<BeadPriority>,
        labels: Vec<String>,
        dependencies: Vec<String>,
    ) -> String {
        let _ = town_id;
        let bead = Bead::create(
            BeadType::Issue,
            title,
            body,
            priority,
            labels,
            serde_json::json!({}),
            Some(rig_id),
            None,
            Some(if town_id.is_empty() { "system" } else { "system" }),
        );
        let id = bead.bead_id.clone();
        self.store.put_bead(&bead);
        for dep in dependencies {
            self.store.put_dependency(&crate::orchestration::state::Dependency {
                bead_id: id.clone(),
                depends_on_bead_id: dep,
                kind: crate::orchestration::state::DependencyKind::Blocks,
            });
        }
        id
    }

    /// Create a convoy with beads + dependencies (the Mayor's decomposition
    /// primitive). Returns the convoy bead id.
    pub fn create_convoy(
        &self,
        town_id: &str,
        rig_id: &str,
        title: &str,
        beads: Vec<ConvoyBeadInput>,
        merge_mode: crate::orchestration::state::MergeMode,
        staged: bool,
        feature_branch: Option<String>,
    ) -> String {
        let _ = town_id;
        let convoy = Bead::create(
            BeadType::Convoy,
            title,
            None,
            Some(BeadPriority::High),
            vec!["gt:convoy".to_string()],
            serde_json::json!({}),
            Some(rig_id),
            None,
            Some("system"),
        );
        let convoy_id = convoy.bead_id.clone();
        self.store.put_bead(&convoy);
        let feature = feature_branch.unwrap_or_else(|| format!("convoy/{}", &convoy_id[..8]));
        self.store.put_convoy_metadata(&crate::orchestration::state::ConvoyMetadata {
            bead_id: convoy_id.clone(),
            total_beads: 0,
            closed_beads: 0,
            landed_at: None,
            feature_branch: Some(feature),
            merge_mode,
            staged,
        });

        // Tracked bead ids (in input order) for dependency resolution.
        let mut bead_ids: Vec<String> = Vec::new();
        for inp in &beads {
            let b = Bead::create(
                BeadType::Issue,
                &inp.title,
                inp.body.as_deref(),
                inp.priority,
                inp.labels.clone(),
                serde_json::json!({}),
                Some(rig_id),
                Some(&convoy_id),
                Some("mayor"),
            );
            let id = b.bead_id.clone();
            self.store.put_bead(&b);
            // tracks edge: bead → convoy.
            self.store.put_dependency(&crate::orchestration::state::Dependency {
                bead_id: id.clone(),
                depends_on_bead_id: convoy_id.clone(),
                kind: crate::orchestration::state::DependencyKind::Tracks,
            });
            bead_ids.push(id);
        }
        // Blocks edges by index.
        for (i, inp) in beads.iter().enumerate() {
            for dep in &inp.depends_on_index {
                if let Some(dep_id) = bead_ids.get(*dep) {
                    self.store.put_dependency(&crate::orchestration::state::Dependency {
                        bead_id: bead_ids[i].clone(),
                        depends_on_bead_id: dep_id.clone(),
                        kind: crate::orchestration::state::DependencyKind::Blocks,
                    });
                }
            }
        }
        // Update convoy metadata total.
        if let Some(mut cm) = self.store.get_convoy_metadata(&convoy_id) {
            cm.total_beads = bead_ids.len() as u32;
            self.store.put_convoy_metadata(&cm);
        }
        convoy_id
    }

    /// Dispatch a real Mayor pass to decompose a user request (host wires the
    /// runner; here we provide the in-process facade that creates the convoy).
    pub fn mayor_decompose(
        &self,
        town_id: &str,
        rig_id: &str,
        decision: &serde_json::Value,
    ) -> serde_json::Value {
        match decision["action"].as_str() {
            Some("create_convoy") => {
                let title = decision["title"].as_str().unwrap_or("Mayor convoy").to_string();
                let merge_mode = match decision["merge_mode"].as_str() {
                    Some("review-and-merge") => crate::orchestration::state::MergeMode::ReviewAndMerge,
                    _ => crate::orchestration::state::MergeMode::ReviewThenLand,
                };
                let mut bead_inputs: Vec<ConvoyBeadInput> = Vec::new();
                for b in decision["beads"].as_array().unwrap_or(&Vec::new()) {
                    bead_inputs.push(ConvoyBeadInput {
                        title: b["title"].as_str().unwrap_or("task").to_string(),
                        body: b["body"].as_str().map(str::to_string),
                        priority: Some(BeadPriority::parse(b["priority"].as_str())),
                        labels: vec![],
                        depends_on_index: b["depends_on"]
                            .as_array()
                            .unwrap_or(&Vec::new())
                            .iter()
                            .filter_map(|d| d.as_u64().map(|x| x as usize))
                            .collect(),
                    });
                }
                let id = self.create_convoy(
                    town_id,
                    rig_id,
                    &title,
                    bead_inputs,
                    merge_mode,
                    false,
                    None,
                );
                serde_json::json!({"action": "created_convoy", "convoy_id": id, "started": true})
            }
            Some("create_bead") => {
                let title = decision["title"].as_str().unwrap_or("bead").to_string();
                let body = decision["body"].as_str().map(str::to_string);
                let pr = BeadPriority::parse(decision["priority"].as_str());
                let labels = vec!["gt:held".to_string()]; // released by user
                self.create_bead(town_id, rig_id, &title, body.as_deref(), Some(pr), labels, vec![]);
                serde_json::json!({"action": "created_bead", "held": true})
            }
            _ => serde_json::json!({"action": "acknowledged", "done": true}),
        }
    }

    /// Start a staged convoy.
    pub fn start_convoy(&self, convoy_id: &str) {
        if let Some(mut cm) = self.store.get_convoy_metadata(convoy_id) {
            cm.staged = false;
            self.store.put_convoy_metadata(&cm);
        }
    }

/// Close/fail a bead from the control surface. Routes through the canonical
    /// transition boundary (Control origin) so terminal changes still require
    /// verification or an audited admin override, and cross-scope writes are
    /// rejected by ownership resolution.
    pub fn set_bead_status(&self, bead_id: &str, status: BeadStatus) -> Result<TransitionResult, TransitionError> {
        // Resolve ownership (rule 5). If the bead's rig is not attached to a
        // canonical town, we still allow the boundary to run but tag it.
        let _owner = self.store.bead_serialized(bead_id); // reads bead; ownership via rig->town below
        let ctx = TransitionCtx::new(TransitionOrigin::Control, "control").with_reason("control api");
        self.store.transition_bead(bead_id, status, &ctx)
    }

    /// CPU-free effective config for a rig (exposed for the interface).
    pub fn rig_effective(&self, town: &Town, rig_id: &str) -> serde_json::Value {
        let rig = self.store.get_rig(rig_id);
        let ec = resolve_rig_config(town, rig.as_ref());
        serde_json::json!({
            "default_model": ec.default_model,
            "role_models": {
                "mayor": ec.role_models.mayor,
                "refinery": ec.role_models.refinery,
                "polecat": ec.role_models.polecat,
            },
            "merge_strategy": match ec.merge_strategy { crate::orchestration::state::MergeStrategy::Direct => "direct", crate::orchestration::state::MergeStrategy::Pr => "pr" },
            "convoy_merge_mode": match ec.convoy_merge_mode { crate::orchestration::state::MergeMode::ReviewThenLand => "review-then-land", crate::orchestration::state::MergeMode::ReviewAndMerge => "review-and-merge" },
            "code_review": ec.code_review,
            "review_mode": match ec.review_mode { crate::orchestration::state::ReviewMode::Rework => "rework", crate::orchestration::state::ReviewMode::Comments => "comments" },
            "auto_resolve_pr_feedback": ec.auto_resolve_pr_feedback,
            "auto_resolve_merge_conflicts": ec.auto_resolve_merge_conflicts,
            "auto_merge_delay_minutes": ec.auto_merge_delay_minutes,
            "max_concurrent_polecats": ec.max_concurrent_polecats,
            "max_dispatch_attempts": ec.max_dispatch_attempts,
        })
    }
}

/// A bead within a convoy being created (with icon dependency index lists).
#[derive(Debug, Clone)]
pub struct ConvoyBeadInput {
    pub title: String,
    pub body: Option<String>,
    pub priority: Option<BeadPriority>,
    pub labels: Vec<String>,
    /// Indices (into the convoy's beads array) this bead depends on.
    pub depends_on_index: Vec<usize>,
}

/// Re-export the reconciler's helpers for the interface (status parsing etc).
pub use reconciler::parse_iso_ms as parse_iso;

/// Strip the Windows extended-length path prefix (\\?\ and \\?\UNC\) that
/// std::fs::canonicalize on Windows may prepend. Git on Windows cannot create
/// worktree/lock paths under the \\?\ VC-prefixed form, so attached-filesystem
/// rigs store a plain C:\... path instead.
fn strip_windows_extended_prefix(p: &str) -> String {
    let p = p.trim();
    if let Some(rest) = p.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{}", rest);
    }
    p.strip_prefix(r"\\?\").unwrap_or(p).to_string()
}
