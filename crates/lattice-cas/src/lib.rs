//! Content-addressed artifact store (Lattice spec §10).
//!
//! Large/binary artifacts live in a CAS with BLAKE3 whole-object identity, not
//! inside hot-KV or analytical rows. Logical URI:
//! `artifact://blake3/<whole-object-hash>`.
//!
//! Owned CAS path example:
//! ```text
//! <root>/v1/cas/objects/ab/ab31f9...
//! ```
//! Manifest (owned + external AAF locations) persists alongside for the
//! `artifact.*` NAF/1 capabilities and the AAF resolver (§11).

pub mod manifest;
pub mod object;
pub mod report;

use std::path::PathBuf;

pub use crate::manifest::ManifestStore;
pub use crate::object::ObjectStore;
pub use crate::report::{cas_report, CasReport, CorruptIndexEntry, CAS_REPORT_SCHEMA};

/// Error raised by the CAS.
#[derive(Debug, thiserror::Error)]
pub enum CasError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest: {0}")]
    Manifest(String),
    #[error("artifact confirmation: expected {expected}, got {got}")]
    HashMismatch { expected: String, got: String },
    #[error("artifact not found: {0}")]
    NotFound(String),
    #[error("range out of bounds: start {start}, end {end}, size {size}")]
    RangeOutOfBounds { start: u64, end: u64, size: u64 },
}

/// A fully opened artifact: its manifest plus access to the owned bytes.
pub struct OpenedArtifact {
    pub manifest: nai_lattice_core::ArtifactManifest,
    pub owned_path: PathBuf,
    pub size: u64,
}

/// Range-read result for `open_range`.
pub struct RangeRead {
    pub data: Vec<u8>,
    pub start: u64,
    pub end: u64, // exclusive
}