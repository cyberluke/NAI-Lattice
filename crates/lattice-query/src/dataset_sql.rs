//! Structural SQL-over-Parquet layer over the lakehouse (spec §7 `lattice-query` /
//! §9 "SQL/Parquet" resolver class).
//!
//! Many datasets' primary resolver is **structural** (not RAG): ratings, strategies,
//! conversion logs, UI actions, and benchmark ground truth are queried by
//! filters / aggregation, not by embedding similarity. This module reads canonical
//! records back from Parquet shards and supports:
//! - filtering on identity columns: `source_id`, `kind`, `split`, `language` plus an
//!   optional JSON-path predicate on `payload`;
//! - projecting matched record ids;
//! - aggregations (total + group-by count on a payload field);
//! - resolving full records by id (the "resolve original records from Parquet" step
//!   of the hybrid pipeline).
//!
//! It is intentionally lightweight and dependency-light: it does not embed a full
//! DataFusion SQL engine. It reuses the lakehouse's real Arrow/Parquet reader and
//! operates over the normalized [`crate::manifest::NormalizedRecordV1`] model.

use lattice_lakehouse::manifest::NormalizedRecordV1;
use lattice_lakehouse::snapshot::SnapshotLayout;

/// Errors from the structural query layer.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("snapshot: {0}")]
    Snapshot(#[from] lattice_lakehouse::snapshot::SnapshotError),
    #[error("no active snapshot for source {0}")]
    NoActiveSnapshot(String),
    #[error("record not found: {0}")]
    NotFound(String),
    #[error("invalid predicate: {0}")]
    Predicate(String),
}

/// A structural filter on the canonical identity columns + a payload JSON-path test.
#[derive(Debug, Clone, Default)]
pub struct StructFilter {
    pub source_id: Option<String>,
    pub kind: Option<String>,
    pub split: Option<String>,
    pub language: Option<String>,
    /// A `"payload.a.b"`-style path with an expected value (string keep simple).
    pub payload_path: Option<(String, String)>,
}

impl StructFilter {
    pub fn matches(&self, r: &NormalizedRecordV1) -> bool {
        if let Some(s) = &self.source_id {
            if r.source_id != *s {
                return false;
            }
        }
        if let Some(k) = &self.kind {
            if r.kind != *k {
                return false;
            }
        }
        if let Some(sp) = &self.split {
            if r.split != *sp {
                return false;
            }
        }
        if let Some(l) = &self.language {
            if r.language.as_deref() != Some(l.as_str()) {
                return false;
            }
        }
        if let Some((path, expected)) = &self.payload_path {
            let value = json_path(&r.payload, path);
            match value {
                Some(v) if v.as_str().map(|s| s == expected).unwrap_or(false) => {}
                Some(v) if v.as_number().map(|n| n.to_string() == *expected).unwrap_or(false) => {}
                _ => return false,
            }
        }
        true
    }
}

/// Walk a dotted JSON path (e.g. `"choice.chosen.score"`).
fn json_path<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = v;
    for part in path.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

/// Structural query result: project matched record ids (optionally limited), and
/// report the full candidate sets when requested.
#[derive(Debug, Clone)]
pub struct StructResult {
    /// Matched record ids in source order.
    pub record_ids: Vec<String>,
    /// Full records for the matched ids (resolved from Parquet), in the same order.
    pub records: Vec<NormalizedRecordV1>,
    pub total_matched: u64,
}

/// Structural projection over the active snapshot of a source.
pub fn query_structural(
    layout: &SnapshotLayout,
    filter: &StructFilter,
    limit: Option<usize>,
) -> Result<StructResult, QueryError> {
    let rev = layout
        .active_revision()?
        .ok_or_else(|| QueryError::NoActiveSnapshot(layout.source_id().to_string()))?;
    let records = layout.load_records(&rev)?;
    let mut matched = Vec::new();
    for r in records {
        if filter.matches(&r) {
            matched.push(r);
        }
    }
    let total_matched = matched.len() as u64;
    if let Some(l) = limit {
        matched.truncate(l);
    }
    let record_ids = matched.iter().map(|r| r.id.clone()).collect();
    Ok(StructResult {
        record_ids,
        records: matched,
        total_matched,
    })
}

