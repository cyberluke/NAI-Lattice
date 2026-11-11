//! Gas Town reconciler + action dispatcher (`orchestration::reconciler`).
//!
//! Faithful reimplementation of `services/gastown/src/dos/town/reconciler.ts`
//! and `actions.ts`: read-only examine current state, emit `Action`s describing
//! needed mutations, then apply them. Rules run on every scheduler tick.

use crate::orchestration::state::{
    self, Agent, AgentRole, AgentStatus, Bead, BeadStatus, BeadType, DependencyKind,
    EffectiveConfig, MergeMode, OrchestrationStore, ReviewMetadata, Town,
};
use serde::{Deserialize, Serialize};

/// Reconciler timing constants (upstream values).
pub const MAX_DISPATCH_ATTEMPTS: u32 = 5;
pub const CIRCUIT_BREAKER_FAILURE_THRESHOLD: u32 = 20;
pub const CIRCUIT_BREAKER_WINDOW_MINUTES: i64 = 30;
pub const STUCK_REVIEW_TIMEOUT_MS: i64 = 30 * 60 * 1000;
pub const ABANDONED_MR_TIMEOUT_MS: i64 = 2 * 60 * 1000;
pub const ORPHANED_SOURCE_TIMEOUT_MS: i64 = 5 * 60 * 1000;
pub const ORPHANED_PR_REVIEW_TIMEOUT_MS: i64 = 30 * 60 * 1000;
pub const STALE_IN_PROGRESS_TIMEOUT_MS: i64 = 5 * 60 * 1000;
pub const STALLED_AUTO_IDLE_MS: i64 = 150 * 60 * 1000; // 2.5h
pub const PR_POLL_INTERVAL_MS: i64 = 60_000;
pub const PR_POLL_NULL_THRESHOLD: u32 = 10;
pub const PR_POLL_NON_TRANSIENT_THRESHOLD: u32 = 3;
pub const GUPP_ESCALATE_MS: i64 = 60 * 60 * 1000;
pub const GUPP_FORCE_STOP_MS: i64 = 2 * 60 * 60 * 1000;
pub const AGENT_GC_RETENTION_MS: i64 = 24 * 60 * 60 * 1000;
pub const MAX_LANDING_MR_ATTEMPTS: u32 = 5;
pub const LANDING_MR_COOLDOWN_BASE_MS: i64 = 30_000;
pub const LANDING_MR_COOLDOWN_MAX_MS: i64 = 30 * 60_000;
pub const LONG_HEARTBEAT_GRACE_MS: i64 = 90_000;

/// A reconciler action. Faithful to the upstream `Action` discriminated union.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Action {
    TransitionBead { bead_id: String, from: Option<BeadStatus>, to: BeadStatus, reason: String, actor: String },
    ClearBeadAssignee { bead_id: String },
    AssignBead { bead_id: String, agent_id: String },
    TransitionAgent { agent_id: String, from: Option<AgentStatus>, to: AgentStatus, reason: String },
    ResetAgentDispatchAttempts { agent_id: String },
    HookAgent { agent_id: String, bead_id: String },
    UnhookAgent { agent_id: String, reason: String },
    ClearAgentCheckpoint { agent_id: String },
    DeleteAgent { agent_id: String, reason: String },
    UpdateConvoyProgress { convoy_id: String, closed_beads: u32 },
    SetConvoyReadyToLand { convoy_id: String },
    CloseConvoy { convoy_id: String },
    FailConvoy { convoy_id: String, reason: String },
    DispatchAgent { agent_id: String, bead_id: String, rig_id: String },
    StopAgent { agent_id: String, reason: String },
    CreateMrBead {
        source_bead_id: String,
        agent_id: String,
        rig_id: String,
        branch: String,
        target_branch: String,
        pr_url: Option<String>,
        summary: Option<String>,
    },
    CreateLandingMr { convoy_id: String, rig_id: String, feature_branch: String, target_branch: String },
    PollPr { bead_id: String, pr_url: String },
    MergePr { bead_id: String, pr_url: String },
    SendNudge { agent_id: String, message: String, tier: String },
    CreateTriageRequest { agent_id: String, triage_type: String, reason: String },
    NotifyMayor { message: String },
    EmitEvent { event_name: String, data: serde_json::Value },
    SetReviewPrUrl { bead_id: String, pr_url: String },
    CloseSiblingMrs { source_bead_id: String, exclude_mr_id: String },
}

/// A reconciler event fed to the projection like upstream `town_events`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TownEvent {
    pub event_id: String,
    pub event_type: String,
    pub agent_id: Option<String>,
    pub bead_id: Option<String>,
    pub payload: serde_json::Value,
    pub created_at: String,
}

// Convenience constructors so rule code reads closely to upstream.

fn stale_ms(ts: Option<&str>, threshold_ms: i64, now_ms: i64) -> bool {
    let Some(ts) = ts else { return true };
    let ms = parse_iso_ms(ts).unwrap_or(0);
    now_ms - ms > threshold_ms
}

/// Parse an ISO-8601 UTC timestamp (with optional ms + Z) back to epoch ms.
/// Handles `YYYY-MM-DDTHH:MM:SS.mmmZ` and `YYYY-MM-DD HH:MM:SS`.
pub fn parse_iso_ms(s: &str) -> Option<i64> {
    // Extract numeric tokens in order.
    let tokens: Vec<i64> = s
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse().ok())
        .collect();
    if tokens.len() < 5 {
        return None;
    }
    let y = tokens[0];
    let m = tokens[1];
    let d = tokens[2];
    let hh = tokens[3];
    let mm = tokens[4];
    let ss = tokens.get(5).copied().unwrap_or(0);
    // Milliseconds token: the value after the '.' — but '.' splits on non-digit
    // so the fractional is already part of tokens (unless 3-digit). We take the
    // 7th token as raw ms only if it came from the fractional part. Safer: parse
    // the substring after the last '.' (or after seconds) as the +ss fraction.
    let millis = if let Some(frac) = s.rfind('.') {
        let after = &s[frac + 1..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() { 0 } else {
            // treat as milliseconds directly
            match digits.len() {
                1 => digits.parse::<i64>().ok()? * 100,
                2 => digits.parse::<i64>().ok()? * 10,
                _ => digits.chars().take(3).collect::<String>().parse().ok()?,
            }
        }
    } else {
        0
    };
    let days = date_to_days(y, m, d);
    let secs = days * 86400 + hh * 3600 + mm * 60 + ss;
    Some(secs * 1000 + millis)
}

