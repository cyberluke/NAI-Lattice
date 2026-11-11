//! Real migration of the legacy Gas Town SQLite + report files into the
//! NAI-Lattice canonical store (spec §6, order).
//!
//! Idempotent and resumable: each legacy entity is imported by a stable
//! `import_key`; re-running never duplicates. Source artifacts are preserved
//! untouched; every imported artifact is stored immutably (CAS) and referenced
//! by hash. Original DB/rows/files are never mutated or deleted.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::ImportKind;

/// Minimal immutable-content provider (the CLI supplies a CAS-backed impl so
/// `lattice-gastown` never owns transport).
pub trait ArtifactStore {
    /// Store bytes immutably; return a stable URI (e.g. `artifact://blake3/{hash}`).
    fn store(&mut self, content: &[u8], kind: &str) -> Result<String, String>;
}

/// Idempotent migration ledger that also imports each artifact via `store`.
#[derive(Debug, Default)]
pub struct LegacyMigration {
    pub import: super::ImportLedger,
    /// source_key -> stored_uri
    pub stored: HashMap<String, String>,
    pub counts: ImportCounts,
}

impl LegacyMigration {
    /// Rebuild the in-memory ledger state from a prior persisted migration
    /// manifest, so a later run is resumable and idempotent (MIG-002): already
    /// applied legacy ids are not re-imported or duplicated by bookkeeping.
    pub fn load_manifest(&mut self, manifest: &[serde_json::Value]) {
        self.import.applied.clear();
        self.import.mappings.clear();
        for m in manifest {
            let Some(ik) = m.get("import_key").and_then(|v| v.as_str()) else { continue; };
            let Some(new_id) = m.get("new_id").and_then(|v| v.as_str()) else { continue; };
            let Some(uri) = m.get("uri").and_then(|v| v.as_str()) else { continue; };
            let Some(kind) = m.get("kind").and_then(|v| v.as_str()) else { continue; };
            let Some(legacy_id) = m.get("legacy_id").and_then(|v| v.as_str()) else { continue; };
            self.import.applied.insert(ik.to_string(), new_id.to_string());
            self.import.mappings.push(super::ImportedRecord {
                import_key: ik.to_string(),
                legacy_id: legacy_id.to_string(),
                new_id: new_id.to_string(),
                kind: Self::kind_from_str(kind),
                content_hash: m.get("content_hash").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            });
            self.stored.insert(ik.to_string(), uri.to_string());
        }
    }

