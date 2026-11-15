//! End-to-end federated ingest over **real** WritingPreferenceBench data.
//!
//! Pipeline (spec §12 step 1):
//!   1. PARSE the real acquired records (`D:\NAI\DataLake\raw\writing-preference`,
//!      pinned at `676d7114ce37ad5d8eff5b39f3648b855730bf77`).
//!   2. WRITE real Parquet shard + immutable snapshot manifest.
//!   3. REPARSE (same source) -> commit -> assert zero delta (idempotence).
//!   4. QUERY structurally by kind + language, and aggregate by prompt tag.
//!   5. BUILD a reproducible knowledge pack from full records; assert it keeps both
//!      the chosen and rejected responses.
//!
//! The test self-skips when the real acquisition tree is absent (so CI is not
//! coupled to the data lake), but on this machine it runs against the verified
//! 1,800-record snapshot.

use lattice_lakehouse::adapters::WritingPreferenceAdapter;
use lattice_lakehouse::snapshot::SnapshotLayout;
use lattice_lakehouse::{Acquisition, SourceManifestV1};
use lattice_query::dataset_sql::StructFilter;
use lattice_query::{aggregate_by, build_pack, query_structural};

const RAW_ROOT: &str = r"D:\NAI\DataLake\raw\writing-preference";
const REV: &str = "676d7114ce37ad5d8eff5b39f3648b855730bf77";

fn real_root() -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(RAW_ROOT);
    p.join("WP_bench_english-1.json").exists().then(|| p.to_path_buf())
}

#[test]
fn e2e_ingest_query_and_pack_on_real_data() {
    let Some(root) = real_root() else {
        eprintln!("SKIP e2e: writing-preference not acquired at {RAW_ROOT}");
        return;
    };

    let lake = std::env::temp_dir().join(format!("lattice-e2e-{}", uuidish()));
    let layout = SnapshotLayout::new(&lake, "writing-preference");
    let adapter = WritingPreferenceAdapter::new();

    // 1+2. Parse the real files and commit a snapshot.
    let records = adapter.parse_dir(&root, "writing-preference", REV).unwrap();
    assert_eq!(records.len(), 1800, "real pair count");
    let manifest = SourceManifestV1 {
        source_id: "writing-preference".into(),
        source_revision: REV.into(),
        acquisition: Acquisition::Git,
        raw_root: RAW_ROOT.into(),
        adapter_version: adapter.adapter_version.clone(),
        schema_version: 1,
        complete_snapshot: true,
        partitions: vec![],
    };
    let hash1 = layout.commit(&manifest, &records, 512).unwrap();
    assert!(layout.exists(REV));
    assert_eq!(layout.active_revision().unwrap().as_deref(), Some(REV));

    // 3. Re-parse (same source, same parser) -> commit again -> zero delta.
    let records2 = adapter.parse_dir(&root, "writing-preference", REV).unwrap();
    let hash2 = layout.commit(&manifest, &records2, 512).unwrap();
    // Same source+records -> the same manifest content hash (idempotence).
    assert_eq!(hash1, hash2);
    // Delta must be a no-op: identical normalized hashes -> no inserts/updates/deletes.
    let delta = lattice_lakehouse::delta::diff(
        "writing-preference",
        Some(REV),
        REV,
        &records,
        &records2,
        true,
    );
    assert!(delta.is_empty(), "idempotent re-ingest must produce a zero delta");

    // 4. Structural query: all English creative-preference-pair records.
    let f = StructFilter {
        kind: Some("creative-preference-pair".into()),
        language: Some("en".into()),
        split: Some("train".into()),
        ..Default::default()
    };
    let q = query_structural(&layout, &f, Some(50)).unwrap();
    assert_eq!(q.total_matched, 1200, "english has 600+600 pairs");
    assert_eq!(q.record_ids.len(), 50, "limit honored");
    // Aggregate by prompt tag across the whole (nonempty) source.
    let agg = aggregate_by(&layout, &StructFilter::default(), "tag").unwrap();
    assert!(!agg.is_empty());

    // 5. Knowledge pack from the real matched records (both sides preserved).
    let pack = build_pack(
        "creative",
        "rankPair",
        vec![hash1.clone()],
        &q.records,
        vec!["Never lose the rejected response".into()],
    );
    assert_eq!(q.records.len(), 50);
    assert_eq!(pack.comparisons.len(), q.records.len());
    for c in &pack.comparisons {
        assert!(c.right_id.ends_with("/rejected"), "rejected side present");
    }
    assert!(pack.token_count > 0);
    // Reproducibility gate: same inputs -> same pack hash.
    let pack2 = build_pack(
        "creative",
        "rankPair",
        vec![hash1],
        &q.records,
        vec!["Never lose the rejected response".into()],
    );
    assert_eq!(pack.pack_hash, pack2.pack_hash);

    std::fs::remove_dir_all(&lake).ok();
}

fn uuidish() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64
}