/// Days since 1970-01-01 from a civil date (inverse of civil_from_days).
fn date_to_days(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Context the reconciler needs to compute per-rig effective configs.
pub struct ReconcileCtx<'a> {
    pub now_ms: i64,
    pub town: &'a Town,
}

/// A single reconcile pass over a town's store. Returns the actions to apply.
/// Faithful to `reconciler.reconcile` (rules 1-7 + convoy + GUPP + GC).
pub fn reconcile(ctx: &ReconcileCtx, store: &OrchestrationStore, _town: &Town) -> Vec<Action> {
    let mut actions = Vec::new();
    actions.extend(reconcile_agents(ctx, store));
    actions.extend(reconcile_beads(ctx, store));
    actions.extend(reconcile_review_queue(ctx, store));
    actions.extend(reconcile_convoys(ctx, store));
    actions.extend(reconcile_gupp(ctx, store));
    actions.extend(reconcile_gc(ctx, store));
    actions
}

fn get_effective(store: &OrchestrationStore, town: &Town, rig_id: Option<&str>) -> EffectiveConfig {
    let rig = rig_id.and_then(|r| store.get_rig(r));
    state::resolve_rig_config(town, rig.as_ref())
}

/// Per-rig max dispatch attempts (`MAX_DISPATCH_ATTEMPTS` default).
fn rig_max_dispatch(ctx: &ReconcileCtx, store: &OrchestrationStore, rig_id: Option<&str>) -> u32 {
    let ec = get_effective(store, ctx.town, rig_id);
    ec.max_dispatch_attempts.unwrap_or(MAX_DISPATCH_ATTEMPTS)
}

fn dispatch_cooldown_ms(attempts: u32) -> i64 {
    match attempts {
        0 | 1 => 30_000,
        2 => 60_000,
        3 => 120_000,
        4 => 300_000,
        _ => 600_000,
    }
}

/// Circuit breaker: beads with dispatch attempts in the window not yet closed.
fn circuit_breaker_open(store: &OrchestrationStore, now_ms: i64) -> bool {
    let cutoff = now_ms - CIRCUIT_BREAKER_WINDOW_MINUTES * 60_000;
    let count = store
        .list_beads()
        .into_iter()
        .filter(|b| {
            // P0-B/C: a CORRECTIVE iteration (new bounded plan) is not penalised by
            // the breaker — it represents a CHANGED hypothesis, so it must be able to
            // dispatch even after the identical prior strategy exhausted the breaker.
            // Everything else that keeps failing identical strategies trips the gate.
            if b.has_label("gt:corrective") { return false; }
            if b.status.is_terminal() || b.dispatch_attempts == 0 {
                return false;
            }
            b.last_dispatch_attempt_at
                .as_deref()
                .and_then(parse_iso_ms)
                .map(|t| t > cutoff)
                .unwrap_or(false)
        })
        .count();
    count as u32 >= CIRCUIT_BREAKER_FAILURE_THRESHOLD
}

// ────────────────────────────────────────────────────────────────────────────
// reconcileAgents
// ────────────────────────────────────────────────────────────────────────────

pub fn reconcile_agents(ctx: &ReconcileCtx, store: &OrchestrationStore) -> Vec<Action> {
    let mut actions = Vec::new();
    let now = ctx.now_ms;
    for agent in store.list_agents() {
        // Working agents w/ stale or missing heartbeat → idle. Mayors excluded.
        if agent.role != AgentRole::Mayor && agent.status == AgentStatus::Working {
            if agent.last_activity_at.is_none() {
                actions.push(Action::TransitionAgent {
                    agent_id: agent.bead_id.clone(),
                    from: Some(AgentStatus::Working),
                    to: AgentStatus::Idle,
                    reason: "no heartbeat received since dispatch".into(),
                });
            } else if stale_ms(agent.last_activity_at.as_deref(), LONG_HEARTBEAT_GRACE_MS, now) {
                actions.push(Action::TransitionAgent {
                    agent_id: agent.bead_id.clone(),
                    from: Some(AgentStatus::Working),
                    to: AgentStatus::Idle,
                    reason: "heartbeat lost (3 missed cycles)".into(),
                });
            } else if agent.current_hook_bead_id.is_none() {
                actions.push(Action::TransitionAgent {
                    agent_id: agent.bead_id.clone(),
                    from: Some(AgentStatus::Working),
                    to: AgentStatus::Idle,
                    reason: "working agent has no hook (gt_done already completed)".into(),
                });
            }
        }
        // Stalled agents past auto-idle threshold → idle + unhook.
        if agent.status == AgentStatus::Stalled {
            let entered = agent.stalled_at.as_deref().or(agent.last_activity_at.as_deref());
            if let Some(st) = entered.and_then(parse_iso_ms) {
                if now - st > STALLED_AUTO_IDLE_MS {
                    actions.push(Action::TransitionAgent {
                        agent_id: agent.bead_id.clone(),
                        from: Some(AgentStatus::Stalled),
                        to: AgentStatus::Idle,
                        reason: "stalled_timeout (exceeded 2h 30min)".into(),
                    });
                    if agent.current_hook_bead_id.is_some() {
                        actions.push(Action::UnhookAgent {
                            agent_id: agent.bead_id.clone(),
                            reason: "stalled_timeout (exceeded 2h 30min)".into(),
                        });
                    }
                }
            }
        }
        // Reset stale dispatch attempts (>= max && stale 30min).
        if agent.dispatch_attempts >= MAX_DISPATCH_ATTEMPTS
            && stale_ms(agent.last_activity_at.as_deref(), 30 * 60_000, now)
        {
            actions.push(Action::ResetAgentDispatchAttempts { agent_id: agent.bead_id.clone() });
        }
        // Idle agents hooked to live beads when heartbeat stale → unhook+reopen.
        if agent.status == AgentStatus::Idle && agent.current_hook_bead_id.is_some() {
            if let Some(hook) = store.get_bead(&agent.current_hook_bead_id.clone().unwrap()) {
                if hook.status.is_terminal() {
                    actions.push(Action::UnhookAgent { agent_id: agent.bead_id.clone(), reason: "hooked bead is terminal".into() });
                    actions.push(Action::ClearAgentCheckpoint { agent_id: agent.bead_id.clone() });
                } else if (hook.status == BeadStatus::InProgress || hook.status == BeadStatus::Open)
                    && stale_ms(agent.last_activity_at.as_deref(), LONG_HEARTBEAT_GRACE_MS, now)
                {
                    actions.push(Action::UnhookAgent { agent_id: agent.bead_id.clone(), reason: "idle agent hooked to live bead (dispatch failed)".into() });
                    actions.push(Action::TransitionBead {
                        bead_id: hook.bead_id.clone(),
                        from: Some(hook.status),
                        to: BeadStatus::Open,
                        reason: "agent went idle without completing work — reset for re-dispatch".into(),
                        actor: "system".into(),
                    });
                }
            }
        }
    }
    actions
}

