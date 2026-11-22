//! Ambient Asset Federation (AAF) resolver (Lattice spec §11, AAF doctrine).
//!
//! "Never create/acquire/move/duplicate/pay for a capability that already
//! exists and can be safely federated." Existing compatible bytes on the user's
//! machine can satisfy an artifact without copying or downloading them again.
//!
//! Preference order (spec §11):
//! ```text
//! verified owned CAS
//! verified stable local external source
//! verified local provider with range access
//! reflink/hardlink-capable immutable source when safe
//! local copy acquisition
//! remote/cloud acquisition
//! ```
//!
//! Reads are read-only by default: never mutate/reformat another product's
//! storage. If an external artifact disappears, mark the location unavailable
//! and resolve another location — never let it corrupt NAI canonical state.

pub mod resolver;
pub mod source;

use std::ops::Range;

/// Error raised by the AAF layer.
#[derive(Debug, thiserror::Error)]
pub enum AafError {
    #[error("provider not registered: {0}")]
    UnknownProvider(String),
    #[error("no compatible location available for {0}")]
    NoLocation(String),
    #[error("fingerprint mismatch for {0}")]
    FingerprintMismatch(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("cas: {0}")]
    Cas(#[from] nai_lattice_cas::CasError),
    #[error("source: {0}")]
    Source(String),
}

/// An opaque locator for a candidate object at a provider.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Locator(pub String);

/// A content fingerprint (whole-object BLAKE3 preferred).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Fingerprint {
    /// Whole-object BLAKE3 hex when the provider can compute it.
    pub blake3: Option<String>,
    /// Approximate size in bytes.
    pub size: u64,
}

/// A discovered candidate artifact at a provider.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Candidate {
    pub provider_id: String,
    pub locator: Locator,
    pub fingerprint: Fingerprint,
    pub media_type_hint: Option<String>,
    pub logical_type_hint: Option<String>,
    /// Best-effort provenance describing where the artifact is being reused from.
    pub provenance: String,
}

/// Filter for `discover`.
#[derive(Debug, Clone, Default)]
pub struct DiscoverFilter {
    /// Filter by logical type (e.g. `onnx-model`, `tokenizer`).
    pub logical_type: Option<String>,
    /// Filter by media type.
    pub media_type: Option<String>,
}

/// Health of a provider.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProviderHealth {
    Healthy,
    Degraded { detail: String },
    Unavailable { detail: String },
}

/// A resolved source of truth for an artifact — either an owned CAS path or an
/// external provider locator (with optional pre-staged copy).
#[derive(Debug, Clone)]
pub enum Resolution {
    /// Already in owned CAS; reuse the object path.
    Owned { object_path: std::path::PathBuf, hash: String },
    /// Reuse bytes from an external provider without copying.
    External { provider_id: String, locator: Locator },
    /// A local staged copy was produced; caller may reflink/copy it into CAS.
    Staged { local_path: std::path::PathBuf },
    /// Acquired from remote/cloud.
    Remote { provider_id: String, locator: Locator },
}

/// A byte range request.
pub type RangeRequest = Range<u64>;

pub use crate::resolver::{AafResolver, ProviderHandle, TestProvider};
pub use crate::source::{
    ByteStream, DirectorySource, ExternalArtifactSource, MemorySource,
};