    /// Serialize the durable manifest (one entry per imported legacy artifact).
    pub fn manifest_json(&self) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for (key, uri) in &self.stored {
            if let Some(m) = self.import.mappings.iter().find(|r| r.import_key == *key) {
                out.push(serde_json::json!({
                    "import_key": key,
                    "legacy_id": m.legacy_id,
                    "new_id": m.new_id,
                    "kind": format!("{:?}", m.kind).to_lowercase(),
                    "content_hash": m.content_hash,
                    "uri": uri,
                }));
            }
        }
        out
    }

    fn kind_from_str(s: &str) -> super::ImportKind {
        match s {
            "job" => super::ImportKind::Job,
            "report" => super::ImportKind::Report,
            "finding" => super::ImportKind::Finding,
            "snapshot" => super::ImportKind::Snapshot,
            "lease" => super::ImportKind::Lease,
            _ => super::ImportKind::Workspace,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ImportCounts {
    pub source_workspaces: u64,
    pub imported_workspaces: u64,
    pub source_snapshots: u64,
    pub imported_snapshots: u64,
    pub source_jobs: u64,
    pub imported_jobs: u64,
    pub source_findings: u64,
    pub imported_findings: u64,
    pub source_reports: u64,
    pub imported_reports: u64,
    pub deduped: u64,
    pub errors: u64,
}

/// One escaped importable row of a legacy table.
pub struct LegacyRow {
    pub table: &'static str,
    /// Stable legacy primary key (workspace_id / job_id / snapshot_id / ...).
    pub legacy_id: String,
    pub kind: ImportKind,
    /// Canonical JSON/bytes to store immutably.
    pub content: Vec<u8>,
}

impl LegacyMigration {
    pub fn new(schema_manifest: serde_json::Value) -> Self {
        Self { import: super::ImportLedger::new(schema_manifest), counts: ImportCounts::default(), stored: HashMap::new() }
    }

    /// Import one legacy row idempotently. Returns the stored URI when newly
    /// imported, or the already-stored URI on a dedup replay.
    pub fn import_row(&mut self, store: &mut dyn ArtifactStore, row: LegacyRow) -> Result<Option<String>, String> {
        let key = format!("{}:{}", row.table, row.legacy_id);
        Self::bump_source(&mut self.counts, row.table);
        let fresh = !self.import.applied.contains_key(&key);
        let new_id = self.import.import(&key, &row.legacy_id, row.kind, &row.content)?;
        if !fresh {
            // Idempotent replay: no new artifact, return the already-stored uri.
            self.counts.deduped += 1;
            return Ok(self.stored.get(&key).cloned());
        }
        let _ = new_id;
        let uri = store.store(&row.content, row.table)?;
        self.stored.insert(key.clone(), uri.clone());
        Self::bump_imported(&mut self.counts, row.table);
        Ok(Some(uri))
    }

    fn bump_source(c: &mut ImportCounts, table: &str) {
        match table {
            "workspaces" => c.source_workspaces += 1,
            "snapshots" => c.source_snapshots += 1,
            "review_jobs" => c.source_jobs += 1,
            "findings" => c.source_findings += 1,
            "reports" => c.source_reports += 1,
            _ => {}
        }
    }
    fn bump_imported(c: &mut ImportCounts, table: &str) {
        match table {
            "workspaces" => c.imported_workspaces += 1,
            "snapshots" => c.imported_snapshots += 1,
            "review_jobs" => c.imported_jobs += 1,
            "findings" => c.imported_findings += 1,
            "reports" => c.imported_reports += 1,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MemStore(HashMap<String, String>);
    impl ArtifactStore for MemStore {
        fn store(&mut self, content: &[u8], kind: &str) -> Result<String, String> {
            let h = blake3::hash(content).to_hex().to_string();
            let uri = format!("artifact://blake3/{h}");
            self.0.insert(format!("{kind}:{h}"), uri.clone());
            Ok(uri)
        }
    }

    #[test]
    fn migration_is_idempotent_and_resumable() {
        let mut m = LegacyMigration::new(serde_json::json!({"legacy":"gastown-v7"}));
        let mut store = MemStore(HashMap::new());
        let row = LegacyRow { table: "review_jobs", legacy_id: "job1".into(), kind: ImportKind::Job, content: b"{\"state\":\"COMPLETED\"}".to_vec() };
        let first = m.import_row(&mut store, row).unwrap().unwrap();
        // Re-run same row (resumable) -> dedup, returns same stored uri (no new import).
        let again = m.import_row(&mut store, LegacyRow { table: "review_jobs", legacy_id: "job1".into(), kind: ImportKind::Job, content: b"{\"state\":\"COMPLETED\"}".to_vec() }).unwrap();
        assert_eq!(again.as_deref(), Some(first.as_str()), "replay returns the already-stored uri");
        assert_eq!(m.counts.source_jobs, 2);
        assert_eq!(m.counts.imported_jobs, 1);
        assert_eq!(m.counts.deduped, 1);
        assert!(first.starts_with("artifact://blake3/"));
    }

    #[test]
    fn content_drift_between_runs_is_rejected() {
        let mut m = LegacyMigration::new(serde_json::json!({}));
        let mut store = MemStore(HashMap::new());
        m.import_row(&mut store, LegacyRow { table: "snapshots", legacy_id: "s1".into(), kind: ImportKind::Snapshot, content: b"v1".to_vec() }).unwrap();
        // A changed row on rerun = integrity drift -> error, must not be dropped.
        let r = m.import_row(&mut store, LegacyRow { table: "snapshots", legacy_id: "s1".into(), kind: ImportKind::Snapshot, content: b"v2-should-not-happen".to_vec() });
        assert!(r.is_err());
    }
}