// ────────────────────────────────────────────────────────────────────────────
// reconcileBeads
// ────────────────────────────────────────────────────────────────────────────

pub fn reconcile_beads(ctx: &ReconcileCtx, store: &OrchestrationStore) -> Vec<Action> {
    let mut actions = Vec::new();
    let now = ctx.now_ms;
    let draining = false;
    let broker_open = circuit_breaker_open(store, now);

    let issue_open = |b: &Bead| {
        b.bead_type == BeadType::Issue
            && b.status == BeadStatus::Open
            && b.rig_id.is_some()
            && b.assignee_agent_bead_id.is_none()
            && !b.has_label("gt:triage-request")
            && !b.has_label("gt:held")
            && !has_unresolved_blockers(store, &b.bead_id)
            && !is_tracked_by_staged_convoy(store, &b.bead_id)
    };

    // Rule 1: open unassigned issue beads → dispatch (or fail on max attempts).
    for b in store.list_beads().into_iter().filter(issue_open) {
        let rig = b.rig_id.clone().unwrap();
        if draining {
            continue;
        }
        let max = rig_max_dispatch(ctx, store, Some(&rig));
        if b.dispatch_attempts >= max {
            actions.push(Action::TransitionBead {
                bead_id: b.bead_id.clone(),
                from: Some(BeadStatus::Open),
                to: BeadStatus::Failed,
                reason: format!("max dispatch attempts exceeded ({})", b.dispatch_attempts),
                actor: "system".into(),
            });
            continue;
        }
        let cooldown = dispatch_cooldown_ms(b.dispatch_attempts);
        if !stale_ms(b.last_dispatch_attempt_at.as_deref(), cooldown, now) {
            continue;
        }
        if broker_open {
            continue;
        }
        actions.push(Action::DispatchAgent { agent_id: String::new(), bead_id: b.bead_id.clone(), rig_id: b.rig_id.clone().unwrap_or_default() });
    }

    // Rule 1b: open beads with stale assignee (agent not hooked) → clear.
    for b in store.list_beads() {
        if b.bead_type == BeadType::Issue
            && b.status == BeadStatus::Open
            && b.assignee_agent_bead_id.is_some()
            && b.rig_id.is_some()
        {
            let assigned = b.assignee_agent_bead_id.clone().unwrap();
            if assigned == "system" || b.has_label("gt:triage-request") {
                continue;
            }
            let hooked = store
                .get_agent(&assigned)
                .map(|a| a.current_hook_bead_id.as_deref() == Some(b.bead_id.as_str()))
                .unwrap_or(false);
            if !hooked {
                actions.push(Action::ClearBeadAssignee { bead_id: b.bead_id.clone() });
            }
        }
    }

    // Rule 2: idle agents with hooks → dispatch (non-refinery).
    for agent in store.list_agents() {
        if agent.status == AgentStatus::Idle
            && agent.current_hook_bead_id.is_some()
            && agent.role != AgentRole::Refinery
        {
            let hook = agent.current_hook_bead_id.clone().unwrap();
            let Some(hb) = store.get_bead(&hook) else { continue };
            if hb.status != BeadStatus::Open {
                continue;
            }
            if hb.dispatch_attempts >= MAX_DISPATCH_ATTEMPTS {
                actions.push(Action::TransitionBead {
                    bead_id: hook,
                    from: None,
                    to: BeadStatus::Failed,
                    reason: format!("max dispatch attempts exceeded ({})", hb.dispatch_attempts),
                    actor: "system".into(),
                });
                actions.push(Action::UnhookAgent { agent_id: agent.bead_id.clone(), reason: "max dispatch attempts".into() });
                continue;
            }
            let cooldown = dispatch_cooldown_ms(hb.dispatch_attempts);
            if !stale_ms(hb.last_dispatch_attempt_at.as_deref(), cooldown, now) {
                continue;
            }
            if has_unresolved_blockers(store, &hook) {
                continue;
            }
            if draining || broker_open {
                continue;
            }
            actions.push(Action::DispatchAgent {
                agent_id: agent.bead_id.clone(),
                bead_id: hook,
                rig_id: hb.rig_id.clone().or(agent.rig_id.clone()).unwrap_or_default(),
            });
        }
    }

    // Rule 3: in_progress issue beads with no working agent → reopen.
    for b in store.list_beads() {
        if b.bead_type != BeadType::Issue || b.status != BeadStatus::InProgress {
            continue;
        }
        if !stale_ms(Some(&b.updated_at), STALE_IN_PROGRESS_TIMEOUT_MS, now) {
            continue;
        }
        let alive = store
            .list_agents()
            .into_iter()
            .any(|a| {
                a.current_hook_bead_id.as_deref() == Some(b.bead_id.as_str())
                    && (a.status == AgentStatus::Working
                        || a.status == AgentStatus::Stalled
                        || a.last_activity_at
                            .as_deref()
                            .map(|t| now - parse_iso_ms(t).unwrap_or(0) < LONG_HEARTBEAT_GRACE_MS)
                            .unwrap_or(false))
            });
        if alive {
            continue;
        }
        if b.dispatch_attempts >= MAX_DISPATCH_ATTEMPTS {
            actions.push(Action::TransitionBead {
                bead_id: b.bead_id.clone(),
                from: Some(BeadStatus::InProgress),
                to: BeadStatus::Failed,
                reason: "agent lost, max dispatch attempts exhausted".into(),
                actor: "system".into(),
            });
            actions.push(Action::ClearBeadAssignee { bead_id: b.bead_id.clone() });
        } else {
            actions.push(Action::TransitionBead {
                bead_id: b.bead_id.clone(),
                from: Some(BeadStatus::InProgress),
                to: BeadStatus::Open,
                reason: "agent lost".into(),
                actor: "system".into(),
            });
            actions.push(Action::ClearBeadAssignee { bead_id: b.bead_id.clone() });
        }
    }

    // Rule 4: in_review beads with all-MR-terminal → close or reopen.
    for b in store.list_beads() {
        if b.bead_type != BeadType::Issue || b.status != BeadStatus::InReview {
            continue;
        }
        if !stale_ms(Some(&b.updated_at), ORPHANED_SOURCE_TIMEOUT_MS, now) {
            continue;
        }
        // MR beads tracking this source.
        let mrs: Vec<Bead> = store
            .dependencies_for(&b.bead_id)
            .into_iter()
            .filter(|d| d.kind == DependencyKind::Tracks)
            .filter_map(|d| store.get_bead(&d.depends_on_bead_id))
            .filter(|mr| mr.bead_type == BeadType::MergeRequest)
            .collect();
        if mrs.is_empty() {
            continue;
        }
        let all_terminal = mrs.iter().all(|mr| mr.status.is_terminal());
        if !all_terminal {
            continue;
        }
        let any_merged = mrs.iter().any(|mr| mr.status == BeadStatus::Closed);
        if any_merged {
            actions.push(Action::TransitionBead {
                bead_id: b.bead_id.clone(),
                from: Some(BeadStatus::InReview),
                to: BeadStatus::Closed,
                reason: "MR merged (reconciler safety net)".into(),
                actor: "system".into(),
            });
        } else {
            actions.push(Action::TransitionBead {
                bead_id: b.bead_id.clone(),
                from: Some(BeadStatus::InReview),
                to: BeadStatus::Open,
                reason: "all reviews failed".into(),
                actor: "system".into(),
            });
            actions.push(Action::ClearBeadAssignee { bead_id: b.bead_id.clone() });
        }
    }

    if broker_open {
        actions.push(Action::NotifyMayor {
            message: format!(
                "Dispatch circuit breaker is OPEN: {} dispatch failures in the last {} minutes",
                CIRCUIT_BREAKER_FAILURE_THRESHOLD, CIRCUIT_BREAKER_WINDOW_MINUTES
            ),
        });
    }
    actions
}

