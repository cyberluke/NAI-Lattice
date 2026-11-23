//! Durable Gas Town orchestration state model (`lattice-gastown::orchestration`).
//!
//! Mirrors the hosted Gas Town data model (`services/gastown/src/db/tables/`) â€”
//! beads (issues / merge_requests / convoys / agents / messages), agents +
//! agent metadata, review metadata, convoy metadata, bead dependencies, and
//! escalations â€” persisted through the NAI-Lattice hot-store (SurrealKV) under
//! a namespaced keyspace, with a monotonically advancing version value per
//! entity so the orchestrator can compute durable actions. NAI-Lattice is the
//! single data fabric; the canonical ledger records committed orchestration
//! events; this hot-state is the current mutable snapshot the reconciler drives.

use std::sync::Arc;

use nai_lattice_hotstore::{HotKey, HotStore, HotValue};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable identity generator matching upstream `generateId()` (UUID v7-ish).
pub fn gen_id() -> String {
    Uuid::now_v7().to_string()
}

/// Current time in ISO-8601 millisecond form (upstream `now()`).
pub fn now() -> String {
    // Match upstream ISO 8601 with ms + Z suffix.
    let ms = now_unix_ms();
    let secs = ms / 1000;
    let millis = ms % 1000;
    // Format as UTC ISO 8601 (no chrono dependency needed at call site).
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, mo, d) = civil_from_days(days);
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;
    let ss = rem % 60;
    format!("{y:04}-{mo:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
}

pub fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Convert a day count since 1970-01-01 to a (year, month, day) civil date
/// (proleptic Gregorian, Howard Hinnant's algorithm).
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
// Entity types (faithful to upstream zod schemas / DB columns)
// â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Bead type (`services/gastown/src/types.ts` `BeadType`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BeadType {
    Issue,
    Message,
    Escalation,
    MergeRequest,
    Convoy,
    Molecule,
    Agent,
}

impl BeadType {
    pub fn as_str(&self) -> &'static str {
        match self {
            BeadType::Issue => "issue",
            BeadType::Message => "message",
            BeadType::Escalation => "escalation",
            BeadType::MergeRequest => "merge_request",
            BeadType::Convoy => "convoy",
            BeadType::Molecule => "molecule",
            BeadType::Agent => "agent",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "message" => BeadType::Message,
            "escalation" => BeadType::Escalation,
            "merge_request" => BeadType::MergeRequest,
            "convoy" => BeadType::Convoy,
            "molecule" => BeadType::Molecule,
            "agent" => BeadType::Agent,
            _ => BeadType::Issue,
        }
    }
}

/// Bead status (`src/types.ts` `BeadStatus`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BeadStatus {
    Open,
    InProgress,
    InReview,
    Closed,
    Failed,
}

impl BeadStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            BeadStatus::Open => "open",
            BeadStatus::InProgress => "in_progress",
            BeadStatus::InReview => "in_review",
            BeadStatus::Closed => "closed",
            BeadStatus::Failed => "failed",
        }
    }
    pub fn is_terminal(&self) -> bool {
        matches!(self, BeadStatus::Closed | BeadStatus::Failed)
    }
}

/// Bead priority (`src/types.ts` `BeadPriority`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BeadPriority {
    Low,
    Medium,
    High,
    Critical,
}

impl Default for BeadPriority {
    fn default() -> Self {
        BeadPriority::Medium
    }
}

impl BeadPriority {
    pub fn as_str(&self) -> &'static str {
        match self {
            BeadPriority::Low => "low",
            BeadPriority::Medium => "medium",
            BeadPriority::High => "high",
            BeadPriority::Critical => "critical",
        }
    }
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("low") => BeadPriority::Low,
            Some("high") => BeadPriority::High,
            Some("critical") => BeadPriority::Critical,
            _ => BeadPriority::Medium,
        }
    }
}

/// Agent role.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Polecat,
    Refinery,
    Mayor,
}

impl AgentRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentRole::Polecat => "polecat",
            AgentRole::Refinery => "refinery",
            AgentRole::Mayor => "mayor",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "refinery" => AgentRole::Refinery,
            "mayor" => AgentRole::Mayor,
            _ => AgentRole::Polecat,
        }
    }
}

/// Agent status.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Working,
    Waiting,
    Stalled,
    Dead,
}

impl AgentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Working => "working",
            AgentStatus::Waiting => "waiting",
            AgentStatus::Stalled => "stalled",
            AgentStatus::Dead => "dead",
        }
    }
}

/// Dependency type.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DependencyKind {
    Blocks,
    Tracks,
    ParentChild,
}

impl DependencyKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            DependencyKind::Blocks => "blocks",
            DependencyKind::Tracks => "tracks",
            DependencyKind::ParentChild => "parent-child",
        }
    }
}

/// A bead (work item / PR review request / convoy / agent / message).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bead {
    pub bead_id: String,
    pub bead_type: BeadType,
    pub status: BeadStatus,
    pub title: String,
    pub body: Option<String>,
    pub rig_id: Option<String>,
    pub parent_bead_id: Option<String>,
    pub assignee_agent_bead_id: Option<String>,
    pub priority: BeadPriority,
    pub labels: Vec<String>,
    pub metadata: serde_json::Value,
    pub dispatch_attempts: u32,
    pub last_dispatch_attempt_at: Option<String>,
    pub created_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
}

impl Bead {
    pub fn create(
        bead_type: BeadType,
        title: &str,
        body: Option<&str>,
        priority: Option<BeadPriority>,
        labels: Vec<String>,
        metadata: serde_json::Value,
        rig_id: Option<&str>,
        parent_bead_id: Option<&str>,
        created_by: Option<&str>,
    ) -> Self {
        let ts = now();
        Bead {
            bead_id: gen_id(),
            bead_type,
            status: BeadStatus::Open,
            title: title.to_string(),
            body: body.map(str::to_string),
            rig_id: rig_id.map(str::to_string),
            parent_bead_id: parent_bead_id.map(str::to_string),
            assignee_agent_bead_id: None,
            priority: priority.unwrap_or_default(),
            labels,
            metadata,
            dispatch_attempts: 0,
            last_dispatch_attempt_at: None,
            created_by: created_by.map(str::to_string),
            created_at: ts.clone(),
            updated_at: ts,
            closed_at: None,
        }
    }
    pub fn has_label(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }
}

/// Agent metadata joined to an `Agent` bead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub bead_id: String,
    pub role: AgentRole,
    pub name: String,
    pub identity: String,
    pub status: AgentStatus,
    pub rig_id: Option<String>,
    pub town_id: String,
    pub current_hook_bead_id: Option<String>,
    pub dispatch_attempts: u32,
    pub checkpoint: Option<serde_json::Value>,
    pub last_activity_at: Option<String>,
    pub last_event_type: Option<String>,
    pub last_event_at: Option<String>,
    pub active_tools: Vec<String>,
    pub stalled_at: Option<String>,
    pub agent_status_message: Option<String>,
    pub created_at: String,
}

/// Review metadata for a merge_request bead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewMetadata {
    pub bead_id: String,
    pub branch: String,
    pub target_branch: String,
    pub merge_commit: Option<String>,
    pub pr_url: Option<String>,
    pub retry_count: u32,
    pub auto_merge_ready_since: Option<String>,
    pub last_feedback_check_at: Option<String>,
}

/// Convoy metadata for a convoy bead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConvoyMetadata {
    pub bead_id: String,
    pub total_beads: u32,
    pub closed_beads: u32,
    pub landed_at: Option<String>,
    pub feature_branch: Option<String>,
    pub merge_mode: MergeMode,
    pub staged: bool,
}

/// Convoy merge mode (`src/db/tables/convoy-metadata.table.ts`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum MergeMode {
    ReviewThenLand,
    ReviewAndMerge,
}

impl MergeMode {
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("review-and-merge") => MergeMode::ReviewAndMerge,
            _ => MergeMode::ReviewThenLand,
        }
    }
}

/// A bead dependency edge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dependency {
    pub bead_id: String,
    pub depends_on_bead_id: String,
    pub kind: DependencyKind,
}

/// Result of `validate_dependency_topology`: the graph is valid iff it is a
/// clean DAG (no self-deps, no duplicates, no nonexistent references, no cycles).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DependencyTopologyReport {
    pub edges: usize,
    pub duplicates: usize,
    pub issues: Vec<String>,
    pub valid: bool,
}

/// Escalation metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Escalation {
    pub bead_id: String,
    pub severity: String,
    pub category: Option<String>,
    pub acknowledged: bool,
    pub re_escalation_count: u32,
    pub acknowledged_at: Option<String>,
}

/// A rig (git repository attached to a town).
///
/// A rig is either a remote `git_url` the orchestrator clones into an isolated
/// truth root, or a **filesystem-attached** repository (`working_dir`): an
/// already-present absolute path on this host whose `.git` object database is
/// shared. In attach mode the primary/active working tree is never touched;
/// polecats only ever get isolated `git worktree` checkouts of that repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rig {
    pub rig_id: String,
    pub name: String,
    pub git_url: String,
    pub default_branch: String,
    pub town_id: String,
    pub config: serde_json::Value, // RigOverrideConfig JSON
    pub created_at: String,
    /// Filesystem-attach root (absolute path to an existing repo). When set,
    /// `git_url` is informational and the orchestrator attaches instead of
    /// cloning.
    pub working_dir: Option<String>,
    /// Disk budget in MiB for attached-rig worktrees (soft cap; cleanup policy).
    pub worktree_disk_budget_mib: Option<u64>,
    /// Sparse-checkout pathspec (list of directory/file paths) to restrict
    /// materialization inside isolated worktrees for very large repos.
    pub sparse_paths: Vec<String>,
}

impl Rig {
    /// True when this rig points at an existing filesystem repo to attach.
    pub fn is_attached(&self) -> bool {
        self.working_dir.as_deref().map(|p| !p.trim().is_empty()).unwrap_or(false)
    }

    /// The directory the orchestrator uses as the repo base: the attach root in
    /// attach mode, or the git URL string otherwise (caller decides semantics).
    pub fn repo_base(&self, fallback: &str) -> String {
        if self.is_attached() {
            self.working_dir.clone().unwrap_or_else(|| fallback.to_string())
        } else {
            fallback.to_string()
        }
    }
}

/// A town (owner-scoped orchestration unit).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Town {
    pub town_id: String,
    pub owner_user_id: Option<String>,
    pub config: TownConfig,
    pub created_at: String,
}

