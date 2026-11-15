//! Immutable snapshot lifecycle for the lakehouse.
//!
//! Snapshot layout (all under a configurable lakehouse root, e.g. `D:\NAI\DataLake\`):
//!
//! ```text
//! <root>/snapshots/<source_id>/<source_revision>/
//!     records.parquet        # real Parquet columnar shard
//!     manifest.json          # SourceManifestV1
//! <root>/snapshots/<source_id>/active.revision.json   # atomically-swapped pointer
//! ```
//!
//! A snapshot is immutable once written: re-ingesting the same revision is skipped
//! (zero delta). The `active` pointer is swapped atomically (write-temp + rename) so
//! a crash mid-swap never leaves a half-visible snapshot.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::manifest::{Partition, SourceManifestV1};
use crate::parquet_writer::{read_records_parquet, write_records_parquet};

/// Errors from the snapshot lifecycle.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest: {0}")]
    Manifest(String),
    #[error("parquet: {0}")]
    Parquet(#[from] crate::parquet_writer::ParquetError),
}

/// Path helpers for a single source's snapshot area.
pub struct SnapshotLayout {
    source_id: String,
    snapshots_root: PathBuf,
}

impl SnapshotLayout {
    /// Root for all of a source's snapshots: `<root>/snapshots/<source_id>`.
    pub fn new(root: &Path, source_id: &str) -> Self {
        Self {
            source_id: source_id.to_string(),
            snapshots_root: root.join("snapshots").join(source_id),
        }
    }

    /// The source identifier this layout serves.
    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    /// Directory holding a given revision's immutable files.
    pub fn revision_dir(&self, revision: &str) -> PathBuf {
        self.snapshots_root.join(revision)
    }
    pub fn manifest_path(&self, revision: &str) -> PathBuf {
        self.revision_dir(revision).join("manifest.json")
    }
    pub fn records_path(&self, revision: &str) -> PathBuf {
        self.revision_dir(revision).join("records.parquet")
    }
    pub fn active_pointer_path(&self) -> PathBuf {
        self.snapshots_root.join("active.revision.json")
    }

    /// Whether a given revision is already fully materialized (idempotence gate).
    pub fn exists(&self, revision: &str) -> bool {
        self.manifest_path(revision).exists() && self.records_path(revision).exists()
    }

    /// Persist a snapshot: write `records.parquet`, then `manifest.json`
    /// (immutable), then atomically set the active pointer. Returns the produced
    /// manifest content-hash.
    pub fn commit(
        &self,
        manifest: &SourceManifestV1,
        records: &[crate::manifest::NormalizedRecordV1],
        row_group_size: usize,
    ) -> Result<String, SnapshotError> {
        let revision = manifest.source_revision.clone();
        let dir = self.revision_dir(&revision);
        fs::create_dir_all(&dir)?;

        let records_path = dir.join("records.parquet");
        if !records_path.exists() {
            // Write the Parquet shard first (content-addressed by path+revision).
            write_records_parquet(&records_path, records, row_group_size)?;
        }

        // Compute partition entry (single shard in this layout).
        let meta = fs::metadata(&records_path)?;
        let blob = fs::read(&records_path)?;

        let mut manifest = manifest.clone();
        manifest.partitions = vec![Partition {
            path: "records.parquet".into(),
            blake3: blake3::hash(&blob).to_hex().to_string(),
            bytes: meta.len(),
            row_count: Some(records.len() as u64),
        }];
        let manifest_hash = manifest.content_hash();

        // Write manifest only after the parquet shard is durably on disk.
        let manifest_path = dir.join("manifest.json");
        let json = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| SnapshotError::Manifest(e.to_string()))?;
        // Atomic write: temp file + rename.
        let tmp = dir.join("manifest.json.tmp");
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        fs::rename(&tmp, &manifest_path)?;

        // Atomically swap the active pointer.
        let pointer = serde_json::json!({
            "sourceId": manifest.source_id,
            "sourceRevision": revision,
            "manifestHash": manifest_hash,
        });
        let ptr_tmp = self.snapshots_root.join("active.revision.json.tmp");
        let mut pf = fs::File::create(&ptr_tmp)?;
        pf.write_all(serde_json::to_vec_pretty(&pointer).unwrap().as_slice())?;
        pf.sync_all()?;
        fs::rename(&ptr_tmp, self.active_pointer_path())?;

