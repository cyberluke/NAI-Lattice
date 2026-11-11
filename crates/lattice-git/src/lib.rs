//! Reusable Git observation for the Lattice service (spec §8).
//!
//! Captures an immutable-enough snapshot of a repository's HEAD/index/working
//! tree so the backend can reconstruct turn-start state, detect changes, and
//! record expected review coverage — WITHOUT spawning any subprocess from the
//! interactive path. This only reads `.git` metadata and the working tree via
//! the filesystem; it never runs `git`, never writes, never commits.
//!
//! A baseline captured later than the first code change is unreliable evidence
//! of turn-start state (spec §8); callers must note `BASELINE_UNCERTAIN` when
//! the snapshot timestamp is not attributable to the triggering event.

use std::path::{Path, PathBuf};

/// A filesystem-only snapshot of a Git repository at a point (no subprocess).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct GitSnapshot {
    /// Absolute path of the repository (workspace root).
    pub repo_root: PathBuf,
    /// Raw HEAD (e.g. `ref: refs/heads/main` or a detached SHA).
    pub head_raw: String,
    /// Resolved commit, when parseable (short prefix for display).
    pub head_commit: Option<String>,
    /// True when HEAD points at a branch ref (not detached).
    pub on_branch: bool,
    /// Unix ms of the snapshot read.
    pub captured_unix_ms: i64,
    /// BLAKE3 over the sorted relative paths + sizes + mtimes of tracked-ish
    /// working files under the repo (bounded, shallow). Used only as an
    /// optimistic change sensor; authoritative identity is the ledger.
    pub working_tree_fingerprint: String,
    /// Whether the fingerprint scan was truncated for safety (true => partial).
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a git repository (no .git/HEAD at {0})")]
    NotARepository(PathBuf),
}

const MAX_SCAN_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SCAN_FILES: usize = 4096;

/// Read `.git/HEAD` and, when it is a symlink-style or gitdir pointer, resolve
/// the actual gitdir path. Returns the real gitdir absolute path.
fn git_dir_of(repo_root: &Path) -> PathBuf {
    // Support worktree metadata: `<root>/.git` may contain `gitdir: <path>`.
    let dotgit = repo_root.join(".git");
    if let Ok(raw) = std::fs::read_to_string(&dotgit) {
        if let Some(rest) = raw.strip_prefix("gitdir:") {
            let target = rest.trim();
            let p = PathBuf::from(target);
            return if p.is_absolute() { p } else { repo_root.join(p) };
        }
    }
    dotgit
}

/// Capture a shallow, bounded Git + working-tree snapshot. Never runs `git`.
pub fn snapshot(repo_root: &Path, now_unix_ms: i64) -> Result<GitSnapshot, GitError> {
    let gitdir = git_dir_of(repo_root);
    let head_file = gitdir.join("HEAD");
    if !head_file.exists() {
        return Err(GitError::NotARepository(repo_root.to_path_buf()));
    }
    let head_raw = std::fs::read_to_string(&head_file)?;
    let head_raw = head_raw.trim().to_string();
    let (on_branch, head_commit) = if let Some(rest) = head_raw.strip_prefix("ref:") {
        let refname = rest.trim().to_string();
        // Resolve the ref file under refs/ when present.
        let ref_path = gitdir.join(refname.replace('/', std::path::MAIN_SEPARATOR_STR));
        let commit = std::fs::read_to_string(&ref_path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        (true, commit)
    } else {
        (false, Some(head_raw.clone()))
    };

    // Bounded working-tree fingerprint over source-like files (shallow, cached
    // stat data only; we do not parse blobs).
    let mut hasher = blake3::Hasher::new();
    let mut count = 0usize;
    let mut scanned = 0u64;
    let mut truncated = false;
    let mut entries: Vec<(String, u64, i64)> = Vec::new();
    let max_depth = 4;
    collect_entries(repo_root, repo_root, 0, max_depth, &mut entries, &mut count, &mut scanned, &mut truncated);
    // Skip entries under `.git` (the controller) for the working-tree signal.
    entries.sort();
    for (rel, size, mtime) in entries {
        hasher.update(rel.as_bytes());
        hasher.update(&size.to_le_bytes());
        hasher.update(&mtime.to_le_bytes());
    }
    let working_tree_fingerprint = hasher.finalize().to_hex().to_string();

    Ok(GitSnapshot {
        repo_root: repo_root.to_path_buf(),
        head_raw,
        head_commit,
        on_branch,
        captured_unix_ms: now_unix_ms,
        working_tree_fingerprint,
        truncated,
    })
}

fn collect_entries(
    root: &Path,
    dir: &Path,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<(String, u64, i64)>,
    count: &mut usize,
    scanned: &mut u64,
    truncated: &mut bool,
) {
    if depth > max_depth || *scanned >= MAX_SCAN_BYTES || *count >= MAX_SCAN_FILES {
        if *count >= MAX_SCAN_FILES || *scanned >= MAX_SCAN_BYTES {
            *truncated = true;
        }
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        if *count >= MAX_SCAN_FILES || *scanned >= MAX_SCAN_BYTES {
            *truncated = true;
            return;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" || name == "node_modules" || name == "target" || name == ".cache" {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().into_owned();
        if let Ok(meta) = entry.metadata() {
            if meta.is_dir() {
                collect_entries(root, &path, depth + 1, max_depth, out, count, scanned, truncated);
            } else if meta.is_file() && meta.len() > 0 {
                *count += 1;
                *scanned = scanned.saturating_add(meta.len());
                let mtime = meta.modified().ok().map(|t| t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)).unwrap_or(0);
                out.push((rel, meta.len(), mtime));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_head_and_fingerprint_without_subprocess() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let _ = std::fs::create_dir_all(root.join(".git/refs/heads"));
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join(".git/refs/heads/main"), "abc123def456\n").unwrap();
        std::fs::create_dir_all(root.join("src"));
        std::fs::write(root.join("src/main.rs"), "fn main(){}\n").unwrap();
        std::fs::write(root.join("README.md"), "# hi\n").unwrap();

        let snap = snapshot(root, 1_700_000_000_000).unwrap();
        assert_eq!(snap.on_branch, true);
        assert_eq!(snap.head_commit.as_deref(), Some("abc123def456"));
        assert_eq!(snap.head_raw, "ref: refs/heads/main");
        assert_eq!(snap.truncated, false);
        assert!(!snap.working_tree_fingerprint.is_empty());
        // Fingerprint is deterministic for identical trees.
        let snap2 = snapshot(root, 1_700_000_000_000).unwrap();
        assert_eq!(snap.working_tree_fingerprint, snap2.working_tree_fingerprint);
    }

    #[test]
    fn rejects_non_repository() {
        let dir = tempfile::tempdir().unwrap();
        assert!(snapshot(dir.path(), 0).is_err());
    }

    #[test]
    fn supports_gitdir_indirection() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("worktree");
        let g = dir.path().join("realgit");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(g.join("refs/heads")).unwrap();
        std::fs::write(g.join("HEAD"), "ref: refs/heads/dev\n").unwrap();
        std::fs::write(root.join(".git"), format!("gitdir: {}\n", g.display())).unwrap();
        std::fs::write(g.join("refs/heads/dev"), "feedface\n").unwrap();
        let snap = snapshot(&root, 1).unwrap();
        assert_eq!(snap.on_branch, true);
        assert_eq!(snap.head_commit.as_deref(), Some("feedface"));
    }
}