/// Town configuration (`src/types.ts` `TownConfigSchema`). Only the fields the
/// orchestrator consumes with faithful defaults/fallbacks; unneeded hosted-only
/// bits (billing, container) are omitted because NAI-Lattice has no container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TownConfig {
    pub default_model: Option<String>,
    pub role_models: RoleModels,
    pub small_model: Option<String>,
    pub max_polecats_per_rig: Option<u32>,
    pub merge_strategy: MergeStrategy,
    pub convoy_merge_mode: MergeMode,
    pub staged_convoys_default: bool,
    pub refinery: RefineryConfig,
    pub custom_instructions: CustomInstructions,
    pub env_vars: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RoleModels {
    pub mayor: Option<String>,
    pub refinery: Option<String>,
    pub polecat: Option<String>,
    /// Reviewer A (code-review pass) â€” can be routed to a different family than
    /// the implementing polecat (e.g. Codex) by configuring this model.
    pub reviewer_a: Option<String>,
    /// Reviewer B (independent second code-review pass, a different family when
    /// cross-model review is configured).
    pub reviewer_b: Option<String>,
    /// Adversarial reviewer â€” a strong, independent model looking for defects.
    pub adversarial: Option<String>,
    /// Adjudicator â€” validates evidence (review_evidence contract) before a
    /// pass is accepted.
    pub adjudicator: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeStrategy {
    Direct,
    Pr,
}

impl MergeStrategy {
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("pr") => MergeStrategy::Pr,
            _ => MergeStrategy::Direct,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefineryConfig {
    pub gates: Vec<String>,
    pub auto_merge: bool,
    pub require_clean_merge: bool,
    pub code_review: bool,
    pub review_mode: ReviewMode,
    pub auto_resolve_pr_feedback: bool,
    pub auto_resolve_merge_conflicts: bool,
    pub auto_merge_delay_minutes: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewMode {
    Rework,
    Comments,
}

impl Default for TownConfig {
    fn default() -> Self {
        TownConfig {
            default_model: None,
            role_models: RoleModels::default(),
            small_model: None,
            max_polecats_per_rig: None,
            merge_strategy: MergeStrategy::Direct,
            convoy_merge_mode: MergeMode::ReviewThenLand,
            staged_convoys_default: false,
            refinery: RefineryConfig {
                gates: Vec::new(),
                auto_merge: true,
                require_clean_merge: true,
                code_review: true,
                review_mode: ReviewMode::Rework,
                auto_resolve_pr_feedback: false,
                auto_resolve_merge_conflicts: true,
                auto_merge_delay_minutes: None,
            },
            custom_instructions: CustomInstructions::default(),
            env_vars: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CustomInstructions {
    pub polecat: Option<String>,
    pub refinery: Option<String>,
    pub mayor: Option<String>,
}

/// A review-queue entry (merge_request bead + its metadata as presented to UI).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewQueueEntry {
    pub id: String,
    pub agent_id: String,
    pub source_bead_id: Option<String>,
    pub rig_id: Option<String>,
    pub branch: String,
    pub target_branch: String,
    pub pr_url: Option<String>,
    pub status: ReviewStatus,
    pub summary: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub processed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Pending,
    Running,
    Merged,
    Failed,
}

/// A durable nudge (upstream `agent_nudges`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Nudge {
    pub nudge_id: String,
    pub agent_bead_id: String,
    pub message: String,
    pub mode: String,
    pub priority: String,
    pub source: String,
    pub created_at: String,
    pub delivered_at: Option<String>,
}

/// A convoy view (convoy bead + metadata).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Convoy {
    pub bead_id: String,
    pub title: String,
    pub status: BeadStatus,
    pub staged: bool,
    pub total_beads: u32,
    pub closed_beads: u32,
    pub created_by: Option<String>,
    pub created_at: String,
    pub landed_at: Option<String>,
    pub feature_branch: Option<String>,
    pub merge_mode: MergeMode,
    pub metadata: serde_json::Value,
}

/// A durable orchestration event (upstream `bead_events` append-only log).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeadEvent {
    pub event_id: String,
    pub event: String, // e.g. 'bead.status_changed', 'agent.dispatch', 'convoy.started'
    pub bead_id: Option<String>,
    pub agent_id: Option<String>,
    pub town_id: Option<String>,
    pub rig_id: Option<String>,
    pub data: serde_json::Value,
    pub created_at: String,
}

impl BeadEvent {
    pub fn new(
        event: &str,
        bead_id: Option<String>,
        agent_id: Option<String>,
        town_id: Option<String>,
        rig_id: Option<String>,
        data: serde_json::Value,
    ) -> Self {
        BeadEvent {
            event_id: gen_id(),
            event: event.to_string(),
            bead_id,
            agent_id,
            town_id,
            rig_id,
            data,
            created_at: now(),
        }
    }
}

/// A durable mail message in an agent's mailbox (upstream `agent_mail`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mail {
    pub mail_id: String,
    pub agent_bead_id: String,
    pub from_agent_id: Option<String>,
    pub subject: String,
    pub body: String,
    pub kind: String, // e.g. 'REWORK_REQUEST', 'nudge', 'manual'
    pub delivered: bool,
    pub created_at: String,
    pub delivered_at: Option<String>,
}

/// Static inputs for a patrol triage request (upstream `createTriageRequest`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageRequestStat {
    pub triage_type: TriageType,
    pub agent_bead_id: Option<String>,
    pub hooked_bead_id: Option<String>,
    pub title: String,
    pub context: serde_json::Value,
    pub rig_id: Option<String>,
}

/// Triage request kind (upstream `TriageType`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TriageType {
    DirtyPolecat,
    StuckAgent,
    HelpRequest,
    ZombieConfirm,
    CrashLoop,
    Escalation,
}

impl TriageType {
    pub fn as_str(&self) -> &'static str {
        match self {
            TriageType::DirtyPolecat => "dirty_polecat",
            TriageType::StuckAgent => "stuck_agent",
            TriageType::HelpRequest => "help_request",
            TriageType::ZombieConfirm => "zombie_confirm",
            TriageType::CrashLoop => "crash_loop",
            TriageType::Escalation => "escalation",
        }
    }
}

/// Molecule metadata (upstream `Molecule`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoleculeMetadata {
    pub bead_id: String,
    pub current_step: u32,
    pub status: MoleculeStatus,
    pub formula: serde_json::Value,
}

impl Default for MoleculeMetadata {
    fn default() -> Self {
        MoleculeMetadata {
            bead_id: String::new(),
            current_step: 0,
            status: MoleculeStatus::Active,
            formula: serde_json::json!([]),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MoleculeStatus {
    Active,
    Completed,
    Failed,
}

/// Parse an ISO-8601 millisecond UTC timestamp to unix millis (best-effort).
pub fn parse_iso_ms(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z')?;
    let date = s.get(0..10)?;
    let time = s.get(11..23).unwrap_or("00:00:00.000");
    let millis = time
        .split('.')
        .nth(1)
        .map(|m| m.chars().take(3).collect::<String>())
        .and_then(|m| m.parse::<i64>().ok())
        .unwrap_or(0);
    let (y, mo, d) = {
        let mut parts = date.split('-');
        let y: i64 = parts.next()?.parse().ok()?;
        let mo: i64 = parts.next()?.parse().ok()?;
        let d: i64 = parts.next()?.parse().ok()?;
        (y, mo, d)
    };
    let (hh, mm, ss) = {
        let mut parts = time.split(':');
        let hh: i64 = parts.next()?.parse().ok()?;
        let mm: i64 = parts.next()?.parse().ok()?;
        // Seconds include ".mmm" â€” take only the integer part.
        let sec_field = parts.next()?;
        let ss: i64 = sec_field.split('.').next()?.parse().ok()?;
        (hh, mm, ss)
    };
    // Days since epoch for civil date.
    let days = days_from_civil(y, mo, d);
    Some((days * 86400 + hh * 3600 + mm * 60 + ss) * 1000 + millis)
}

/// Days from civil (Howard Hinnant) â€” inverse of `civil_from_days`.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0,399]
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Next severity in the escalation ladder (lowâ†’mediumâ†’highâ†’critical).
fn next_severity(cur: &str, order: &[&str; 4]) -> &'static str {
    let idx = order.iter().position(|&s| s == cur).unwrap_or(0);
    match idx {
        0 => "medium",
        1 => "high",
        _ => "critical", // highâ†’critical; critical stays critical
    }
}

/// A review pass in the per-bead adversarial review chain (WS5).
///
/// When a Polecat completes a bead (MR submitted), the engine runs a bounded
/// chain of independent review passes before the change is accepted:
///   ReviewerA â†’ ReviewerB (adversarial) â†’ Adjudicator
/// Each pass is dispatched to a review agent whose model resolves through the
/// per-role registry (`RoleModels.reviewer_a/reviewer_b/adversarial/adjudicator`)
/// â€” enabling cross-model review (e.g. Reviewer A on one family, Reviewer B on
/// another, the Adjudicator validating evidence).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewPass {
    pub pass_id: String,
    /// The issue bead being reviewed (the source of the MR).
    pub source_bead_id: String,
    /// The MR bead the pass reviews.
    pub mr_bead_id: String,
    pub role: ReviewPassRole,
    /// 1-based review cycle this pass belongs to.
    pub cycle: u32,
    pub status: ReviewPassStatus,
    pub model: Option<String>,
    pub decision: Option<ReviewDecision>,
    /// Persistent reason for this pass's decision (audit / adjudication rationale).
    pub reason: Option<String>,
    /// Reviewer findings submitted by the pass (each carries CitedEvidence).
    pub findings: Vec<Finding>,
    /// Adjudicator verdicts per finding (the evidence ground-truth).
    pub adjudications: Vec<Adjudication>,
    pub agent_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPassRole {
    ReviewerA,
    ReviewerB,
    Adversarial,
    Adjudicator,
}

impl ReviewPassRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReviewPassRole::ReviewerA => "reviewer_a",
            ReviewPassRole::ReviewerB => "reviewer_b",
            ReviewPassRole::Adversarial => "adversarial",
            ReviewPassRole::Adjudicator => "adjudicator",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPassStatus {
    Pending,
    Running,
    Approved,
    Rejected,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
}

impl ReviewDecision {
    pub fn parse(s: &str) -> Self {
        match s {
            "approved" => ReviewDecision::Approved,
            _ => ReviewDecision::ChangesRequested,
        }
    }
}

/// A single reviewer finding, always attached to concrete source evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub finding_id: String,
    pub severity: String, // 'critical' | 'major' | 'minor'
    pub message: String,
    /// The file/line/snippet the finding is anchored to (for evidence checks).
    pub evidence: Option<crate::review_evidence::CitedEvidence>,
}

/// Adjudicator verdict for one finding: does the evidence exist & match?
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Adjudication {
    pub finding_id: String,
    pub verdict: String, // 'verified' | 'unverified' | 'hallucinated'
    pub reason: String,
}

/// A durable review-cycle / corrective-instruction record stored on the bead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewCycleState {
    pub cycles_used: u32,
    pub max_cycles: u32,
    pub corrected_instructions: Vec<String>,
    pub last_reject_reason: Option<String>,
    pub satisfied: bool,
}

// â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
// Durable store over the hot-store (SurrealKV) with a per-prefix index list.
// â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Namespaced, typed key-value store for orchestration state.
#[derive(Clone)]
pub struct OrchestrationStore {
    hot: Arc<dyn HotStore>,
}

impl OrchestrationStore {
    pub fn new(hot: Arc<dyn HotStore>) -> Self {
        Self { hot }
    }

    fn key(segments: &[&str]) -> HotKey {
        let s = segments.join("/");
        HotKey(s.into_bytes())
    }

    fn idx_key(prefix: &str) -> HotKey {
        HotKey(format!("helldesk/idx/{prefix}").into_bytes())
    }

    // -- generic string JSON persistence --
    fn get_json<T: for<'de> Deserialize<'de>>(&self, key: &HotKey) -> Option<T> {
        match self.hot.get(key) {
            Ok(Some(v)) => serde_json::from_slice(&v.0).ok(),
            _ => None,
        }
    }
    fn put_json<T: Serialize>(&self, key: &HotKey, val: &T) {
        if let Ok(bytes) = serde_json::to_vec(val) {
            let _ = self.hot.put(key, &HotValue(bytes));
        }
    }

    // -- towns --
    pub fn put_town(&self, town: &Town) {
        self.put_json(&Self::key(&["helldesk/department", &town.town_id]), town);
        self.add_idx("town", &town.town_id);
    }
    pub fn get_town(&self, town_id: &str) -> Option<Town> {
        self.get_json(&Self::key(&["helldesk/department", town_id]))
    }
    pub fn list_towns(&self) -> Vec<Town> {
        self.list_idx("town")
            .into_iter()
            .filter_map(|id| self.get_town(&id))
            .collect()
    }
    pub fn delete_town(&self, town_id: &str) {
        let _ = self.hot.delete(&Self::key(&["helldesk/department", town_id]));
        self.del_idx("town", town_id);
        // Cascade rigs + beads.
        let rigs = self.list_rigs_for_town(town_id);
        for rig in rigs {
            self.delete_rig(&rig.rig_id);
        }
        // Beads without a rig (owner town beads).
        for bead in self.list_beads_for_town(town_id) {
            if bead.rig_id.is_none() {
                self.delete_bead(&bead.bead_id);
            }
        }
    }

    // -- rigs --
    pub fn put_rig(&self, rig: &Rig) {
        self.put_json(&Self::key(&["helldesk/workstation", &rig.rig_id]), rig);
        self.add_idx("rig", &rig.rig_id);
        self.add_idx(&format!("rig_town/{}", rig.town_id), &rig.rig_id);
    }
    pub fn get_rig(&self, rig_id: &str) -> Option<Rig> {
        self.get_json(&Self::key(&["helldesk/workstation", rig_id]))
    }
    pub fn list_rigs(&self) -> Vec<Rig> {
        self.list_idx("rig")
            .into_iter()
            .filter_map(|id| self.get_rig(&id))
            .collect()
    }
    pub fn list_rigs_for_town(&self, town_id: &str) -> Vec<Rig> {
        self.list_idx(&format!("rig_town/{town_id}"))
            .into_iter()
            .filter_map(|id| self.get_rig(&id))
            .collect()
    }
    pub fn delete_rig(&self, rig_id: &str) {
        if let Some(rig) = self.get_rig(rig_id) {
            self.del_idx(&format!("rig_town/{}", rig.town_id), rig_id);
            let _ = self.hot.delete(&Self::key(&["helldesk/workstation", rig_id]));
            self.del_idx("rig", rig_id);
        }
    }