/// Whether a bead has an unresolved `blocks` blocker (blended with MR status).
///
/// P0-A invariant: a `blocks` blocker releases its downstream bead ONLY when the
/// blocker achieved a real VERIFIED_10_10 — Inquisitor verdict COMPLETE + all
/// deterministic task gates PASS + source_revision matching the current epoch.
/// A manually Closed, Failed, or revision-superseded blocker is NOT sufficient to
/// unlock the next primary bead. This is enforced atomically here, in the
/// scheduler's single dependency-release decision.
fn has_unresolved_blockers(store: &OrchestrationStore, bead_id: &str) -> bool {
    store
        .dependencies_for(bead_id)
        .into_iter()
        .filter(|d| d.kind == DependencyKind::Blocks)
        .any(|d| {
            let Some(blocker) = store.get_bead(&d.depends_on_bead_id) else {
                return false;
            };
            // Release only when the blocker carries a current VERIFIED_10_10 stamp
            // (terminal AND COMPLETE AND all gates PASS AND revision still matches).
            let verified = store.is_bead_verified_current(&blocker.bead_id)
                && !store
                    .dependents_on(&blocker.bead_id)
                    .into_iter()
                    .any(|dd| {
                        dd.kind == DependencyKind::Tracks
                            && store
                                .get_bead(&dd.bead_id)
                                .map(|child| {
                                    child.bead_type == BeadType::MergeRequest
                                        && !child.status.is_terminal()
                                })
                                .unwrap_or(false)
                    });
            !verified
        })
}

/// Whether a bead is tracked by a staged convoy.
fn is_tracked_by_staged_convoy(store: &OrchestrationStore, bead_id: &str) -> bool {
    store
        .dependencies_for(bead_id)
        .into_iter()
        .filter(|d| d.kind == DependencyKind::Tracks)
        .filter_map(|d| store.get_convoy_metadata(&d.depends_on_bead_id))
        .any(|cm| cm.staged)
}

// ────────────────────────────────────────────────────────────────────────────
// reconcileReviewQueue — refinery dispatch, PR polling
// ────────────────────────────────────────────────────────────────────────────

