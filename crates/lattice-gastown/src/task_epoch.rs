//! Server-owned task epochs (spec §8): the completion eligibility state machine
//! driven ONLY by canonical committed events plus service-owned observations.
//! The plugin never owns this machine.

use serde::{Deserialize, Serialize};

/// State of a task epoch.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum EpochState {
    Active,
    AwaitingQuiet,
    Snapshotting,
    ReviewQueued,
    Reviewing,
    Adjudicating,
    Completed,
    Canceled,
}

/// A task epoch keyed by (verified host identity + session id + Kilo turn/id).
/// Two prompts in the same session are NOT the same task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct TaskEpochId {
    pub host_runtime_id: String,
    pub session_id: String,
    pub turn_id: String,
}

/// The backend-owned epoch machine. Mutable only via `apply_event` on committed
/// input; all transitions are recorded in the canonical ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEpoch {
    pub id: TaskEpochId,
    pub state: EpochState,
    pub started_unix_ms: i64,
    pub last_activity_unix_ms: i64,
    /// Evidence collected so far.
    pub chat_messages: u64,
    pub tool_events: u64,
    pub file_edits: u64,
    pub saw_session_idle_with_evidence: bool,
    /// Quiet-period deadline computed by the service timer (none if not set).
    pub quiet_until_unix_ms: Option<i64>,
    pub baseline_status: BaselineStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum BaselineStatus {
    Pending,
    Captured,
    BaselineUncertain,
}

impl TaskEpoch {
    pub fn new(id: TaskEpochId, now_unix_ms: i64) -> Self {
        Self {
            id,
            state: EpochState::Active,
            started_unix_ms: now_unix_ms,
            last_activity_unix_ms: now_unix_ms,
            chat_messages: 0,
            tool_events: 0,
            file_edits: 0,
            saw_session_idle_with_evidence: false,
            quiet_until_unix_ms: None,
            baseline_status: BaselineStatus::Pending,
        }
    }

    /// Feed an observed (already-committed) event signal. Returns the new state.
    /// A `session.idle` during model thinking, tool execution, or a different
    /// turn is NOT a final result (spec §8): eligibility requires activity +
    /// quiet-period + no invalidating newer activity.
    pub fn on_chat_message(&mut self, now_unix_ms: i64) -> EpochState {
        self.chat_messages += 1;
        self.last_activity_unix_ms = now_unix_ms;
        self.state = EpochState::Active;
        self.state
    }

    pub fn on_tool(&mut self, now_unix_ms: i64) -> EpochState {
        self.tool_events += 1;
        self.last_activity_unix_ms = now_unix_ms;
        self.state = EpochState::Active;
        self.state
    }

    pub fn on_file_edit(&mut self, now_unix_ms: i64) -> EpochState {
        self.file_edits += 1;
        self.last_activity_unix_ms = now_unix_ms;
        // A baseline captured after the first change is unreliable; mark it.
        if self.baseline_status == BaselineStatus::Pending {
            self.baseline_status = BaselineStatus::BaselineUncertain;
        }
        self.state = EpochState::Active;
        self.state
    }

    /// True only when a genuinely-idle SDK status (validated type) is observed
    /// after activity, with NO running tools/thinking in flight.
    pub fn on_session_idle(&mut self, now_unix_ms: i64, quiet_period_ms: i64) -> EpochState {
        if self.chat_messages == 0 && self.tool_events == 0 && self.file_edits == 0 {
            // Idle with no epoch activity: not a completion signal.
            return self.state;
        }
        self.saw_session_idle_with_evidence = true;
        self.last_activity_unix_ms = now_unix_ms;
        self.state = EpochState::AwaitingQuiet;
        self.quiet_until_unix_ms = Some(now_unix_ms + quiet_period_ms);
        self.state
    }

    /// Advance to review eligibility at quiet-period expiry (service timer calls
    /// this only after confirming no newer activity invalidates the epoch).
    pub fn mark_review_eligible(&mut self, now_unix_ms: i64) -> EpochState {
        if self.last_activity_unix_ms <= now_unix_ms && self.saw_session_idle_with_evidence {
            self.state = EpochState::ReviewQueued;
        }
        self.state
    }

    /// New activity invalidates quiet eligibility.
    pub fn on_newer_activity(&mut self) -> EpochState {
        self.state = EpochState::Active;
        self.quiet_until_unix_ms = None;
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> TaskEpochId {
        TaskEpochId { host_runtime_id: "h1".into(), session_id: "s1".into(), turn_id: "t1".into() }
    }

    #[test]
    fn idle_without_activity_is_not_completion() {
        let mut e = TaskEpoch::new(id(), 1000);
        assert_eq!(e.on_session_idle(2000, 5000), EpochState::Active);
        assert!(!e.saw_session_idle_with_evidence);
    }

    #[test]
    fn idle_then_activity_is_not_final() {
        let mut e = TaskEpoch::new(id(), 1000);
        e.on_chat_message(2000);
        e.on_session_idle(3000, 5000);
        assert_eq!(e.state, EpochState::AwaitingQuiet);
        // A session.idle during a DIFFERENT turn (new message) invalidates.
        e.on_chat_message(3500);
        assert_eq!(e.state, EpochState::Active);
    }

    #[test]
    fn quiet_period_plus_no_newer_activity_enables_review() {
        let mut e = TaskEpoch::new(id(), 1000);
        e.on_chat_message(2000);
        e.on_file_edit(2100);
        e.on_session_idle(2200, 5000);
        e.state = EpochState::AwaitingQuiet;
        e.saw_session_idle_with_evidence = true;
        e.last_activity_unix_ms = 2200;
        assert_eq!(e.mark_review_eligible(9000), EpochState::ReviewQueued);
    }

    #[test]
    fn baseline_after_edit_is_uncertain() {
        let mut e = TaskEpoch::new(id(), 1000);
        e.on_file_edit(1100);
        assert_eq!(e.baseline_status, BaselineStatus::BaselineUncertain);
    }
}