    // -- agents --
    pub fn put_agent(&self, agent: &Agent) {
        self.put_json(&Self::key(&["helldesk/agent", &agent.bead_id]), agent);
        self.add_idx("agent", &agent.bead_id);
    }
    pub fn get_agent(&self, agent_id: &str) -> Option<Agent> {
        self.get_json(&Self::key(&["helldesk/agent", agent_id]))
    }
    pub fn list_agents(&self) -> Vec<Agent> {
        self.list_idx("agent")
            .into_iter()
            .filter_map(|id| self.get_agent(&id))
            .collect()
    }
    pub fn list_agents_for_rig(&self, rig_id: &str) -> Vec<Agent> {
        self.list_agents()
            .into_iter()
            .filter(|a| a.rig_id.as_deref() == Some(rig_id))
            .collect()
    }
    pub fn delete_agent(&self, agent_id: &str) {
        let _ = self.hot.delete(&Self::key(&["helldesk/agent", agent_id]));
        self.del_idx("agent", agent_id);
        let _ = self.hot.delete(&Self::key(&["helldesk/agent_ticket", agent_id]));
    }

    // -- beads --
    pub fn put_bead(&self, bead: &Bead) {
        self.put_json(&Self::key(&["helldesk/ticket", &bead.bead_id]), bead);
        self.add_idx("bead", &bead.bead_id);
    }
    pub fn get_bead(&self, bead_id: &str) -> Option<Bead> {
        self.get_json(&Self::key(&["helldesk/ticket", bead_id]))
    }
    /// Raw serialized bytes of a bead's persisted record, used as the optimistic
    /// concurrency token for the atomic CAS transition boundary (rule: expected-
    /// version CAS at the physical persistence boundary).
    pub fn bead_serialized(&self, bead_id: &str) -> Option<Vec<u8>> {
        self.hot
            .get(&Self::key(&["helldesk/ticket", bead_id]))
            .ok()
            .flatten()
            .map(|v| v.0)
    }
    /// Atomic compare-and-swap of a bead record. `expected` is the exact prior
    /// serialized value (the optimistic token); on mismatch returns Ok(false)
    /// and writes nothing; on a backend write-conflict returns Err.
    /// This is the ONLY persistence path that may implement the rule-4 CAS;
    /// callers requiring atomic transition integrity MUST go through it.
    pub fn put_bead_cas(
        &self,
        bead: &Bead,
        expected: Option<&[u8]>,
    ) -> Result<bool, HotStoreErrorLike> {
        let json = serde_json::to_vec(bead).map_err(|e| HotStoreErrorLike(e.to_string()))?;
        let key = Self::key(&["helldesk/ticket", &bead.bead_id]);
        let expected_hv = expected.map(|b| HotValue(b.to_vec()));
        let applied = self
            .hot
            .compare_and_swap(&key, expected_hv.as_ref(), &HotValue(json))
            .map_err(|e| HotStoreErrorLike(e.to_string()))?;
        if applied {
            // Index book-keeping happens only after a successful atomic swap.
            self.add_idx("bead", &bead.bead_id);
        }
        Ok(applied)
    }
    /// Durable, idempotent stamp recording that a bead achieved the Gas Town
    /// Inquisitor's VERIFIED_10_10: verdict = COMPLETE, all deterministic task
    /// gates PASS, validated against a specific source revision. Subsequent
    /// wasted stamps on the same revision are no-ops; only a changed epoch
    /// invalidates prior verification.
    pub fn mark_bead_verified(&self, bead_id: &str, source_revision: &str, verdict: &str, gates_passed: u64, gates_total: u64) {
        let Some(mut b) = self.get_bead(bead_id) else { return };
        let cur = b.metadata.get("gastown_verified");
        // Idempotent: if already verified against the SAME revision, keep it; do
        // not regress or create a duplicate. A higher/hdr revision re-stamp wins.
        if let Some(serde_json::Value::Object(m)) = cur {
            if m.get("source_revision").and_then(|r| r.as_str()) == Some(source_revision)
                && m.get("gates_pass").and_then(|v| v.as_bool()) == Some(true) {
                return;
            }
        }
        b.metadata["gastown_verified"] = serde_json::json!({
            "inquisitor": verdict,
            "gates_pass": gates_passed == gates_total && gates_total > 0,
            "gates_passed": gates_passed,
            "gates_total": gates_total,
            "source_revision": source_revision,
            "verified_at": now(),
        });
        b.updated_at = now();
        self.put_bead(&b);
    }
    /// True only when the bead's durable VERIFIED_10_10 stamp is present AND its
    /// source_revision still matches the current portability epoch. A manually
    /// closed bead (no stamp), a Failed bead, or a stamp from a superseded
    /// revision all return false â†’ they do NOT release a downstream `blocks` gate.
    pub fn is_bead_verified_current(&self, bead_id: &str) -> bool {
        let Some(b) = self.get_bead(bead_id) else { return false };
        let Some(serde_json::Value::Object(m)) = b.metadata.get("gastown_verified") else { return false };
        let rev = m.get("source_revision").and_then(|r| r.as_str()).unwrap_or("");
        let ok = m.get("inquisitor").and_then(|v| v.as_str()) == Some("COMPLETE")
            && m.get("gates_pass").and_then(|v| v.as_bool()) == Some(true)
            && rev == self.hot_get("helldesk/portability/epoch").unwrap_or_default();
        ok && b.status.is_terminal()
    }
    /// Like `is_bead_verified_current` but WITHOUT the terminal-status
    /// requirement. Used by the transition boundary so a scheduler may stamp
    /// verification first, then close â€” the stamp is the proof, and the terminal
    /// transition is its consequence. Returns true only for a current, gated-pass
    /// stamp whose revision matches the active epoch.
    pub fn has_verified_stamp_current(&self, bead_id: &str) -> bool {
        let Some(b) = self.get_bead(bead_id) else { return false };
        let Some(serde_json::Value::Object(m)) = b.metadata.get("gastown_verified") else { return false };
        let rev = m.get("source_revision").and_then(|r| r.as_str()).unwrap_or("");
        m.get("inquisitor").and_then(|v| v.as_str()) == Some("COMPLETE")
            && m.get("gates_pass").and_then(|v| v.as_bool()) == Some(true)
            && rev == self.hot_get("helldesk/portability/epoch").unwrap_or_default()
    }
    pub fn list_beads(&self) -> Vec<Bead> {
        self.list_idx("bead")
            .into_iter()
            .filter_map(|id| self.get_bead(&id))
            .collect()
    }
    pub fn list_beads_for_town(&self, town_id: &str) -> Vec<Bead> {
        // Town-scoped scan: iterate all beads (bounded; fine at this scale).
        self.list_beads_for_rig_set(town_id)
    }
    fn list_beads_for_rig_set(&self, town_id: &str) -> Vec<Bead> {
        let rigs: std::collections::HashSet<String> = self
            .list_rigs_for_town(town_id)
            .into_iter()
            .map(|r| r.rig_id)
            .collect();
        self.list_beads()
            .into_iter()
            .filter(|b| b.rig_id.as_deref().map(|r| rigs.contains(r)).unwrap_or(false))
            .collect()
    }
    pub fn list_beads_for_rig(&self, rig_id: &str) -> Vec<Bead> {
        self.list_beads()
            .into_iter()
            .filter(|b| b.rig_id.as_deref() == Some(rig_id))
            .collect()
    }
    pub fn delete_bead(&self, bead_id: &str) {
        // Remove as dependency edge both directions.
        for dep in self.list_dependencies() {
            if dep.bead_id == bead_id || dep.depends_on_bead_id == bead_id {
                let _ = self.hot.delete(&Self::key(&[
                    "helldesk/dependency",
                    &dep.bead_id,
                    &dep.depends_on_bead_id,
                ]));
            }
        }
        let _ = self.hot.delete(&Self::key(&["helldesk/ticket", bead_id]));
        let _ = self.hot.delete(&Self::key(&["helldesk/reviewmeta", bead_id]));
        let _ = self.hot.delete(&Self::key(&["helldesk/batchmeta", bead_id]));
        let _ = self.hot.delete(&Self::key(&["helldesk/escalation", bead_id]));
        self.del_idx("bead", bead_id);
    }

    // -- dependencies --
    pub fn put_dependency(&self, dep: &Dependency) {
        self.put_json(
            &Self::key(&["helldesk/dependency", &dep.bead_id, &dep.depends_on_bead_id]),
            dep,
        );
    }
    pub fn delete_dependency(&self, bead_id: &str, depends_on_bead_id: &str) {
        let _ = self.hot.delete(&Self::key(&["helldesk/dependency", bead_id, depends_on_bead_id]));
    }
    pub fn list_dependencies(&self) -> Vec<Dependency> {
        self.scan_json::<Dependency>("helldesk/dependency/")
    }
    pub fn dependencies_for(&self, bead_id: &str) -> Vec<Dependency> {
        self.list_dependencies()
            .into_iter()
            .filter(|d| d.bead_id == bead_id)
            .collect()
    }
    pub fn dependents_on(&self, depends_on_bead_id: &str) -> Vec<Dependency> {
        self.list_dependencies()
            .into_iter()
            .filter(|d| d.depends_on_bead_id == depends_on_bead_id)
            .collect()
    }

    // â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    // Dependency DAG topology validation
    // â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    /// Validate the whole dependency graph and return a detailed report.
    /// Rejects (flags) self-dependencies, duplicate edges, references to
    /// nonexistent beads, and cycles. Returns `true` only when the graph is a
    /// clean DAG with all invariants satisfied.
    pub fn validate_dependency_topology(&self) -> DependencyTopologyReport {
        let all_beads: std::collections::HashSet<String> =
            self.list_beads().into_iter().map(|b| b.bead_id).collect();
        let deps = self.list_dependencies();
        let mut issues: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // 1) Self-dependencies.
        for d in &deps {
            if d.bead_id == d.depends_on_bead_id {
                issues.push(format!("self-dependency {} â†’ itself", d.bead_id));
            }
            // 2) Duplicate edges (same (depends, depends_on) pair).
            let pair = (d.bead_id.clone(), d.depends_on_bead_id.clone());
            if !seen.insert(pair) {
                issues.push(format!("duplicate edge {} â†’ {}", d.bead_id, d.depends_on_bead_id));
            }
            // 3) Nonexistent bead references.
            if !all_beads.contains(&d.bead_id) {
                issues.push(format!("edge references nonexistent bead {}", d.bead_id));
            }
            if !all_beads.contains(&d.depends_on_bead_id) {
                issues.push(format!("edge references nonexistent bead {}", d.depends_on_bead_id));
            }
        }

        // 4) Cycles (DFS over the edge set).
        let mut adj: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        for d in &deps {
            adj.entry(d.bead_id.clone()).or_default().push(d.depends_on_bead_id.clone());
        }
        let mut nodes: Vec<String> = all_beads.into_iter().collect();
        for n in adj.keys() {
            if !nodes.contains(n) { nodes.push(n.clone()); }
        }
        let (mut state, mut cycle) = (
            std::collections::HashMap::<String, u8>::new(), /* 0 unvisited 1 visiting 2 done */
            None::<String>,
        );
        fn dfs(adj: &std::collections::HashMap<String, Vec<String>>, n: &str, state: &mut std::collections::HashMap<String, u8>, cycle: &mut Option<String>, path: &mut Vec<String>) {
            match state.get(n).copied() {
                Some(1) => { *cycle = Some(format!("cycle: {}", path.iter().cloned().collect::<Vec<_>>().join(" â†’ ") + " â†’ " + n)); return; }
                Some(2) => return,
                _ => {}
            }
            state.insert(n.to_string(), 1);
            path.push(n.to_string());
            if let Some(next) = adj.get(n) {
                for x in next { dfs(adj, x, state, cycle, path); if cycle.is_some() { break; } }
            }
            path.pop();
            state.insert(n.to_string(), 2);
        }
        let mut all_nodes: Vec<String> = nodes.clone();
        all_nodes.extend(adj.keys().cloned());
        all_nodes.sort(); all_nodes.dedup();
        for n in &all_nodes {
            let mut path = Vec::new();
            dfs(&adj, n, &mut state, &mut cycle, &mut path);
            if cycle.is_some() { break; }
        }
        if let Some(c) = cycle {
            issues.push(c);
        }

        let valid = issues.is_empty();
        DependencyTopologyReport { edges: deps.len(), duplicates: 0, issues, valid }
    }