pub fn reconcile_review_queue(ctx: &ReconcileCtx, store: &OrchestrationStore) -> Vec<Action> {
    let mut actions = Vec::new();
    let now = ctx.now_ms;
    let broker_open = circuit_breaker_open(store, now);

    // All open/in_progress MR beads.
    let mrs: Vec<Bead> = store
        .list_beads()
        .into_iter()
        .filter(|b| {
            b.bead_type == BeadType::MergeRequest
                && (b.status == BeadStatus::Open || b.status == BeadStatus::InProgress)
        })
        .collect();

    for mr_row in &mrs {
        let mr = mr_row;
        let meta = store.get_review_metadata(&mr.bead_id);
        let pr_url = meta.as_ref().and_then(|m| m.pr_url.clone());
        let meta_meta: &serde_json::Value = &mr.metadata;

        // Rule 1: PR-strategy in_progress → poll_pr (rate-limited by last_poll_at).
        if mr.status == BeadStatus::InProgress {
            if let Some(purl) = &pr_url {
                let last_poll = meta_meta["last_poll_at"].as_str().and_then(parse_iso_ms);
                let since = last_poll.map(|t| now - t).unwrap_or(i64::MAX);
                if since >= PR_POLL_INTERVAL_MS {
                    actions.push(Action::PollPr { bead_id: mr.bead_id.clone(), pr_url: purl.clone() });
                }
                let auto_merge_pending = meta_meta["auto_merge_pending"].as_bool().unwrap_or(false);
                if auto_merge_pending {
                    actions.push(Action::MergePr { bead_id: mr.bead_id.clone(), pr_url: purl.clone() });
                }
            }
            // Rule 2: stuck MR in_progress, no PR, stale → reopen.
            if pr_url.is_none() && stale_ms(Some(&mr.updated_at), STUCK_REVIEW_TIMEOUT_MS, now) {
                if has_unresolved_rework_blockers(store, &mr.bead_id) {
                    continue;
                }
                let working = has_working_agent(store, &mr.bead_id);
                if !working {
                    actions.push(Action::TransitionBead {
                        bead_id: mr.bead_id.clone(),
                        from: Some(BeadStatus::InProgress),
                        to: BeadStatus::Open,
                        reason: "stuck review, no working agent".into(),
                        actor: "system".into(),
                    });
                }
            }
            // Rule 3: abandoned MR in_progress no PR no agent stale.
            if pr_url.is_none() && stale_ms(Some(&mr.updated_at), ABANDONED_MR_TIMEOUT_MS, now) {
                if has_unresolved_rework_blockers(store, &mr.bead_id) {
                    continue;
                }
                let any = store
                    .list_agents()
                    .into_iter()
                    .any(|a| a.current_hook_bead_id.as_deref() == Some(mr.bead_id.as_str()));
                if !any {
                    actions.push(Action::TransitionBead {
                        bead_id: mr.bead_id.clone(),
                        from: Some(BeadStatus::InProgress),
                        to: BeadStatus::Open,
                        reason: "abandoned, no agent hooked".into(),
                        actor: "system".into(),
                    });
                }
            }
        }
    }

    // Group open MRs by rig for refinery dispatch.
    let open_by_rig: std::collections::BTreeMap<String, Vec<Bead>> = mrs
        .into_iter()
        .filter(|b| b.status == BeadStatus::Open && b.rig_id.is_some())
        .fold(std::collections::BTreeMap::new(), |mut m, b| {
            m.entry(b.rig_id.clone().unwrap()).or_default().push(b);
            m
        });

    for (rig_id, open_mrs) in open_by_rig {
        let ec = get_effective(store, ctx.town, Some(&rig_id));
        // code_review=false → only dispatch refinery for convoy review-and-merge
        // or system landing MRs; otherwise fast-track poll_pr (handled by poll
        // rule above when in_progress). We keep refinery dispatch for code_review=true.
        if !ec.code_review {
            continue;
        }
        // When per-bead adversarial review is enabled, the per-bead MR lifecycle
        // is owned by the adversarial policy (dispatches Reviewer A/B/Adversarial/
        // Adjudicator, then merges on Approve, else rejects/escalates). The base
        // reconciler's refinery dispatch must NOT auto-merge such MRs — only
        // landing MRs (source=convoy) still flow through the refinery for a final
        // merge. Suppress here so the two paths never double-merge.
        if ec.adversarial_review {
            let non_landing = open_mrs.iter().any(|mr| !mr.metadata["convoy_landing"].as_bool().unwrap_or(false));
            if non_landing {
                continue; // per-bead adversarial chain owns these MRs
            }
        }
        // Serial refinery: max one in_progress MR without pr_url per rig.
        let in_progress_locked = store
            .list_beads()
            .into_iter()
            .any(|b| {
                b.bead_type == BeadType::MergeRequest
                    && b.status == BeadStatus::InProgress
                    && b.rig_id.as_deref() == Some(rig_id.as_str())
                    && store.get_review_metadata(&b.bead_id).and_then(|m| m.pr_url).is_none()
            });
        if in_progress_locked {
            continue;
        }
        // Find an idle refinery for this rig.
        let refinery: Vec<Agent> = store
            .list_agents()
            .into_iter()
            .filter(|a| a.role == AgentRole::Refinery && a.rig_id.as_deref() == Some(rig_id.as_str()))
            .collect();
        let refinery_idle = refinery
            .iter()
            .find(|a| a.status == AgentStatus::Idle && a.current_hook_bead_id.is_none());
        let oldest = open_mrs
            .iter()
            .min_by_key(|b| parse_iso_ms(&b.created_at).unwrap_or(0));
        let Some(oldest) = oldest else { continue };

        if broker_open {
            continue;
        }

        match refinery_idle {
            Some(ref_agent) => {
                if ref_agent.status == AgentStatus::Idle && ref_agent.current_hook_bead_id.is_none() {
                    actions.push(Action::TransitionBead {
                        bead_id: oldest.bead_id.clone(),
                        from: Some(BeadStatus::Open),
                        to: BeadStatus::InProgress,
                        reason: "popped for review".into(),
                        actor: "system".into(),
                    });
                    actions.push(Action::HookAgent { agent_id: ref_agent.bead_id.clone(), bead_id: oldest.bead_id.clone() });
                    actions.push(Action::DispatchAgent { agent_id: ref_agent.bead_id.clone(), bead_id: oldest.bead_id.clone(), rig_id: rig_id.clone() });
                }
            }
            None => {
                // No refinery exists — getOrCreate on dispatch with empty agent_id
                // infers role from bead type. But default dispatch in applyAction
                // only picks agent when agent_id empty; here we create + hook via
                // a dispatch_agent with empty id (applyAction creates refinery).
                actions.push(Action::TransitionBead {
                    bead_id: oldest.bead_id.clone(),
                    from: Some(BeadStatus::Open),
                    to: BeadStatus::InProgress,
                    reason: "popped for review (creating refinery)".into(),
                    actor: "system".into(),
                });
                actions.push(Action::DispatchAgent { agent_id: String::new(), bead_id: oldest.bead_id.clone(), rig_id: rig_id.clone() });
            }
        }
    }

    // Rule 6: idle refinery hooked to in_progress MR → re-dispatch.
    for ref_agent in store.list_agents() {
        if ref_agent.role != AgentRole::Refinery
            || ref_agent.status != AgentStatus::Idle
            || ref_agent.current_hook_bead_id.is_none()
        {
            continue;
        }
        let hook = ref_agent.current_hook_bead_id.clone().unwrap();
        let Some(mr) = store.get_bead(&hook) else { continue };
        if mr.bead_type != BeadType::MergeRequest || mr.status != BeadStatus::InProgress {
            continue;
        }
        let awaiting = mr.metadata["awaiting_approval"].as_bool().unwrap_or(false);
        if awaiting {
            continue;
        }
        if mr.dispatch_attempts >= max_for_rig(ctx, store, mr.rig_id.as_deref()) {
            actions.push(Action::TransitionBead {
                bead_id: hook.clone(),
                from: None,
                to: BeadStatus::Failed,
                reason: format!("refinery max dispatch attempts exceeded ({})", mr.dispatch_attempts),
                actor: "system".into(),
            });
            actions.push(Action::UnhookAgent { agent_id: ref_agent.bead_id.clone(), reason: "max dispatch attempts".into() });
            continue;
        }
        let cooldown = dispatch_cooldown_ms(mr.dispatch_attempts);
        if !stale_ms(mr.last_dispatch_attempt_at.as_deref(), cooldown, ctx.now_ms) {
            continue;
        }
        if broker_open {
            continue;
        }
        let rig = mr.rig_id.clone().or(ref_agent.rig_id.clone()).unwrap_or_default();
        actions.push(Action::DispatchAgent { agent_id: ref_agent.bead_id.clone(), bead_id: hook.clone(), rig_id: rig });
    }

    // Rule 7: working refinery hooked to terminal MR → stop + unhook + idle.
    for ref_agent in store.list_agents() {
        if ref_agent.role != AgentRole::Refinery {
            continue;
        }
        if (ref_agent.status == AgentStatus::Working || ref_agent.status == AgentStatus::Stalled)
            && ref_agent.current_hook_bead_id.is_some()
        {
            let hook = ref_agent.current_hook_bead_id.clone().unwrap();
            if let Some(mr) = store.get_bead(&hook) {
                if mr.status.is_terminal() {
                    actions.push(Action::StopAgent { agent_id: ref_agent.bead_id.clone(), reason: format!("MR bead {hook} is terminal") });
                    actions.push(Action::UnhookAgent { agent_id: ref_agent.bead_id.clone(), reason: "MR bead terminal — cleanup".into() });
                    actions.push(Action::TransitionAgent { agent_id: ref_agent.bead_id.clone(), from: Some(ref_agent.status), to: AgentStatus::Idle, reason: "MR bead terminal — cleanup".into() });
                }
            }
        }
    }

    actions
}