        Ok(manifest_hash)
    }

    /// Read the currently-active revision string, if any.
    pub fn active_revision(&self) -> Result<Option<String>, SnapshotError> {
        let path = self.active_pointer_path();
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&path)?;
        let v: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| SnapshotError::Manifest(format!("active pointer parse: {e}")))?;
        Ok(v.get("sourceRevision")
            .and_then(|r| r.as_str())
            .map(str::to_string))
    }

    /// Load the active snapshot's manifest, if any.
    pub fn active_manifest(&self) -> Result<Option<SourceManifestV1>, SnapshotError> {
        let Some(rev) = self.active_revision()? else {
            return Ok(None);
        };
        self.load_manifest(&rev).map(Some)
    }

    /// Load a specific revision's manifest (used by delta compare).
    pub fn load_manifest(&self, revision: &str) -> Result<SourceManifestV1, SnapshotError> {
        let path = self.manifest_path(revision);
        let raw = fs::read_to_string(&path)?;
        serde_json::from_str(&raw)
            .map_err(|e| SnapshotError::Manifest(format!("manifest parse {revision}: {e}")))
    }

    /// Load all records of a revision from its Parquet shard.
    pub fn load_records(
        &self,
        revision: &str,
    ) -> Result<Vec<crate::manifest::NormalizedRecordV1>, SnapshotError> {
        read_records_parquet(&self.records_path(revision)).map_err(SnapshotError::from)
    }

    /// List every materialized revision (directories with a manifest).
    pub fn revisions(&self) -> Result<Vec<String>, SnapshotError> {
        if !self.snapshots_root.exists() {
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.snapshots_root)? {
            let entry = entry?;
            if entry.path().is_dir() && entry.path().join("manifest.json").exists() {
                if let Some(name) = entry.file_name().to_str() {
                    out.push(name.to_string());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn snapshots_root(&self) -> &Path {
        &self.snapshots_root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Acquisition, NormalizedRecordV1};

    fn sample(id: &str) -> NormalizedRecordV1 {
        NormalizedRecordV1 {
            id: id.to_string(),
            source_id: "writing-preference".into(),
            source_revision: "rev-a".into(),
            kind: "creative-pair".into(),
            split: "train".into(),
            language: Some("en".into()),
            raw_artifact_ref: "x.json".into(),
            normalized_hash: String::new(),
            payload: serde_json::json!({"prompt_id": id}),
            evidence: vec![],
            relations: vec![],
        }
        .with_computed_hash()
    }

    fn manifest(source_revision: &str) -> SourceManifestV1 {
        SourceManifestV1 {
            source_id: "writing-preference".into(),
            source_revision: source_revision.into(),
            acquisition: Acquisition::Git,
            raw_root: "D:\\NAI\\DataLake\\raw\\writing-preference".into(),
            adapter_version: "1.0.0".into(),
            schema_version: 1,
            complete_snapshot: true,
            partitions: vec![],
        }
    }

    #[test]
    fn commits_and_locks_revision_idempotently() {
        let dir = std::env::temp_dir().join(format!("lattice-snap-{}", uuidish()));
        let layout = SnapshotLayout::new(&dir, "writing-preference");

        let records = vec![sample("p1"), sample("p2"), sample("p3")];
        let h1 = layout.commit(&manifest("rev-a"), &records, 64).unwrap();
        assert!(layout.exists("rev-a"));
        assert_ne!(h1, "");
        assert!(layout.active_revision().unwrap().as_deref() == Some("rev-a"));

        // Re-committing the same revision is idempotent: same manifest hash, same
        // one revision, and the shard is not rewritten.
        let before = fs::read(&layout.records_path("rev-a")).unwrap();
        let h2 = layout.commit(&manifest("rev-a"), &records, 64).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(layout.revisions().unwrap(), vec!["rev-a".to_string()]);
        let after = fs::read(&layout.records_path("rev-a")).unwrap();
        assert_eq!(before, after, "shard must not be rewritten on idempotent commit");

        // Newer revision is activatable and kept alongside old.
        layout.commit(&manifest("rev-b"), &records, 64).unwrap();
        assert_eq!(layout.active_revision().unwrap().as_deref(), Some("rev-b"));
        let mut revs = layout.revisions().unwrap();
        revs.sort();
        assert_eq!(revs, vec!["rev-a".to_string(), "rev-b".to_string()]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn loads_back_records_from_active_snapshot() {
        let dir = std::env::temp_dir().join(format!("lattice-snap-{}", uuidish()));
        let layout = SnapshotLayout::new(&dir, "writing-preference");
        let records = vec![sample("r1"), sample("r2")];
        layout.commit(&manifest("v1"), &records, 64).unwrap();

        let manifest = layout.active_manifest().unwrap().unwrap();
        assert_eq!(manifest.source_revision, "v1");
        let back = layout.load_records("v1").unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].id, "r1");

        std::fs::remove_dir_all(&dir).ok();
    }

    fn uuidish() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64
    }
}