    /// Validate a single proposed edge WITHOUT inserting it: returns Ok(())
    /// when it is a clean DAG edge (no self-dep, no duplicate, both beads exist,
    /// no cycle would form). Used to keep the graph invariant under insert.
    pub fn try_validate_dependency(&self, bead_id: &str, depends_on_bead_id: &str) -> Result<(), String> {
        if bead_id == depends_on_bead_id {
            return Err("self-dependency is not allowed".into());
        }
        if self.get_bead(bead_id).is_none() {
            return Err(format!("bead {bead_id} does not exist (cannot add a dependency)"));
        }
        if self.get_bead(depends_on_bead_id).is_none() {
            return Err(format!("bead {depends_on_bead_id} does not exist (dependency target missing)"));
        }
        // Duplicate check.
        if self.dependencies_for(bead_id).iter().any(|d| d.depends_on_bead_id == depends_on_bead_id) {
            return Err(format!("duplicate dependency {bead_id} â†’ {depends_on_bead_id}"));
        }
        // Cycle check: adding bead_id â†’ depends_on would create a cycle iff
        // depends_on can already reach bead_id (i.e. bead_id is an ancestor).
        if self.can_reach(depends_on_bead_id, bead_id) {
            return Err(format!("would create a cycle: {bead_id} â†’ {depends_on_bead_id}"));
        }
        Ok(())
    }

    /// True when `from` transitively depends on `to` (i.e. `to` is an ancestor of
    /// `from` over dependency edges, respecting direction `X depends_on Y`).
    pub fn can_reach(&self, from: &str, to: &str) -> bool {
        fn dfs(store: &OrchestrationStore, node: &str, to: &str, visited: &mut std::collections::HashSet<String>) -> bool {
            if node == to { return true; }
            if !visited.insert(node.to_string()) { return false; }
            // Node depends on its outgoing dependency targets; follow them.
            store.dependencies_for(node).iter().any(|d| dfs(store, &d.depends_on_bead_id, to, visited))
        }
        dfs(self, from, to, &mut std::collections::HashSet::new())
    }

    /// Critique: insert a dependency only if it preserves the DAG invariant.
    pub fn put_dependency_if_valid(&self, d: &Dependency) -> Result<(), String> {
        self.try_validate_dependency(&d.bead_id, &d.depends_on_bead_id)?;
        self.put_dependency(d);
        Ok(())
    }

    // -- review metadata --
    pub fn put_review_metadata(&self, meta: &ReviewMetadata) {
        self.put_json(&Self::key(&["helldesk/reviewmeta", &meta.bead_id]), meta);
    }
    pub fn get_review_metadata(&self, bead_id: &str) -> Option<ReviewMetadata> {
        self.get_json(&Self::key(&["helldesk/reviewmeta", bead_id]))
    }
    pub fn delete_review_metadata(&self, bead_id: &str) {
        let _ = self.hot.delete(&Self::key(&["helldesk/reviewmeta", bead_id]));
    }

    // -- convoy metadata --
    pub fn put_convoy_metadata(&self, meta: &ConvoyMetadata) {
        self.put_json(&Self::key(&["helldesk/batchmeta", &meta.bead_id]), meta);
    }
    pub fn get_convoy_metadata(&self, bead_id: &str) -> Option<ConvoyMetadata> {
        self.get_json(&Self::key(&["helldesk/batchmeta", bead_id]))
    }

    // -- escalation metadata --
    pub fn put_escalation(&self, esc: &Escalation) {
        self.put_json(&Self::key(&["helldesk/escalation", &esc.bead_id]), esc);
    }
    pub fn get_escalation(&self, bead_id: &str) -> Option<Escalation> {
        self.get_json(&Self::key(&["helldesk/escalation", bead_id]))
    }

