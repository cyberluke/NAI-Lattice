//! Streaming delta between two snapshot revisions of the same source.
//!
//! The delta planner implements the spec §2/§8 full-outer-join semantics:
//!
//! ```sql
//! SELECT COALESCE(n.id, p.id) AS id,
//!   CASE WHEN p.id IS NULL THEN 'INSERT'
//!        WHEN n.id IS NULL THEN 'DELETE'
//!        WHEN p.normalized_hash <> n.normalized_hash THEN 'UPDATE'
//!        ELSE 'UNCHANGED' END AS delta_type
//! FROM previous p FULL OUTER JOIN next n ON p.id = n.id;
//! ```
//!
//! Both revisions are sorted by record `id` before comparison, so the join is a
//! streaming two-pointer walk over sorted shards — never a giant in-RAM `Map`
//! (spec §2: "u milionových datasetů ten merge-join nesmí vytvořit obrovský
//! JavaScript `Map`"). This is the CPU/disk-friendly path for large sources.
//!
//! `DELETE` classification is applied **only** when the previous snapshot was a
//! complete one (`complete_snapshot=true`). A non-complete observation of the source
//! is never treated as evidence a record was removed (absent video is not a deleted
//! ad).

use crate::manifest::{DatasetDeltaV1, NormalizedRecordV1};

/// What happened to a single record across the two revisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Insert,
    Update,
    Delete,
    Unchanged,
}

impl DeltaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DeltaKind::Insert => "INSERT",
            DeltaKind::Update => "UPDATE",
            DeltaKind::Delete => "DELETE",
            DeltaKind::Unchanged => "UNCHANGED",
        }
    }
}

/// One per-record delta row.
#[derive(Debug, Clone)]
pub struct DeltaRow {
    pub id: String,
    pub kind: DeltaKind,
    pub previous_hash: Option<String>,
    pub current_hash: Option<String>,
}

/// Result of a delta pass, mirroring `DatasetDeltaV1`.
#[derive(Debug, Clone, Default)]
pub struct DeltaResult {
    pub inserts: Vec<String>,
    pub updates: Vec<String>,
    pub deletes: Vec<String>,
    pub unchanged: u64,
}

/// Sort records by `id` in place (stable) so the merge-join is a linear walk.
/// Streaming-friendly: can be called per shard chunk before merging.
pub fn sort_by_id(records: &mut [NormalizedRecordV1]) {
    records.sort_by(|a, b| a.id.cmp(&b.id));
}

/// Build the delta by streaming full-outer-join over two **id-sorted** slices.
///
/// `allow_deletes` must be `previous.complete_snapshot` (from its manifest). Callers
/// should pass `false` when the previous snapshot was incomplete.
pub fn diff_sorted(
    previous: &[NormalizedRecordV1],
    next: &[NormalizedRecordV1],
    allow_deletes: bool,
) -> DeltaResult {
    let mut out = DeltaResult::default();
    let (mut i, mut j) = (0usize, 0usize);
    while i < previous.len() || j < next.len() {
        match (previous.get(i), next.get(j)) {
            (Some(p), Some(n)) => {
                match p.id.cmp(&n.id) {
                    std::cmp::Ordering::Less => {
                        // Only in previous: candidate DELETE.
                        if allow_deletes {
                            out.deletes.push(p.id.clone());
                        }
                        i += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        // Only in next: INSERT.
                        out.inserts.push(n.id.clone());
                        j += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        if p.normalized_hash == n.normalized_hash {
                            out.unchanged += 1;
                        } else {
                            out.updates.push(p.id.clone());
                        }
                        i += 1;
                        j += 1;
                    }
                }
            }
            (Some(p), None) => {
                if allow_deletes {
                    out.deletes.push(p.id.clone());
                }
                i += 1;
            }
            (None, Some(n)) => {
                out.inserts.push(n.id.clone());
                j += 1;
            }
            (None, None) => break,
        }
    }
    out
}

/// Convenience: sort both sides, diff them, and return a [`DatasetDeltaV1`].
pub fn diff(
    source_id: &str,
    from_revision: Option<&str>,
    to_revision: &str,
    previous: &[NormalizedRecordV1],
    next: &[NormalizedRecordV1],
    allow_deletes: bool,
) -> DatasetDeltaV1 {
    let mut p = previous.to_vec();
    let mut n = next.to_vec();
    sort_by_id(&mut p);
    sort_by_id(&mut n);
    let r = diff_sorted(&p, &n, allow_deletes);
    DatasetDeltaV1 {
        source_id: source_id.to_string(),
        from_revision: from_revision.map(str::to_string),
        to_revision: to_revision.to_string(),
        inserts: r.inserts,
        updates: r.updates,
        deletes: r.deletes,
        unchanged: r.unchanged,
        changed_partitions: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, content: &str) -> NormalizedRecordV1 {
        let mut r = NormalizedRecordV1 {
            id: id.to_string(),
            source_id: "writing-preference".into(),
            source_revision: "rev".into(),
            kind: "creative-pair".into(),
            split: "train".into(),
            language: Some("en".into()),
            raw_artifact_ref: "x.json".into(),
            normalized_hash: String::new(),
            payload: serde_json::json!({"c": content}),
            evidence: vec![],
            relations: vec![],
        };
        r.payload = serde_json::json!({"c": content});
        r.with_computed_hash()
    }

    #[test]
    fn full_outer_join_classifies_all_four_delta_kinds() {
        // previous: r1(unchanged), r2(update), r3(delete)
        // next:     r1(unchanged), r2(changed), r4(insert)
        let prev = vec![rec("r1", "a"), rec("r2", "b"), rec("r3", "c")];
        let next = vec![rec("r1", "a"), rec("r2", "B"), rec("r4", "d")];
        let d = diff("s", Some("v1"), "v2", &prev, &next, true);
        assert_eq!(d.inserts, vec!["r4".to_string()]);
        assert_eq!(d.updates, vec!["r2".to_string()]);
        assert_eq!(d.deletes, vec!["r3".to_string()]);
        assert_eq!(d.unchanged, 1);
        assert!(!d.is_empty());
    }

    #[test]
    fn idempotent_rerun_is_empty_delta() {
        let recs = vec![rec("r1", "a"), rec("r2", "b"), rec("r3", "c")];
        let d = diff("s", Some("v1"), "v2", &recs, &recs, true);
        assert!(d.inserts.is_empty());
        assert!(d.updates.is_empty());
        assert!(d.deletes.is_empty());
        assert_eq!(d.unchanged, 3);
        assert!(d.is_empty());
    }

    #[test]
    fn deletes_suppressed_when_previous_snapshot_incomplete() {
        let prev = vec![rec("r1", "a"), rec("r2", "b")];
        let next = vec![rec("r2", "b")]; // r1 now absent (e.g. referenced video missing)
        // allow_deletes=false -> r1 must NOT be classified as DELETE.
        let d = diff("s", Some("v1"), "v2", &prev, &next, false);
        assert!(d.deletes.is_empty());
        assert!(d.updates.is_empty());
        // r1 is simply not present in the next (incomplete) observation.
    }
}