/// Aggregate counts over a payload field, optionally subgrouping by another
/// payload path. Returns `(payload_value, count)` pairs ordered by count desc.
pub fn aggregate_by(
    layout: &SnapshotLayout,
    filter: &StructFilter,
    group_path: &str,
) -> Result<Vec<(String, u64)>, QueryError> {
    let rev = layout
        .active_revision()?
        .ok_or_else(|| QueryError::NoActiveSnapshot(layout.source_id().to_string()))?;
    let records = layout.load_records(&rev)?;
    let mut counts: std::collections::BTreeMap<String, u64> = Default::default();
    for r in records {
        if !filter.matches(&r) {
            continue;
        }
        // Fall back to the record id when the group path is absent.
        let key = json_path(&r.payload, group_path)
            .and_then(|v| match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .unwrap_or_else(|| r.id.clone());
        *counts.entry(key).or_default() += 1;
    }
    let mut out: Vec<(String, u64)> = counts.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(out)
}

/// Simple total-count aggregate.
pub fn count(
    layout: &SnapshotLayout,
    filter: &StructFilter,
) -> Result<u64, QueryError> {
    query_structural(layout, filter, None).map(|q| q.total_matched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lattice_lakehouse::manifest::{Acquisition, NormalizedRecordV1, SourceManifestV1};
    use lattice_lakehouse::SnapshotLayout;

    fn rec(id: &str, kind: &str, split: &str, lang: Option<&str>, payload: serde_json::Value) -> NormalizedRecordV1 {
        NormalizedRecordV1 {
            id: id.into(),
            source_id: "writing-preference".into(),
            source_revision: "v1".into(),
            kind: kind.into(),
            split: split.into(),
            language: lang.map(str::to_string),
            raw_artifact_ref: "x.json".into(),
            normalized_hash: String::new(),
            payload,
            evidence: vec![],
            relations: vec![],
        }
        .with_computed_hash()
    }

    fn mint(layout: &SnapshotLayout, records: &[NormalizedRecordV1]) {
        let m = SourceManifestV1 {
            source_id: "writing-preference".into(),
            source_revision: "v1".into(),
            acquisition: Acquisition::Git,
            raw_root: "raw".into(),
            adapter_version: "a".into(),
            schema_version: 1,
            complete_snapshot: true,
            partitions: vec![],
        };
        layout.commit(&m, records, 64).unwrap();
    }

    fn temp_layout(tag: &str) -> (std::path::PathBuf, SnapshotLayout) {
        let dir = std::env::temp_dir().join(format!("lattice-query-{tag}-{}", uuidish()));
        (dir.clone(), SnapshotLayout::new(&dir, "writing-preference"))
    }

    #[test]
    fn structural_filter_project_and_aggregate() {
        let (_dir, layout) = temp_layout("sa");
        mint(&layout, &[
            rec("p1", "creative-preference-pair", "train", Some("en"), serde_json::json!({"tag":"funny","score":3})),
            rec("p2", "creative-preference-pair", "train", Some("en"), serde_json::json!({"tag":"scriptwriting","score":1})),
            rec("p3", "creative-preference-pair", "train", Some("en"), serde_json::json!({"tag":"funny","score":3})),
            rec("p4", "creative-preference-pair", "train", Some("zh"), serde_json::json!({"tag":"funny","score":5})),
        ]);

        // Filter by kind + language.
        let f = StructFilter { kind: Some("creative-preference-pair".into()), language: Some("en".into()), ..Default::default() };
        let q = query_structural(&layout, &f, None).unwrap();
        assert_eq!(q.total_matched, 3);
        assert_eq!(q.record_ids, vec!["p1", "p2", "p3"]);

        // JSON-path predicate.
        let f2 = StructFilter { payload_path: Some(("tag".into(), "funny".into())), kind: Some("creative-preference-pair".into()), ..Default::default() };
        let q2 = query_structural(&layout, &f2, None).unwrap();
        assert_eq!(q2.total_matched, 3); // p1,p3,p4

        // Aggregate by language.
        let agg = aggregate_by(&layout, &StructFilter::default(), "tag").unwrap();
        assert_eq!(agg, vec![("funny".to_string(), 3), ("scriptwriting".to_string(), 1)]);

        // Count.
        assert_eq!(count(&layout, &StructFilter::default()).unwrap(), 4);
    }

    #[test]
    fn no_active_snapshot_is_reported() {
        let (_dir, layout) = temp_layout("empty");
        let err = query_structural(&layout, &StructFilter::default(), None).unwrap_err();
        assert!(matches!(err, QueryError::NoActiveSnapshot(_)));
    }

    fn uuidish() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64
    }
}