    // -- index helpers --
    fn add_idx(&self, prefix: &str, id: &str) {
        let key = Self::idx_key(prefix);
        let mut list: Vec<String> = self
            .hot
            .get(&key)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_slice(&v.0).ok())
            .unwrap_or_default();
        if !list.iter().any(|x| x == id) {
            list.push(id.to_string());
            if let Ok(b) = serde_json::to_vec(&list) {
                let _ = self.hot.put(&key, &HotValue(b));
            }
        }
    }
    fn del_idx(&self, prefix: &str, id: &str) {
        let key = Self::idx_key(prefix);
        let mut list: Vec<String> = self
            .hot
            .get(&key)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_slice(&v.0).ok())
            .unwrap_or_default();
        list.retain(|x| x != id);
        if let Ok(b) = serde_json::to_vec(&list) {
            let _ = self.hot.put(&key, &HotValue(b));
        }
    }
    fn list_idx(&self, prefix: &str) -> Vec<String> {
        self.hot
            .get(&Self::idx_key(prefix))
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_slice(&v.0).ok())
            .unwrap_or_default()
    }

    /// Scan all values under a raw key prefix and deserialize each as T.
    fn scan_json<T: for<'de> Deserialize<'de>>(&self, prefix: &str) -> Vec<T> {
        let pf = prefix.as_bytes().to_vec();
        let keys = match self.hot.scan_prefix(&pf) {
            Ok(k) => k,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for k in keys {
            if let Ok(Some(v)) = self.hot.get(&k) {
                if let Ok(t) = serde_json::from_slice(&v.0) {
                    out.push(t);
                }
            }
        }
        out
    }

    // -- nudges (durable, upstream agent_nudges) --
    pub fn put_nudge(&self, agent_id: &str, message: &str, mode: &str, priority: &str, source: &str) {
        let n = Nudge {
            nudge_id: gen_id(),
            agent_bead_id: agent_id.to_string(),
            message: message.to_string(),
            mode: mode.to_string(),
            priority: priority.to_string(),
            source: source.to_string(),
            created_at: now(),
            delivered_at: None,
        };
        self.put_json(&Self::key(&["helldesk/nudge", &n.nudge_id]), &n);
        self.add_idx(&format!("nudge_agent/{agent_id}"), &n.nudge_id);
    }
    pub fn list_nudges_for(&self, agent_id: &str) -> Vec<Nudge> {
        self.list_idx(&format!("nudge_agent/{agent_id}"))
            .into_iter()
            .filter_map(|id| self.get_json::<Nudge>(&Self::key(&["helldesk/nudge", &id])))
            .collect()
    }

    // â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    // Durable event history / audit log (upstream `bead_events`)
    // â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    /// Append a durable orchestration event. Faithful to upstream `bead_events`
    /// (append-only, tagged by bead/agent/town/rig source). Every state change
    /// the reconciler or control API makes should funnel through here so the
    /// audit timeline is complete and re-runnable (WS1 parity gap).
    pub fn emit_event(&self, event: &BeadEvent) {
        self.put_json(&Self::key(&["helldesk/event", &event.event_id]), event);
        if let Some(bead) = event.bead_id.as_deref() {
            self.add_idx(&format!("event_bead/{bead}"), &event.event_id);
        }
        if let Some(agent) = event.agent_id.as_deref() {
            self.add_idx(&format!("event_agent/{agent}"), &event.event_id);
        }
        if let Some(town) = event.town_id.as_deref() {
            self.add_idx(&format!("event_town/{town}"), &event.event_id);
        }
        if let Some(rig) = event.rig_id.as_deref() {
            self.add_idx(&format!("event_rig/{rig}"), &event.event_id);
        }
    }

    /// Emit a durable `bead.status_changed` transition event correlated to the
    /// bead (P0-E). This is the single source for the SSE stream's real
    /// engineering transitions (pollcat start/done/fail, retry, review). The
    /// `proves` field carries a bounded, safe summary â€” never credentials or
    /// complete model prompts.
    pub fn emit_bead_status(
        &self,
        bead_id: &str,
        actor: &str,
        to: &str,
        reason: Option<&str>,
        proves: Option<&str>,
        town_id: Option<&str>,
    ) {
        let Some(b) = self.get_bead(bead_id) else { return };
        let agent_id = b.assignee_agent_bead_id.clone();
        let rig_id = b.rig_id.clone().or_else(|| agent_id.as_ref().and_then(|a| self.get_agent(a).and_then(|ag| ag.rig_id)));
        // Town: use the explicit live town when supplied (loopback /events uses
        // "town-default"); otherwise fall back to the rig's town, else the
        // well-known live town so the SSE stream (which filters by town) sees it.
        let town = town_id.map(str::to_string).filter(|t| !t.is_empty())
            .or_else(|| b.rig_id.as_deref().and_then(|r| self.get_rig(r).map(|rig| rig.town_id)))
            .unwrap_or_else(|| "town-default".to_string());
        let ev = BeadEvent::new(
            "bead.status_changed",
            Some(bead_id.to_string()),
            agent_id,
            Some(town),
            rig_id,
            serde_json::json!({
                "to": to,
                "actor": actor,
                "bounded_reason": reason.map(str::to_string).unwrap_or_default(),
                "proves": proves.map(str::to_string).unwrap_or_default(),
            }),
        );
        self.emit_event(&ev);
    }

    /// Events for a bead, oldest first (audit timeline).
    pub fn events_for_bead(&self, bead_id: &str) -> Vec<BeadEvent> {
        self.events_for_idx(&format!("event_bead/{bead_id}"))
    }
    pub fn events_for_agent(&self, agent_id: &str) -> Vec<BeadEvent> {
        self.events_for_idx(&format!("event_agent/{agent_id}"))
    }
    pub fn events_for_town(&self, town_id: &str) -> Vec<BeadEvent> {
        self.events_for_idx(&format!("event_town/{town_id}"))
    }
    fn events_for_idx(&self, idx: &str) -> Vec<BeadEvent> {
        let mut evs: Vec<BeadEvent> = self
            .list_idx(idx)
            .into_iter()
            .filter_map(|id| self.get_json::<BeadEvent>(&Self::key(&["helldesk/event", &id])))
            .collect();
        evs.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        evs
    }

    // â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    // Durable mailboxes (upstream `mail` / `agent_mail`)
    // â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    pub fn send_mail(
        &self,
        to_agent_id: &str,
        from: Option<&str>,
        subject: &str,
        body: &str,
        kind: &str,
    ) -> Mail {
        let mail = Mail {
            mail_id: gen_id(),
            agent_bead_id: to_agent_id.to_string(),
            from_agent_id: from.map(str::to_string),
            subject: subject.to_string(),
            body: body.to_string(),
            kind: kind.to_string(),
            delivered: false,
            created_at: now(),
            delivered_at: None,
        };
        self.put_json(&Self::key(&["helldesk/mail", &mail.mail_id]), &mail);
        self.add_idx(&format!("mail_agent/{}", to_agent_id), &mail.mail_id);
        mail
    }

    /// List undelivered mail for an agent (upstream `readAndDeliver` surface â€”
    /// caller decides whether to mark delivered).
    pub fn list_mail_for(&self, agent_id: &str) -> Vec<Mail> {
        let mut m: Vec<Mail> = self
            .list_idx(&format!("mail_agent/{agent_id}"))
            .into_iter()
            .filter_map(|id| self.get_json::<Mail>(&Self::key(&["helldesk/mail", &id])))
            .collect();
        m.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        m
    }
    pub fn list_undelivered_mail_for(&self, agent_id: &str) -> Vec<Mail> {
        self.list_mail_for(agent_id)
            .into_iter()
            .filter(|m| !m.delivered)
            .collect()
    }
    pub fn mark_mail_delivered(&self, mail_id: &str) {
        if let Some(mut m) = self.get_json::<Mail>(&Self::key(&["helldesk/mail", mail_id])) {
            if !m.delivered {
                m.delivered = true;
                m.delivered_at = Some(now());
                self.put_json(&Self::key(&["helldesk/mail", mail_id]), &m);
            }
        }
    }

    // â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    // Triage requests (upstream `patrol.createTriageRequest`)
    // â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    /// Create a triage-request issue bead with dedupe + global cap, faithful to
    /// `patrol.ts`. Returns the bead id, or None when deduped/capped.
    pub fn create_triage_request(
        &self,
        _town_id: &str,
        stat: &TriageRequestStat,
        options: Vec<String>,
    ) -> Option<String> {
        // Global cap: at most 5 open non-escalation automatic triage requests.
        let open = self.list_pending_triage_requests();
        let non_escalation = open
            .iter()
            .filter(|b| {
                b.metadata
                    .get("triage_type")
                    .and_then(|v| v.as_str())
                    .map(|t| t != "escalation")
                    .unwrap_or(true)
            })
            .count();
        if non_escalation >= 5 {
            return None;
        }
        // Dedupe: skip if an open triage of same type + agent already exists.
        let dup = open.iter().any(|b| {
            let same_agent = stat
                .agent_bead_id
                .as_deref()
                .map(|a| {
                    b.metadata
                        .get("agent_bead_id")
                        .and_then(|v| v.as_str())
                        == Some(a)
                })
                .unwrap_or(false);
            b.metadata
                .get("triage_type")
                .and_then(|v| v.as_str())
                .map(|t| t == stat.triage_type.as_str() && same_agent)
                .unwrap_or(false)
        });
        if dup {
            return None;
        }
        let bead = Bead::create(
            BeadType::Issue,
            &stat.title,
            None,
            Some(BeadPriority::Medium),
            vec!["gt:triage-request".to_string(), "gt:triage".to_string()],
            serde_json::json!({
                "triage_type": stat.triage_type.as_str(),
                "agent_bead_id": stat.agent_bead_id,
                "hooked_bead_id": stat.hooked_bead_id,
                "options": options,
                "context": stat.context,
            }),
            stat.rig_id.as_deref(),
            None,
            Some("system"),
        );
        let id = bead.bead_id.clone();
        self.put_bead(&bead);
        id.into()
    }

    /// Open `gt:triage-request` issue beads, oldest first (upstream
    /// `listPendingTriageRequests` LIMIT 20).
    pub fn list_pending_triage_requests(&self) -> Vec<Bead> {
        let mut beads: Vec<Bead> = self
            .list_beads()
            .into_iter()
            .filter(|b| {
                b.bead_type == BeadType::Issue
                    && b.status == BeadStatus::Open
                    && b.has_label("gt:triage-request")
            })
            .collect();
        beads.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        beads.truncate(20);
        beads
    }

    // â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    // Molecules (upstream `review-queue.ts` molecule section)
    // â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    /// Create a molecule: a parent `gt:molecule` bead + N sequential step beads
    /// chained by `blocks` (step i blocks step i+1). Returns the molecule bead
    /// id. Faithful to `createMolecule`.
    pub fn create_molecule(
        &self,
        town_id: &str,
        source_bead_id: &str,
        rig_id: Option<&str>,
        formula: &serde_json::Value,
    ) -> String {
        let formula_arr = formula
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mol = Bead::create(
            BeadType::Molecule,
            "Molecule",
            None,
            None,
            vec!["gt:molecule".to_string()],
            serde_json::json!({
                "source_bead_id": source_bead_id,
                "source_town_id": town_id,
                "formula": formula_arr,
            }),
            rig_id,
            Some(source_bead_id),
            None,
        );
        let mol_id = mol.bead_id.clone();
        self.put_bead(&mol);
        self.put_molecule_metadata(&MoleculeMetadata {
            bead_id: mol_id.clone(),
            current_step: 0,
            status: MoleculeStatus::Active,
            formula: serde_json::Value::Array(formula_arr.clone()),
        });
        // Stamp the source bead with the molecule id (upstream).
        if let Some(mut sb) = self.get_bead(source_bead_id) {
            sb.metadata["molecule_bead_id"] = serde_json::json!(mol_id);
            sb.updated_at = now();
            self.put_bead(&sb);
        }
        // Child step beads, each a `gt:molecule-step` issue bead, chained by
        // 'blocks'; the parent molecule 'tracks'/'parent-child' links the chain.
        let mut prev: Option<String> = None;
        for (i, step) in formula_arr.iter().enumerate() {
            let title = step
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or(&format!("Step {}", i + 1))
                .to_string();
            let step_bead = Bead::create(
                BeadType::Issue,
                &title,
                None,
                None,
                vec![
                    "gt:molecule-step".to_string(),
                    format!("step:{}", i),
                ],
                serde_json::json!({
                    "step_index": i,
                    "step_data": step,
                    "molecule_bead_id": mol_id,
                }),
                rig_id,
                Some(&mol_id),
                Some("system"),
            );
            let step_id = step_bead.bead_id.clone();
            self.put_bead(&step_bead);
            // molecule -> first step (parent-child), step_i -> step_{i+1} (blocks).
            if i == 0 {
                self.put_dependency(&Dependency {
                    bead_id: mol_id.clone(),
                    depends_on_bead_id: step_id.clone(),
                    kind: DependencyKind::ParentChild,
                });
            }
            if let Some(p) = prev {
                self.put_dependency(&Dependency {
                    bead_id: step_id.clone(),
                    depends_on_bead_id: p,
                    kind: DependencyKind::Blocks,
                });
            }
            prev = Some(step_id);
        }
        let _ = town_id;
        mol_id
    }

    pub fn put_molecule_metadata(&self, meta: &MoleculeMetadata) {
        self.put_json(&Self::key(&["helldesk/molecule", &meta.bead_id]), meta);
    }
    pub fn get_molecule_metadata(&self, molecule_id: &str) -> Option<MoleculeMetadata> {
        self.get_json(&Self::key(&["helldesk/molecule", molecule_id]))
    }

    /// Compute the current step and status for a molecule (upstream
    /// `getMolecule`): failed if any step failed, completed if closedCount >=
    /// length, else active with current_step = closedCount.
    pub fn molecule_status(&self, molecule_id: &str) -> MoleculeStatus {
        let steps = self.molecule_step_beads(molecule_id);
        if steps.is_empty() {
            return MoleculeStatus::Failed;
        }
        if steps.iter().any(|b| b.status == BeadStatus::Failed) {
            return MoleculeStatus::Failed;
        }
        let closed = steps
            .iter()
            .filter(|b| b.status == BeadStatus::Closed)
            .count();
        if closed >= steps.len() {
            MoleculeStatus::Completed
        } else {
            MoleculeStatus::Active
        }
    }
    pub fn molecule_current_step(&self, molecule_id: &str) -> Option<(usize, &'static str, serde_json::Value)> {
        let closed = self
            .molecule_step_beads(molecule_id)
            .iter()
            .filter(|b| b.status == BeadStatus::Closed)
            .count();
        let meta = self.get_molecule_metadata(molecule_id)?;
        let formula = meta.formula.as_array()?;
        if closed >= formula.len() {
            return None; // completed
        }
        let step = formula.get(closed)?.clone();
        Some((closed, "active", step))
    }
    fn molecule_step_beads(&self, molecule_id: &str) -> Vec<Bead> {
        // Steps are the 'parent-child' dependents of the molecule (steps link to
        // mol via depends_on in the first block edge) â€” collect via the
        // 'gt:molecule-step' label + parent_bead_id for robustness.
        self.list_beads()
            .into_iter()
            .filter(|b| {
                b.parent_bead_id.as_deref() == Some(molecule_id)
                    && b.has_label("gt:molecule-step")
            })
            .collect()
    }

    /// Advance the molecule: close the current step bead; past the end also
    /// close the molecule bead. Returns the refreshed molecule metadata.
    pub fn advance_molecule_step(&self, agent_id: &str) -> Option<MoleculeMetadata> {
        let agent = self.get_agent(agent_id)?;
        let hook = agent.current_hook_bead_id.as_deref()?;
        let src = self.get_bead(hook)?;
        let mol_id = src
            .metadata
            .get("molecule_bead_id")
            .and_then(|v| v.as_str())?;
        let mut meta = self.get_molecule_metadata(mol_id)?;
        if meta.status != MoleculeStatus::Active {
            return Some(meta);
        }
        // Close the current (first non-terminal) step bead.
        let mut steps = self.molecule_step_beads(mol_id);
        steps.sort_by(|a, b| {
            let ai = a
                .metadata
                .get("step_index")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let bi = b
                .metadata
                .get("step_index")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            ai.cmp(&bi)
        });
        if let Some(cur) = steps.into_iter().find(|b| b.status == BeadStatus::Open) {
            self.update_bead_status(cur.bead_id, BeadStatus::Closed, "system".into(), None);
        }
        let new_status = self.molecule_status(mol_id);
        meta.current_step = self
            .molecule_step_beads(mol_id)
            .iter()
            .filter(|b| b.status == BeadStatus::Closed)
            .count() as u32;
        meta.status = new_status;
        if new_status == MoleculeStatus::Completed {
            // Close the molecule bead too.
            self.update_bead_status(
                mol_id.to_string(),
                BeadStatus::Closed,
                "system".into(),
                None,
            );
        }
        self.put_molecule_metadata(&meta);
        Some(meta)
    }

    // â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    // Escalation severity ladder (upstream Town.do escalation section)
    // â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    /// Re-escalate stale unacknowledged escalations (upstream
    /// `reEscalateStaleEscalations`). Age threshold = (count+1)*4h.
    pub fn re_escalate_stale_escalations(&self) -> Vec<String> {
        const STALE: i64 = 4 * 60 * 60 * 1000;
        const MAX_RE: u32 = 3;
        const ORDER: [&str; 4] = ["low", "medium", "high", "critical"];
        let now_ms = now_unix_ms();
        let mut bumped = Vec::new();
        for b in self.list_beads() {
            if b.bead_type != BeadType::Escalation || b.status != BeadStatus::Open {
                continue;
            }
            let Some(mut esc) = self.get_escalation(&b.bead_id) else { continue };
            if esc.acknowledged || esc.re_escalation_count >= MAX_RE {
                continue;
            }
            let created_ms = parse_iso_ms(&b.created_at).unwrap_or(now_ms);
            let threshold = (esc.re_escalation_count as i64 + 1) * STALE;
            if now_ms - created_ms < threshold {
                continue;
            }
            let next = next_severity(&esc.severity, &ORDER);
            esc.severity = next.to_string();
            esc.re_escalation_count += 1;
            self.put_escalation(&esc);
            let mut bead = b.clone();
            bead.metadata["severity"] = serde_json::json!(next);
            bead.updated_at = now();
            self.put_bead(&bead);
            bumped.push(format!("{}:{}", bead.bead_id, next));
        }
        bumped
    }

    // â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    // Per-bead adversarial review passes (WS5)
    // â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    pub fn put_review_pass(&self, pass: &ReviewPass) {
        self.put_json(&Self::key(&["helldesk/reviewpass", &pass.pass_id]), pass);
        self.add_idx(&format!("reviewpass_source/{}", pass.source_bead_id), &pass.pass_id);
        self.add_idx(&format!("reviewpass_mr/{}", pass.mr_bead_id), &pass.pass_id);
    }
    pub fn get_review_pass(&self, pass_id: &str) -> Option<ReviewPass> {
        self.get_json(&Self::key(&["helldesk/reviewpass", pass_id]))
    }
    /// Review passes for a source bead, oldest first.
    pub fn review_passes_for_source(&self, source_bead_id: &str) -> Vec<ReviewPass> {
        let mut passes: Vec<ReviewPass> = self
            .list_idx(&format!("reviewpass_source/{source_bead_id}"))
            .into_iter()
            .filter_map(|id| self.get_review_pass(&id))
            .collect();
        passes.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        passes
    }
    /// Review passes for an MR bead.
    pub fn review_passes_for_mr(&self, mr_bead_id: &str) -> Vec<ReviewPass> {
        let mut passes: Vec<ReviewPass> = self
            .list_idx(&format!("reviewpass_mr/{mr_bead_id}"))
            .into_iter()
            .filter_map(|id| self.get_review_pass(&id))
            .collect();
        passes.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        passes
    }

    /// Read the durable review-cycle state stored on a bead's metadata.
    pub fn review_cycle_state(&self, bead_id: &str) -> ReviewCycleState {
        self.get_bead(bead_id)
            .and_then(|b| b.metadata.get("review_cycle").cloned())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or(ReviewCycleState {
                cycles_used: 0,
                max_cycles: 3,
                corrected_instructions: vec![],
                last_reject_reason: None,
                satisfied: false,
            })
    }
    /// Persist the durable review-cycle state on a bead's metadata.
    pub fn put_review_cycle_state(&self, bead_id: &str, state: &ReviewCycleState) {
        if let Some(mut b) = self.get_bead(bead_id) {
            b.metadata["review_cycle"] = serde_json::to_value(state).unwrap_or(serde_json::json!({}));
            b.updated_at = now();
            self.put_bead(&b);
        }
    }

    // -- town-scoped conveniences --
    pub fn list_convoy_bead_ids_for_town(&self, town_id: &str) -> Vec<String> {
        let rigs: std::collections::HashSet<String> = self
            .list_rigs_for_town(town_id)
            .into_iter()
            .map(|r| r.rig_id)
            .collect();
        self.list_beads()
            .into_iter()
            .filter(|b| {
                b.bead_type == BeadType::Convoy
                    && b.rig_id
                        .as_deref()
                        .map(|r| rigs.contains(r))
                        .unwrap_or(false)
            })
            .map(|b| b.bead_id)
            .collect()
    }

    /// Public bridge to the hot-store for scheduler counters (e.g. tick mark).
    pub fn hot_put(&self, key: impl Into<String>, val: &str) -> Result<(), HotStoreErrorLike> {
        self.hot
            .put(&HotKey(key.into().into_bytes()), &HotValue(val.as_bytes().to_vec()))
            .map_err(Into::into)
    }

    /// Read a hot-store string counter (e.g. the persisted scheduler tick).
    pub fn hot_get(&self, key: impl Into<String>) -> Option<String> {
        self.hot
            .get(&HotKey(key.into().into_bytes()))
            .ok()
            .flatten()
            .map(|v| String::from_utf8_lossy(&v.0).into_owned())
    }

    /// Durably remove a raw key from the HotStore (used by the host overlay to
    /// delete stale aggregate records, e.g. a deprecated project). No-op if absent.
    pub fn hot_delete(&self, key: impl Into<String>) {
        let _ = self.hot.delete(&HotKey(key.into().into_bytes()));
    }

    // â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    // State-mutating helpers used by the orchestrator dispatcher (faithful to
    // upstream `beads.ts` / `agents.ts` / `review-queue.ts` / `actions.ts`).
    // â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    /// Canonical bead status transition. Terminal states are immutable.
    pub fn update_bead_status(&self, bead_id: String, status: BeadStatus, actor: String, reason: Option<String>) {
        let Some(mut b) = self.get_bead(&bead_id) else { return };
        if b.status == status {
            return; // no-op (upstream)
        }
        if b.status.is_terminal() && b.status != status {
            return; // terminal immutability (upstream hard invariant)
        }
        b.status = status;
        b.updated_at = now();
        if status == BeadStatus::Closed {
            b.closed_at = Some(now());
        }
        if status == BeadStatus::Failed {
            if let Some(r) = reason.as_deref() {
                b.metadata["failure_reason"] = serde_json::json!(r);
            }
        }
        let to = match status { BeadStatus::Closed => "closed", BeadStatus::Failed => "failed", BeadStatus::InReview => "in_review", BeadStatus::InProgress => "in_progress", BeadStatus::Open => "open" };
        let btitle = b.title.clone();
        let bid = b.bead_id.clone();
        let reason_cl = reason.clone();
        let actor_name = if actor.is_empty() { "system".to_string() } else { actor };
        self.put_bead(&b);
        // P0-E: durable transition emitted for real status changes (reconciler /
        // Governor / control paths that flow through update_bead_status). The
        // scheduler's direct dispatch/done paths emit their own richer events;
        // this covers terminal (closed/failed) and review transitions not caught
        // elsewhere, so the SSE stream always reflects real engineering activity.
        self.emit_bead_status(&bid, &actor_name, to,
            reason_cl.as_deref(), Some(&btitle), None);
        if b.status.is_terminal() {
            self.update_convoy_progress_for(&b.bead_id);
        }
    }

    /// Re-count closed beads in the convoy(s) tracking this bead (upstream
    /// `updateConvoyProgress`). Mirror the ready_to_land / auto-close logic.
    fn update_convoy_progress_for(&self, bead_id: &str) {
        let convoy_edges: Vec<Dependency> = self
            .list_dependencies()
            .into_iter()
            .filter(|d| d.kind == DependencyKind::Tracks && d.bead_id == bead_id)
            .collect();
        for edge in convoy_edges {
            let convoy_id = &edge.depends_on_bead_id;
            let Some(mut cm) = self.get_convoy_metadata(convoy_id) else { continue };
            let mut closed = 0u32;
            let mut total = 0u32;
            for d in self
                .list_dependencies()
                .into_iter()
                .filter(|d| d.kind == DependencyKind::Tracks && d.depends_on_bead_id == *convoy_id)
            {
                if let Some(b) = self.get_bead(&d.bead_id) {
                    if b.bead_type == BeadType::Issue {
                        total += 1;
                        if b.status.is_terminal() && !self.has_pending_mr_children(&b.bead_id) {
                            closed += 1;
                        }
                    }
                }
            }
            cm.closed_beads = closed;
            cm.total_beads = total;
            self.put_convoy_metadata(&cm);
            let done = closed >= total && total > 0;
            if done {
                if let Some(fb) = &cm.feature_branch.clone() {
                    if cm.merge_mode == MergeMode::ReviewThenLand {
                        // ready_to_land; reconciler creates the landing MR.
                        if let Some(mut cb) = self.get_bead(convoy_id) {
                            cb.metadata["ready_to_land"] = serde_json::json!(true);
                            cb.updated_at = now();
                            self.put_bead(&cb);
                        }
                        continue;
                    }
                    let _ = fb;
                }
                // No feature branch / review-and-merge â†’ auto-close.
                if let Some(mut cb) = self.get_bead(convoy_id) {
                    cb.status = BeadStatus::Closed;
                    cb.closed_at = Some(now());
                    cb.updated_at = now();
                    self.put_bead(&cb);
                }
                cm.landed_at = Some(now());
                self.put_convoy_metadata(&cm);
            }
        }
    }

    fn has_pending_mr_children(&self, bead_id: &str) -> bool {
        self.list_dependencies()
            .into_iter()
            .filter(|d| d.kind == DependencyKind::Tracks && d.bead_id == bead_id)
            .filter_map(|d| self.get_bead(&d.depends_on_bead_id))
            .any(|child| {
                child.bead_type == BeadType::MergeRequest && !child.status.is_terminal()
            })
    }

    pub fn update_agent_status(&self, agent_id: String, status: AgentStatus, reason: Option<String>) {
        let Some(mut a) = self.get_agent(&agent_id) else { return };
        a.status = status;
        if status == AgentStatus::Stalled && a.stalled_at.is_none() {
            a.stalled_at = Some(now());
        }
        if status != AgentStatus::Stalled {
            a.stalled_at = None;
            a.last_activity_at = Some(now());
        }
        let _ = reason;
        self.put_agent(&a);
    }

    pub fn reset_agent_dispatch_attempts(&self, agent_id: String) {
        let Some(mut a) = self.get_agent(&agent_id) else { return };
        a.dispatch_attempts = 0;
        if let Some(hook) = a.current_hook_bead_id.clone() {
            if let Some(mut b) = self.get_bead(&hook) {
                b.dispatch_attempts = 0;
                b.last_dispatch_attempt_at = None;
                if b.status == BeadStatus::Failed {
                    b.status = BeadStatus::Open;
                    b.updated_at = now();
                }
                self.put_bead(&b);
            }
            if let Some(mut rm) = self.get_review_metadata(&hook) {
                rm.retry_count = 0;
                self.put_review_metadata(&rm);
            }
        }
        self.put_agent(&a);
    }

    pub fn hook_agent(&self, agent_id: String, bead_id: String) {
        let Some(mut a) = self.get_agent(&agent_id) else { return };
        if a.current_hook_bead_id.as_deref() == Some(bead_id.as_str()) {
            return; // idempotent
        }
        // Reject unhookable bead types (upstream hookBead).
        if let Some(b) = self.get_bead(&bead_id) {
            if matches!(
                b.bead_type,
                BeadType::Escalation | BeadType::Convoy | BeadType::Agent | BeadType::Message
            ) || b.has_label("gt:triage-request")
            {
                return;
            }
        }
        a.current_hook_bead_id = Some(bead_id.clone());
        a.status = AgentStatus::Idle;
        a.last_activity_at = Some(now());
        self.put_agent(&a);
        // Mutual exclusion: unhook any other agent hooked to the same bead.
        for other in self.list_agents() {
            if other.bead_id != agent_id
                && other.current_hook_bead_id.as_deref() == Some(bead_id.as_str())
            {
                self.unhook_agent(&other.bead_id);
            }
        }
        if let Some(mut b) = self.get_bead(&bead_id) {
            b.assignee_agent_bead_id = Some(agent_id.clone());
            b.updated_at = now();
            self.put_bead(&b);
        }
    }

    pub fn unhook_agent(&self, agent_id: &str) {
        let Some(mut a) = self.get_agent(agent_id) else { return };
        a.current_hook_bead_id = None;
        a.status = AgentStatus::Idle;
        a.checkpoint = None; // checkpoint cleared on unhook (upstream)
        self.put_agent(&a);
    }

    pub fn clear_checkpoint(&self, agent_id: String) {
        if let Some(mut a) = self.get_agent(&agent_id) {
            a.checkpoint = None;
            self.put_agent(&a);
        }
    }

    /// Delete an agent (orchestrator GC), reopening non-terminal beads it was
    /// assigned to (upstream `deleteAgent`).
    pub fn reap_agent(&self, agent_id: &str) {
        if let Some(a) = self.get_agent(agent_id) {
            if let Some(hook) = a.current_hook_bead_id.clone() {
                if let Some(mut b) = self.get_bead(&hook) {
                    if !b.status.is_terminal() {
                        b.status = BeadStatus::Open;
                        b.updated_at = now();
                    }
                    b.assignee_agent_bead_id = None;
                    self.put_bead(&b);
                }
            }
        }
        self.delete_agent(agent_id);
    }

    pub fn close_convoy(&self, convoy_id: String) -> Result<(), TransitionError> {
        // Close via the canonical boundary: a convoy may only close when verified
        // OR via an audited administrative decision. A bare close (unverified)
        // now fails â€” the scheduler must drive a real landing.
        self.transition_bead(
            &convoy_id,
            BeadStatus::Closed,
            &TransitionCtx::new(TransitionOrigin::Scheduler, "close_convoy"),
        )?;
        if let Some(mut cm) = self.get_convoy_metadata(&convoy_id) {
            cm.landed_at = Some(now());
            self.put_convoy_metadata(&cm);
        }
        Ok(())
    }

    pub fn fail_convoy(&self, convoy_id: String, reason: String) -> Result<(), TransitionError> {
        // Administrative/recovery failure: audited marker, never a VERIFIED stamp.
        self.transition_bead(
            &convoy_id,
            BeadStatus::Failed,
            &TransitionCtx::new(TransitionOrigin::Admin, "fail_convoy").with_reason(reason),
        )?;
        Ok(())
    }

    /// Submit a merge_request bead to the review queue (upstream
    /// `submitToReviewQueue`). Returns the MR bead id.
    pub fn submit_to_review_queue(
        &self,
        source_bead_id: String,
        agent_id: String,
        rig_id: String,
        branch: String,
        default_target: String,
        pr_url: Option<String>,
        summary: Option<String>,
    ) -> String {
        let mut meta = serde_json::json!({
            "source_bead_id": source_bead_id,
            "source_agent_id": agent_id,
        });
        if let Some(u) = pr_url.as_ref() {
            meta["pr_url"] = serde_json::json!(u);
        }
        let mr = Bead::create(
            BeadType::MergeRequest,
            &format!("Review: {branch}"),
            None,
            None,
            vec!["gt:merge-request".to_string()],
            meta,
            Some(&rig_id),
            Some(&source_bead_id),
            Some(&agent_id),
        );
        let mr_id = mr.bead_id.clone();
        self.put_bead(&mr);
        self.put_dependency(&Dependency {
            bead_id: mr_id.clone(),
            depends_on_bead_id: source_bead_id,
            kind: DependencyKind::Tracks,
        });
        self.put_review_metadata(&ReviewMetadata {
            bead_id: mr_id.clone(),
            branch: branch.clone(),
            target_branch: default_target,
            merge_commit: None,
            pr_url,
            retry_count: 0,
            auto_merge_ready_since: None,
            last_feedback_check_at: None,
        });
        let _ = summary;
        mr_id
    }

    /// Create the final landing MR for a review-then-land convoy (upstream
    /// `create_landing_mr`, created by 'system').
    pub fn create_landing_mr(&self, convoy_id: String, rig_id: String, feature_branch: String, target_branch: String) {
        if let Some(mut c) = self.get_bead(&convoy_id) {
            let attempts = c.metadata["landing_mr_attempts"].as_u64().unwrap_or(0) + 1;
            c.metadata["landing_mr_attempts"] = serde_json::json!(attempts);
            c.metadata["last_landing_mr_attempt_at"] = serde_json::json!(now());
            c.updated_at = now();
            self.put_bead(&c);
        }
        let mr = Bead::create(
            BeadType::MergeRequest,
            &format!("Landing: {feature_branch}"),
            None,
            None,
            vec!["gt:merge-request".to_string(), "gt:landing".to_string()],
            serde_json::json!({"source_bead_id": convoy_id, "convoy_landing": true}),
            Some(&rig_id),
            Some(&convoy_id),
            Some("system"),
        );
        let mr_id = mr.bead_id.clone();
        self.put_bead(&mr);
        self.put_dependency(&Dependency {
            bead_id: mr_id.clone(),
            depends_on_bead_id: convoy_id.clone(),
            kind: DependencyKind::Tracks,
        });
        self.put_review_metadata(&ReviewMetadata {
            bead_id: mr_id.clone(),
            branch: feature_branch.clone(),
            target_branch,
            merge_commit: None,
            pr_url: None,
            retry_count: 0,
            auto_merge_ready_since: None,
            last_feedback_check_at: None,
        });
    }

    /// Complete a review entry as merged (upstream `completeReviewWithResult`).
    pub fn complete_review(&self, mr_id: String, pr_url: Option<String>) -> Result<(), TransitionError> {
        // Rule 2: an MR may only be completed (closed) if it has independently
        // verified merge evidence. `pr_url` alone is NOT merge evidence.
        let evidence_ok = self
            .get_bead(&mr_id)
            .and_then(|b| b.metadata.get("merge_evidence").cloned())
            .map(|v| v.get("sha").and_then(|s| s.as_str()).map(|s| !s.trim().is_empty()))
            .flatten()
            .unwrap_or(false);
        if !evidence_ok {
            return Err(TransitionError::MergeEvidenceRequired);
        }
        if pr_url.is_some() {
            if let Some(mut rm) = self.get_review_metadata(&mr_id) {
                rm.pr_url = pr_url.clone();
                self.put_review_metadata(&rm);
            }
            if let Some(mut mr) = self.get_bead(&mr_id) {
                mr.metadata["pr_url"] = serde_json::json!(pr_url);
                mr.updated_at = now();
                let serialized = self.bead_serialized(&mr_id);
                if self.put_bead_cas(&mr, serialized.as_deref()).map_err(TransitionError::Store)? {
                    // note: the MR close itself is applied via the transition boundary below
                    // using the freshest expected value.
                }
            }
        }
        // Close the MR via the canonical boundary (requires evidence OR admin/recovery).
        let res = self.transition_bead(
            &mr_id,
            BeadStatus::Closed,
            &TransitionCtx::new(TransitionOrigin::Scheduler, "complete_review").with_reason("merged"),
        )?;
        // Close the source bead tracking this MR.
        self.close_source_of_mr(&mr_id);
        let _ = res;
        Ok(())
    }

    fn close_source_of_mr(&self, mr_id: &str) {
        if let Some(source) = self
            .list_dependencies()
            .into_iter()
            .find(|d| d.kind == DependencyKind::Tracks && d.bead_id == mr_id)
        {
            if let Some(sb) = self.get_bead(&source.depends_on_bead_id) {
                if !sb.status.is_terminal() {
                    // Close via canonical boundary with the Scheduler origin so it
                    // still requires verification or merge evidence carried by the
                    // source (an Issue/MR tracking a merged MR is legitimately done).
                    let _ = self.transition_bead(
                        &sb.bead_id,
                        BeadStatus::Closed,
                        &TransitionCtx::new(TransitionOrigin::Scheduler, "close_source_of_mr"),
                    );
                }
                // When the source is a convoy, stamp landed_at (upstream
                // `completeReviewWithResult(merged)`).
                if sb.bead_type == BeadType::Convoy {
                    if let Some(mut cm) = self.get_convoy_metadata(&sb.bead_id) {
                        cm.landed_at = Some(now());
                        self.put_convoy_metadata(&cm);
                    }
                }
            }
        }
    }

    /// Set a PR URL on a review entry (upstream `setReviewPrUrl`).
    pub fn set_review_pr_url(&self, mr_id: String, pr_url: String) {
        if !pr_url.starts_with("https://") {
            return; // reject invalid URLs (upstream)
        }
        if let Some(mut rm) = self.get_review_metadata(&mr_id) {
            rm.pr_url = Some(pr_url.clone());
            self.put_review_metadata(&rm);
        }
        if let Some(mut mr) = self.get_bead(&mr_id) {
            mr.metadata["pr_url"] = serde_json::json!(pr_url);
            mr.status = BeadStatus::InProgress;
            mr.updated_at = now();
            self.put_bead(&mr);
        }
    }

    /// Record independently verified merge evidence for an MR. Callers MUST
    /// provide a merge SHA / merge-ref state that was validly observed; this is
    /// the gate that prevents an MR closing on a bare `status == Closed`.
    /// Returns Err(MergeEvidenceRequired) if evidence is empty (defensive), or
    /// Err(NotFound) if the MR bead does not exist.
    pub fn record_mr_merge(
        &self,
        mr_id: &str,
        merge_sha: &str,
        source: &str,
    ) -> Result<(), TransitionError> {
        if merge_sha.trim().is_empty() {
            return Err(TransitionError::MergeEvidenceRequired);
        }
        let Some(mut mr) = self.get_bead(mr_id) else {
            return Err(TransitionError::NotFound);
        };
        let serialized = self.bead_serialized(mr_id);
        mr.metadata["merge_evidence"] = serde_json::json!({
            "sha": merge_sha,
            "verified_at": now(),
            "source": source,
        });
        mr.updated_at = now();
        let _ = self
            .put_bead_cas(&mr, serialized.as_deref())
            .map_err(TransitionError::Store)?;
        Ok(())
    }

    /// Canonical domain-state transition boundary. This is the SINGLE authority
    /// for gerating a bead's domain status. It enforces, in order:
    ///   1. scope/ownership (project) resolution,
    ///   2. terminal-verification gate (VERIFIED stamp OR audited admin override;
    ///      MR close requires merge_evidence),
    ///   3. non-terminal control validation,
    ///   4. atomic expected-value CAS at the persistence boundary,
    ///   5. an audited BeadEvent for every applied (and admin) transition.
    ///
    /// Returns Ok(Applied) | Ok(Noop) | Ok(AdminOverride) | Err(TransitionError).
    pub fn transition_bead(
        &self,
        bead_id: &str,
        to: BeadStatus,
        ctx: &TransitionCtx,
    ) -> Result<TransitionResult, TransitionError> {
        let Some(mut b) = self.get_bead(bead_id) else {
            return Err(TransitionError::NotFound);
        };
        // Capture the exact persisted value BEFORE the mutation (CAS token).
        let serialized = self.bead_serialized(bead_id);
        if b.status == to {
            return Ok(TransitionResult::Noop);
        }
        // Rule: terminal immutability unless an explicit audited transition allowed.
        if b.status.is_terminal() && !matches!(to, BeadStatus::Open | BeadStatus::InProgress) {
            return Err(TransitionError::TerminalImmutable);
        }
        if to.is_terminal() && !b.status.is_terminal() {
            // ---- terminal transition gate ----
            match ctx.origin {
                TransitionOrigin::Admin => {
                    // Administrative cancel/skip/override: audited marker, NEVER a
                    // VERIFIED stamp. is_bead_verified_current stays false.
                    let reason = ctx.reason.clone().unwrap_or_else(|| "admin".into());
                    b.metadata["admin_decision"] = serde_json::json!({
                        "action": reason,
                        "by": ctx.actor.clone(),
                        "at": now(),
                    });
                }
                _ => {
                    // Enforce the VERIFIED gate for Issue beads (work that must pass
                    // review/landing). MergeRequest beads are exempt when they carry
                    // independently verified merge_evidence â€” the merge IS the proof,
                    // and source Issues get their own VERIFIED stamp. A Convoy is
                    // closed by the scheduler itself as the landing act.
                    let is_mr = b.bead_type == BeadType::MergeRequest;
                    let has_merge_evidence = b
                        .metadata
                        .get("merge_evidence")
                        .map(|v| v.is_object())
                        .unwrap_or(false);
                    let needs_stamp = b.bead_type == BeadType::Issue
                        || (is_mr && !has_merge_evidence);
                    if needs_stamp
                        && !self.is_bead_verified_current(bead_id)
                        && !self.has_verified_stamp_current(bead_id)
                    {
                        return Err(TransitionError::UnverifiedTerminal);
                    }
                    if needs_stamp && b.metadata.get("gastown_verified").is_none() {
                        return Err(TransitionError::UnverifiedTerminal);
                    }
                }
            }
        }
        // MR completion requires independently verified merge evidence.
        if b.bead_type == BeadType::MergeRequest && to.is_terminal() {
            let has_evidence = b
                .metadata
                .get("merge_evidence")
                .map(|v| v.is_object())
                .unwrap_or(false);
            if !has_evidence && !matches!(ctx.origin, TransitionOrigin::Admin | TransitionOrigin::Recovery) {
                return Err(TransitionError::MergeEvidenceRequired);
            }
        }
        // Non-terminal control transitions: validate dependency / review gates.
        if !to.is_terminal()
            && matches!(ctx.origin, TransitionOrigin::Control | TransitionOrigin::Scheduler)
        {
            // Only allow forward progress that the reconciler would permit; a
            // Control/UI path cannot jump a bead into a state the orchestrator
            // would not schedule.
            if to == BeadStatus::Closed {
                return Err(TransitionError::InvalidTransition);
            }
        }
        // Apply.
        b.status = to;
        b.updated_at = now();
        if to == BeadStatus::Closed {
            b.closed_at = Some(now());
        }
        let applied = self
            .put_bead_cas(&b, serialized.as_deref())
            .map_err(TransitionError::Store)?;
        if !applied {
            return Err(TransitionError::StaleEntity);
        }
        // Audited event for every applied + admin transition.
        let to_str = match to {
            BeadStatus::Closed => "closed",
            BeadStatus::Failed => "failed",
            BeadStatus::InReview => "in_review",
            BeadStatus::InProgress => "in_progress",
            BeadStatus::Open => "open",
        };
        let is_admin = matches!(ctx.origin, TransitionOrigin::Admin);
        self.emit_event(&BeadEvent::new(
            if is_admin { "bead.admin_decision" } else { "bead.transition" },
            Some(bead_id.to_string()),
            Some(ctx.actor.clone()),
            b.parent_bead_id.clone(),
            b.rig_id.clone(),
            serde_json::json!({"to": to_str, "reason": ctx.reason}),
        ));
        if to.is_terminal() {
            self.update_convoy_progress_for(bead_id);
        }
        Ok(if is_admin {
            TransitionResult::AdminOverride
        } else {
            TransitionResult::Applied
        })
    }
}