fn max_for_rig(ctx: &ReconcileCtx, store: &OrchestrationStore, rig_id: Option<&str>) -> u32 {
    rig_max_dispatch(ctx, store, rig_id)
}

fn has_unresolved_rework_blockers(store: &OrchestrationStore, mr_bead_id: &str) -> bool {
    store
        .dependencies_for(mr_bead_id)
        .into_iter()
        .filter(|d| d.kind == DependencyKind::Blocks)
        .filter_map(|d| store.get_bead(&d.depends_on_bead_id))
        .any(|b| !b.status.is_terminal())
}

fn has_working_agent(store: &OrchestrationStore, bead_id: &str) -> bool {
    store
        .list_agents()
        .into_iter()
        .any(|a| {
            a.current_hook_bead_id.as_deref() == Some(bead_id)
                && (a.status == AgentStatus::Working || a.status == AgentStatus::Stalled)
        })
}

// ────────────────────────────────────────────────────────────────────────────
// reconcileConvoys — progress, landing MR creation
// ────────────────────────────────────────────────────────────────────────────

pub fn reconcile_convoys(ctx: &ReconcileCtx, store: &OrchestrationStore) -> Vec<Action> {
    let mut actions = Vec::new();
    let now = ctx.now_ms;

    let convoys: Vec<Bead> = store
        .list_beads()
        .into_iter()
        .filter(|b| b.bead_type == BeadType::Convoy && b.status == BeadStatus::Open)
        .collect();

    for convoy_bead_ref in convoys {
        let convoy_bead = &convoy_bead_ref;
        let cid = &convoy_bead.bead_id;
        let Some(cm) = store.get_convoy_metadata(cid) else { continue };
        let (total, closed) = state::convoy_progress(store, cid);
        let has_inflight_reviews = store
            .list_dependencies()
            .into_iter()
            .filter(|d| d.kind == DependencyKind::Tracks && d.depends_on_bead_id == *cid)
            .any(|d| {
                store
                    .dependents_on(&d.bead_id)
                    .into_iter()
                    .any(|dd| {
                        dd.kind == DependencyKind::Tracks
                            && store
                                .get_bead(&dd.bead_id)
                                .map(|mr| {
                                    mr.bead_type == BeadType::MergeRequest
                                        && (mr.status == BeadStatus::Open
                                            || mr.status == BeadStatus::InProgress)
                                })
                                .unwrap_or(false)
                    })
            });

        if closed != cm.closed_beads {
            actions.push(Action::UpdateConvoyProgress { convoy_id: cid.clone(), closed_beads: closed });
        }
        let all_done = closed >= total && total > 0 && !has_inflight_reviews;
        if !all_done {
            continue;
        }

        // ready_to_land flag.
        let ready = convoy_bead.metadata["ready_to_land"].as_bool().unwrap_or(false);

        if cm.merge_mode == MergeMode::ReviewThenLand && cm.feature_branch.is_some() {
            if !ready {
                actions.push(Action::SetConvoyReadyToLand { convoy_id: cid.clone() });
                continue;
            }
            // Check existing landing MRs for this convoy.
            let landing_mrs: Vec<Bead> = store
                .dependents_on(cid)
                .into_iter()
                .filter(|d| d.kind == DependencyKind::Tracks)
                .filter_map(|d| store.get_bead(&d.bead_id))
                .filter(|b| b.bead_type == BeadType::MergeRequest)
                .collect();
            let has_merged_landing = landing_mrs.iter().any(|m| m.status == BeadStatus::Closed);
            if has_merged_landing {
                actions.push(Action::CloseConvoy { convoy_id: cid.clone() });
                continue;
            }
            let has_active = landing_mrs
                .iter()
                .any(|m| m.status == BeadStatus::Open || m.status == BeadStatus::InProgress);
            if has_active {
                continue;
            }
            let has_pending_external = landing_mrs.iter().any(|m| {
                m.status == BeadStatus::Failed && m.metadata["awaiting_approval"].as_bool().unwrap_or(false)
            });
            if has_pending_external {
                continue;
            }
            // Cooldown between landing attempts.
            let attempts = convoy_bead.metadata["landing_mr_attempts"].as_u64().unwrap_or(0) as u32;
            if attempts >= MAX_LANDING_MR_ATTEMPTS {
                actions.push(Action::FailConvoy { convoy_id: cid.clone(), reason: format!("Landing MR creation failed after {} attempts", MAX_LANDING_MR_ATTEMPTS) });
                continue;
            }
            if let Some(last_at) = convoy_bead.metadata["last_landing_mr_attempt_at"].as_str() {
                if let Some(last_ms) = parse_iso_ms(last_at) {
                    let elapsed = now - last_ms;
                    let cooldown = (2i64.pow(attempts.min(30)) * LANDING_MR_COOLDOWN_BASE_MS)
                        .min(LANDING_MR_COOLDOWN_MAX_MS);
                    if elapsed < cooldown {
                        continue;
                    }
                }
            }
            // Determine rig for the landing MR (first tracked bead's rig).
            let rig_id = store
                .list_dependencies()
                .into_iter()
                .find(|d| d.kind == DependencyKind::Tracks && d.depends_on_bead_id == *cid)
                .and_then(|d| store.get_bead(&d.bead_id))
                .and_then(|b| b.rig_id);
            if let Some(rig_id) = rig_id {
                let rig = store.get_rig(&rig_id);
                let target = rig.as_ref().map(|r| r.default_branch.clone()).unwrap_or_else(|| "main".to_string());
                actions.push(Action::CreateLandingMr {
                    convoy_id: cid.clone(),
                    rig_id,
                    feature_branch: cm.feature_branch.clone().unwrap_or_default(),
                    target_branch: target,
                });
            }
        } else {
            // review-and-merge or no feature branch → auto-close.
            actions.push(Action::CloseConvoy { convoy_id: cid.clone() });
        }
    }
    actions
}

