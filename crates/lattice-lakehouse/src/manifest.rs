//! Canonical lakehouse data contract (tutty Knowledge Federation spec §2).
//!
//! Every dataset has three separate identities:
//! - **source revision** — the concrete version of the published source collection
//!   (HF commit / Git SHA / snapshot hash);
//! - **record identity** — a stable identifier for a single ad/question/scenario/UI
//!   action that does not depend on row order;
//! - **derived revision** — the version of our parser/normalization/extraction/
//!   embedding/index.
//!
//! These types are the durable, replayable contract that the Parquet engine and the
//! `lattice-query` SQL layer only ever project from. The Rust types mirror the
//! canonical `source-manifest.v1.json` / `normalized-record.v1.json` schemas 1:1.

use serde::{Deserialize, Serialize};

/// Source of the acquired raw bytes (spec §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Acquisition {
    Hf,
    Git,
    HttpArchive,
    Gcs,
    Local,
}

/// One resolved partition of a source snapshot (spec §2 `SourceManifestV1`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Partition {
    pub path: String,
    pub blake3: String,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_count: Option<u64>,
}

/// Canonical source manifest (spec §2 `SourceManifestV1`).
///
/// `complete_snapshot=true` means the whole published source was enumerated and
/// ingested successfully. Only then may a delta planner treat a missing record as
/// a `DELETE`. A `false` value means an incomplete observation of the source, never
/// evidence a record was deleted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceManifestV1 {
    pub source_id: String,
    pub source_revision: String,
    pub acquisition: Acquisition,
    pub raw_root: String,
    pub adapter_version: String,
    pub schema_version: u64,
    pub complete_snapshot: bool,
    pub partitions: Vec<Partition>,
}

impl SourceManifestV1 {
    /// BLAKE3 over the canonical JSON of the manifest — the *derived* identity used
    /// to detect "same source, same parser, same files" without re-parsing.
    pub fn content_hash(&self) -> String {
        let canonical = serde_json::to_vec(self).expect("manifest serializes");
        blake3::hash(&canonical).to_hex().to_string()
    }
}

/// A single evidence item attached to a normalized record (spec §2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Evidence {
    pub label: String,
    pub value: serde_json::Value,
    #[serde(rename = "evidenceType")]
    pub evidence_type: String,
}

/// A typed relation from this record to another record (spec §2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Relation {
    pub kind: String,
    #[serde(rename = "targetId")]
    pub target_id: String,
}

/// A canonical, normalized record (spec §2 `NormalizedRecordV1`).
///
/// `id` is the source-stable identity, `normalized_hash` the content hash of the
/// normalized payload. Two records with the same `id` but different
/// `normalized_hash` represent an `UPDATE`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NormalizedRecordV1 {
    pub id: String,
    #[serde(rename = "sourceId")]
    pub source_id: String,
    #[serde(rename = "sourceRevision")]
    pub source_revision: String,
    pub kind: String,
    pub split: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(rename = "rawArtifactRef")]
    pub raw_artifact_ref: String,
    #[serde(rename = "normalizedHash")]
    pub normalized_hash: String,
    pub payload: serde_json::Value,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    #[serde(default)]
    pub relations: Vec<Relation>,
}

impl NormalizedRecordV1 {
    /// Compute the whole-record BLAKE3 from its stable identity + normalized payload.
    /// This is the `normalized_hash` used by the delta merge-join.
    pub fn compute_normalized_hash(&self) -> String {
        // Only the stable identity and normalized payload participate; they define the
        // semantic content. Evidence / relations may be derived and re-computed.
        let mut h = blake3::Hasher::new();
        h.update(self.id.as_bytes());
        h.update(self.kind.as_bytes());
        h.update(self.split.as_bytes());
        let payload = serde_json::to_vec(&self.payload).expect("payload serializes");
        h.update(&payload);
        h.finalize().to_hex().to_string()
    }

    /// Populate `normalized_hash` for a record built by an adapter.
    pub fn with_computed_hash(mut self) -> Self {
        self.normalized_hash = self.compute_normalized_hash();
        self
    }
}

/// The delta between two source revisions of the same source (spec §2
/// `DatasetDeltaV1`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatasetDeltaV1 {
    #[serde(rename = "sourceId")]
    pub source_id: String,
    #[serde(rename = "fromRevision")]
    pub from_revision: Option<String>,
    #[serde(rename = "toRevision")]
    pub to_revision: String,
    pub inserts: Vec<String>,
    pub updates: Vec<String>,
    pub deletes: Vec<String>,
    pub unchanged: u64,
    #[serde(rename = "changedPartitions")]
    pub changed_partitions: Vec<String>,
}

impl DatasetDeltaV1 {
    /// Is this a full no-op sync (nothing inserted/updated/deleted)?
    pub fn is_empty(&self) -> bool {
        self.inserts.is_empty() && self.updates.is_empty() && self.deletes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record(id: &str, kind: &str) -> NormalizedRecordV1 {
        NormalizedRecordV1 {
            id: id.to_string(),
            source_id: "writing-preference".into(),
            source_revision: "676d7114ce37ad5d8eff5b39f3648b855730bf77".into(),
            kind: kind.into(),
            split: "train".into(),
            language: Some("en".into()),
            raw_artifact_ref: "WP_bench_english-1.json".into(),
            normalized_hash: String::new(),
            payload: serde_json::json!({"prompt": "a", "chosen": "b", "rejected": "c"}),
            evidence: vec![],
            relations: vec![],
        }
        .with_computed_hash()
    }

    #[test]
    fn normalized_hash_is_stable_and_content_sensitive() {
        let a = sample_record("p1", "creative-pair");
        let b = sample_record("p1", "creative-pair");
        assert_eq!(a.normalized_hash, b.normalized_hash);

        let changed = NormalizedRecordV1 {
            payload: serde_json::json!({"prompt": "a", "chosen": "B", "rejected": "c"}),
            ..b.clone()
        }
        .with_computed_hash();
        assert_ne!(a.normalized_hash, changed.normalized_hash);
        // Identity is stable across payload change (that is the delta contract).
        assert_eq!(a.id, changed.id);
    }

    #[test]
    fn manifest_hash_changes_with_fields() {
        let mut m = SourceManifestV1 {
            source_id: "writing-preference".into(),
            source_revision: "abc".into(),
            acquisition: Acquisition::Git,
            raw_root: "D:\\NAI\\DataLake\\raw\\writing-preference".into(),
            adapter_version: "1.0.0".into(),
            schema_version: 1,
            complete_snapshot: true,
            partitions: vec![Partition {
                path: "records.parquet".into(),
                blake3: "deadbeef".into(),
                bytes: 100,
                row_count: Some(1800),
            }],
        };
        let h1 = m.content_hash();
        m.complete_snapshot = false;
        let h2 = m.content_hash();
        assert_ne!(h1, h2);
    }

    #[test]
    fn delta_is_empty_is_correct() {
        let dn = DatasetDeltaV1 {
            source_id: "x".into(),
            from_revision: Some("a".into()),
            to_revision: "b".into(),
            inserts: vec![],
            updates: vec![],
            deletes: vec![],
            unchanged: 10,
            changed_partitions: vec![],
        };
        assert!(dn.is_empty());
        let d = DatasetDeltaV1 {
            inserts: vec!["r1".into()],
            ..dn
        };
        assert!(!d.is_empty());
    }
}