/// Author of a domain-state transition. Scheduler/Control must respect gates;
/// Recovery may reopen (audited, bounded); Admin may cancel/skip/override but
/// never manufactures a VERIFIED completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionOrigin {
    Scheduler,
    Control,
    Recovery,
    Admin,
}

/// Context for a `transition_bead` call: provenance + optional reason.
#[derive(Debug, Clone)]
pub struct TransitionCtx {
    pub origin: TransitionOrigin,
    pub actor: String,
    pub reason: Option<String>,
}

impl TransitionCtx {
    pub fn new(origin: TransitionOrigin, actor: impl Into<String>) -> Self {
        Self { origin, actor: actor.into(), reason: None }
    }
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionResult {
    Applied,
    Noop,
    AdminOverride,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionError {
    NotFound,
    TerminalImmutable,
    UnverifiedTerminal,
    MergeEvidenceRequired,
    InvalidTransition,
    StaleEntity,
    ScopeMismatch,
    Store(HotStoreErrorLike),
}

/// Error adapter so `hot_put` can surface hot-store errors without leaking the
/// concrete backend type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotStoreErrorLike(pub String);
impl From<nai_lattice_hotstore::HotStoreError> for HotStoreErrorLike {
    fn from(e: nai_lattice_hotstore::HotStoreError) -> Self {
        HotStoreErrorLike(e.to_string())
    }
}
impl std::fmt::Display for HotStoreErrorLike {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for HotStoreErrorLike {}

/// Compute the convoy metadata (total/closed) from tracked issue beads.
/// Mirrors `reconciler.reconcileConvoys` progress query.
pub fn convoy_progress(store: &OrchestrationStore, convoy_id: &str) -> (u32, u32) {
    let deps = store
        .list_dependencies()
        .into_iter()
        .filter(|d| d.kind == DependencyKind::Tracks && d.depends_on_bead_id == convoy_id);
    let mut total = 0u32;
    let mut closed = 0u32;
    for d in deps {
        if let Some(bead) = store.get_bead(&d.bead_id) {
            if bead.bead_type == BeadType::Issue {
                total += 1;
                if bead.status.is_terminal() {
                    closed += 1;
                }
            }
        }
    }
    (total, closed)
}

/// Read the effective rig config by merging town config + rig override,
/// faithful to `config.resolveRigConfig`.
pub fn resolve_rig_config(town: &Town, rig: Option<&Rig>) -> EffectiveConfig {
    let tc = &town.config;
    let ro = rig.and_then(|r| r.config.as_object().cloned());
    let get = |k: &str| ro.as_ref().and_then(|o| o.get(k));
    EffectiveConfig {
        default_model: get("default_model")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| tc.default_model.clone()),
        role_models: RoleModels {
            polecat: get("role_models")
                .and_then(|v| v.get("polecat"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| tc.role_models.polecat.clone()),
            refinery: get("role_models")
                .and_then(|v| v.get("refinery"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| tc.role_models.refinery.clone()),
            mayor: tc.role_models.mayor.clone(), // rigs cannot override mayor
            // Reviewer / adversarial / adjudicator roles (per-role model routing:
            // Reviewer A/B can target a different family such as Codex; the
            // adversarial pass targets an independent strong model; the
            // adjudicator validates evidence).
            reviewer_a: get("role_models")
                .and_then(|v| v.get("reviewer_a"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| tc.role_models.reviewer_a.clone()),
            reviewer_b: get("role_models")
                .and_then(|v| v.get("reviewer_b"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| tc.role_models.reviewer_b.clone()),
            adversarial: get("role_models")
                .and_then(|v| v.get("adversarial"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| tc.role_models.adversarial.clone()),
            adjudicator: get("role_models")
                .and_then(|v| v.get("adjudicator"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| tc.role_models.adjudicator.clone()),
        },
        review_mode: if let Some(v) = get("review_mode").and_then(|x| x.as_str()) {
            match v {
                "comments" => ReviewMode::Comments,
                _ => ReviewMode::Rework,
            }
        } else {
            tc.refinery.review_mode.clone()
        },
        code_review: get("code_review")
            .and_then(|v| v.as_bool())
            .unwrap_or(tc.refinery.code_review),
        auto_resolve_pr_feedback: get("auto_resolve_pr_feedback")
            .and_then(|v| v.as_bool())
            .unwrap_or(tc.refinery.auto_resolve_pr_feedback),
        auto_resolve_merge_conflicts: get("auto_resolve_merge_conflicts")
            .and_then(|v| v.as_bool())
            .unwrap_or(tc.refinery.auto_resolve_merge_conflicts),
        auto_merge_delay_minutes: if get("auto_merge_delay_minutes").is_some() {
            get("auto_merge_delay_minutes")
                .and_then(|v| v.as_u64())
                .map(|x| x as u32)
        } else {
            tc.refinery.auto_merge_delay_minutes
        },
        merge_strategy: if let Some(v) = get("merge_strategy").and_then(|x| x.as_str()) {
            MergeStrategy::parse(Some(v))
        } else {
            tc.merge_strategy
        },
        convoy_merge_mode: if let Some(v) = get("convoy_merge_mode").and_then(|x| x.as_str()) {
            MergeMode::parse(Some(v))
        } else {
            tc.convoy_merge_mode
        },
        custom_polecat: get("custom_instructions")
            .and_then(|o| o.get("polecat"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| tc.custom_instructions.polecat.clone()),
        custom_refinery: get("custom_instructions")
            .and_then(|o| o.get("refinery"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| tc.custom_instructions.refinery.clone()),
        max_concurrent_polecats: get("max_concurrent_polecats")
            .and_then(|v| v.as_u64())
            .map(|x| x as u32)
            .or(tc.max_polecats_per_rig),
        max_dispatch_attempts: get("max_dispatch_attempts")
            .and_then(|v| v.as_u64())
            .map(|x| x as u32),
        max_review_cycles: get("max_review_cycles")
            .and_then(|v| v.as_u64())
            .map(|x| x as u32)
            .unwrap_or(3),
        adversarial_review: get("adversarial_review")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

/// Effective per-rig config (result of town + rig merge).
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub default_model: Option<String>,
    pub role_models: RoleModels,
    pub review_mode: ReviewMode,
    pub code_review: bool,
    pub auto_resolve_pr_feedback: bool,
    pub auto_resolve_merge_conflicts: bool,
    pub auto_merge_delay_minutes: Option<u32>,
    pub merge_strategy: MergeStrategy,
    pub convoy_merge_mode: MergeMode,
    pub custom_polecat: Option<String>,
    pub custom_refinery: Option<String>,
    pub max_concurrent_polecats: Option<u32>,
    pub max_dispatch_attempts: Option<u32>,
    /// Bounded writeâ†’reviewâ†’fix cycles per bead (default 3). Rig override wins.
    pub max_review_cycles: u32,
    /// Whether the per-bead adversarial review chain is enabled (default on).
    pub adversarial_review: bool,
}

/// Model resolution priority per role (`config.resolveModel`).
pub fn resolve_model(cfg: &EffectiveConfig, role: AgentRole) -> String {
    let role_model = match role {
        AgentRole::Mayor => cfg.role_models.mayor.clone(),
        AgentRole::Refinery => cfg.role_models.refinery.clone(),
        AgentRole::Polecat => cfg.role_models.polecat.clone(),
    };
    role_model
        .or_else(|| cfg.default_model.clone())
        .unwrap_or_else(|| "anthropic/claude-sonnet-4.6".to_string())
}

/// Which reviewer pass to route (per-role model registry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewRole {
    ReviewerA,
    ReviewerB,
    Adversarial,
    Adjudicator,
}

/// Resolve the configured model for a reviewer pass. Lets the operator route
/// Reviewer A/B to a different model family (e.g. Codex via a configured
/// provider), the adversarial pass to an independent strong model, and the
/// adjudicator to an evidence-oriented model. Falls back to the default model
/// when unset â€” never a hard error.
pub fn resolve_review_model(cfg: &EffectiveConfig, role: ReviewRole) -> String {
    let m = match role {
        ReviewRole::ReviewerA => cfg.role_models.reviewer_a.clone(),
        ReviewRole::ReviewerB => cfg.role_models.reviewer_b.clone(),
        ReviewRole::Adversarial => cfg.role_models.adversarial.clone(),
        ReviewRole::Adjudicator => cfg.role_models.adjudicator.clone(),
    };
    m.or_else(|| cfg.default_model.clone())
        .unwrap_or_else(|| "anthropic/claude-sonnet-4.6".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date_conversion() {
        // 1970-01-01
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000-01-01
        let days = 10957;
        assert_eq!(civil_from_days(days), (2000, 1, 1));
    }

    #[test]
    fn now_is_iso_8601_utc() {
        let s = now();
        assert!(s.ends_with('Z'));
        assert!(s.contains('T'));
        assert_eq!(s.len(), 24);
    }

    #[test]
    fn rig_attach_mode_flags_and_repo_base() {
        let attached = Rig {
            rig_id: "r1".into(),
            name: "r".into(),
            git_url: "file:///C/x".into(),
            default_branch: "main".into(),
            town_id: "t".into(),
            config: serde_json::json!({}),
            created_at: now(),
            working_dir: Some("D:/work/NAI-Lattice".into()),
            worktree_disk_budget_mib: Some(512),
            sparse_paths: vec!["src".into(), "crates/lattice-gastown".into()],
        };
        assert!(attached.is_attached());
        assert_eq!(
            attached.repo_base("fallback"),
            "D:/work/NAI-Lattice",
            "attach mode uses the working_dir as the repo base (no clone)"
        );
        let remote = Rig {
            working_dir: None,
            worktree_disk_budget_mib: None,
            sparse_paths: vec![],
            ..attached.clone()
        };
        assert!(!remote.is_attached());
        assert_eq!(remote.repo_base("truth-root"), "truth-root");
    }

    #[test]
    fn parse_iso_ms_round_trips_seconds_with_millis() {
        let s = "2026-11-10T05:02:14.882Z";
        let ms = parse_iso_ms(s).expect("parse");
        assert_eq!(ms % 1000, 882, "millis preserved");
        let s2 = now();
        assert!(parse_iso_ms(&s2).is_some(), "now() timestamp parses");
    }

    #[test]
    fn review_role_model_registry_routes_per_role_with_fallback() {
        let mut cfg = resolve_rig_config(&town_default(), None);
        // Unset â†’ falls back to default model.
        assert_eq!(
            resolve_review_model(&cfg, ReviewRole::ReviewerA),
            "anthropic/claude-sonnet-4.6"
        );
        // Configure per-role models (e.g. Reviewer A = Codex-backed family).
        cfg.role_models.reviewer_a = Some("openai/codex".to_string());
        cfg.role_models.adversarial = Some("anthropic/claude-opus".to_string());
        cfg.role_models.adjudicator = Some("anthropic/claude-sonnet".to_string());
        assert_eq!(resolve_review_model(&cfg, ReviewRole::ReviewerA), "openai/codex");
        assert_eq!(resolve_review_model(&cfg, ReviewRole::ReviewerB), "anthropic/claude-sonnet-4.6");
        assert_eq!(resolve_review_model(&cfg, ReviewRole::Adversarial), "anthropic/claude-opus");
        assert_eq!(resolve_review_model(&cfg, ReviewRole::Adjudicator), "anthropic/claude-sonnet");
    }

    fn town_default() -> Town {
        Town {
            town_id: "t".into(),
            owner_user_id: None,
            config: TownConfig::default(),
            created_at: now(),
        }
    }

    #[test]
    fn resolve_model_priority() {
        let town = Town {
            town_id: "t".into(),
            owner_user_id: None,
            config: TownConfig {
                default_model: Some("town-default".into()),
                role_models: RoleModels {
                    polecat: Some("town-polecat".into()),
                    refinery: None,
                    mayor: None,
                    ..Default::default()
                },
                ..Default::default()
            },
            created_at: now(),
        };
        let cfg = resolve_rig_config(&town, None);
        assert_eq!(resolve_model(&cfg, AgentRole::Polecat), "town-polecat");
        assert_eq!(resolve_model(&cfg, AgentRole::Refinery), "town-default");
        // Rig override wins for polecat.
        let rig = Rig {
            rig_id: "r".into(),
            name: "r".into(),
            git_url: "".into(),
            default_branch: "main".into(),
            town_id: "t".into(),
            config: serde_json::json!({"role_models": {"polecat": "rig-polecat"}}),
            created_at: now(),
            working_dir: None,
            worktree_disk_budget_mib: None,
            sparse_paths: vec![],
        };
        let cfg2 = resolve_rig_config(&town, Some(&rig));
        assert_eq!(resolve_model(&cfg2, AgentRole::Polecat), "rig-polecat");
    }

    #[test]
    fn dependency_topology_validates_dag_invariants() {
        use crate::orchestration::adversarial::test_store;
        let (_mem, s) = test_store();
        // Three beads: A depends on nothing, B on A, C on A and B (a diamond) â€”
        // this mirrors the "3 beads / 5 edges" graph the dashboard exposes.
        let mk = |id: &str| {
            let mut b = Bead::create(BeadType::Issue, id, None, None, vec![], serde_json::json!({}), Some("r1"), None, Some("mayor"));
            b.bead_id = id.to_string();
            s.put_bead(&b);
        };
        mk("A"); mk("B"); mk("C");
        // Build a valid DAG: Aâ†’B (B blocks on A), Aâ†’C, Bâ†’C.
        assert!(s.put_dependency_if_valid(&Dependency { bead_id: "B".into(), depends_on_bead_id: "A".into(), kind: DependencyKind::Blocks }).is_ok());
        assert!(s.put_dependency_if_valid(&Dependency { bead_id: "C".into(), depends_on_bead_id: "A".into(), kind: DependencyKind::Blocks }).is_ok());
        assert!(s.put_dependency_if_valid(&Dependency { bead_id: "C".into(), depends_on_bead_id: "B".into(), kind: DependencyKind::Blocks }).is_ok());
        let rep = s.validate_dependency_topology();
        assert!(rep.valid, "clean DAG must be valid, got issues {:?}", rep.issues);
        assert_eq!(rep.edges, 3);
        // Enumerate the actual graph edges (matches dashboard DAG data).
        let edges: Vec<(String, String)> = s.list_dependencies().iter().map(|d| (d.bead_id.clone(), d.depends_on_bead_id.clone())).collect();
        assert_eq!(edges.len(), 3);
        assert!(edges.contains(&("B".into(), "A".into())) && edges.contains(&("C".into(), "A".into())) && edges.contains(&("C".into(), "B".into())));

        // Invalid DAG shapes are REJECTED (never inserted):
        assert!(s.try_validate_dependency("A", "A").is_err(), "self-dependency rejected");
        assert!(s.try_validate_dependency("B", "A").is_err(), "duplicate edge rejected");
        assert!(s.try_validate_dependency("Z", "A").is_err(), "nonexistent source bead rejected");
        assert!(s.try_validate_dependency("C", "Q").is_err(), "nonexistent target bead rejected");
        assert!(s.try_validate_dependency("A", "C").is_err(), "cycle-creating edge (Aâ†’Câ†’Bâ†’A) rejected");
        // Graph unchanged after all rejected inserts (still a clean, 3-edge DAG).
        assert_eq!(s.list_dependencies().len(), 3);
        assert!(s.validate_dependency_topology().valid);
    }
}