// ────────────────────────────────────────────────────────────────────────────
// reconcileGUPP + GC
// ────────────────────────────────────────────────────────────────────────────

pub fn reconcile_gupp(_ctx: &ReconcileCtx, store: &OrchestrationStore) -> Vec<Action> {
    let mut actions = Vec::new();
    let now = _ctx.now_ms;
    for agent in store.list_agents() {
        if agent.role == AgentRole::Mayor
            || (agent.status != AgentStatus::Working && agent.status != AgentStatus::Stalled)
        {
            continue;
        }
        let activity = agent.last_event_at.clone().or_else(|| agent.last_activity_at.clone());
        let Some(activity) = activity.and_then(|x| parse_iso_ms(&x)) else { continue };
        let elapsed = now - activity;
        if elapsed < 0 {
            continue;
        }
        // Stalled auto-idle owners are handled by reconcileAgents.
        if agent.status == AgentStatus::Stalled {
            let since = agent.stalled_at.clone().or_else(|| agent.last_activity_at.clone());
            if let Some(s) = since.and_then(|x| parse_iso_ms(&x)) {
                if now - s > STALLED_AUTO_IDLE_MS {
                    continue;
                }
            }
        }
        if elapsed > GUPP_FORCE_STOP_MS {
            actions.push(Action::TransitionAgent { agent_id: agent.bead_id.clone(), from: Some(agent.status), to: AgentStatus::Stalled, reason: "GUPP force stop — no SDK activity for 2h".into() });
            actions.push(Action::StopAgent { agent_id: agent.bead_id.clone(), reason: "exceeded 2h GUPP limit".into() });
            actions.push(Action::CreateTriageRequest { agent_id: agent.bead_id.clone(), triage_type: "stuck_agent".into(), reason: "GUPP force stop".into() });
        } else if elapsed > GUPP_ESCALATE_MS {
            if !has_recent_nudge(store, &agent.bead_id, "escalate", now) {
                actions.push(Action::SendNudge { agent_id: agent.bead_id.clone(), message: "You have been working for over 1 hour without completing your task. Please wrap up or report if you are stuck.".into(), tier: "escalate".into() });
                actions.push(Action::CreateTriageRequest { agent_id: agent.bead_id.clone(), triage_type: "stuck_agent".into(), reason: "GUPP escalation".into() });
            }
        } else if elapsed > 15 * 60_000 {
            if agent.active_tools.is_empty() && !has_recent_nudge(store, &agent.bead_id, "warn", now) {
                actions.push(Action::SendNudge { agent_id: agent.bead_id.clone(), message: "You have been idle for 15 minutes with no tool activity. Please check your progress.".into(), tier: "warn".into() });
            }
        }
    }
    actions
}

