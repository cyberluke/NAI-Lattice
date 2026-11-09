//! Artifact fabric / content-addressed object model (spec §10).
//!
//! Large/binary artifacts never live in hot-KV or analytical rows. They are
//! content-addressed by BLAKE3 whole-object identity. Logical URI:
//! `artifact://blake3/<whole-object-hash>`.

use serde::{Deserialize, Serialize};

/// A BLAKE3 whole-object hash rendered as lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Blake3Hash(pub String);

impl Blake3Hash {
    pub fn as_hex(&self) -> &str {
        &self.0
    }
}

/// A content-defined chunk boundary in an optional FastCDC-style manifest
/// (spec §10, min 1 MiB / avg 4 MiB / max 16 MiB). Chunking is a transfer/dedup
/// optimization, never a change to logical identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkDescriptor {
    pub offset: u64,
    pub length: u64,
    pub hash: Blake3Hash,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkManifest {
    pub chunk_size_min: u64,
    pub chunk_size_avg: u64,
    pub chunk_size_max: u64,
    pub chunks: Vec<ChunkDescriptor>,
}

/// Stability classes for external AAF artifact locations (§11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StabilityClass {
    /// Owned CAS. Highest trust.
    Owned,
    /// Stable local external source (e.g. retained model cache).
    StableLocal,
    /// Local provider with range access.
    LocalRange,
    /// Immutable source safe for reflink/hardlink.
    ImmutableLink,
    /// Remote/cloud source.
    Remote,
}

/// An external or owned location for an artifact (§10 manifest fields,
/// §11 external location schema).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactLocation {
    pub provider_id: String,
    pub opaque_locator: String,
    pub expected_hash: Blake3Hash,
    pub size: u64,
    pub stability: StabilityClass,
    /// True when the source supports random-access ranges.
    pub random_access_capability: bool,
    pub cost_class: u8,
    #[serde(default)]
    pub owned: bool,
    #[serde(default)]
    pub last_verified: Option<i64>,
    #[serde(default)]
    pub health: LocationHealth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LocationHealth {
    #[default]
    Unknown,
    Reachable,
    Unavailable,
}

/// Complete artifact manifest (§10).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactManifest {
    pub artifact_id: String,
    pub whole_hash: Blake3Hash,
    pub size: u64,
    pub media_type: String,
    pub logical_type: String,
    pub created_at_ms: i64,
    pub provenance: String,
    #[serde(default)]
    pub chunk_manifest: Option<ChunkManifest>,
    #[serde(default)]
    pub owned_locations: Vec<ArtifactLocation>,
    #[serde(default)]
    pub external_locations: Vec<ArtifactLocation>,
    #[serde(default)]
    pub verification_state: VerificationState,
    #[serde(default)]
    pub last_verified_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VerificationState {
    #[default]
    Missing,
    Pending,
    Verified,
    Mismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_roundtrips_with_location_kinds() {
        let m = ArtifactManifest {
            artifact_id: "a1".into(),
            whole_hash: Blake3Hash("ab31f9".into()),
            size: 12_000_000,
            media_type: "application/octet-stream".into(),
            logical_type: "onnx-model".into(),
            created_at_ms: 0,
            provenance: "lusi-request".into(),
            chunk_manifest: None,
            owned_locations: vec![ArtifactLocation {
                provider_id: "nai-cas".into(),
                opaque_locator: "objects/ab/ab31f9".into(),
                expected_hash: Blake3Hash("ab31f9".into()),
                size: 12_000_000,
                stability: StabilityClass::Owned,
                random_access_capability: true,
                cost_class: 0,
                owned: true,
                last_verified: None,
                health: LocationHealth::Unknown,
            }],
            external_locations: vec![ArtifactLocation {
                provider_id: "naibrowser-opfs".into(),
                opaque_locator: "profile-0/model.bin".into(),
                expected_hash: Blake3Hash("ab31f9".into()),
                size: 12_000_000,
                stability: StabilityClass::StableLocal,
                random_access_capability: false,
                cost_class: 3,
                owned: false,
                last_verified: None,
                health: LocationHealth::Unknown,
            }],
            verification_state: VerificationState::Pending,
            last_verified_at: None,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ArtifactManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.whole_hash, m.whole_hash);
        assert_eq!(back.external_locations[0].provider_id, "naibrowser-opfs");
    }
}