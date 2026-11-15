//! Source adapters: parse real acquired datasets into normalized records.
//!
//! Each adapter is a registered parser (not a new crate) that turns one source's
//! published schema into [`NormalizedRecordV1`] with source-stable identity +
//! content-hash. This module holds the first calibration source fixed by spec §12
//! step 1: **WritingPreferenceBench**.

pub mod creative;

/// Errors from an adapter.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("source file read: {0}")]
    Source(String),
    #[error("json parse of {0}: {1}")]
    Json(String, String),
}

pub use creative::WritingPreferenceAdapter;