fn has_recent_nudge(store: &OrchestrationStore, agent_id: &str, tier: &str, now: i64) -> bool {
    // Nudges persisted under a lightweight keyspace; we dedupe via reconciled
    // counters. For parity with upstream (60-min window), we read our nudges
    // from the store's bead metadata (nudge log). Practical: track in agent
    // checkpoint? We store a nudge marker in the agent's metadata via a helper
    // that ScanKeyPrefix providers. Since nudges are ephemeral coordination, we
    // keep a small durable nudge record here.
    store
        .list_agents()
        .into_iter()
        .find(|a| a.bead_id == agent_id)
        .map(|a| {
            a.checkpoint
                .as_ref()
                .and_then(|c| c.get("nudge")?.get(tier)?.as_str())
                .and_then(|ts| parse_iso_ms(ts))
                .map(|t| now - t < 60 * 60_000)
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

pub fn reconcile_gc(_ctx: &ReconcileCtx, store: &OrchestrationStore) -> Vec<Action> {
    let mut actions = Vec::new();
    let now = _ctx.now_ms;
    for agent in store.list_agents() {
        if (agent.status == AgentStatus::Idle || agent.status == AgentStatus::Dead)
            && (agent.role == AgentRole::Polecat || agent.role == AgentRole::Refinery)
            && agent.current_hook_bead_id.is_none()
        {
            if stale_ms(agent.last_activity_at.as_deref(), AGENT_GC_RETENTION_MS, now) {
                actions.push(Action::DeleteAgent { agent_id: agent.bead_id.clone(), reason: "GC: idle > 24h".into() });
            }
        }
    }
    actions
}

/// Read a convoy view from a convoy bead + its metadata.
pub fn to_convoy(bead: &Bead, cm: &state::ConvoyMetadata) -> state::Convoy {
    state::Convoy {
        bead_id: bead.bead_id.clone(),
        title: bead.title.clone(),
        status: bead.status,
        staged: cm.staged,
        total_beads: cm.total_beads,
        closed_beads: cm.closed_beads,
        created_by: bead.created_by.clone(),
        created_at: bead.created_at.clone(),
        landed_at: cm.landed_at.clone(),
        feature_branch: cm.feature_branch.clone(),
        merge_mode: cm.merge_mode,
        metadata: bead.metadata.clone(),
    }
}

/// Read a review-queue entry view from an MR bead + metadata.
pub fn to_review_entry(bead: &Bead, rm: &ReviewMetadata) -> state::ReviewQueueEntry {
    state::ReviewQueueEntry {
        id: bead.bead_id.clone(),
        agent_id: bead.created_by.clone().unwrap_or_default(),
        source_bead_id: bead.parent_bead_id.clone(),
        rig_id: bead.rig_id.clone(),
        branch: rm.branch.clone(),
        target_branch: rm.target_branch.clone(),
        pr_url: rm.pr_url.clone(),
        status: match bead.status {
            BeadStatus::Open => state::ReviewStatus::Pending,
            BeadStatus::InProgress => state::ReviewStatus::Running,
            BeadStatus::Closed => state::ReviewStatus::Merged,
            _ => state::ReviewStatus::Failed,
        },
        summary: None,
        created_at: bead.created_at.clone(),
        updated_at: bead.updated_at.clone(),
        processed_at: if bead.updated_at == bead.created_at { None } else { Some(bead.updated_at.clone()) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_iso_roundtrip() {
        let iso = "2026-11-10T00:17:47.000Z";
        let ms = parse_iso_ms(iso).unwrap();
        assert!(ms > 1_700_000_000_000);
        // monotonic
        let later = parse_iso_ms("2026-11-10T00:18:47.000Z").unwrap();
        assert!(later > ms);
    }

    #[test]
    fn stale_ms_matches() {
        let now = parse_iso_ms("2026-11-10T00:18:00.000Z").unwrap_or(1_700_000_000_000);
        let old = "2026-11-10T00:00:00.000Z";
        assert!(stale_ms(Some(old), 60_000, now));
        assert!(!stale_ms(Some("2026-11-10T00:17:30.000Z"), 60_000, now));
        assert!(stale_ms(None, 60_000, now));
    }

    #[test]
    fn unresolved_blockers_detects_pending() {
        // Use an in-mem hotstore? Use a fake via LiteHotStore below.
    }

    /// P0-A: a `blocks` blocker only releases VERIFIED_10_10 closure.
    /// A manually Closed (unverified), a Failed, or a revision-superseded
    /// verified bead must NOT unlock the downstream primary bead.
    #[test]
    fn p0a_manual_close_does_not_unlock_next_primary() {
        use crate::orchestration::state::{BeadStatus, Dependency, DependencyKind};
        let (_mem, store) = crate::orchestration::adversarial::test_store();
        store.hot_put("gastown/portability/epoch", "rev-1");

        let mk = |title: &str| {
            let b = crate::orchestration::state::Bead::create(
                crate::orchestration::state::BeadType::Issue, title, None, None,
                vec![], serde_json::json!({}), None, None, Some("mayor"),
            );
            let id = b.bead_id.clone();
            store.put_bead(&b);
            id
        };
        let b1 = mk("b1"); let b2 = mk("b2"); let b3 = mk("b3");
        store.put_dependency(&Dependency { bead_id: b2.clone(), depends_on_bead_id: b1.clone(), kind: DependencyKind::Blocks });
        store.put_dependency(&Dependency { bead_id: b3.clone(), depends_on_bead_id: b2.clone(), kind: DependencyKind::Blocks });

        // 1) b1 manually closed, never verified → does NOT release b2.
        store.update_bead_status(b1.clone(), BeadStatus::Closed, "operator".into(), None);
        assert!(has_unresolved_blockers(&store, &b2), "manual close must NOT unlock b2");

        // 2) b1 verified against CURRENT rev-1 → releases b2.
        store.mark_bead_verified(&b1, "rev-1", "COMPLETE", 8, 8);
        assert!(!has_unresolved_blockers(&store, &b2), "verified(current) must unlock b2");
        // Idempotent: re-stamping the same revision does not regress.
        store.mark_bead_verified(&b1, "rev-1", "COMPLETE", 8, 8);
        assert!(!has_unresolved_blockers(&store, &b2));

        // 3) rev-2 arrives (source revision changed) → b1's stamp is stale,
        //    even though it is still Closed. Must NOT release b2 (or b3).
        store.hot_put("gastown/portability/epoch", "rev-2");
        assert!(has_unresolved_blockers(&store, &b2), "superseded revision must block b2 again");
        assert!(has_unresolved_blockers(&store, &b3));
    }
}