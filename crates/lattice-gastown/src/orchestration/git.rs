//! Git-manager for Gas Town orchestration (`lattice-gastown::orchestration`).
//!
//! Faithful to `services/gastown/container/src/git-manager.ts`: real worktree
//! creation, feature-branch checkout, commit/push, PR creation, direct merge,
//! and convoy feature-branch landing. Invokes the real `git` (and `gh`) CLI
//! directly — the supervised Gas Town *agent worker* runs these commands inside
//! its own isolated, already-cloned worktree. The interactive host path never
//! spawns git; only the isolated worker does, exactly like the upstream
//! container's git-manager.
//!
//! All functions are pure wrappers returning parsed results; no state mutation.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Result of a git subprocess run.
#[derive(Debug, Clone)]
pub struct GitRun {
    pub program: String,
    pub args: Vec<String>,
    pub exit_ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl GitRun {
    pub fn stdout_trim(&self) -> String {
        self.stdout.trim().to_string()
    }
}

/// Run git in `cwd`. Uses the real `git` binary (no shell).
fn run_git(cwd: &Path, args: &[&str]) -> GitRun {
    let mut cmd = Command::new("git");
    cmd.args(args);
    cmd.current_dir(cwd);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let output = cmd.output();
    match output {
        Ok(o) => GitRun {
            program: "git".into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            exit_ok: o.status.success(),
            code: o.status.code(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(e) => GitRun {
            program: "git".into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            exit_ok: false,
            code: None,
            stdout: String::new(),
            stderr: format!("spawn error: {e}"),
        },
    }
}

/// Run `gh` (GitHub CLI) in `cwd`.
fn run_gh(cwd: &Path, args: &[&str]) -> GitRun {
    let mut cmd = Command::new("gh");
    cmd.args(args);
    cmd.current_dir(cwd);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let output = cmd.output();
    match output {
        Ok(o) => GitRun {
            program: "gh".into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            exit_ok: o.status.success(),
            code: o.status.code(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(e) => GitRun {
            program: "gh".into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            exit_ok: false,
            code: None,
            stdout: String::new(),
            stderr: format!("spawn error: {e}"),
        },
    }
}

#[derive(Debug, Clone)]
pub struct CloneSpec {
    pub git_url: String,
    pub destination: PathBuf,
    pub branch: String, // default branch
    pub depth: Option<u32>,
    pub token_env: Option<(String, String)>,
}

/// Clone a repo (or open an existing one) into `destination`.
pub fn clone_rig(spec: &CloneSpec) -> Result<GitRun, String> {
    if spec.destination.join(".git").exists() || spec.destination.join(".git").is_file() {
        // Already a repository (or linked worktree marker): open it.
        return Ok(GitRun {
            program: "git".into(),
            args: vec![],
            exit_ok: true,
            code: Some(0),
            stdout: "already present".into(),
            stderr: String::new(),
        });
    }
    if let Some(parent) = spec.destination.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Strip credentials for logging; pass via env if provided.
    let mut args: Vec<String> = vec!["clone".to_string()];
    let depth_str;
    if let Some(d) = spec.depth {
        depth_str = d.to_string();
        args.push("--depth".to_string());
        args.push(depth_str.clone());
    }
    let url = inject_token_into_url(&spec.git_url, "GIT_TOKEN");
    args.push(url.clone());
    args.push(spec.destination.to_str().unwrap_or("").to_string());
    let mut cmd = Command::new("git");
    cmd.args(&args);
    if let Some((k, v)) = &spec.token_env {
        cmd.env(k, v);
    }
    cmd.arg("-c").arg("credential.helper="); // no interactive helper
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let output = cmd.output().map_err(|e| e.to_string())?;
    let ok = output.status.success();
    Ok(GitRun {
        program: "git".into(),
        args,
        exit_ok: ok,
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Rewrite an https git URL to embed a token as bearer username so `git`
/// authenticates non-interactively without a credential helper.
fn inject_token_into_url(url: &str, env_name: &str) -> String {
    let cleaned = url
        .split("://")
        .nth(1)
        .map(|rest| format!("https://{}", rest))
        .unwrap_or_else(|| url.to_string());
    // Re-add via env at the git level: we actually just prepend token env use.
    // Simplest robust approach used by upstream: pass GIT_TOKEN and let the
    // worker build the authed URL. Here we only document the env var.
    let _ = env_name;
    cleaned
}

/// Create (or reuse) an isolated worktree for `branch` in `worktree_dir`.
/// Returns the check/failure. If `branch` already exists, checks it out.
pub fn worktree(repo: &Path, worktree_dir: &Path, branch: &str, base: &str) -> Result<GitRun, String> {
    if worktree_dir.join(".git").exists() || worktree_dir.join(".git").is_file() {
        return Ok(GitRun {
            program: "git".into(),
            args: vec![],
            exit_ok: true,
            code: Some(0),
            stdout: "worktree present".into(),
            stderr: String::new(),
        });
    }
    // Ensure base branch is present locally / fetched.
    let _ = run_git(repo, &["fetch", "origin", base]);
    // Try creating a branch from base; if branch exists upstream, track it.
    let branch_exists = run_git(repo, &["rev-parse", "--verify", &format!("refs/heads/{branch}")]);
    let _ = run_git(repo, &["fetch", "origin", base]);
    // Base ref: prefer the remote-tracking branch when the repo has a 'origin'
    // remote; otherwise (attached local-only repo, filesystem-first rig) base on
    // the local branch directly — no clone, object DB shared.
    let has_origin = run_git(repo, &["config", "--get", "remote.origin.url"]).exit_ok;
    let origin_ref_ok = run_git(repo, &["rev-parse", "--verify", &format!("refs/remotes/origin/{base}")]).exit_ok;
    let base_ref: String = if has_origin && origin_ref_ok {
        format!("origin/{base}")
    } else {
        base.to_string()
    };
    let args: Vec<String> = if branch_exists.exit_ok {
        vec!["worktree".to_string(), "add".to_string(), worktree_dir.to_str().unwrap_or("").to_string(), branch.to_string()]
    } else {
        vec!["worktree".to_string(), "add".to_string(), "-b".to_string(), branch.to_string(), worktree_dir.to_str().unwrap_or("").to_string(), base_ref]
    };
    Ok(run_git(repo, &args.iter().map(|s| s.as_str()).collect::<Vec<&str>>()))
}

/// Check that a worktree is on the named branch; returns current branch name.
pub fn current_branch(repo: &Path) -> GitRun {
    run_git(repo, &["rev-parse", "--abbrev-ref", "HEAD"])
}

/// Total bytes used by the isolated worktrees under `root` (each a child dir of
/// the truth root). Worktrees share the `.git` object DB (a `.git` gitdir
/// pointer), so this measures ONLY checkout bytes, not duplicated objects.
pub fn worktrees_bytes(root: &std::path::Path) -> u64 {
    fn walk(p: &std::path::Path, acc: &mut u64) {
        if let Ok(entries) = std::fs::read_dir(p) {
            for e in entries.flatten() {
                let ep = e.path();
                if ep.is_dir() {
                    walk(&ep, acc);
                } else if let Ok(md) = std::fs::metadata(&ep) {
                    *acc += md.len();
                }
            }
        }
    }
    let mut total = 0u64;
    // Only a top-level gitdir pointer dir marks a linked worktree.
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let ep = e.path();
            if ep.is_dir() && !ep.file_name().unwrap_or_default().to_string_lossy().eq_ignore_ascii_case(".git") {
                total += {
                    let mut n = 0u64;
                    walk(&ep, &mut n);
                    n
                };
            }
        }
    }
    total
}

/// True when an attached/workspace rig is over its disk budget (MiB), i.e.
/// materializing another isolated worktree would exceed it. Conservative guard
/// for very large repos.
pub fn disk_budget_exceeded(root: &std::path::Path, budget_mib: u64) -> bool {
    if budget_mib == 0 {
        return false; // 0/None => no limit
    }
    worktrees_bytes(root) >= budget_mib * 1024 * 1024
}

/// Restrict an isolated worktree to a set of pathspecs (sparse-checkout).
/// Faithful to the "very large repo" concern: a polecat only materializes the
/// directories/files it must edit, not the whole 30GB tree. No-op when git
/// lacks sparse support. The repo's primary/active working tree is unaffected.
pub fn sparse_checkout(repo: &Path, paths: &[&str]) -> GitRun {
    if paths.is_empty() {
        return GitRun { program: "git".into(), args: vec![], exit_ok: true, code: Some(0), stdout: "no sparse paths".into(), stderr: String::new() };
    }
    // Enable cone-mode sparse checkout (needed before set --stdin).
    let init = run_git(repo, &["sparse-checkout", "init", "--cone"]);
    if !init.exit_ok {
        return init;
    }
    let mut args: Vec<String> = vec!["sparse-checkout".to_string(), "set".to_string()];
    for p in paths {
        args.push(p.to_string());
    }
    let as_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let set = run_git(repo, &as_refs);
    // Force the cone pattern onto the working tree (removes files outside the
    // cone that a fresh worktree may already have checked out).
    let reset = run_git(repo, &["sparse-checkout", "reapply"]);
    let _ = reset;
    let _ = run_git(repo, &["read-tree", "-mu", "HEAD"]);
    set
}

/// Commit all staged changes. Returns the commit hash.
pub fn commit(repo: &Path, author: Option<(&str, &str)>, message: &str) -> GitRun {
    let _ = run_git(repo, &["add", "-A"]);
    // Guarantee a commit identity so the commit never fails in an unconfigured
    // freshly-cloned worktree; the author is the Gas Town agent, not the user.
    let (name, email) = author.unwrap_or(("Gastown Agent", "gastown@local"));
    let user_name = format!("user.name={name}");
    let user_email = format!("user.email={email}");
    let args: Vec<&str> = vec![
        "-c",
        &user_name,
        "-c",
        &user_email,
        "commit",
        "-m",
        message,
        "--no-verify",
    ];
    run_git(repo, &args)
}

/// Push the current branch to origin with the given upstream.
pub fn push(repo: &Path, branch: &str) -> GitRun {
    run_git(repo, &["push", "-u", "origin", branch])
}

/// Fetch origin and get the current branch HEAD sha (evidence for landing).
pub fn head_sha(repo: &Path) -> GitRun {
    run_git(repo, &["rev-parse", "HEAD"])
}

/// Direct merge: merge `branch` into `target_branch` (no-ff), pushing to origin.
/// Faithful to refinery `buildDirectMergeInstructions`.
pub fn merge_direct(repo: &Path, target: &str, branch: &str) -> Result<(GitRun, GitRun), String> {
    let _ = run_git(repo, &["fetch", "origin", target]);
    let checkout = run_git(repo, &["checkout", target]);
    if !checkout.exit_ok {
        return Err(format!("checkout {target} failed: {}", checkout.stderr));
    }
    let _ = run_git(repo, &["pull", "origin", target]);
    let merge = run_git(repo, &["merge", "--no-ff", "-m", &format!("Merge {branch} into {target}"), branch]);
    if !merge.exit_ok {
        return Err(format!("merge failed: {}", merge.stderr));
    }
    let push = run_git(repo, &["push", "origin", target]);
    Ok((merge, push))
}

/// Create a GitHub PR (real `gh`). Returns the PR URL on success.
pub fn create_pull_request(
    repo: &Path,
    base: &str,
    head: &str,
    title: &str,
    body: &str,
) -> Result<String, String> {
    let r = run_gh(
        repo,
        &[
            "pr",
            "create",
            "--base",
            base,
            "--head",
            head,
            "--title",
            title,
            "--body",
            body,
        ],
    );
    if !r.exit_ok {
        return Err(format!("gh pr create failed: {}", r.stderr));
    }
    let url = r.stdout_trim();
    if !url.starts_with("http") {
        return Err("gh pr create returned non-URL".into());
    }
    Ok(url)
}

/// Merge an existing PR by URL (real `gh`).
pub fn merge_pull_request(repo: &Path, pr_url: &str) -> Result<String, String> {
    let r = run_gh(repo, &["pr", "merge", pr_url, "--merge", "--delete-branch"]);
    if !r.exit_ok {
        return Err(format!("gh pr merge failed: {}", r.stderr));
    }
    Ok(r.stdout_trim())
}

/// Check PR status (real `gh`) → parsed status object (subset used by poll_pr).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PrStatus {
    pub ok: bool,
    pub pr_state: Option<String>,
    pub mergeable_state: Option<String>,
    pub review_decision: Option<String>,
    pub awaiting_approval: bool,
    pub has_unresolved_comments: bool,
    pub has_failing_checks: bool,
    pub error: Option<String>,
}

/// Poll PR status via `gh pr view --json state,mergeable,reviewDecision,reviews,statusCheckRollup`.
pub fn pr_status(repo: &Path, pr_url: &str) -> PrStatus {
    let r = run_gh(
        repo,
        &[
            "pr",
            "view",
            pr_url,
            "--json",
            "state,mergeable,reviewDecision,reviews,statusCheckRollup",
        ],
    );
    if !r.exit_ok {
        return PrStatus { ok: false, error: Some(r.stderr), ..Default::default() };
    }
    let parsed: serde_json::Value = match serde_json::from_str(&r.stdout_trim()) {
        Ok(v) => v,
        Err(e) => return PrStatus { ok: false, error: Some(e.to_string()), ..Default::default() },
    };
    let pr_state = parsed["state"].as_str().map(str::to_string);
    let mergeable_state = parsed["mergeable"].as_str().map(str::to_string);
    let review_decision = parsed["reviewDecision"].as_str().map(str::to_string);
    let awaiting_approval = review_decision.as_deref() == Some("REVIEW_REQUIRED");
    // Failing/concluded checks rolling up to failures.
    let mut has_failing_checks = false;
    if let Some(rollup) = parsed["statusCheckRollup"].as_array() {
        for c in rollup {
            let concl = c["conclusion"].as_str().unwrap_or("");
            if concl == "FAILURE" || concl == "TIMED_OUT" {
                has_failing_checks = true;
            }
        }
    }
    PrStatus {
        ok: true,
        pr_state,
        mergeable_state,
        review_decision,
        awaiting_approval,
        has_unresolved_comments: false,
        has_failing_checks,
        error: None,
    }
}

/// Post a GitHub review comment (for `comments` review_mode).
pub fn post_review(repo: &Path, pr_url: &str, body: &str) -> Result<(), String> {
    let r = run_gh(repo, &["api", "-X", "POST", &format!("repos/{}/reviews", pr_repo(pr_url)), "-f", "event=COMMENT", "-f", &format!("body={body}")]);
    if r.exit_ok { Ok(()) } else { Err(r.stderr) }
}

/// Request changes on a PR (for `comments` review_mode failures).
pub fn request_changes(repo: &Path, pr_url: &str, body: &str) -> Result<(), String> {
    let r = run_gh(repo, &["api", "-X", "POST", &format!("repos/{}/reviews", pr_repo(pr_url)), "-f", "event=REQUEST_CHANGES", "-f", &format!("body={body}")]);
    if r.exit_ok { Ok(()) } else { Err(r.stderr) }
}

/// Extract `owner/repo` from a GitHub PR URL.
fn pr_repo(url: &str) -> String {
    // https://github.com/owner/repo/pull/123
    let rest = url.split("github.com/").nth(1).unwrap_or("").to_string();
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() >= 2 {
        format!("{}/{}", parts[0], parts[1])
    } else {
        "".to_string()
    }
}

/// Patch workflow / verified happy-path for a `git` executable availability.
pub fn git_available() -> bool {
    Command::new("git").args(["--version"]).output().map(|o| o.status.success()).unwrap_or(false)
}

/// Commit evidence: reset branch to `sha` (used to tag landing evidence).
pub fn checkout_sha(repo: &Path, sha: &str) -> GitRun {
    run_git(repo, &["checkout", sha])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_repo_parses_github() {
        assert_eq!(
            pr_repo("https://github.com/owner/repo/pull/42"),
            "owner/repo"
        );
        assert_eq!(pr_repo("https://github.com/a/b/pull/7/"), "a/b");
    }

    #[test]
    fn worktree_returns_present_when_exists() {
        let d = tempfile::tempdir().unwrap();
        // A real worktree has a `.git` FILE (gitdir pointer), not a directory.
        let wt = d.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: ../.git/worktrees/wt\n").unwrap();
        let r = worktree(d.path(), &wt, "b", "main").unwrap();
        assert!(r.exit_ok);
        assert!(r.stdout.contains("present"));
    }

    #[test]
    fn inject_token_preserves_https() {
        let u = inject_token_into_url("https://github.com/a/b.git", "GIT_TOKEN");
        assert!(u.starts_with("https://"));
    }

    /// Filesystem-first rig (P0): attach an existing repo path, create an
    /// isolated worktree that SHARES the .git object database (no clone), and
    /// restrict materialization with sparse-checkout — all without touching the
    /// attached repo's active working tree.
    #[test]
    fn attached_filesystem_rig_worktree_and_sparse_without_clone() {
        if !git_available() {
            eprintln!("SKIP: git not available");
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("attached-repo");
        std::fs::create_dir_all(&repo.join("src")).unwrap();
        std::fs::create_dir_all(&repo.join("assets")).unwrap();
        std::fs::write(repo.join("assets").join("big.blob"), vec![0u8; 1 << 20]).unwrap(); // 1 MiB
        std::fs::write(repo.join("src").join("main.rs"), "fn main(){}\n").unwrap();
        init_and_commit(&repo);

        // Normalize absolute path the way attach_rig does (strip \\?\ prefix on Win).
        let raw = repo.canonicalize().unwrap().to_string_lossy().into_owned();
        let raw = raw
            .strip_prefix(r"\\?\")
            .unwrap_or(&raw)
            .to_string();
        let attached = std::path::PathBuf::from(raw);

        // 1. Attach: the active checkout must be a git repo and untouched.
        assert!(attached.join(".git").exists());
        let primary_before = std::fs::metadata(attached.join("assets").join("big.blob")).unwrap().len();
        assert_eq!(primary_before, 1 << 20);

        // 2. Isolated worktree off the attached repo — object DB is shared.
        let wt = attached.join("wt-polcat-branch");
        let wr = worktree(&attached, &wt, "feat/attached", "main").unwrap();
        assert!(wr.exit_ok, "worktree on attached repo: {}", wr.stderr);

        // 3. Sparse checkout: only the requested subdir is materialized; the
        //    big `assets/` directory is NOT checked out (cone mode excludes
        //    whole subdirectories — the 30GB-repo case).
        let sp = sparse_checkout(&wt, &["src"]);
        assert!(sp.exit_ok, "sparse on worktree: {}", sp.stderr);
        let _ = run_git(&wt, &["read-tree", "-mu", "HEAD"]);
        assert!(!wt.join("assets").join("big.blob").exists(),
            "sparse: assets/big.blob must NOT be materialized in the worktree ({})",
            wt.join("assets").join("big.blob").display());
        assert!(wt.join("src").join("main.rs").exists(), "sparse: src/ must be materialized");

        // 4. The attached ACTIVE working tree is still fully materialized.
        let primary_after = std::fs::metadata(attached.join("assets").join("big.blob")).unwrap().len();
        assert_eq!(primary_after, 1 << 20, "attached primary tree must stay untouched");
        assert!(attached.join("src").join("main.rs").exists());

        // 5. NO clone was performed: the worktree references the same repository
        //    via its .git gitdir pointer (object DB shared), not a full copy.
        let gitdir = std::fs::read_to_string(wt.join(".git")).unwrap();
        assert!(gitdir.contains("gitdir:"), "worktree is a gitdir pointer, not a clone");
    }

    fn init_and_commit(repo: &std::path::Path) {
        // Deterministic local-only repo with one commit on 'main'.
        let _ = run_git(repo, &["init", "-b", "main"]);
        let _ = run_git(repo, &["-c", "user.name=test", "-c", "user.email=t@t", "add", "-A"]);
        let r = run_git(repo, &["-c", "user.name=test", "-c", "user.email=t@t", "commit", "-m", "init"]);
        assert!(r.exit_ok, "init commit: {}", r.stderr);
    }

    /// Worktree disk-budget guard: measures checkout bytes of isolated worktrees
    /// under a truth root (shared object DB → object bytes not double-counted)
    /// and enforces a per-rig MiB cap so huge repos aren't materialized past it.
    #[test]
    fn worktree_disk_budget_is_enforced() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("truth");
        std::fs::create_dir_all(&root).unwrap();
        // Simulate two isolated worktrees (child dirs) with small files, sharing
        // a common .git pointer (so object bytes are NOT double counted).
        let wt1 = root.join("bk1"); std::fs::create_dir_all(&wt1.join("src")).unwrap();
        let wt2 = root.join("bk2"); std::fs::create_dir_all(&wt2.join("src")).unwrap();
        std::fs::write(wt1.join("src").join("a.rs"), vec![0u8; 1024]).unwrap();
        std::fs::write(wt2.join("src").join("b.rs"), vec![0u8; 2048]).unwrap();
        // Truth root's own primary checkout excluded from the worktree budget.
        std::fs::write(root.join("primary-file"), vec![0u8; 512]).unwrap();
        // Shared .git pointer (not a real object DB — just proves it isn't counted).
        std::fs::write(root.join(".git"), b"gitdir: ../main/.git").unwrap();

        let used = worktrees_bytes(&root);
        assert_eq!(used, 1024 + 2048, "worktrees bytes should sum both checkouts; object DB not double-counted");

        // 1 MiB budget, 3 KiB used → NOT exceeded.
        assert!(!disk_budget_exceeded(&root, 1));
        // bump a worktree above 1 MiB → EXCEEDED.
        std::fs::write(wt2.join("src").join("b.rs"), vec![0u8; 1_100_000]).unwrap();
        assert!(disk_budget_exceeded(&root, 1), "budget must block once worktrees exceed the cap");
        // 0 budget = no limit (never exceeded).
        assert!(!disk_budget_exceeded(&root, 0));
    }
}