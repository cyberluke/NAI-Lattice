//! Zero-downtime-ish migration from the legacy Gas Town daemon (spec §11).
//!
//! A REPEATABLE, idempotent import facility into the canonical NAFI-Lattice
//! ledger/CAS. Rerunning import must not duplicate jobs or reports (MIG-002).
//! Old-to-new ID mappings and an imported schema manifest are stored so rollback
//! and reconciliation (MIG-004) can be demonstrated. It never deletes legacy
//! data; legacy files remain archival until explicit CTO order.

pub mod legacy;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// One legacy record imported (job/report/state mapping).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedRecord {
    pub import_key: String,
    pub legacy_id: String,
    pub new_id: String,
    pub kind: ImportKind,
    pub content_hash: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImportKind { Job, Report, Finding, Snapshot, Lease, Workspace }

/// Idempotency bookkeeping: import keys already applied.
#[derive(Debug, Default)]
pub struct ImportLedger {
    pub applied: HashMap<String, String>, // import_key -> new_id
    pub mappings: Vec<ImportedRecord>,
    /// The schema manifest of the legacy source (for transparency).
    pub schema_manifest: serde_json::Value,
}

impl ImportLedger {
    pub fn new(schema_manifest: serde_json::Value) -> Self {
        Self { schema_manifest, ..Default::default() }
    }

    /// Import one record. Returns Ok(Some(new_id)) when newly applied,
    /// Ok(None) when a duplicate import key was already applied (idempotent),
    /// and Err on an integrity mismatch (content hash drift between runs).
    pub fn import(
        &mut self,
        import_key: &str,
        legacy_id: &str,
        kind: ImportKind,
        content: &[u8],
    ) -> Result<Option<String>, String> {
        let hash = blake3::hash(content).to_hex().to_string();
        if let Some(existing) = self.applied.get(import_key) {
            // Idempotent rerun: verify content hash is unchanged.
            let prior = self
                .mappings
                .iter()
                .find(|m| m.import_key == import_key)
                .ok_or("missing mapping for prior import")?;
            if prior.content_hash != hash {
                return Err(format!("content hash drift on idempotent rerun for {import_key}"));
            }
            return Ok(Some(existing.clone()));
        }
        let new_id = migrate_id(legacy_id);
        self.applied.insert(import_key.to_string(), new_id.clone());
        self.mappings.push(ImportedRecord {
            import_key: import_key.to_string(),
            legacy_id: legacy_id.to_string(),
            new_id: new_id.clone(),
            kind,
            content_hash: hash,
        });
        Ok(Some(new_id))
    }

    pub fn imported_count(&self) -> usize {
        self.mappings.len()
    }
}

/// Deterministic legacy->new id mapping (stable across runs).
fn migrate_id(legacy_id: &str) -> String {
    format!("gmt-{}", blake3::hash(legacy_id.as_bytes()).to_hex())
}

/// Suggested import order / coverage for the migration gate.
pub const IMPORT_ORDER: &[ImportKind] = &[
    ImportKind::Lease,
    ImportKind::Job,
    ImportKind::Finding,
    ImportKind::Snapshot,
    ImportKind::Report,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rerun_is_idempotent_without_duplicates() {
        let mut il = ImportLedger::new(serde_json::json!({"schema":"legacy-v7"}));
        let a = il.import("job:7", "legacy-job-7", ImportKind::Job, b"content-a").unwrap().unwrap();
        let b = il.import("job:7", "legacy-job-7", ImportKind::Job, b"content-a").unwrap().unwrap();
        assert_eq!(a, b, "same new id on rerun");
        assert_eq!(il.mappings.len(), 1, "no duplicate import");
    }

    #[test]
    fn content_drift_is_rejected() {
        let mut il = ImportLedger::new(serde_json::json!({}));
        il.import("job:1", "l1", ImportKind::Job, b"v1").unwrap();
        assert!(il.import("job:1", "l1", ImportKind::Job, b"v2-CHANGED").is_err());
    }

    #[test]
    fn different_legacy_ids_map_to_different_new_ids() {
        let mut il = ImportLedger::new(serde_json::json!({}));
        let a = il.import("job:1", "job1", ImportKind::Job, b"x").unwrap().unwrap();
        let b = il.import("job:2", "job2", ImportKind::Job, b"x").unwrap().unwrap();
        assert_ne!(a, b);
    }
}