//! AAF-backed dataset federation for the lakehouse (spec §11/§7).
//!
//! A materialized snapshot lives on local disk; this module exposes its snapshot
//! directory as an AAF [`ExternalArtifactSource`] so other subsystems can discover
//! and range-read the canonical Parquet shards without re-copying them. Follows the
//! AAF doctrine: never re-acquire bytes that are already a verified local snapshot.

use std::sync::Arc;

use lattice_aaf::source::DirectorySource;

use crate::snapshot::SnapshotLayout;

/// A federation object exposing one source's materialized snapshots to the AAF
/// resolver. The provider `id` is `datasets/<source_id>`; its root is the snapshot
/// parent `<root>/snapshots/<source_id>`.
pub struct DatasetFederation {
    layout: SnapshotLayout,
}

impl DatasetFederation {
    /// Build a federation for `source_id` under `root`.
    pub fn new(root: &std::path::Path, source_id: &str) -> Self {
        let layout = SnapshotLayout::new(root, source_id);
        Self { layout }
    }

    /// Register this source's snapshots with an AAF resolver so its canonical
    /// Parquet shards are discoverable as federated artifacts. The provider wraps a
    /// fresh [`DirectorySource`] over the snapshot root (DirectorySource is not
    /// Clone; it only holds an id + root).
    pub fn register(&self, resolver: &lattice_aaf::AafResolver) {
        let provider: Arc<dyn lattice_aaf::source::ExternalArtifactSource> = Arc::new(
            DirectorySource::new(&self.provider_id(), self.root()),
        );
        resolver.register(provider).ok();
    }

    /// The underlying snapshot layout (for direct read/write).
    pub fn layout(&self) -> &SnapshotLayout {
        &self.layout
    }

    /// The provider id under which this source is federated.
    pub fn provider_id(&self) -> String {
        format!("datasets/{}", self.layout.source_id())
    }

    /// The on-disk provider root (where snapshots live).
    pub fn root(&self) -> std::path::PathBuf {
        self.layout.snapshots_root().to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Acquisition, NormalizedRecordV1, SourceManifestV1};

    fn sample(id: &str) -> NormalizedRecordV1 {
        NormalizedRecordV1 {
            id: id.to_string(),
            source_id: "writing-preference".into(),
            source_revision: "v1".into(),
            kind: "creative-preference-pair".into(),
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

    #[test]
    fn federation_exposes_snapshot_shards_as_artifacts() {
        let dir = std::env::temp_dir().join(format!("lattice-fed-{}", uuidish()));
        let m = SourceManifestV1 {
            source_id: "writing-preference".into(),
            source_revision: "v1".into(),
            acquisition: Acquisition::Git,
            raw_root: "D:\\NAI\\DataLake\\raw\\writing-preference".into(),
            adapter_version: "a".into(),
            schema_version: 1,
            complete_snapshot: true,
            partitions: vec![],
        };
        let fed = DatasetFederation::new(&dir, "writing-preference");
        fed.layout()
            .commit(&m, &vec![sample("p1"), sample("p2")], 64)
            .unwrap();

        // Register with an AAF resolver and discover the committed shard. Need a
        // CAS to construct the resolver; use the in-memory/temp CAS via object store.
        let cas = Arc::new(
            lattice_cas::ObjectStore::new(dir.join("cas")).expect("temp cas"),
        );
        let resolver = lattice_aaf::AafResolver::new(cas);
        fed.register(&resolver);
        assert_eq!(resolver.providers(), vec!["datasets/writing-preference"]);
        let filt = lattice_aaf::DiscoverFilter { logical_type: None, media_type: None };
        let cands = resolver.discover_all(&filt);
        assert!(!cands.is_empty());
        assert_eq!(cands[0].provider_id, "datasets/writing-preference");

        std::fs::remove_dir_all(&dir).ok();
    }

    fn uuidish() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64
    }
}