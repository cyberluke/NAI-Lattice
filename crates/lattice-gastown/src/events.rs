//! Typed Gas Town event payloads (spec §5). All live on the canonical ledger
//! as `event.append` events carrying JSON payloads; these helpers serialize and
//! interpret them without owning a second store.

use serde::{Deserialize, Serialize};

/// The canonical kind discriminator for Gas Town events.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    ChatMessage,
    MessageUpdated,
    ToolStarted,
    ToolCompleted,
    FileEdited,
    SessionStatus,
    SessionIdle,
    SessionDiff,
    SessionFinished,
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::ChatMessage => "chat.message",
            EventKind::MessageUpdated => "message.updated",
            EventKind::ToolStarted => "tool.started",
            EventKind::ToolCompleted => "tool.completed",
            EventKind::FileEdited => "file.edited",
            EventKind::SessionStatus => "session.status",
            EventKind::SessionIdle => "session.idle",
            EventKind::SessionDiff => "session.diff",
            EventKind::SessionFinished => "session.finished",
        }
    }
}

/// Canonical minimal event body (spec §5 minimum typed fields). The DEEP
/// payload (prompt text, tool args, diff) is deliberately excluded here and
/// stored only in a scoped CAS snapshot with controlled access — never in an
/// infrastructure log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GastownEvent {
    pub schema_version: u32,
    pub producer_id: String,
    pub producer_epoch: u64,
    pub producer_sequence: u64,
    pub event_id: String, // UUIDv7/ULID, persistent through retries
    pub correlation_id: Option<String>,
    pub causation_id: Option<String>,
    pub host_runtime_id: Option<String>,
    pub host_process_identity: Option<String>,
    pub workspace_id: Option<String>,
    pub session_id: Option<String>,
    pub task_epoch_id: Option<String>,
    pub turn_id: Option<String>,
    pub kind: EventKind,
    pub event_timestamp_utc_ms: i64,
    pub payload_schema_version: u32,
    /// BLAKE3 over the externally-kept deep payload bytes.
    pub payload_hash: String,
}

impl GastownEvent {
    pub fn encode(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
    /// Stable idempotency identity: producer + epoch + sequence.
    pub fn idempotency_key(&self) -> String {
        format!("{}:{}:{}", self.producer_id, self.producer_epoch, self.producer_sequence)
    }
}

/// Client Ack as returned by the backend after durable commit (spec §5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckStatus {
    /// Monotonic per producer within the epoch as observed by the client.
    pub producer_epoch: u64,
    pub contiguous_sequence: u64,
    pub event_ids: Vec<String>,
    /// Canonical runtime sequence assigned by the ledger.
    pub ledger_sequence: u64,
    pub durability_class: String,
    /// True when this event was a duplicate (idempotent replay).
    pub duplicated: bool,
}

/// Reply cursor used for reconnect replay (spec §5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayCursor {
    pub producer_id: String,
    pub producer_epoch: u64,
    pub next_sequence: u64,
    pub acked_through: u64,
}