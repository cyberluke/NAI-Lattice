//! Canonical event ledger types (spec §9).
//!
//! The ledger is the durable, replayable source of behavioral/application
//! change. It is **not** a SQL database and never the same thing as any
//! projection.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::GlobalId;

/// Device identifier: a machine-scoped, stable id (per-user data root).
pub type DeviceId = uuid::Uuid;

/// Event identifier: UUIDv7 or ULID. We accept either form in the envelope
/// (the `raw` string preserves the producer's chosen id shape).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventId {
    pub raw: String,
}

impl EventId {
    pub fn ulid() -> Self {
        let ulid = ulid::Ulid::new();
        Self {
            raw: ulid.to_string(),
        }
    }
    pub fn uuid_v7() -> Self {
        let id = uuid::Uuid::now_v7();
        Self {
            raw: id.to_string(),
        }
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

/// One canonical event envelope (spec §9).
///
/// The envelope is versioned and forward-compatible: an older runtime can
/// replay an envelope it does not semantically understand as long as it can
/// parse the structural fields and preserve `payload_bytes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalEvent {
    pub event_id: EventId,
    /// Monotonic local sequence, assigned by the append writer at commit time.
    pub runtime_sequence: u64,
    pub timestamp_utc_ms: i64,
    pub device_id: DeviceId,
    pub producer_app: String,
    pub producer_version: String,
    pub namespace: String,
    pub entity_id: Option<GlobalId>,
    pub event_type: String,
    pub schema_version: u32,
    pub correlation_id: Option<String>,
    pub causation_id: Option<String>,
    pub payload_type: String,
    /// Preserved opaque payload. Unknown types remain replayable by older tooling.
    pub payload_bytes: Vec<u8>,
    pub artifact_refs: Vec<GlobalId>,
    /// Previous frame's BLAKE3 root (hash-chain continuity, §9 "Segment format").
    pub previous_hash: Option<String>,
    /// Actual frame hash this event produced (assigned by the ledger writer).
    #[serde(default)]
    pub frame_hash: Option<String>,
}

impl CanonicalEvent {
    /// The durable idempotency key a producer passes so a retry never creates a
    /// duplicate. May be None for genuinely new events.
    pub fn signing_input(&self, previous_hash: Option<&str>) -> Vec<u8> {
        let mut out = Vec::new();
        push(&mut out, &self.event_id.raw);
        push(&mut out, &self.runtime_sequence.to_le_bytes());
        push(&mut out, &self.timestamp_utc_ms.to_le_bytes());
        push(&mut out, &self.device_id.as_bytes());
        push(&mut out, &self.producer_app);
        push(&mut out, &self.producer_version);
        push(&mut out, &self.namespace);
        if let Some(entity) = &self.entity_id {
            push(&mut out, &entity.uri);
        }
        push(&mut out, &self.event_type);
        push(&mut out, &self.schema_version.to_le_bytes());
        push(&mut out, &self.payload_type);
        push(&mut out, &self.payload_bytes);
        for a in &self.artifact_refs {
            push(&mut out, &a.uri);
        }
        if let Some(prev) = previous_hash {
            push(&mut out, prev.as_bytes());
        }
        out
    }
}

fn push<B: AsRef<[u8]>>(out: &mut Vec<u8>, b: B) {
    let b = b.as_ref();
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

/// The idempotency record the ledger writes once per (producer, key) so retries
/// never create duplicate durable events (§8).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdempotencyRecord {
    pub producer_id: String,
    pub key: String,
    pub committed_sequence: u64,
    pub committed_event_id: EventId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_accepts_ulid_and_v7_and_signs_deterministically() {
        let a = CanonicalEvent {
            event_id: EventId::ulid(),
            runtime_sequence: 7,
            timestamp_utc_ms: 1234,
            device_id: uuid::Uuid::nil(),
            producer_app: "nai.email".into(),
            producer_version: "1.0.0".into(),
            namespace: "nai.email".into(),
            entity_id: Some(GlobalId::entity("email", "m1")),
            event_type: "email.message.ingested".into(),
            schema_version: 1,
            correlation_id: None,
            causation_id: None,
            payload_type: "application/json".into(),
            payload_bytes: b"{}".to_vec(),
            artifact_refs: vec![GlobalId::artifact_blake3("ab31f9")],
            previous_hash: Some("prev".into()),
            frame_hash: None,
        };
        let b = a.clone();
        assert_eq!(a.signing_input(Some("prev")), b.signing_input(Some("prev")));
        // Changing payload changes the signing input.
        let mut c = a.clone();
        c.payload_bytes = b"[]".to_vec();
        assert_ne!(a.signing_input(None), c.signing_input(None));
    }
}