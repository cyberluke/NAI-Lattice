//! NAI Lattice `lattice-lakehouse` — Parquet/Arrow ingest and snapshot delta.
//!
//! This crate implements the **lakehouse layer** of the Tutty Knowledge Federation
//! blueprint (`docs/data_ingest_workflows.md` §7): real Apache Parquet/Arrow columnar
//! storage of normalized records, immutable per-revision snapshot manifests with
//! BLAKE3 content hashes, and streaming delta synchronization (spec §2).
//!
//! Data identity (spec §2):
//! - **source revision** — the concrete source collection version (Git/HF SHA);
//! - **record identity** — the stable `id` of a single ad/question/scenario/UI action;
//! - **derived revision** — our parser / normalization version, captured in the
//!   manifest.
//!
//! Layout under a lakehouse root (e.g. `D:\NAI\DataLake\`):
//! ```text
//! <root>/snapshots/<source_id>/<source_revision>/records.parquet  # real Parquet
//! <root>/snapshots/<source_id>/<source_revision>/manifest.json    # SourceManifestV1
//! <root>/snapshots/<source_id>/active.revision.json               # atomic pointer
//! ```

pub mod adapters;
pub mod delta;
pub mod federation;
pub mod manifest;
pub mod parquet_writer;
pub mod snapshot;

pub use adapters::WritingPreferenceAdapter;
pub use delta::{diff, diff_sorted, DeltaKind, DeltaResult};
pub use federation::DatasetFederation;
pub use manifest::{
    Acquisition, DatasetDeltaV1, Evidence, NormalizedRecordV1, Partition, Relation,
    SourceManifestV1,
};
pub use parquet_writer::{
    normalized_schema, read_records_parquet, write_records_parquet,
};
pub use snapshot::{SnapshotError, SnapshotLayout};