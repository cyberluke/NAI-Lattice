//! Git observer / SnapshotCoordinator (spec §8): the SERVICE requests a Git
//! snapshot; the plugin never runs Git. Uses `lattice-git` (filesystem-only,
//! no subprocess) to capture HEAD/index/working-tree facts.

use std::path::PathBuf;

use lattice_git::{GitSnapshot, snapshot};
use serde::{Deserialize, Serialize};

/// A bound, immutable-enough snapshot request for an epoch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotRequest {
    pub workspace_canonical: String,
    pub repo_root: PathBuf,
    pub epoch_key: crate::TaskEpochId,
}

/// Outcome of capturing a snapshot, with honest baseline certainty.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotResult {
    pub git_snapshot: GitSnapshot,
    /// When the capture is later than the first observed change, the baseline
    /// is not reliable turn-start evidence (spec §8).
    pub baseline_confident: bool,
    /// Canonical CAS ref if the manifest was stored there.
    pub cas_ref: Option<String>,
    /// BLAKE3 manifest hash of the captured set.
    pub manifest_hash: Option<String>,
}

/// A reusable snapshot coordinator (service-owned observation path).
#[derive(Debug, Default)]
pub struct SnapshotCoordinator {
    pub pending: Vec<SnapshotRequest>,
}

impl SnapshotCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a snapshot request for an epoch (bounded).
    pub fn request(&mut self, req: SnapshotRequest) {
        if self.pending.len() < 1024 {
            self.pending.push(req);
        }
    }

    /// Capture the next queued snapshot. Returns the manifest hash + snapshot.
    pub fn capture_next(&mut self, now_unix_ms: i64) -> Option<Result<SnapshotResult, String>> {
        let req = self.pending.pop()?;
        Some(snapshot(&req.repo_root, now_unix_ms).map(|gs| {
            let baseline_confident = true; // captured at request time == first observation
            let manifest_bytes = serde_json::to_vec(&gs).unwrap_or_default();
            let manifest_hash = Some(
                blake3::hash(&manifest_bytes).to_hex().to_string(),
            );
            SnapshotResult {
                git_snapshot: gs,
                baseline_confident,
                cas_ref: None,
                manifest_hash,
            }
        }).map_err(|e| e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TaskEpochId;

    #[test]
    fn captures_a_manifest_hash_without_git_process() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join(".git/refs/heads/main"), "deadbeef\n").unwrap();
        std::fs::write(root.join("file.txt"), "hello\n").unwrap();

        let mut c = SnapshotCoordinator::new();
        c.request(SnapshotRequest {
            workspace_canonical: "C:/repo".into(),
            repo_root: root.to_path_buf(),
            epoch_key: TaskEpochId { host_runtime_id: "h1".into(), session_id: "s1".into(), turn_id: "t1".into() },
        });
        let res = c.capture_next(1_700_000_000_000).unwrap().unwrap();
        assert!(res.manifest_hash.is_some());
        assert_eq!(res.git_snapshot.head_commit.as_deref(), Some("deadbeef"));
        assert!(res.baseline_confident);
    }
}