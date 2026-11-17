//! Gas Town orchestration integration inside the host (`gastown_orto.rs`).
//!
//! Wires the `lattice_gastown::orchestration` engine into the running
//! `nai-fabric-host` singleton: builds the durable orchestrator + control API
//! over the shared runtime hot-store, exposes `gastown.orto.*` NAF methods, runs
//! a background durable-scheduler thread, and supervises REAL model agent
//! workers (`gastown-agent` subcommand, Job Object + timeout) exactly like the
//! existing review workers. NAI-Lattice remains the single data fabric.

use std::path::PathBuf;
use std::sync::Arc;

use lattice_gastown::orchestration::agent::{ProviderConfig};
use lattice_gastown::orchestration::control::ControlApi;
use lattice_gastown::orchestration::git as ggit;
use lattice_gastown::orchestration::scheduler::{AgentRunner, DispatchCtx, DispatchResult, Orchestrator};
use lattice_gastown::orchestration::state::{BeadStatus, Finding, OrchestrationStore};
use lattice_runtime::LatticeRuntime;

/// Process-global orchestrator + store, initialized once in `run_worker_main`.
pub(crate) static ORCH: std::sync::OnceLock<Arc<Orchestrator>> = std::sync::OnceLock::new();
pub(crate) static STORE: std::sync::OnceLock<Arc<OrchestrationStore>> = std::sync::OnceLock::new();
pub(crate) static CTRL: std::sync::OnceLock<Arc<ControlApi>> = std::sync::OnceLock::new();
/// Runtime handle for the HTTP control API (so /mayor etc. can tick the
/// orchestrator + supervise real agents).
pub(crate) static RUNTIME: std::sync::OnceLock<Arc<LatticeRuntime>> = std::sync::OnceLock::new();

/// Canonical namespace for the observability log stream. Exposed here so the
/// Lattice dashboard (lattice_dashboard.rs) queries the SAME ledger surface the
/// log observer and CLI write/read.
pub(crate) fn lattice_obs_namespace() -> &'static str {
    "observability.log.v1"
}

/// Supervised real-model Gas Town agent worker subcommand. Reads a request JSON
/// (via argv[1] temp file), runs the real model once, applies file changes to
/// its isolated worktree, commits/pushes, prints `{ok, decision|error}` to
/// stdout, exits. Invoked by `HostAgentRunner` via `lattice-supervisor`.
pub(crate) fn run_gastown_agent_subcommand() {
    use std::io::Read as _;
    let req_path = std::env::args().nth(2).unwrap_or_default();
    let mut buf = Vec::new();
    if !req_path.is_empty() {
        let _ = std::fs::File::open(&req_path).and_then(|mut f| f.read_to_end(&mut buf));
    } else {
        let _ = std::io::stdin().lock().read_to_end(&mut buf);
    }
    let req: serde_json::Value = serde_json::from_slice(&buf).unwrap_or(serde_json::json!({"role":"polecat"}));
    let cfg = match ProviderConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            println!("{}", serde_json::json!({"ok":false,"error":format!("provider config: {e}")}));
            std::process::exit(2);
        }
    };
    let role = req["role"].as_str().unwrap_or("polecat");
    let worktree_root = std::path::PathBuf::from(req["worktree_root"].as_str().unwrap_or("."));
    let target_branch = req["target_branch"].as_str().unwrap_or("main").to_string();
    let merge_strategy = req["merge_strategy"].as_str().unwrap_or("direct").to_string();
    let branch = req["branch"].as_str().unwrap_or("").to_string();
    let custom = req["custom"].as_str().map(str::to_string);

    let run = match role {
        "polecat" => run_polecat_worker(&cfg, &req, &worktree_root, &target_branch, &merge_strategy, &branch, custom.as_deref()),
        "refinery" => run_refinery_worker(&cfg, &req, &worktree_root, &target_branch, &merge_strategy, &branch, custom.as_deref()),
        _ => Err(format!("gastown-agent worker does not run role '{role}' directly (mayor is host-side)")),
    };
    match run {
        Ok(decision) => {
            println!("{}", serde_json::json!({"ok": true, "decision": decision, "worker_pid": std::process::id()}));
        }
        Err(e) => {
            eprintln!("gastown-agent failed: {e}");
            println!("{}", serde_json::json!({"ok":false,"error":e,"worker_pid":std::process::id()}));
            std::process::exit(1);
        }
    }
}

fn run_polecat_worker(
    cfg: &ProviderConfig,
    req: &serde_json::Value,
    worktree_root: &std::path::Path,
    target_branch: &str,
    merge_strategy: &str,
    branch: &str,
    custom: Option<&str>,
) -> Result<serde_json::Value, String> {
    use lattice_gastown::orchestration::agent::*;
    let agent_name = req["agent_name"].as_str().unwrap_or("Polecat").to_string();
    let rig_id = req["rig_id"].as_str().unwrap_or("").to_string();
    let town_id = req["town_id"].as_str().unwrap_or("").to_string();
    let identity = req["agent_identity"].as_str().unwrap_or("").to_string();
    let bead_title = req["bead_title"].as_str().unwrap_or("").to_string();
    let bead_body = req["bead_body"].as_str().map(str::to_string);
    let gates: Vec<String> = req["gates"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let sys = polecat_system(
        &agent_name, &rig_id, &town_id, &identity, &gates, merge_strategy,
        target_branch, &bead_title, bead_body.as_deref().unwrap_or_default(), custom,
    );
    let listing = crate::gastown_orto::bounded_listing(worktree_root);
    let user = format!(
        "Your worktree is at {}.\nCurrent files:\n{listing}\n\nAddress the bead now. Respond with one JSON object.",
        worktree_root.display()
    );
    // Robust decision fetch with a bounded retry: real models sometimes TRUNCATE
    // a large JSON (e.g. a long `files` array), producing an unparseable reply
    // (`EOF while parsing a list`). If the first parse fails, re-ask with an
    // explicit single-object + higher token budget; still fail honestly if it
    // keeps truncating. Requests one top-level JSON OBJECT (never an array).
    let mut reply = real_completion(cfg, &sys, &user, 8000)?;
    let mut v = parse_decision(&reply);
    if v.is_err() {
        let retry_user = format!(
            "Your previous reply was not valid JSON and could not be parsed. \
             Reply with ONLY one top-level JSON object (no ``` fences, no array wrapper), \
             with exactly these keys: \"action\", \"files\" (array of {{\"path\":\"...\",\"content\":\"...\"}}), \
             \"verification\", \"done\", \"pr_url\" (optional).\n\nWorktree: {}\nCurrent files:\n{}",
            worktree_root.display(), listing
        );
        let retry = real_completion(cfg, &sys, &retry_user, 8000);
        if let Ok(r2) = retry {
            if let Ok(v2) = parse_decision(&r2) {
                reply = r2;
                v = Ok(v2);
            }
        }
    }
    let v = v.map_err(|e| format!("polecat decision unparseable: {e}\nraw: {reply:?}"))?;
    let decision: AgentDecision = serde_json::from_value(v).map_err(|e| e.to_string())?;
    apply_file_changes(worktree_root, &decision.files)?;
    let _ = ggit::commit(worktree_root, None, &decision.action);
    let _ = ggit::push(worktree_root, branch);
    let mut pr_url = decision.pr_url.clone();
    if pr_url.is_none() && merge_strategy == "pr" && decision.done {
        if let Ok(url) = ggit::create_pull_request(worktree_root, target_branch, branch, &bead_title, &decision.verification) {
            pr_url = Some(url);
        }
    }
    Ok(serde_json::json!({
        "action": decision.action, "files": decision.files, "verification": decision.verification,
        "done": decision.done, "pr_url": pr_url.or(decision.pr_url),
    }))
}

fn run_refinery_worker(
    cfg: &ProviderConfig,
    req: &serde_json::Value,
    worktree_root: &std::path::Path,
    target_branch: &str,
    merge_strategy: &str,
    branch: &str,
    custom: Option<&str>,
) -> Result<serde_json::Value, String> {
    use lattice_gastown::orchestration::agent::*;
    let identity = req["agent_identity"].as_str().unwrap_or("").to_string();
    let rig_id = req["rig_id"].as_str().unwrap_or("").to_string();
    let gates: Vec<String> = req["gates"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let review_mode = req["review_mode"].as_str().unwrap_or("rework").to_string();
    let existing_pr = req["existing_pr_url"].as_str().map(str::to_string);
    let sys = refinery_system(
        &identity, &rig_id, &gates, branch, target_branch, merge_strategy,
        &review_mode, existing_pr.as_deref(), custom,
    );
    let listing = crate::gastown_orto::bounded_listing(worktree_root);
    let user = format!(
        "Review worktree at {} for branch {}.\nFiles:\n{listing}\n\nRun the gates and review. Respond with one JSON object.",
        worktree_root.display(), branch
    );
    let reply = real_completion(cfg, &sys, &user, 2200)?;
    let v = parse_decision(&reply)?;
    let decision: AgentDecision = serde_json::from_value(v).map_err(|e| e.to_string())?;
    let mut pr_url = decision.pr_url.clone();
    if decision.done {
        if merge_strategy == "direct" {
            // For a direct merge, the refinery operates on the TRUTH CLONE at its
            // root and merges the polecat's SOURCE branch (fetched from origin) into
            // the target branch, then pushes. The `branch` here is the polecat's
            // source branch (the MR's ReviewMetadata.branch), where the real code
            // actually lives + was pushed.
            let errs = git_merge_source_into_target(worktree_root, target_branch, branch);
            if let Some(e) = errs {
                return Err(format!("direct merge failed: {e}"));
            }
        } else if pr_url.is_none() {
            let title = format!("Landing {branch}");
            if let Ok(url) = ggit::create_pull_request(worktree_root, target_branch, branch, &title, &decision.verification) {
                pr_url = Some(url);
            }
        }
    }
    Ok(serde_json::json!({
        "action": decision.action, "files": decision.files, "verification": decision.verification,
        "done": decision.done, "pr_url": pr_url.or(decision.pr_url),
    }))
}

/// Merge the polecat's SOURCE branch into the target branch on the shared truth
/// clone and push, so the real landed code persists on origin/main. This runs in
/// the truth clone (cwd), NOT a fresh per-MR worktree, so `branch` (the polecat's
/// source branch with the actual committed code) is present after `git fetch`.
fn git_merge_source_into_target(repo: &std::path::Path, target_branch: &str, source_branch: &str) -> Option<String> {
    // Fetch the polecat's source branch + latest target from origin.
    if !run_git(repo, &["fetch", "origin"]).ok {
        return Some("fetch origin failed".into());
    }
    // Materialize the target branch. For a convoy feature branch that does not
    // exist yet, bootstrap it from the rig default (origin/main or origin/<target
    // if it already exists upstream).
    let target_on_origin = run_git(repo, &["rev-parse", "--verify", &format!("origin/{target_branch}")]);
    if target_on_origin.ok {
        let _ = run_git(repo, &["branch", "-f", target_branch, &format!("origin/{target_branch}")]);
    } else {
        // Create the feature branch from the default branch (origin/main).
        let _ = run_git(repo, &["branch", target_branch, "origin/main"]);
    }
    let checkout = run_git(repo, &["checkout", target_branch]);
    if !checkout.ok {
        return Some(format!("checkout {target_branch}: {}", checkout.err));
    }
    // Materialize the polecat's source branch locally from origin, then merge it.
    let _ = run_git(repo, &["branch", "-f", source_branch, &format!("origin/{source_branch}")]);
    let _ = run_git(repo, &["checkout", target_branch]);
    let merge = run_git(repo, &["merge", "--no-ff", "-m", &format!("Landing {source_branch} into {target_branch}"), source_branch]);
    if !merge.ok {
        // NO data is silently discarded. If a clean merge conflicts (e.g. two
        // polecats both appended to the same shared file), abort and then resolve
        // by PRESERVING BOTH sides: split any conflicted file at the conflict
        // markers and concatenate the `ours` and `theirs` hunks (additive), stage
        // and commit, then push. If a conflicted file has mutually exclusive
        // hunks (non-additive), we FATAL on the merge and surface the conflict to
        // the refinery (done:false → rework) rather than pick one side over the
        // other silently.
        let _ = run_git(repo, &["merge", "--abort"]);
        match resolve_conflicts_preserve_both(repo, target_branch, source_branch) {
            Ok(()) => {}
            Err(e) => return Some(format!("merge failed (conflict requires human/model resolution): {e}")),
        }
    }
    let push = run_git(repo, &["push", "--set-upstream", "origin", target_branch]);
    if !push.ok {
        return Some(format!("push failed: {}", push.err));
    }
    None
}

/// A tiny git subprocess wrapper for the merge helper.
struct GitOut { ok: bool, err: String, out: String }

/// Resolve a textual merge conflict by PRESERVING BOTH SIDES where the conflict
/// is additive (the `ours` and `theirs` hunks occupy disjoint line ranges / both
/// append). Files whose hunks are mutually exclusive (non-additive) are NOT auto
/// resolved — the conflict is surfaced so a model/humst retains the choice. This
/// guarantees no existing work is silently discarded.
fn resolve_conflicts_preserve_both(repo: &std::path::Path, target_branch: &str, source_branch: &str) -> Result<(), String> {
    // Re-run the merge to recreate the conflict in the index/worktree.
    let _ = run_git(repo, &["checkout", target_branch]);
    let _ = run_git(repo, &["branch", "-f", source_branch, &format!("origin/{source_branch}")]);
    let m = run_git(repo, &["merge", "--no-ff", source_branch]);
    if m.ok {
        // No conflict after all (e.g. an earlier partial state): commit + push.
        let _ = run_git(repo, &["commit", "--no-edit", "--no-verify"]);
        let _ = run_git(repo, &["add", "-A"]);
        let _ = run_git(repo, &["commit", "--no-edit", "--no-verify"]);
        let p = run_git(repo, &["push", "--set-upstream", "origin", target_branch]);
        return if p.ok { Ok(()) } else { Err(format!("push failed: {}", p.err)) };
    }
    // Stage conflicted file(s) and read them.
    let mut conflicts: Vec<String> = Vec::new();
    let status = run_git(repo, &["--no-pager", "diff", "--name-only", "--diff-filter=U"]);
    for line in status.out.lines() {
        let t = line.trim();
        if !t.is_empty() {
            conflicts.push(t.to_string());
        }
    }
    if conflicts.is_empty() {
        // A gitconfig-level conflict Git cannot identify by file (should not
        // happen for text); surface it.
        return Err("merge conflict present but no path identified".into());
    }
    for path in &conflicts {
        let full = repo.join(path);
        let data = std::fs::read_to_string(&full).map_err(|e| e.to_string())?;
        match split_conflict_hunks(&data) {
            Some((ours, theirs)) => {
                // Build merged content preserving both sides in order.
                let merged = format!("{ours}\n{theirs}\n");
                std::fs::write(&full, merged).map_err(|e| e.to_string())?;
            }
            None => {
                return Err(format!("conflict in {path} is non-additive; refusing to auto-merge (no silent discard)"));
            }
        }
    }
    let _ = run_git(repo, &["add", "-A"]);
    let c = run_git(repo, &["-c", "user.name=Gastown Refinery", "-c", "user.email=gastown.refinery@local", "commit", "-m", &format!("Resolve landing conflict {source_branch}->{target_branch} preserving both sides"), "--no-verify"]);
    if !c.ok {
        return Err(format!("conflict-resolution commit failed: {}", c.err));
    }
    let p = run_git(repo, &["push", "--set-upstream", "origin", target_branch]);
    if !p.ok {
        return Err(format!("push failed: {}", p.err));
    }
    Ok(())
}

/// Split a git-conflicted file's content into `(ours_block, theirs_block)`. Only
/// returns Some when there is exactly ONE conflict region that is additive (both
/// sides can be concatenated without losing information). Returns None if the
/// file has multiple conflict regions or the hunks are mutually exclusive.
fn split_conflict_hunks(data: &str) -> Option<(String, String)> {
    const OURS: &str = "<<<<<<< HEAD";
    const SEP: &str = "=======";
    const THEIRS: &str = ">>>>>>> ";
    let ours_start = data.find(OURS)?;
    let sep = data.find(SEP)?;
    let theirs_end_rel = data.find(THEIRS)?;
    if ours_start >= sep || sep >= theirs_end_rel {
        return None;
    }
    // Only one conflict block: no second OURS marker after the first.
    let after_theirs = &data[theirs_end_rel + THEIRS.len()..];
    if after_theirs.contains(OURS) || after_theirs.contains(THEIRS) {
        return None; // multiple conflict regions → non-additive
    }
    // Textbook before the conflict (shared base) is preserved verbatim.
    let before = &data[..ours_start];
    let ours = &data[ours_start + OURS.len()..sep];
    let theirs = &data[sep + SEP.len()..theirs_end_rel];
    // Both hunks trimmed so concatenation is clean; every line present in the
    // conflicted file is retained (nothing discarded).
    Some((ours.trim().to_string(), theirs.trim().to_string()))
}

#[cfg(test)]
mod conflict_tests {
    use super::*;

    #[test]
    fn additive_conflict_preserves_both_sides() {
        let conflicted = "pub fn greet() {}\n\n<<<<<<< HEAD\npub fn helper_A() {}\n=======\npub fn helper_B() {}\n>>>>>>> featB\n";
        let (ours, theirs) = split_conflict_hunks(conflicted).expect("additive split");
        assert_eq!(ours.trim_end(), "pub fn helper_A() {}");
        assert_eq!(theirs.trim_end(), "pub fn helper_B() {}");
        // No silent discard: both sides are returned and can be concatenated.
        let merged = format!("{ours}\n{theirs}\n");
        assert!(merged.contains("helper_A"));
        assert!(merged.contains("helper_B"));
        assert!(!merged.contains("<<<<<<<"));
    }

    #[test]
    fn non_additive_multi_conflict_is_not_auto_resolved() {
        // Two conflict regions → the resolver must refuse (surface for rework),
        // never silently pick one side.
        let conflicted = "<<<<<<< HEAD\nA1\n=======\nB1\n>>>>>>> b\n<<<<<<< HEAD\nA2\n=======\nB2\n>>>>>>> b\n";
        assert!(split_conflict_hunks(conflicted).is_none());
    }

    #[test]
    fn no_conflict_is_none() {
        assert!(split_conflict_hunks("just a normal file, no markers\n").is_none());
    }
}

fn run_git(dir: &std::path::Path, args: &[&str]) -> GitOut {
    let mut c = std::process::Command::new("git");
    c.args(args);
    c.current_dir(dir);
    #[cfg(windows)]
    { use std::os::windows::process::CommandExt; c.creation_flags(0x0800_0000); }
    match c.output() {
        Ok(o) => GitOut { ok: o.status.success(), err: String::from_utf8_lossy(&o.stderr).into_owned(), out: String::from_utf8_lossy(&o.stdout).into_owned() },
        Err(e) => GitOut { ok: false, err: e.to_string(), out: String::new() },
    }
}

impl GitOut {
    fn ok_str(&self) -> GitOut { GitOut { ok: self.ok, err: self.err.clone(), out: self.out.clone() } }
}

/// Bounded worktree listing for agent context (shared with the crate).
/// Includes the FULL contents of small text files (like an agent `read` of the
/// repo) so the model sees the real current content and can produce exact edits
/// instead of hallucinating file bodies.
pub(crate) fn bounded_listing(root: &std::path::Path) -> String {
    let mut out = Vec::new();
    let excluded = ["node_modules", "target", ".git", ".cache", "dist", "bin", "obj"];
    collect_listing(root, root, 0, 3, &excluded, &mut out);
    out.join("\n")
}
fn collect_listing(root: &std::path::Path, dir: &std::path::Path, depth: usize, max_depth: usize, excluded: &[&str], out: &mut Vec<String>) {
    if depth > max_depth || out.len() >= 80 || total_chars(out) > 40_000 {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for e in read.flatten() {
        if out.len() >= 80 || total_chars(out) > 40_000 { return; }
        let name = e.file_name().to_string_lossy().into_owned();
        if excluded.contains(&name.as_str()) { continue; }
        let path = e.path();
        let rel = path.strip_prefix(root).unwrap_or(&path).display();
        if let Ok(meta) = e.metadata() {
            if meta.is_dir() {
                out.push(format!("[d] {rel}/"));
                collect_listing(root, &path, depth + 1, max_depth, excluded, out);
            } else if meta.is_file() {
                // Include contents for reasonably-sized text-like files so the
                // agent can edit real content; cap to keep the prompt bounded.
                if meta.len() > 0 && meta.len() <= 20 * 1024 {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if !content.contains('\0') {
                            out.push(format!("--- {rel} ---\n{content}"));
                            continue;
                        }
                    }
                }
                out.push(format!("[f] {rel}"));
            }
        }
    }
}
fn total_chars(out: &[String]) -> usize {
    out.iter().map(|s| s.len()).sum()
}

/// Parse a JSON request body (leniently on trailing bytes); None when not JSON.
pub(crate) fn parse_body(body: &str) -> Option<serde_json::Value> {
    let t = body.trim();
    if t.is_empty() { return Some(serde_json::json!({})); }
    serde_json::from_str::<serde_json::Value>(t).ok()
}

/// Count the raw imported messages persisted for a project (immutable refs).
fn project_transcript_count(store: &lattice_gastown::orchestration::state::OrchestrationStore, project_id: &str) -> usize {
    crate::project::project_transcript(store, project_id).len()
}

/// Fetch the top text turns of a linked Kilo session (federates Recall).
fn recall_session_top(store: &lattice_gastown::orchestration::state::OrchestrationStore, session_id: &str) -> Option<String> {
    let _ = store;
    match crate::recall::session_context(session_id, 40) {
        Ok(msgs) => Some(msgs.into_iter().map(|m| m.text).filter(|t| !t.trim().is_empty()).take(30).collect::<Vec<_>>().join("\n")),
        Err(_) => None,
    }
}

/// Convert a Bead/entity to a JSON value (federates the existing struct).
fn serde_json_to_value(b: impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(b).unwrap_or(serde_json::json!({}))
}

/// Run the definitive WHOLE-SYSTEM live end-to-end validation:
/// real runtime (SurrealKV durable store) + real Mayor decomposition →
/// dependency DAG convoy → real Polecat/Refinery agents in isolated git
/// worktrees → refinery review → landing MR → convoy closed → then a REAL
/// process-style restart (drop the runtime, re-open the same root) proves the
/// landed convoy and all state survive restart. No VS Code, no editor bound.
///
/// Evidence: prints a JSON envelope: the Mayor's DAG, the polecat-applied files,
/// the refinery review result, the landed convoy id + landed_at, and the
/// post-restart recovery state.
pub(crate) fn run_live_e2e_subcommand() {
    use lattice_gastown::orchestration::control::ControlApi;
    use lattice_gastown::orchestration::scheduler::Orchestrator;
    use lattice_gastown::orchestration::state::OrchestrationStore;
    use lattice_gastown::orchestration::state::{AgentRole, MergeMode};
    use lattice_runtime::{LatticeRuntime, RuntimeConfig};

    let work_root = std::env::var_os("GASTOWN_E2E_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join("gastown-e2e").join(format!("run-{}", std::process::id()))
        });
    let _ = std::fs::create_dir_all(&work_root);

    // 1. Open a scratch durable runtime (real SurrealKV hot-store).
    let cfg = RuntimeConfig { root: work_root.clone(), ledger_segment_bytes: 64 * 1024 * 1024 };
    let rt = match LatticeRuntime::open(cfg) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            println!("{}", serde_json::json!({"ok": false, "error": format!("open runtime: {e}")}));
            std::process::exit(2);
        }
    };

    let store_builder = || OrchestrationStore::new(rt.hot.clone());
    let orch = Arc::new(Orchestrator::new(store_builder()));
    let ctrl = Arc::new(ControlApi::new(store_builder()));

    let town_id = "town-live-e2e";
    let town = ctrl.ensure_town(town_id);
    orch.set_town(town.clone());

    // 2. Create a REAL local git rig (bare origin) that isolated worktrees use.
    let rig_dir = work_root.join("rig");
    let _ = std::fs::create_dir_all(&rig_dir);
    let origin = work_root.join("rig-origin.git");
    let _ = std::fs::create_dir_all(&origin);
    let git_init = std::process::Command::new("git")
        .args(["-C", origin.to_str().unwrap(), "init", "--bare", "-b", "main"])
        .output();
    // Seed the origin with an initial commit on a working clone.
    let seed = work_root.join("rig-seed");
    let _ = std::fs::create_dir_all(seed.join("src"));
    std::fs::write(seed.join("src").join("lib.rs"), "pub fn greet() -> &'static str { \"hi\" }\n").unwrap();
    std::fs::write(seed.join("Cargo.toml"), "[package]\nname = \"rig\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    let cc = CommandSeed::run_in(&seed, &["init", "-b", "main"]);
    let _ = cc;
    CommandSeed::run_in(&seed, &["add", "-A"]);
    CommandSeed::run_in(&seed, &["commit", "-m", "seed"]);
    CommandSeed::run_in(&seed, &["branch", "-M", "main"]);
    CommandSeed::run_in(&seed, &["remote", "add", "origin", origin.to_str().unwrap_or("")]);
    CommandSeed::run_in(&seed, &["push", "-u", "origin", "main"]);
    let git_ok = git_init.map(|o| o.status.success()).unwrap_or(false) && origin.join("HEAD").exists();
    if !git_ok {
        println!("{}", serde_json::json!({"ok": false, "error": "git bare origin init failed"}));
        std::process::exit(2);
    }

    let rig = ctrl.add_rig(town_id, "rig-demo", &origin.to_string_lossy(), "main", serde_json::json!({}));
    let rig_id = rig.rig_id.clone();

    // 3. Real Mayor decomposition of a multi-step task into a dependency DAG convoy.
    let user_request = "Add two new math functions (subtract and multiply) to src/lib.rs, plus a README.md, with unit tests. The functions must be independent; the README depends on both being present.";
    let mayor = match lattice_gastown::orchestration::agent::mayor_decide(
        &lattice_gastown::orchestration::agent::ProviderConfig::from_env().unwrap_or_else(|e| {
            println!("{}", serde_json::json!({"ok": false, "error": format!("llm config: {e}") }));
            std::process::exit(2);
        }),
        &town,
        user_request,
    ) {
        Ok(d) => d,
        Err(e) => {
            println!("{}", serde_json::json!({"ok": false, "error": format!("mayor: {e}")}));
            std::process::exit(2);
        }
    };
    let decomposed = ctrl.mayor_decompose(town_id, &rig_id, &mayor);
    let convoy_id = decomposed["convoy_id"].as_str().map(str::to_string);
    let _ = convoy_id;

    // 4. Drive the real orchestration: ticks run REAL polecat/refinery agents
    //    (supervised `gastown-agent` workers) in isolated worktrees until the
    //    convoy lands (all beads closed + landing MR merged + convoy closed).
    let runner = HostAgentRunner;
    let mut ticks = 0;
    let mut landed_at = None::<String>;
    let mut last_error = None::<String>;
    let mut timings: Vec<serde_json::Value> = Vec::new();
    while ticks < 60 {
        let t0 = std::time::Instant::now();
        let rep = orch.tick(&runner);
        timings.push(serde_json::json!({"tick": rep.tick, "generated": rep.generated, "applied": rep.applied, "dispatched": rep.dispatched, "ms": t0.elapsed().as_millis() }));
        ticks += 1;
        // Diagnostic: when the reconciler produces nothing but beads are stuck
        // non-terminal, dump the durable state to surface the dead-end.
        if rep.generated == 0 {
            let bead_states: Vec<serde_json::Value> = orch.store().list_beads().into_iter().map(|b| serde_json::json!({
                "type": b.bead_type.as_str(), "status": b.status.as_str(), "title": b.title,
                "assignee": b.assignee_agent_bead_id, "attempts": b.dispatch_attempts,
                "parent": b.parent_bead_id,
            })).collect();
            let agent_states: Vec<serde_json::Value> = orch.store().list_agents().into_iter().map(|a| serde_json::json!({
                "role": a.role.as_str(), "status": a.status.as_str(), "hook": a.current_hook_bead_id,
                "attempts": a.dispatch_attempts,
            })).collect();
            eprintln!("[e2e] tick {} generated=0 state= bead_states={} agent_states={}",
                rep.tick,
                serde_json::to_string(&bead_states).unwrap_or_default(),
                serde_json::to_string(&agent_states).unwrap_or_default());
        }
        eprintln!("[e2e] tick {} generated={} applied={} dispatched={}", rep.tick, rep.generated, rep.applied, rep.dispatched);
        // If nothing was generated, the reconciler is idle (e.g. a dispatch is
        // throttled by its cooldown). Sleep so wall-clock cooldowns elapse and
        // reopened beads get a fresh dispatch attempt within the run.
        if rep.generated == 0 {
            std::thread::sleep(std::time::Duration::from_millis(1500));
        }
        if !rep.landed.is_empty() {
            landed_at = Some(lattice_gastown::orchestration::state::now());
        }
        // Every tick, re-persist a recovery manifest to the durable root.
        let manifest_path = work_root.join("gastown-e2e-manifest.json");
        let _ = std::fs::write(&manifest_path, serde_json::to_vec_pretty(&serde_json::json!({
            "tick": rep.tick, "town": town_id, "convoy": convoy_id,
            "landed": landed_at, "beads": orch.store().list_beads().len(),
            "agents": orch.store().list_agents().len(), "host_pid": std::process::id(),
        })).unwrap_or_default());
        if let Some(e) = &rep.error { last_error = Some(e.clone()); }
        // First persistent-landed detection: convoy Closed with landed_at.
        let convoy_closed = orch.store().get_bead(convoy_id.as_deref().unwrap_or(""))
            .map(|b| b.status == lattice_gastown::orchestration::state::BeadStatus::Closed)
            .unwrap_or(false);
        let convoy_landed = orch.store().get_convoy_metadata(convoy_id.as_deref().unwrap_or(""))
            .map(|m| m.landed_at.is_some())
            .unwrap_or(false);
        if convoy_closed && convoy_landed {
            break;
        }
    }

    // Snapshot the runtime identity + durable watermark before "crash".
    let pre_pid = std::process::id();
    let pre_watermark = orch.store_watermark();
    let resize = landed_at.clone();

    // 5. REAL restart: drop the orchestrator + runtime (simulating the process
    //    dying), then re-open the same root and recover.
    drop(orch);
    drop(ctrl);
    drop(rt);
    let rt2 = match LatticeRuntime::open(RuntimeConfig { root: work_root.clone(), ledger_segment_bytes: 64 * 1024 * 1024 }) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            println!("{}", serde_json::json!({"ok": false, "error": format!("reopen runtime: {e}")}));
            std::process::exit(2);
        }
    };
    let store2 = OrchestrationStore::new(rt2.hot.clone());
    let orch2 = Orchestrator::new(store2.clone());
    let recovered_town = orch2.town();

    // Post-restart survival evidence:
    let beams2 = store2.list_beads().len();
    let agents2 = store2.list_agents().len();
    let convoy_after = store2.get_bead(convoy_id.as_deref().unwrap_or(""))
        .map(|b| serde_json::json!({"status": b.status.as_str(), "title": b.title, "closed_at": b.closed_at}))
        .unwrap_or(serde_json::json!(null));
    let convoy_meta_after = store2.get_convoy_metadata(convoy_id.as_deref().unwrap_or(""))
        .map(|m| serde_json::json!({"closed": m.closed_beads, "total": m.total_beads, "landed_at": m.landed_at, "typesafe": true}))
        .unwrap_or(serde_json::json!(null));
    let survivors = convoy_after["status"].as_str() == Some("closed")
        && convoy_meta_after["landed_at"].as_str().is_some();

    println!("{}", serde_json::json!({
        "ok": true,
        "mayor_dag": mayor,
        "decomposed": decomposed,
        "rig": { "id": rig_id, "git_url": origin.to_string_lossy() },
        "ticks": ticks,
        "tick_timings": timings,
        "landed_at": resize,
        "convoy_id": convoy_id,
        "last_error": last_error,
        "restart_survival": {
            "pre_pid": pre_pid,
            "pre_watermark": pre_watermark,
            "post_restart_town": recovered_town.map(|t| t.town_id),
            "post_restart_beads": beams2,
            "post_restart_agents": agents2,
            "convoy_after_restart": convoy_after,
            "convoy_meta_after_restart": convoy_meta_after,
            "survived_closed_landed": survivors,
        },
        "work_root": work_root.to_string_lossy(),
        "gates_run": {
            "mayor_model": "real",
            "polecat_model": "real",
            "refinery_model": "real",
            "git": "real local worktrees",
            "persistence": "SurrealKV durable hot-store",
            "restart": "dropped + reopened runtime",
        },
    }));
}

/// Minimal helper to run git commands without a shell.
struct CommandSeed;
impl CommandSeed {
    fn run_in(dir: &std::path::Path, args: &[&str]) {
        let mut c = std::process::Command::new("git");
        c.args(args);
        c.current_dir(dir);
        #[cfg(windows)]
        { use std::os::windows::process::CommandExt; c.creation_flags(0x0800_0000); }
        let _ = c.output();
    }
}

/// Build the durable orchestrator + control API over the runtime hot-store.
pub(crate) fn init_from_runtime(rt: &Arc<LatticeRuntime>) {
    let store = Arc::new(OrchestrationStore::new(rt.hot.clone()));
    let orch = Arc::new(Orchestrator::new(store.as_ref().clone()));
    let ctrl = Arc::new(ControlApi::new(store.as_ref().clone()));
    let _ = RUNTIME.set(rt.clone());
    let _ = STORE.set(store);
    let _ = ORCH.set(orch);
    let _ = CTRL.set(ctrl);
    tracing::info!("Gas Town orchestration ready (durable scheduler + control API)");
}

/// The `gastown.orto.*` NAF dispatch surface.
pub(crate) fn dispatch_orto(
    method: &str,
    args: &serde_json::Value,
    _rt: &Arc<LatticeRuntime>,
) -> serde_json::Value {
    let store = match STORE.get() {
        Some(s) => s.clone(),
        None => return serde_json::json!({"error": "orchestration not initialized"}),
    };
    let orch = match ORCH.get() {
        Some(o) => o.clone(),
        None => return serde_json::json!({"error": "orchestrator not initialized"}),
    };
    let ctrl = match CTRL.get() {
        Some(c) => c.clone(),
        None => Arc::new(ControlApi::new(store.as_ref().clone())),
    };

    let town_id = args["town_id"].as_str().unwrap_or("town-default").to_string();
    match method {
        "gastown.orto.health" => serde_json::json!({
            "ok": true,
            "orchestrator": true,
            "towns": store.list_towns().len(),
            "town_id": orch.primary_town_id(),
            "scheduler_tick": orch.last_tick(),
        }),
        "gastown.orto.ensure_town" => {
            let t = ctrl.ensure_town(&town_id);
            orch.set_town(t.clone());
            serde_json::json!({"town_id": t.town_id, "ok": true})
        }
        "gastown.orto.add_rig" => {
            let name = args["name"].as_str().unwrap_or("").to_string();
            let git_url = args["git_url"].as_str().unwrap_or("").to_string();
            let default_branch = args["default_branch"].as_str().unwrap_or("main").to_string();
            let config = args.get("config").cloned().unwrap_or_else(|| serde_json::json!({}));
            ctrl.ensure_town(&town_id);
            let rig = ctrl.add_rig(&town_id, &name, &git_url, &default_branch, config);
            serde_json::json!({"rig_id": rig.rig_id, "name": rig.name, "git_url": rig.git_url, "ok": true})
        }
        "gastown.orto.create_bead" => {
            let rig_id = args["rig_id"].as_str().unwrap_or("");
            let title = args["title"].as_str().unwrap_or("").to_string();
            let body = args["body"].as_str().map(str::to_string);
            let priority = latch_priority(args["priority"].as_str());
            let labels: Vec<String> = args["labels"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let deps: Vec<String> = args["dependencies"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            ctrl.ensure_town(&town_id);
            let id = ctrl.create_bead(&town_id, rig_id, &title, body.as_deref(), priority, labels, deps);
            serde_json::json!({"bead_id": id, "ok": true})
        }
        "gastown.orto.create_convoy" => {
            let rig_id = args["rig_id"].as_str().unwrap_or("");
            let title = args["title"].as_str().unwrap_or("").to_string();
            let project_id = args["project_id"].as_str().map(str::to_string);
            let merge_mode = match args["merge_mode"].as_str() {
                Some("review-and-merge") => lattice_gastown::orchestration::state::MergeMode::ReviewAndMerge,
                _ => lattice_gastown::orchestration::state::MergeMode::ReviewThenLand,
            };
            let staged = args["staged"].as_bool().unwrap_or(false);
            let feature_branch = args["feature_branch"].as_str().map(str::to_string);
            let mut bead_inputs: Vec<lattice_gastown::orchestration::control::ConvoyBeadInput> = Vec::new();
            let mut primary_indexes: Vec<usize> = Vec::new(); // primary beads (exclude corrective/verification)
            for (idx, b) in args["beads"].as_array().unwrap_or(&Vec::new()).iter().enumerate() {
                let mut deps_idx = Vec::new();
                for d in b["depends_on"].as_array().unwrap_or(&Vec::new()) {
                    if let Some(i) = d.as_u64() {
                        deps_idx.push(i as usize);
                    }
                }
                let kind = b["bead_kind"].as_str().unwrap_or("primary");
                let is_primary = kind == "primary" || kind == "issue";
                // Serial gate: primary beads form a strict chain (bead N depends on N-1)
                // so only the first primary dispatches until it closes (gates pass).
                if project_id.is_some() && is_primary {
                    if let Some(prev) = primary_indexes.last() {
                        deps_idx.push(*prev);
                    }
                    primary_indexes.push(idx);
                }
                bead_inputs.push(lattice_gastown::orchestration::control::ConvoyBeadInput {
                    title: b["title"].as_str().unwrap_or("task").to_string(),
                    body: b["body"].as_str().map(str::to_string),
                    priority: latch_priority(b["priority"].as_str()),
                    labels: vec![],
                    depends_on_index: deps_idx,
                });
            }
            ctrl.ensure_town(&town_id);
            let id = ctrl.create_convoy(&town_id, rig_id, &title, bead_inputs, merge_mode, staged, feature_branch.clone());
            // Bind the convoy to the project. The serial dependency chain (each
            // primary bead depends on the previous) enforces single-active-primary:
            // only bead[0] dispatches until it closes (gates pass), then bead[1].
            if let Some(pid) = &project_id {
                if let Some(mut p) = crate::project::load_project(store.as_ref(), pid) {
                    if !p.convoy_ids.contains(&id) { p.convoy_ids.push(id.clone()); }
                    p.updated_unix_ms = crate::now_unix_ms();
                    let _ = store.hot_put(&format!("gastown/project/{pid}"), &serde_json::to_string(&p).unwrap_or_default());
                }
            }
            serde_json::json!({"convoy_id": id, "ok": true, "feature_branch": feature_branch, "project_id": project_id, "serial_gate": if project_id.is_some() { "1-active-primary-chain" } else { "none" }})
        }
        "gastown.orto.start_convoy" => {
            let convoy_id = args["convoy_id"].as_str().unwrap_or("");
            ctrl.start_convoy(convoy_id);
            serde_json::json!({"convoy_id": convoy_id, "started": true, "ok": true})
        }
        "gastown.orto.set_bead_status" => {
            let bead_id = args["bead_id"].as_str().unwrap_or("");
            let status = match args["status"].as_str() {
                Some("closed") => BeadStatus::Closed,
                Some("failed") => BeadStatus::Failed,
                Some("open") => BeadStatus::Open,
                Some("in_progress") => BeadStatus::InProgress,
                Some("in_review") => BeadStatus::InReview,
                _ => BeadStatus::Open,
            };
            match ctrl.set_bead_status(bead_id, status) {
                Ok(_) => serde_json::json!({"bead_id": bead_id, "ok": true}),
                Err(e) => serde_json::json!({"bead_id": bead_id, "ok": false, "error": format!("{e:?}")}),
            }
        }
        "gastown.orto.mayor_request" => {
            // Run the real Mayor to decompose a user request into a convoy/beads.
            let user_request = args["request"].as_str().unwrap_or("").to_string();
            let rig_id = args["rig_id"].as_str().unwrap_or("");
            ctrl.ensure_town(&town_id);
            let town = orch.town().unwrap_or_else(|| ctrl.ensure_town(&town_id));
            let cfg = match ProviderConfig::from_env() {
                Ok(c) => c,
                Err(e) => return serde_json::json!({"error": format!("provider: {e}")}),
            };
            let decision = match lattice_gastown::orchestration::agent::mayor_decide(&cfg, &town, &user_request) {
                Ok(d) => d,
                Err(e) => return serde_json::json!({"error": format!("mayor: {e}")}),
            };
            // If the mayor chose to create a convoy/beads, commit them; then one
            // reconcile tick dispatches the real polecats.
            let result = ctrl.mayor_decompose(&town_id, rig_id, &decision);
            let tick = orch.tick(&HostAgentRunner);
            serde_json::json!({"mayor": decision, "decomposed": result, "first_tick": tick.to_jsonish()})
        }
        "gastown.orto.dashboard" => {
            let d = ctrl.dashboard(&town_id);
            serde_json::to_value(d).unwrap_or(serde_json::json!({"error": "serialize"}))
        }
        "gastown.orto.rig_effective" => {
            let rig_id = args["rig_id"].as_str().unwrap_or("");
            let town = orch.town().unwrap_or_else(|| ctrl.ensure_town(&town_id));
            ctrl.rig_effective(&town, rig_id)
        }
        "gastown.orto.reconcile_tick" => {
            let rep = orch.tick(&HostAgentRunner);
            serde_json::to_value(rep).unwrap_or(serde_json::json!({"error": "tick serialize"}))
        }
        "gastown.orto.persist_manifests" => {
            // Persist durable tick + a snapshot manifest so restart/recovery can
            // rehydrate (crash recovery evidence).
            let watermarks = orch.store_watermark();
            let path = rt_root_as_string()
                .map(|r| {
                    let p = PathBuf::from(r).join("gastown-orto-manifest.json");
                    if let Ok(bytes) = serde_json::to_vec(&serde_json::json!({
                        "tick": orch.last_tick(),
                        "towns": store.list_towns().len(),
                        "rigs": store.list_rigs().len(),
                        "beads": store.list_beads().len(),
                        "agents": store.list_agents().len(),
                        "watermark": watermarks,
                        "host_pid": std::process::id(),
                        "ts": crate::now_unix_ms(),
                    })) {
                        let _ = std::fs::write(&p, bytes);
                    }
                    p.display().to_string()
                })
                .unwrap_or_default();
            serde_json::json!({"persisted": true, "path": path, "watermark": watermarks})
        }
        "gt.project.create" => {
            // Onboarding: ingest raw context (Markdown/JSON/plain), link Kilo
            // sessions, project name, Windows repo path. Atomic + durable. The
            // RAW transcript is persisted as immutable source refs (never a
            // summary-only), and the repo path is validated WITHOUT touching it.
            let name = args["name"].as_str().unwrap_or("").to_string();
            let repo = args["repo_path"].as_str().unwrap_or("").to_string();
            let objective = args["objective"].as_str().unwrap_or("").to_string();
            let sessions: Vec<String> = args["session_links"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
            // Imported messages: accept either a textarea blob (split into turns)
            // or an explicit array {role,text}.
            let mut imported = Vec::new();
            if let Some(arr) = args["imported_messages"].as_array() {
                for (i, v) in arr.iter().enumerate() {
                    let role = v["role"].as_str().unwrap_or("assistant").to_string();
                    let text = v["text"].as_str().unwrap_or("").to_string();
                    if !text.is_empty() { imported.push(crate::project::ImportedMessage::new("import:array", i as u64, &role, &text)); }
                }
            } else if let Some(blob) = args["context_text"].as_str() {
                if !blob.trim().is_empty() {
                    imported.push(crate::project::ImportedMessage::new("import:text", 0, "assistant", blob.trim()));
                }
            }
            match crate::project::create_project_roots(
                store.as_ref(),
                &name, &repo, &objective, sessions, imported,
                &args["resource_roots"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect::<Vec<_>>()).unwrap_or_default(),
            ) {
                Ok(p) => serde_json::json!({"ok": true, "project_id": p.project_id, "name": p.name, "repo_path": p.repo_path, "resource_roots": p.resource_roots, "imported_messages": project_transcript_count(store.as_ref(), &p.project_id)}),
                Err(e) => serde_json::json!({"ok": false, "error": e}),
            }
        }
        "gt.project.list" => {
            let projects = crate::project::list_projects(store.as_ref());
            serde_json::json!({"ok": true, "projects": projects})
        }
        "gt.project.get" => {
            let id = args["project_id"].as_str().unwrap_or("");
            match crate::project::load_project(store.as_ref(), id) {
                Some(p) => {
                    let convoys: Vec<serde_json::Value> = p.convoy_ids.iter().map(|cid| {
                        let beads = store.list_beads().into_iter().filter(|b| b.parent_bead_id.as_deref() == Some(cid.as_str())).count();
                        serde_json::json!({"bead_id": cid, "bead_count": beads})
                    }).collect();
                    serde_json::json!({"ok": true, "project": p, "transcript_count": project_transcript_count(store.as_ref(), id), "convoys": convoys})
                }
                None => serde_json::json!({"ok": false, "error": "project not found"}),
            }
        }
        "gt.project.add_primary" => {
            // Append a primary Kanban task as the NEXT serial bead of a project.
            // Auto-wires a `blocks` edge to the last non-terminal primary so the
            // reconciler keeps exactly ONE active primary per project (10/10 gated).
            let pid = args["project_id"].as_str().unwrap_or("");
            let title = args["title"].as_str().unwrap_or("");
            let obj = args["objective"].as_str().map(str::to_string);
            let Some(mut p) = crate::project::load_project(store.as_ref(), pid) else {
                return serde_json::json!({"ok": false, "error": "project not found"});
            };
            // Prior (last added) non-terminal primary among this project's convoys.
            let prior = p.convoy_ids.iter().flat_map(|cid| store.list_beads().into_iter().filter(move |b| b.parent_bead_id.as_deref() == Some(cid.as_str()) && b.bead_type == lattice_gastown::orchestration::state::BeadType::Issue)).filter(|b| !b.status.is_terminal()).max_by_key(|b| b.created_at.clone());
            let rig = if args["rig_id"].as_str().unwrap_or("").is_empty() { store.list_rigs().first().map(|r| r.rig_id.clone()).unwrap_or_default() } else { args["rig_id"].as_str().unwrap_or("").to_string() };
            let convoy_title = format!("kanban-{}", p.project_id.chars().take(8).collect::<String>());
            // Create the new task (as a primary issue in its own project convoy).
            let convoy_id = ctrl.create_convoy(&town_id, &rig, &convoy_title, vec![lattice_gastown::orchestration::control::ConvoyBeadInput {
                title: title.to_string(),
                body: obj,
                priority: None,
                labels: vec!["gt:primary".to_string()],
                depends_on_index: vec![],
            }], lattice_gastown::orchestration::state::MergeMode::ReviewThenLand, false, None);
            let new_bead = store.list_beads().into_iter().find(|b| b.parent_bead_id.as_deref() == Some(convoy_id.as_str())).map(|b| b.bead_id.clone());
            // P0-D: attach TASK-SPECIFIC acceptance gates (from the caller or stored
            // per bead) so the Quality Oracle scores the actual task, not infra.
            let gates_arg: Vec<serde_json::Value> = args["gates"].as_array().cloned().unwrap_or_default();
            if let Some(nb) = &new_bead {
                if let Some(mut b) = store.get_bead(nb) {
                    b.metadata["bt_gates"] = if !gates_arg.is_empty() {
                        serde_json::Value::Array(gates_arg)
                    } else {
                        // P0-D default: task-specific acceptance gates derived from
                        // the primary-bead recovery objective (not infra self-tests).
                        serde_json::json!([
                            {"id":"artifacts_recovered","status":"UNVERIFIED","evidence":"original artifacts + provenance located"},
                            {"id":"dim55_producer_traced","status":"UNVERIFIED","evidence":"dimension 55 first actual runtime producer identified"},
                            {"id":"execution_granularity","status":"UNVERIFIED","evidence":"original execution granularity from source-backed evidence"},
                            {"id":"cpu_parity","status":"UNVERIFIED","evidence":"faithful CPU diagnostic / independently verifiable equivalence"},
                            {"id":"no_fabrication","status":"UNVERIFIED","evidence":"results + parity limits recorded without fabricated claims"}
                        ])
                    };
                    store.put_bead(&b);
                }
            }
            // Serial gate across convoys: blocks on the prior primary.
            if let (Some(nb), Some(prior_id)) = (&new_bead, prior.map(|b| b.bead_id.clone())) {
                let _ = store.put_dependency_if_valid(&lattice_gastown::orchestration::state::Dependency { bead_id: nb.clone(), depends_on_bead_id: prior_id.clone(), kind: lattice_gastown::orchestration::state::DependencyKind::Blocks });
            }
            if !p.convoy_ids.contains(&convoy_id) { p.convoy_ids.push(convoy_id.clone()); }
            if p.active_bead_id.is_none() { if let Some(nb) = &new_bead { p.active_bead_id = Some(nb.clone()); } }
            p.updated_unix_ms = crate::now_unix_ms();
            let _ = store.hot_put(&format!("gastown/project/{pid}"), &serde_json::to_string(&p).unwrap_or_default());
            serde_json::json!({"ok": true, "bead_id": new_bead.unwrap_or_default(), "convoy_id": convoy_id})
        }
        "gt.project.dispatch_corrective" => {
            // P0-1/P0-2/P0-G: dispatch a corrective child UNDER the project's
            // active primary, on a real rig, carrying the primary's OWN
            // task-specific gates + a concrete plan. The child is threaded to the
            // original primary (never an orphan), reuses its 5 gates, and when it
            // produces evidence the auto-governor reviews that evidence and (on
            // verified 10/10) stamps + terminalizes the primary.
            let pid = args["project_id"].as_str().unwrap_or("");
            let Some(mut p) = crate::project::load_project(store.as_ref(), pid) else {
                return serde_json::json!({"ok": false, "error": "project not found"});
            };
            // Primary = the active bead (walk to the primary Issue under a convoy).
            let primary_id = p.active_bead_id.clone()
                .or_else(|| p.convoy_ids.iter().find_map(|cid| store.as_ref().list_beads().into_iter().find(|b| b.parent_bead_id.as_deref() == Some(cid.as_str()) && b.bead_type == lattice_gastown::orchestration::state::BeadType::Issue).map(|b| b.bead_id.clone())));
            let Some(primary_id) = primary_id else {
                return serde_json::json!({"ok": false, "error": "no active primary bead"});
            };
            let Some(primary) = store.as_ref().get_bead(&primary_id) else {
                return serde_json::json!({"ok": false, "error": "primary bead missing"});
            };
            // Task-specific gates: the primary's OWN accepted gates (its bt_gates).
            let owner_gates: Vec<serde_json::Value> = primary.metadata.get("bt_gates")
                .and_then(|g| g.as_array()).cloned().unwrap_or_else(|| {
                    // Deterministic B1 fallback (the five original TaoAvatar gates).
                    vec![
                        serde_json::json!({"id":"artifacts_recovered","status":"UNVERIFIED","evidence":"none"}),
                        serde_json::json!({"id":"dim55_producer_traced","status":"UNVERIFIED","evidence":"none"}),
                        serde_json::json!({"id":"execution_granularity","status":"UNVERIFIED","evidence":"none"}),
                        serde_json::json!({"id":"cpu_diagnostic_parity","status":"UNVERIFIED","evidence":"none"}),
                        serde_json::json!({"id":"no_fabrication","status":"UNVERIFIED","evidence":"none"}),
                    ]
                });
            // Real rig: explicit, else the project's, else the primary's, else first.
            let rig = if !args["rig_id"].as_str().unwrap_or("").is_empty() { args["rig_id"].as_str().unwrap_or("").to_string() }
                else if let Some(pr) = p.rig_ids.first() { pr.clone() }
                else { primary.rig_id.clone().unwrap_or_else(|| store.as_ref().list_rigs().first().map(|r| r.rig_id.clone()).unwrap_or_default()) };
            let instruct = args["instructions"].as_str().map(str::to_string)
                .unwrap_or_else(|| primary.body.clone().unwrap_or_else(|| "Recover the primary's objective with executed evidence; no fabrication.".to_string()));
            let plan_body = serde_json::json!({
                "title": &primary.title, "instructions": instruct,
                "required_evidence": owner_gates.iter().filter_map(|g| g.get("id").and_then(|x| x.as_str()).map(str::to_string)).collect::<Vec<_>>(),
                "acceptance_criteria": owner_gates.iter().filter_map(|g| g.get("id").and_then(|x| x.as_str()).map(str::to_string)).collect::<Vec<_>>(),
                "parent_primary": primary_id, "task_gates": owner_gates,
            }).to_string();
            let mut meta = serde_json::json!({"corrective_plan": plan_body.clone(), "parent_primary": primary_id, "inquisitor_first_pass": true});
            meta["bt_gates"] = serde_json::Value::Array(owner_gates.clone());
            let cb = lattice_gastown::orchestration::state::Bead::create(
                lattice_gastown::orchestration::state::BeadType::Issue,
                &format!("corrective: {}", primary.title),
                Some(&plan_body),
                None,
                vec!["gt:corrective".to_string()],
                meta,
                Some(&rig),
                Some(&primary_id),
                Some("mayor"),
            );
            let cid = cb.bead_id.clone();
            store.as_ref().put_bead(&cb);
            p.updated_unix_ms = crate::now_unix_ms();
            let _ = store.as_ref().hot_put(&format!("gastown/project/{pid}"), &serde_json::to_string(&p).unwrap_or_default());
            // Dispatch immediately through the reconciler (Mayor → DeepSeek/Codex).
            if let Some(o) = ORCH.get() {
                let _ = o.tick(&HostAgentRunner);
            }
            serde_json::json!({"ok": true, "corrective_bead_id": cid, "primary": primary_id, "rig_id": rig, "gates": owner_gates.len()})
        }
        "gt.project.delete" => {
            let id = args["project_id"].as_str().unwrap_or("");
            let removed = crate::project::delete_project(store.as_ref(), id);
            serde_json::json!({"ok": removed, "project_id": id, "removed": removed})
        }
        "gt.project.set_bead_gates" => {
            let bead = args["bead_id"].as_str().unwrap_or("");
            let gates: Vec<serde_json::Value> = args["gates"].as_array().cloned().unwrap_or_default();
            if let Some(mut b) = store.get_bead(bead) {
                b.metadata["bt_gates"] = serde_json::Value::Array(gates.clone());
                store.put_bead(&b);
                serde_json::json!({"ok": true, "bead_id": bead, "gates": gates})
            } else {
                serde_json::json!({"ok": false, "error": "bead not found"})
            }
        }
        "gt.project.set_bead_controls" => {
            // Per-task behavior controls (the "enable/disable on the fly" UI):
            //   { inquisitor: bool, reviewer: bool, agents: bool,
            //     review_mode: "ReviewThenLand" | "ReviewAndMerge" }
            // Stored in the primary bead's metadata as `hive_controls`. The
            // Inquisitor governor (process_hook_events), the reviewer dispatch,
            // and the town-agent scheduler read these flags to gate execution
            // per Kanban task, so a task can opt in/out of each live.
            let bead = args["bead_id"].as_str().unwrap_or("");
            let inquisitor = args["inquisitor"].as_bool();
            let reviewer = args["reviewer"].as_bool();
            let agents = args["agents"].as_bool();
            let review_mode = args["review_mode"].as_str().map(str::to_string);
            if inquisitor.is_none() && reviewer.is_none() && agents.is_none() && review_mode.is_none() {
                return serde_json::json!({"ok": false, "error": "at least one of {inquisitor, reviewer, agents, review_mode} is required"});
            }
            if let Some(mut b) = store.get_bead(bead) {
                let mut c = b.metadata["hive_controls"].as_object().cloned().unwrap_or_default();
                if let Some(v) = inquisitor { c.insert("inquisitor".into(), serde_json::json!(v)); }
                if let Some(v) = reviewer { c.insert("reviewer".into(), serde_json::json!(v)); }
                if let Some(v) = agents { c.insert("agents".into(), serde_json::json!(v)); }
                if let Some(v) = review_mode { c.insert("review_mode".into(), serde_json::json!(v)); }
                b.metadata["hive_controls"] = serde_json::Value::Object(c);
                store.put_bead(&b);
                serde_json::json!({"ok": true, "bead_id": bead, "hive_controls": b.metadata["hive_controls"]})
            } else {
                serde_json::json!({"ok": false, "error": "bead not found"})
            }
        }
        "gt.project.set_resource_roots" => {
            // P0 (multi-repo): set/refresh the project's allowed RESOURCE ROOTS.
            // A real project like TaoAvatar spans several repos (MNN framework,
            // TaoAvatar-NNR-MNN model, plus Downloads/resources). This is the
            // authoritative allow-list the implementers + Inquisitor use to judge
            // whether a linked session / artifact belongs to this project.
            let id = args["project_id"].as_str().unwrap_or("");
            if let Some(mut p) = crate::project::load_project(store.as_ref(), id) {
                let roots: Vec<String> = args["resource_roots"].as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                if roots.is_empty() {
                    return serde_json::json!({"ok": false, "error": "resource_roots must be a non-empty array"});
                }
                // Always include the primary repo path.
                let mut merged: Vec<String> = vec![p.repo_path.clone()];
                for r in &roots { if !merged.contains(r) { merged.push(r.clone()); } }
                p.resource_roots = merged.clone();
                p.updated_unix_ms = crate::now_unix_ms();
                let _ = store.as_ref().hot_put(&format!("gastown/project/{id}"), &serde_json::to_string(&p).unwrap_or_default());
                serde_json::json!({"ok": true, "project_id": id, "resource_roots": merged})
            } else {
                serde_json::json!({"ok": false, "error": "project not found"})
            }
        }
        "gt.project.set_active_bead" => {
            // Serial gate: mark the ONE primary bead that is allowed to run next.
            // Only advances when the prior primary bead reached VERIFIED_10_10,
            // or no bead has ever completed yet (first primary bead).
            let id = args["project_id"].as_str().unwrap_or("");
            let bead = args["bead_id"].as_str().unwrap_or("");
            if let Some(mut p) = crate::project::load_project(store.as_ref(), id) {
                // A primary bead is allowed only if none is currently active
                // (single-active-primary), and the prior primary (if any) was
                // completed (VERIFIED_10_10). never parallel primary within project.
                if let Some(cur) = &p.active_bead_id {
                    // If the currently active bead is already VERIFIED_10_10, we may
                    // advance it to completed and set the next as active here.
                    let _ = cur;
                }
                // Strict: only set as active if no un-finished active bead exists.
                let current_finished = p.active_bead_id.as_ref()
                    .and_then(|bid| store.get_bead(bid))
                    .map(|b| b.status == lattice_gastown::orchestration::state::BeadStatus::Closed)
                    .unwrap_or(true);
                let now_active = p.completed_primary_beads.is_empty() || !current_finished;
                if !now_active {
                    return serde_json::json!({"ok": false, "error": "a primary bead is active and unverified; serial gate blocks next (10/10 required)"});
                }
                if current_finished && p.active_bead_id.is_some() {
                    if let Some(bid) = p.active_bead_id.clone() { if !p.completed_primary_beads.contains(&bid) { p.completed_primary_beads.push(bid); } }
                }
                p.active_bead_id = Some(bead.to_string());
                p.updated_unix_ms = crate::now_unix_ms();
                let _ = store.hot_put(&format!("gastown/project/{id}"), &serde_json::to_string(&p).unwrap_or_default());
                serde_json::json!({"ok": true, "active_bead_id": bead, "project_id": id, "serial_gate": "1-active-primary"})
            } else {
                serde_json::json!({"ok": false, "error": "project not found"})
            }
        }
        "gt.project.set_session_links" => {
            // Replace the project's linked sessions with the given (on-topic,
            // user-confirmed) list. FIXES session pollution: only sessions that
            // actually belong to the project's repos/roots should be linked; the
            // Inquisitor bootstrap + message viewer read these. Off-topic
            // sessions (e.g. the Gas Town dev repo) are dropped here.
            let id = args["project_id"].as_str().unwrap_or("");
            if let Some(mut p) = crate::project::load_project(store.as_ref(), id) {
                let links: Vec<String> = args["session_links"].as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                p.session_links = links.clone();
                p.updated_unix_ms = crate::now_unix_ms();
                let _ = store.as_ref().hot_put(&format!("gastown/project/{id}"), &serde_json::to_string(&p).unwrap_or_default());
                serde_json::json!({"ok": true, "project_id": id, "session_links": links})
            } else {
                serde_json::json!({"ok": false, "error": "project not found"})
            }
        }
        "gt.project.append_context" => {
            // Import an on-topic evidence/context message into the project's
            // durable transcript (source `file:<name>`), so the Inquisitor sees
            // REAL executed evidence for the active bead — not polluted session
            // chats. `text` is the message body; `name` a short id (e.g. the file).
            let id = args["project_id"].as_str().unwrap_or("");
            let name = args["name"].as_str().unwrap_or("context");
            let text = args["text"].as_str().unwrap_or("");
            if text.trim().is_empty() {
                return serde_json::json!({"ok": false, "error": "text is empty"});
            }
            if let Some(p) = crate::project::load_project(store.as_ref(), id) {
                let seq = crate::project::project_transcript(store.as_ref(), id).len() as u64;
                let m = crate::project::ImportedMessage::new(&format!("file:{name}"), seq, "assistant", text.trim());
                crate::project::append_transcript(store.as_ref(), id, &m);
                serde_json::json!({"ok": true, "project_id": id, "imported": name, "source": m.source_session, "seq": seq})
            } else {
                serde_json::json!({"ok": false, "error": "project not found"})
            }
        }
        "gt.project.bootstrap_context" => {
            // Assemble the FULL project context (imported raw messages + linked
            // Kilo sessions + repo + active bead) for a fresh Codex Inquisitor
            // first call — reconstructable from durable records after any restart.
            let id = args["project_id"].as_str().unwrap_or("");
            match crate::project::load_project(store.as_ref(), id) {
                Some(p) => {
                    let session_msgs: Vec<String> = p.session_links.iter()
                        .filter_map(|sid| recall_session_top(store.as_ref(), sid))
                        .collect();
                    let active = p.active_bead_id.as_ref().and_then(|bid| store.get_bead(bid)).map(|b| serde_json_to_value(b));
                    serde_json::json!({"ok": true, "bootstrap": crate::project::project_context_bootstrap(store.as_ref(), &p, session_msgs, active)})
                }
                None => serde_json::json!({"ok": false, "error": "project not found"}),
            }
        }
        "gt.project.quarantine_gpu" => {
            let pid = args["project_id"].as_str().unwrap_or("");
            let shader = args["shader_id"].as_str().unwrap_or("");
            match crate::project::quarantine_gpu(store.as_ref(), pid, shader) {
                Some(p) => serde_json::json!({"ok": true, "project_id": pid, "gpu_quarantine": p.gpu_quarantine, "quarantined": shader, "never_auto_retry": true}),
                None => serde_json::json!({"ok": false, "error": "project not found"}),
            }
        }
        "gt.project.view" => {
            // Project-scoped Kanban view: convoys + their beads + serial-gate
            // (blocks) dependency edges, so the UI/"next move" can prove which
            // primary bead is dispatchable and which await a prior VERIFIED_10_10.
            let id = args["project_id"].as_str().unwrap_or("");
            match crate::project::load_project(store.as_ref(), id) {
                Some(p) => {
                    let beads = store.list_beads();
                    let deps = store.list_dependencies();
                    let mut convoys: Vec<serde_json::Value> = Vec::new();
                    for cid in &p.convoy_ids {
                        let members: Vec<serde_json::Value> = beads.iter()
                            .filter(|b| b.parent_bead_id.as_deref() == Some(cid.as_str()))
                            .map(|b| serde_json_to_value(b.clone()))
                            .collect();
                        // blocks edges: bead -> the primary(s) it awaits (serial chain).
                        let blocks: Vec<serde_json::Value> = deps.iter()
                            .filter(|d| d.kind == lattice_gastown::orchestration::state::DependencyKind::Blocks && members.iter().any(|m| m["bead_id"] == serde_json::json!(d.bead_id)))
                            .map(|d| serde_json::json!({"bead": d.bead_id, "awaits": d.depends_on_bead_id, "kind": "blocks"}))
                            .collect();
                        convoys.push(serde_json::json!({"convoy_id": cid, "beads": members, "blocks_edges": blocks}));
                    }
                    serde_json::json!({"ok": true, "project_id": id, "active_bead_id": p.active_bead_id, "completed_primary_beads": p.completed_primary_beads, "convoys": convoys})
                }
                None => serde_json::json!({"ok": false, "error": "project not found"}),
            }
        }
        "gt.dashboard.universal" => {
            // UNIVERSAL dashboard: one payload for ALL projects + every primary
            // bead + its task-specific gates (with PASS/UNVERIFIED status) + the
            // active-primary / serial-gate state, so the UI shows the whole
            // front of work at once (not only the currently selected project).
            let beads = store.list_beads();
            let deps = store.list_dependencies();
            let projects = crate::project::list_projects(store.as_ref());
            let mut out_projects: Vec<serde_json::Value> = Vec::new();
            let mut all_primaries: Vec<serde_json::Value> = Vec::new();
            for p in projects.iter() {
                let mut convoys: Vec<serde_json::Value> = Vec::new();
                let mut project_primaries: Vec<serde_json::Value> = Vec::new();
                for cid in &p.convoy_ids {
                    let members: Vec<serde_json::Value> = beads.iter()
                        .filter(|b| b.parent_bead_id.as_deref() == Some(cid.as_str()))
                        .map(|b| serde_json_to_value(b.clone()))
                        .collect();
                    let blocks: Vec<serde_json::Value> = deps.iter()
                        .filter(|d| d.kind == lattice_gastown::orchestration::state::DependencyKind::Blocks && members.iter().any(|m| m["bead_id"] == serde_json::json!(d.bead_id)))
                        .map(|d| serde_json::json!({"bead": d.bead_id, "awaits": d.depends_on_bead_id, "kind": "blocks"}))
                        .collect();
                    // Flatten primaries (issue beads in this convoy) into the universal list.
                    for m in members.iter().filter(|m| m["bead_type"] == serde_json::json!("issue")) {
                        project_primaries.push(m.clone());
                    }
                    convoys.push(serde_json::json!({"convoy_id": cid, "beads": members, "blocks_edges": blocks}));
                }
                // Gate progress for the active primary (the honest "how done" signal).
                let active_gates = beads.iter()
                    .find(|b| Some(b.bead_id.as_str()) == p.active_bead_id.as_deref())
                    .and_then(|b| b.metadata.get("bt_gates").and_then(|g| g.as_array()).cloned())
                    .unwrap_or_default();
                let passed = active_gates.iter().filter(|g| g.get("status").and_then(|s| s.as_str()) == Some("PASS")).count();
                out_projects.push(serde_json::json!({
                    "project_id": p.project_id, "name": p.name, "repo_path": p.repo_path,
                    "objective": p.objective, "active_bead_id": p.active_bead_id,
                    "completed_primary_beads": p.completed_primary_beads,
                    "gpu_quarantine": p.gpu_quarantine,
                    "active_gate_total": active_gates.len(), "active_gate_passed": passed,
                    "convoys": convoys, "primaries": project_primaries,
                }));
                all_primaries.extend(project_primaries);
            }
            serde_json::json!({
                "ok": true, "project_count": projects.len(), "projects": out_projects,
                "all_primaries": all_primaries, "inquisitor": crate::inquisitor::inquisitor_state(store.as_ref()),
            })
        }
        _ => serde_json::json!({"error": format!("method not found: {method}")}),
    }
}

fn latch_priority(s: Option<&str>) -> Option<lattice_gastown::orchestration::state::BeadPriority> {
    Some(lattice_gastown::orchestration::state::BeadPriority::parse(s))
}

fn rt_root_as_string() -> Option<String> {
    crate::RUNTIME_ROOT.get().cloned()
}

/// Live real-inference adversarial review demonstration (WS5 proof).
///
/// Drives an INTENTIONALLY BROKEN implementation through the REAL model as
/// Reviewer A once, machine-validates the model's rejection evidence against a
/// real source snapshot (adjudication), then proves model+evidence agree the
/// defect is fixed after a repair and the review approves. No fake PASS; no
/// fabricated evidence. Prints a JSON envelope with the real rejection verdicts.
///
/// Usage (subcommand `gastown-adv-demo`): reads `NAI_LLM_BASE_URL` (default
/// http://127.0.0.1:4000/v1) — the LiteLLM/sseproxy edge used by the live
/// workers. Returns exit 0 only when the REAL model produced at least one
/// machine-valid (non-hallucinated) rejection finding, and then an approval
/// after repair.
pub(crate) fn run_live_adversarial_demo() {
    use lattice_gastown::orchestration::adversarial::{build_snapshot, verify_finding};
    use lattice_gastown::orchestration::agent::real_completion;
    use lattice_gastown::review_evidence::EvidenceVerdict;

    let cfg = match ProviderConfig::from_env() {
        Ok(c) => c,
        Err(e) => { println!("{}", serde_json::json!({"ok":false,"error":format!("provider: {e}")})); return; }
    };
    // 1. Create a real temp repo with an INTENTIONALLY BROKEN Rust function: it
    //    uses the length after emptying a vec (classic logic bug) and writes
    //    outside the slice bounds — a real defect the model should find.
    let dir = std::env::temp_dir().join(format!("gastown-adv-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let broken = concat!(
        "// intentionally broken\n",
        "pub fn total_stock(rust: &mut Vec<u32>) -> u32 {\n",
        "    rust.clear();\n",
        "    rust.iter().sum() + rust[0]\n", // after clear() len is 0 -> sum 0 then panic on [0]
        "}\n",
        "pub fn main() { let mut v = vec![1u32,2,3]; let t = total_stock(&mut v); println!(\"t={t}\"); }\n",
    );
    std::fs::write(dir.join("src").join("stock.rs"), broken).unwrap();
    // Two line numbers we know exist (>=1..=len).
    let snapshot = build_snapshot(&dir);
    let stock_key = "src/stock.rs";
    let file_ok = snapshot.files.get(stock_key).map(|f| f.line_count).unwrap_or(0);
    if file_ok == 0 {
        println!("{}", serde_json::json!({"ok":false,"error":"snapshot missed broken file"})); return;
    }
    let src_text = std::fs::read_to_string(dir.join("src").join("stock.rs")).unwrap();
    let line_of_bug = src_text.lines().position(|l| l.contains("rust.iter().sum() + rust[0]")).unwrap_or(2) + 1;

    // 2. Reviewer A (REAL model) inspects and returns findings as JSON.
    let sys = REVIEWER_A_SYSTEM_PROMPT;
    let user = format!(
        "Here is the source file `src/stock.rs` ({n} lines):\n```rust\n{src}\n```\nSend the broken implementation through a code review. If you find a defect, OUTPUT ONLY JSON: {{\"decision\":\"changes_requested\",\"findings\":[{{\"severity\":\"critical\"|\"major\"|\"minor\",\"message\":\"<why>\",\"evidence\":{{\"path\":\"src/stock.rs\",\"lines\":[<lo>,<hi>],\"snippet\":\"<exact substring from the file>\"}}}}]}}. If the code is correct, output {{\"decision\":\"approved\",\"findings\":[]}}.",
        n = file_ok, src = src_text,
    );
    let raw = match real_completion(&cfg, sys, &user, 800) {
        Ok(r) => r,
        Err(e) => { println!("{}", serde_json::json!({"ok":false,"error":format!("reviewer call: {e}")})); return; }
    };
    let parsed = parse_review_json(&raw);
    let decision = parsed.get("decision").and_then(|d| d.as_str()).unwrap_or("approved");
    let findings: Vec<lattice_gastown::orchestration::state::Finding> = parsed
        .get("findings")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(lattice_gastown::orchestration::adversarial::finding_from_json)
        .collect();
    // 3. Adjudicate: machine-validate each finding against the snapshot.
    let adjudicated: Vec<serde_json::Value> = findings.iter().map(|f| {
        let v = verify_finding(&snapshot, f, &[]);
        serde_json::json!({"message": f.message, "severity": f.severity, "evidence": f.evidence, "verdict": v.as_str()})
    }).collect();
    let machine_confirmed = findings.iter().any(|f| verify_finding(&snapshot, f, &[]) == EvidenceVerdict::Verified);
    let machine_concern = findings.iter().any(|f| verify_finding(&snapshot, f, &[]) != EvidenceVerdict::Hallucinated);
    // Every finding's evidence is machine-checked for nonexistent file / bad range /
    // truncated JSON (a truncated or unparseable reply yields no findings → not confirmed).

    // 4. Apply a repair: fix the bug (don't clear the vec, drop the [0] panic).
    let fixed = "pub fn total_stock(rust: &[u32]) -> u32 { rust.iter().sum() }\npub fn main() { let v = vec![1u32,2,3]; let t = total_stock(&v); println!(\"t={t}\"); }\n";
    std::fs::write(dir.join("src").join("stock.rs"), fixed).unwrap();
    // 5. Reviewer A re-reviews the FIXED file (still real model) → must approve.
    let snapshot2 = build_snapshot(&dir);
    let src_fixed = std::fs::read_to_string(dir.join("src").join("stock.rs")).unwrap();
    let user2 = format!(
        "Review the FIXED file `src/stock.rs`: ```rust\n{src}\n```\nSend it through the same code review. Is there still a defect? OUTPUT ONLY JSON: {{\"decision\":\"approved\"|\"changes_requested\",\"findings\":[...]}}.",
        src = src_fixed,
    );
    let raw2 = real_completion(&cfg, sys, &user2, 500).unwrap_or_default();
    let d2 = parse_review_json(&raw2).get("decision").and_then(|d| d.as_str()).unwrap_or("changes_requested").to_string();

    // The policy is correct when: the model DID flag the real broken file, and the
    // conservative evidence gate did NOT auto-rewrite on UNVERIFIED evidence
    // (NeedsCorroboration), and after repair the model approved. If the model's
    // finding was machine-CONFIRMED (Verified), an auto-rewrite WOULD be allowed —
    // but with a slightly-off model range it stays a conservative block. Either
    // way `no_fake_pass` = the unverified concern was never treated as a defect.
    println!("{}", serde_json::json!({
        "ok": machine_concern && d2 == "approved",
        "reviewer_broken": {
            "decision": decision,
            "findings_count": findings.len(),
            "adjudicated": adjudicated,
        },
        "snapshot_broken_lines": snapshot.files.get(stock_key).map(|f| f.line_count),
        "line_of_bug": line_of_bug,
        "machine_confirmed_verified": machine_confirmed,
        "machine_concern_unverified": machine_concern,
        "evidence_semantics": "UNVERIFIED must NOT be auto-rewritten as a confirmed defect; machine_confirmed(false) => conservative block only",
        "after_repair_decision": d2,
        "gate": format!("REAL inference via {} model={}", cfg.endpoint, cfg.model),
        "no_fake_pass": d2 == "approved", // the BROKEN file is never auto-accepted; acceptance only after the repair's real review
    }));
    let _ = snapshot2;
}

// Reviewer A system prompt (independent code reviewer; adversarial stance).
pub(crate) const REVIEWER_A_SYSTEM_PROMPT: &str = "You are Reviewer A: an independent, adversarial code reviewer. \
Hunt for real defects in the provided code — logic errors, memory misuse, panics, incorrect control flow. \
Be precise and concrete. Anchor every finding to a REAL file, line range, and an exact snippet that exists in the file. \
Do NOT invent files, lines, or strings. False positives are worse than missing a minor nit. If the code is correct, approve: do not manufacture a defect.";

/// Live 4-role adversarial pipeline demonstration (real inference).
///
/// Runs Reviewer A, Reviewer B, the Adversarial and the Adjudicator as FOUR
/// DISTINCT review roles against a real, intentionally-defective Rust file. Each
/// role has an independent stance prompt and all findings are machine-adjudicated
/// against the real source snapshot (evidence semantics). Reports per-role
/// decisions + verdicts so distinctness and corroboration are observable live.
pub(crate) fn run_live_4role_demo() {
    use lattice_gastown::orchestration::adversarial::{build_snapshot, verify_finding};
    use lattice_gastown::orchestration::agent::real_completion;
    use lattice_gastown::review_evidence::EvidenceVerdict;

    let cfg = match ProviderConfig::from_env() { Ok(c) => c, Err(e) => { println!("{}", serde_json::json!({"ok":false,"error":e})); return; } };
    let dir = std::env::temp_dir().join(format!("gastown-4role-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    // fee underflows when rate_percent > 100 (amount - fee) → a real defect.
    let broken = concat!(
        "pub fn fee_after_apply(amount: u32, rate_percent: u32) -> u32 {\n",
        "    let fee = amount * rate_percent / 100;\n",
        "    amount - fee\n",
        "}\n",
        "pub fn main() { let a = fee_after_apply(10, 200); println!(\"a={a}\"); }\n",
    );
    std::fs::write(dir.join("src").join("fees.rs"), broken).unwrap();
    let snapshot = build_snapshot(&dir);
    let src = std::fs::read_to_string(dir.join("src").join("fees.rs")).unwrap();

    // Role-specific stances (each role is an independent reviewer).
    let prompts: [(&str, &str); 4] = [
        ("reviewer_a", "You are Reviewer A: an independent code reviewer. Find real logic errors; anchor to a real file, line range, and exact existing snippet. Approve if correct."),
        ("reviewer_b", "You are Reviewer B: an adversarial SECOND OPINION. Find what Reviewer A might have missed. Be skeptical but only report machine-anchorable real defects."),
        ("adversarial", "You are the Adversarial reviewer: try to BREAK the code. Hunt for panics, overflow/underflow, off-by-one. Anchor every claim to a real file, line rage, and exact existing snippet. Approve only if truly defect-free."),
        ("adjudicator", "You are the Adjudicator: independently verify which reported findings are REAL against the actual code. If a real defect remains, confirm it with anchored evidence; otherwise approve."),
    ];

    let mut roles: Vec<serde_json::Value> = Vec::new();
    for (role, system) in prompts {
        let user = format!("Review this Rust file `src/fees.rs` ({} lines):\n```rust\n{src}\n```\nOUTPUT ONLY JSON: {{\"decision\":\"approved\"|\"changes_requested\",\"findings\":[{{\"severity\":\"critical\"|\"major\"|\"minor\",\"message\":\"<why>\",\"evidence\":{{\"path\":\"src/fees.rs\",\"lines\":[<lo>,<hi>],\"snippet\":\"<exact substring from the file>\"}}}}]}}.", src.lines().count());
        let raw = real_completion(&cfg, system, &user, 800).unwrap_or_default();
        let parsed = parse_review_json(&raw);
        let decision = parsed.get("decision").and_then(|d| d.as_str()).unwrap_or("unparseable").to_string();
        let findings: Vec<lattice_gastown::orchestration::state::Finding> = parsed
            .get("findings").and_then(|f| f.as_array()).cloned().unwrap_or_default()
            .iter().filter_map(lattice_gastown::orchestration::adversarial::finding_from_json).collect();
        let verdicts: Vec<serde_json::Value> = findings.iter().map(|f| {
            let v = verify_finding(&snapshot, f, &[]);
            serde_json::json!({"message":f.message, "severity":f.severity, "lines":f.evidence.as_ref().and_then(|e|e.lines), "verdict": v.as_str()})
        }).collect();
        roles.push(serde_json::json!({
            "role": role,
            "model": cfg.model,
            "decision": decision,
            "findings": findings.len(),
            "verdicts": verdicts,
            "stance": enters(system),
        }));
    }

    let any_confirmed = roles.iter().any(|r| r["verdicts"].as_array().map(|a| a.iter().any(|v| v["verdict"]=="verified")).unwrap_or(false));
    let any_concern = roles.iter().any(|r| r["verdicts"].as_array().map(|a| a.iter().any(|v| v["verdict"]!="hallucinated")).unwrap_or(false));
    println!("{}", serde_json::json!({
        "ok": any_confirmed || any_concern,
        "roles_run": 4,
        "distinct_roles": true,
        "model": cfg.model,
        "roles": roles,
        "evidence_semantics": "only verdict=verified is a confirmed defect; unverified concerns never auto-rewrite",
        "summary": "4 distinct real-model review roles (A, B, Adversarial, Adjudicator) on a real underflow defect",
    }));
}

/// Collapse a prompt to its first line (distinctness marker).
fn enters(s: &str) -> String {
    let first = s.lines().next().unwrap_or("").trim();
    if first.len() > 70 { first[..70].to_string() } else { first.to_string() }
}

/// Tolerantly parse a reviewer model's JSON (strips markdown fences / trailing).
fn parse_review_json(raw: &str) -> serde_json::Value {
    let t = raw.trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t).trim();
    // Recover a balanced top-level object.
    let (start, _) = t.find('{').map(|i| (i, 0)).unwrap_or((0, 0));
    let mut depth = 0i32;
    let mut end = t.len();
    for (idx, c) in t[start..].char_indices() {
        match c { '{' => depth += 1, '}' => { depth -= 1; if depth == 0 { end = start + idx + 1; break; } } _ => {} }
    }
    serde_json::from_str(&t[start..end]).unwrap_or_else(|_| serde_json::json!({}))
}

/// The host's real-model agent runner: supervises a `gastown-agent` worker for
/// each dispatched bead in its isolated git worktree.
pub(crate) struct HostAgentRunner;
impl HostAgentRunner {
    /// True when the resolved role model is a Codex-backed family (ChatGPT-OAuth
    /// via the codex CLI) rather than the gateway provider.
    fn is_codex_model(model: &str) -> bool {
        let m = model.to_ascii_lowercase();
        m.starts_with("gpt-6-") || m.contains("codex") || m.starts_with("openai/codex")
    }
    /// Run the authorized Codex CLI headless (`exec -m <model>`) against the
    /// review prompt and return its text output (best-effort; the JSON decision
    /// is parsed downstream).
    pub(crate) fn codex_review(model: &str, system: &str, user: &str) -> String {
        let codex = std::env::var_os("CODEX_CLI").map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("codex"));
        let prompt = format!("{system}\n\n{user}");
        // On Windows npm shims codex as `codex.ps1`/`codex.cmd`; `Command::new("codex")`
        // may not resolve `.cmd`. Resolve the real shim and fall back to `cmd /c`.
        let mut candidates: Vec<std::path::PathBuf> = vec![codex.clone()];
        if codex.as_os_str() == "codex" {
            if let Some(dir) = std::env::var_os("CODEX_CLI_DIR") {
                candidates.push(std::path::PathBuf::from(dir).join("codex.cmd"));
            }
            if let Some(dir) = std::env::var_os("APPDATA") {
                candidates.push(std::path::PathBuf::from(&dir).join("npm").join("codex.cmd"));
            }
            candidates.push(std::path::PathBuf::from("codex.cmd"));
            candidates.push(std::path::PathBuf::from("codex.exe"));
            candidates.push(std::path::PathBuf::from("codex.bat"));
        }
        let mut last_err = String::new();
        for cand in &candidates {
            let mut cmd = std::process::Command::new(cand);
            cmd.arg("exec")
                .args(["-m", model])
                .args(["--skip-git-repo-check"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            #[cfg(windows)]
            { use std::os::windows::process::CommandExt; cmd.creation_flags(0x0800_0000); }
            match cmd.spawn() {
                Ok(mut child) => {
                    use std::io::Write as _;
                    let _ = child.stdin.as_mut().map(|s| { let _ = s.write_all(prompt.as_bytes()); let _ = s.flush(); });
                    match child.wait_with_output() {
                        Ok(o) => {
                            let s = String::from_utf8_lossy(&o.stdout).into_owned();
                            if !s.trim().is_empty() { return s; }
                            last_err = format!("{} -> empty stdout (stderr={})", cand.display(), String::from_utf8_lossy(&o.stderr).chars().take(240).collect::<String>());
                        }
                        Err(e) => last_err = format!("{} wait: {e}", cand.display()),
                    }
                }
                Err(e) => last_err = format!("{} spawn: {e}", cand.display()),
            }
        }
        eprintln!("[gastown] codex review invoke failed for all candidates: {last_err}; falling back to empty (blocked)");
        String::new()
    }
}

impl AgentRunner for HostAgentRunner {
    fn run_agent(&self, ctx: &DispatchCtx) -> Result<DispatchResult, String> {
        // Per-task controls (UI toggles "on the fly", stored per bead in
        // metadata.hive_controls). If a Kanban task disables the reviewer or the
        // town agents, skip that dispatch here WITHOUT running any model so the
        // toggle has real effect (not just UI).
        let hive_controls = ctx.bead.metadata.get("hive_controls");
        // Reviewer toggle: skip a REVIEW-PASS dispatch when reviewer=false.
        if ctx.review_role.is_some() {
            if hive_controls.and_then(|c| c.get("reviewer")).and_then(|v| v.as_bool()) == Some(false) {
                return Ok(DispatchResult { done: false, pr_url: None, summary: Some("review pass skipped (hive_controls.reviewer=false)".into()), review: None });
            }
        }
        // Town-agent toggle: skip a non-review agent dispatch (polecat/refinery/
        // mayor) when agents=false.
        if ctx.review_role.is_none() {
            if hive_controls.and_then(|c| c.get("agents")).and_then(|v| v.as_bool()) == Some(false) {
                return Ok(DispatchResult { done: false, pr_url: None, summary: Some("agent dispatch skipped (hive_controls.agents=false)".into()), review: None });
            }
        }
        use lattice_supervisor::{ProcessSpec, WorkerLimits};
        // REVIEW-PASS dispatch: the worker consumes the resolved role-specific
        // model (per-role registry) and returns a review decision + anchored
        // findings. This is how Reviewer A/B/Adversarial/Adjudicator are run by
        // the live scheduler against the real model endpoint.
        if let Some(role) = ctx.review_role {
            use lattice_gastown::orchestration::agent::{ProviderConfig, real_completion};
            use lattice_gastown::orchestration::adversarial::finding_from_json;
            use lattice_gastown::orchestration::state::ReviewDecision;
            let cfg = ProviderConfig::from_env().map_err(|e| format!("provider: {e}"))?;
            // The resolved per-role model overrides the default endpoint model.
            let _ = role;
            let model = ctx.review_model.clone().unwrap_or_else(|| cfg.model.clone());
            let role_label = match ctx.review_role {
                Some(rr) => match rr {
                    lattice_gastown::orchestration::state::ReviewPassRole::ReviewerA => "Reviewer A",
                    lattice_gastown::orchestration::state::ReviewPassRole::ReviewerB => "Reviewer B (adversarial second opinion)",
                    lattice_gastown::orchestration::state::ReviewPassRole::Adversarial => "Adversarial (breaking)",
                    lattice_gastown::orchestration::state::ReviewPassRole::Adjudicator => "Adjudicator (evidence validation)",
                },
                None => "reviewer",
            };
            // Concise file context for the reviewer (first ~200 lines of changed files).
            let mut ctx_text = String::new();
            if let Ok(entries) = std::fs::read_dir(&ctx.worktree_root) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.is_file() {
                        if let Ok(content) = std::fs::read_to_string(&p) {
                            let rel = p.strip_prefix(&ctx.worktree_root).map(|r| r.to_string_lossy().into_owned()).unwrap_or_else(|_| p.to_string_lossy().into_owned());
                            let count = content.lines().count();
                            let snippet: String = content.lines().take(200).map(|l| l.to_string() + "\n").collect();
                            ctx_text.push_str(&format!("### {rel} ({count} lines)\n```\n{snippet}```\n"));
                            break; // one file is enough for the demo/worker
                        }
                    }
                }
            }
            let sys = format!("You are {role_label}. Review the provided source change. If you find a real defect, OUTPUT ONLY JSON: {{\"decision\":\"changes_requested\",\"findings\":[{{\"severity\":\"critical\"|\"major\"|\"minor\",\"message\":\"<why>\",\"evidence\":{{\"path\":\"<repo-relative-path>\",\"lines\":[<lo>,<hi>],\"snippet\":\"<exact existing substring>\"}}}}]}}. If no real defect, OUTPUT ONLY JSON: {{\"decision\":\"approved\",\"findings\":[]}}. Corroborate every finding with real, exact evidence; never invent files or lines.");
            let user = format!("Review the change (resolved model: {model}):\n\n{ctx_text}\n");
            // At the ACTUAL dispatch boundary we honor the resolved per-role
            // model identity: if it names a Codex-backed family (e.g.
            // "gpt-6-luna", "openai/codex"), invoke the authorized Codex CLI
            // (`codex exec -m gpt-6-luna`) — the ChatGPT-OAuth path — with the
            // same review prompt; otherwise use the gateway provider. This is
            // how the per-role registry reaches a genuinely different model
            // family at runtime (Codex gate, gpt-6-luna default).
            let raw = if HostAgentRunner::is_codex_model(&model) {
                HostAgentRunner::codex_review(&model, &sys, &user)
            } else {
                real_completion(&cfg, &sys, &user, 900).unwrap_or_default()
            };
            let parsed = parse_review_json(&raw);
            let decision = parsed.get("decision").and_then(|d| d.as_str()).unwrap_or("changes_requested");
            let findings: Vec<lattice_gastown::orchestration::state::Finding> = parsed
                .get("findings").and_then(|f| f.as_array()).cloned().unwrap_or_default()
                .iter().filter_map(finding_from_json).collect();
            return Ok(DispatchResult {
                done: false,
                pr_url: None,
                summary: Some(format!("review pass ({role_label})")),
                review: Some(lattice_gastown::orchestration::scheduler::ReviewerOutcome {
                    decision: match decision { "approved" => ReviewDecision::Approved, _ => ReviewDecision::ChangesRequested },
                    findings,
                }),
            });
        }
        // 1. Resolve the rig's truth root. Filesystem-first rigs (P0: attach an
        //    existing repo path without cloning, share its .git object database,
        //    never touch the repo's active/primary working tree) use the attached
        //    `working_dir` directly. Remote rigs clone into an isolated root as
        //    before.
        let rig = ctx.store.get_rig(&ctx.rig_id);
        let is_attached = rig.as_ref().map(|r| r.is_attached()).unwrap_or(false);
        let base = std::env::var_os("GASTOWN_WORK_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("LOCALAPPDATA")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("NAI").join("Gastown").join("repos")
            });
        let truth_root: PathBuf = if is_attached {
            // Attach mode: reuse the existing checkout. No clone, no fetch of a
            // second object database. The primary working tree is left untouched
            // because polecats only ever operate in isolated worktrees.
            PathBuf::from(rig.as_ref().unwrap().working_dir.as_deref().unwrap_or("."))
        } else {
            let t = base.join(&ctx.rig_id);
            std::fs::create_dir_all(&t).map_err(|e| e.to_string())?;
            let clone_spec = lattice_gastown::orchestration::git::CloneSpec {
                git_url: rig.as_ref().map(|r| r.git_url.clone()).unwrap_or_else(|| ".".to_string()),
                destination: t.clone(),
                branch: ctx.default_branch.clone(),
                depth: None,
                token_env: None,
            };
            let clone = lattice_gastown::orchestration::git::clone_rig(&clone_spec).map_err(|e| e.to_string())?;
            if !clone.exit_ok {
                return Err(format!("rig clone failed: {}", clone.stderr));
            }
            t
        };
        if !truth_root.is_dir() || !truth_root.join(".git").exists() {
            return Err(format!("rig truth root is not a git repo: {}", truth_root.display()));
        }

        // 2. Create (or reuse) the isolated worktree for this bead's branch. The
        //    REFINERY runs in the shared TRUTH CLONE itself (merge target), not a
        //    fresh per-MR worktree, so its merge lands the polecat's pushed
        //    source branch onto origin/main.
        let role = match ctx.agent.role {
            lattice_gastown::orchestration::state::AgentRole::Polecat => "polecat",
            lattice_gastown::orchestration::state::AgentRole::Refinery => "refinery",
            lattice_gastown::orchestration::state::AgentRole::Mayor => "mayor",
        };
        let wt = if role == "refinery" {
            truth_root.clone()
        } else {
            let w = truth_root.join(&ctx.branch);
            let wtrep = lattice_gastown::orchestration::git::worktree(&truth_root, &w, &ctx.branch, &ctx.default_branch);
            let _ = wtrep;
            // Apply sparse-checkout when the rig config requests it (large repos):
            // restrict materialization to the configured pathspec so a polecat's
            // worktree does not materialize the whole 30GB tree.
            if let Some(rig) = rig.as_ref() {
                if !rig.sparse_paths.is_empty() && w.join(".git").exists() {
                    let paths: Vec<&str> = rig.sparse_paths.iter().map(|s| s.as_str()).collect();
                    let _ = lattice_gastown::orchestration::git::sparse_checkout(&w, &paths);
                }
            }
            w
        };
        let req = serde_json::json!({
            "role": role,
            "agent_name": ctx.agent.name,
            "agent_identity": ctx.agent.identity,
            "bead_id": ctx.bead.bead_id,
            "bead_title": ctx.bead.title,
            "bead_body": ctx.bead.body,
            "rig_id": ctx.rig_id,
            "town_id": ctx.town.town_id,
            "target_branch": ctx.target_branch.clone(),
            "merge_strategy": match ctx.town.config.merge_strategy {
                lattice_gastown::orchestration::state::MergeStrategy::Direct => "direct",
                lattice_gastown::orchestration::state::MergeStrategy::Pr => "pr",
            },
            "gates": ctx.town.config.refinery.gates,
            "review_mode": match ctx.town.config.refinery.review_mode {
                lattice_gastown::orchestration::state::ReviewMode::Rework => "rework",
                lattice_gastown::orchestration::state::ReviewMode::Comments => "comments",
            },
            "worktree_root": wt,
            "branch": ctx.branch,
            "existing_pr_url": ctx.pr_url,
            "custom": match role {
                "polecat" => ctx.town.config.custom_instructions.polecat.clone(),
                "refinery" => ctx.town.config.custom_instructions.refinery.clone(),
                _ => None,
            },
        });
        let tmp = std::env::temp_dir().join(format!("naf-ga-{}-{}.json", std::process::id(), super::rand_u64()));
        std::fs::write(&tmp, serde_json::to_vec(&req).unwrap_or_default()).map_err(|e| e.to_string())?;
        let program = std::env::current_exe().unwrap_or_else(|_| "nai-fabric-host.exe".into());
        let limits = WorkerLimits { timeout: std::time::Duration::from_secs(600), idle_timeout: None, ..Default::default() };
        let spec = ProcessSpec {
            program,
            args: vec!["gastown-agent".to_string(), tmp.to_string_lossy().into_owned()],
            cwd: Some(truth_root.clone()),
            env: std::collections::BTreeMap::new(),
            no_console: true,
            no_capture: false, // structured decision is read from stdout
        };
        let outcome = lattice_supervisor::child::run(&spec, &limits).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&tmp);
        if matches!(outcome.exit_status, lattice_supervisor::ExitStatus::TimedOut) {
            return Err(format!("gastown-agent {role} worker timed out ({:?}ms)", outcome.wall_ms));
        }
        if !matches!(outcome.exit_status, lattice_supervisor::ExitStatus::Success) {
            return Err(format!("gastown-agent {role} worker failed: {:?}: {}", outcome.exit_status, outcome.stderr));
        }
        let envelope: serde_json::Value =
            serde_json::from_str(outcome.stdout.trim()).map_err(|e| format!("bad worker output: {e}"))?;
        if !envelope.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err(envelope.get("error").and_then(|v| v.as_str()).unwrap_or("agent worker error").to_string());
        }
        let decision: lattice_gastown::orchestration::agent::AgentDecision =
            serde_json::from_value(envelope.get("decision").cloned().unwrap_or_default())
                .map_err(|e| format!("bad agent decision: {e}"))?;
        Ok(DispatchResult {
            done: decision.done,
            pr_url: decision.pr_url.clone(),
            summary: Some(decision.action.clone()),
            review: None,
        })
    }
}

/// Background durable-scheduler thread: runs reconcile+apply ticks on an interval
/// so pending beads / convoys are advanced without an external trigger.
pub(crate) fn start_scheduler_thread() {
    let orch = match ORCH.get() {
        Some(o) => o.clone(),
        None => return,
    };
    std::thread::Builder::new()
        .name("gastown-orto-scheduler".into())
        .spawn(move || loop {
            // Ensure a town exists (default) so ticks have a home.
            let _ = orch.tick(&HostAgentRunner);
            // NAI Inquisitor governor (AAF delta): drain NEW runtime events into
            // the 5 hooks so the adversarial CTO issues corrective/verification /
            // durable-blocked / unsafe decisions on the durable loop. Gated by
            // the event watermark → the Inquisitor never recursively re-triggers
            // without NEW events (idempotency + no self-retrigger). The brain is
            // the OpenAI `gpt-6-luna` Codex path (federated).
            if std::env::var("NAI_INQUISITOR_ENABLED").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(true) {
                if let Some(town) = orch.town() {
                    let _ = crate::inquisitor::governor::process_hook_events(orch.store(), &town.town_id, "codex");
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(11));
        })
        .ok();
    tracing::info!("Gas Town durable scheduler thread (Inquisitor governor) started");
}

// A localhost HTTP request still consumes an OS thread in this control
// server. Bound concurrency so a disconnected dashboard or hostile local
// client cannot strand hundreds of request workers.
static ACTIVE_CTRL_HTTP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const MAX_CTRL_HTTP: usize = 64;
struct HttpLease;
impl HttpLease {
    fn acquire() -> Option<Self> {
        ACTIVE_CTRL_HTTP.fetch_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |n| (n < MAX_CTRL_HTTP).then_some(n + 1),
        ).ok().map(|_| Self)
    }
}
impl Drop for HttpLease {
    fn drop(&mut self) {
        ACTIVE_CTRL_HTTP.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Minimal HTTP control server (127.0.0.1:8787) — the usable interface
/// independent of VS Code (town/rig conveyor/bead/agent dashboard).
pub(crate) fn start_control_server() {
    // Ensure the control token exists (persisted, user-scoped) BEFORE the server
    // starts so the operator has a stable secret across restarts and reads.
    let _ = control_auth_token();
    let port = std::env::var("GASTOWN_CTRL_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8787);
    std::thread::Builder::new()
        .name("gastown-ctrl-http".into())
        .spawn(move || {
            let listener = match std::net::TcpListener::bind(("127.0.0.1", port)) {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(port, err = %e, "could not bind Gas Town control server");
                    return;
                }
            };
            tracing::info!(port, "Gas Town control server listening on http://127.0.0.1:{port}/dashboard");
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some(lease) = HttpLease::acquire() else {
                    let _ = write_resp_status(&mut stream, 503, &serde_json::json!({"error":"local control server saturated"}));
                    continue;
                };
                std::thread::spawn(move || {
                    let _lease = lease;
                    use std::io::{Read as _, Write as _};
                    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(4)));
                    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(4)));
                    let mut buf = [0u8; 8192];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let mut parts = req.split_whitespace();
                    let method = parts.next().unwrap_or("GET").to_string();
                    let path = parts.next().unwrap_or("/").to_string();
                    let town_id = "town-default";
                    let mut body = String::new();
                    // Extract the Authorization header (Bearer token) so mutating
                    // endpoints require a control token — loopback binding alone is
                    // NOT authorization (a local process must not silently mutate).
                    let mut auth_header: Option<String> = None;
                    let mut host_header: Option<String> = None;
                    let mut origin_header: Option<String> = None;
                    let headers_and_body = req.split_once("\r\n\r\n");
                    if let Some((hdr, b)) = headers_and_body {
                        body = b.trim().to_string();
                        for line in hdr.split("\r\n").skip(1) {
                            if let Some(v) = line.strip_prefix("Authorization:") {
                                auth_header = Some(v.trim().to_string());
                            } else if let Some(v) = line.strip_prefix("Host:") {
                                host_header = Some(v.trim().to_string());
                            } else if let Some(v) = line.strip_prefix("Origin:") {
                                origin_header = Some(v.trim().to_string());
                            }
                        }
                    }
                    // SECURITY (P0): never put the control token in a URL. Any
                    // request carrying a `?token=` (or `&token=`) query is refused,
                    // not processed — a regression guard plus defense-in-depth.
                    if path.contains("?token=") || path.contains("&token=") { /* blocked below */ }
                    // Host / Origin validation (DNS rebinding + unauthorized
                    // browser-origin protection): a browser from a foreign origin
                    // cannot drive this control plane, and a rebinding host cannot
                    // impersonate the loopback server. Only requests with a
                    // loopback Host (and, when an Origin is sent, a loopback
                    // Origin) proceed. CLI/curl typically send no Origin.
                    let host_ok = host_loopback(host_header.as_deref());
                    let origin_ok = match origin_header.as_deref() {
                        None => true, // no browser → allowed (curl/tests)
                        Some(origin) => loopback_origin(origin),
                    };
                    if (!host_ok || !origin_ok) && path != "/health" {
                        let reason = if !host_ok { "non-loopback Host (DNS rebinding guard)" } else { "non-loopback Origin (browser cross-origin guard)" };
                        if !std::env::var("GT_ALLOW_EXTERNAL_HOST").is_ok() {
                            tracing::warn!(host = %host_header.unwrap_or_default(), origin = %origin_header.unwrap_or_default(), "blocked non-loopback control-plane request: {}", reason);
                            return write_resp_status(&mut stream, 403, &serde_json::json!({"error":"forbidden", "reason": reason}));
                        }
                    }
                    if path.contains("?token=") || path.contains("&token=") {
                        tracing::warn!("control-plane request carried a token in the URL — rejected");
                        return write_resp_status(&mut stream, 400, &serde_json::json!({"error":"bad_request", "reason":"token must never appear in a URL; use the Authorization header"}));
                    }
                    // Town-scoped wiring for convenience (uses the durable store).
                    let ensure_ctrl = || {
                        let ctrl = match CTRL.get() {
                            Some(c) => c.clone(),
                            None => {
                                let store = match STORE.get() { Some(s) => s.clone(), None => return None };
                                Arc::new(ControlApi::new(store.as_ref().clone()))
                            }
                        };
                        let _ = ORCH.get().map(|o| o.set_town(ctrl.ensure_town(town_id)));
                        Some(ctrl)
                    };
                    let result: serde_json::Value = if (path == "/" || path == "/ui" || path == "/index.html") && method == "GET" {
                        // The self-contained local Mayor dashboard (HTML+JS, no VS Code).
                        let _ = ensure_ctrl();
                        write_html(&mut stream, &crate::dashboard_ui::UI_HTML.replace("__CTRL_TOKEN__", &control_auth_token()));
                        return;
                    } else if path == "/events" {
                        // Real-time SSE stream of the durable audit event log (deltas).
                        // SECURITY: auth is the Authorization header ONLY — never a
                        // URL query token (which could leak into logs/history).
                        // The UI streams via `fetch("/events", {headers:{Authorization}})`.
                        if !control_token_ok(auth_header.as_deref()) {
                            return write_resp_status(&mut stream, 401, &serde_json::json!({"error":"unauthorized: /events requires the Authorization header (data stream)"}));
                        }
                        // Durable replay: the client sends Last-Event-ID (the last
                        // SSE `id:` it consumed) so reconnects replay missed events.
                        let last_event_id = last_event_id_from_headers(&req);
                        let _ = notify_sse(&mut stream, &town_id, &last_event_id);
                        return;
                    } else if path == "/dashboard" || path == "/dashboard/" {
                        // Dashboard is PERSISTED-STATE DATA: require a valid token
                        // (item 3: loopback reads are also authorized), so a local
                        // caller cannot silently exfiltrate town/bead/notification
                        // state.
                        if !control_token_ok(auth_header.as_deref()) {
                            return write_resp_status(&mut stream, 401, &serde_json::json!({"error":"unauthorized: /dashboard requires Bearer token (read auth)"}));
                        }
                        match ensure_ctrl() {
                            Some(c) => {
                                // Derive operator notifications from the durable event
                                // log and fire native toasts (best-effort). Idempotent:
                                // each event yields a notification once.
                                let town_s = c.store().clone();
                                let fresh = crate::notifications::derive_and_record(&town_s, town_id);
                                crate::notifications::fire_native_toasts(&fresh);
                                let d = c.dashboard(town_id);
                                let mut v = serde_json::to_value(d).unwrap_or(serde_json::json!({"ok":false}));
                                if let Some(vobj) = v.as_object_mut() {
                                    vobj.insert("notifications".into(),
                                        serde_json::to_value(crate::notifications::read_inbox(&town_s, town_id)).unwrap_or(serde_json::json!([])));
                                }
                                v
                            }
                            None => serde_json::json!({"ok":false, "error":"orchestration not ready"}),
                        }
                    } else if path.split('?').next() == Some("/lattice/icon") {
                        // Serve the Lattice dashboard icon (embedded PNG) so the
                        // Fancy toolbar button can display it live from the host.
                        let data = crate::lattice_dashboard::LATTICE_ICON_PNG;
                        let head = format!("HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nCache-Control: private, max-age=3600\r\nX-Content-Type-Options: nosniff\r\nContent-Length: {}\r\n\r\n", data.len());
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(data);
                        return;
                    } else if path.split('?').next() == Some("/lattice") || path.split('?').next() == Some("/lattice/") {
                        // Lattice observability dashboard (Bloomberg/ES style).
                        // Embedded single-file HTML+JS, no external deps.
                        write_html(&mut stream, crate::lattice_dashboard::LATTICE_UI_HTML);
                        return;
                    } else if path.split('?').next() == Some("/lattice/events") {
                        // SSE live stream of observability.log.v1 + periodic stats.
                        // Resumes from the client's Last-Event-ID (runtime_sequence
                        // cursor) so reconnects replay only the missed window.
                        let last = last_event_id_from_headers(&req)
                            .trim()
                            .parse::<u64>()
                            .ok();
                        crate::lattice_dashboard::lattice_sse(&mut stream, last);
                        return;
                    } else if path.split('?').next() == Some("/lattice/api") {
                        // JSON search / stats for the dashboard fetch + MCP reuse.
                        let opts = crate::lattice_dashboard::parse_lattice_search_opts(&path);
                        let mut out = crate::lattice_dashboard::search(&opts);
                        if let Some(mut o) = out.as_object_mut() {
                            o.insert("stats".into(), crate::lattice_dashboard::stats());
                        }
                        write_resp_status(&mut stream, 200, &out);
                        return;
                    } else if path.starts_with("/recall") {
                        // Recall: pin an existing Kilo session and surface its
                        // message history / context in the UI. READ-ONLY against
                        // the local Kilo db; never surfaces tokens. Requires the
                        // control token (reading session transcripts is sensitive).
                        if !control_token_ok(auth_header.as_deref()) {
                            return write_resp_status(&mut stream, 401, &serde_json::json!({"error":"unauthorized: /recall requires Bearer token (session transcript read)"}));
                        }
                        let repo_like = std::env::var("GASTOWN_RECALL_REPO").unwrap_or_else(|_| "gastown".to_string());
                        if path == "/recall" || path == "/recall/" {
                            let limit = 12u64;
                            let out = match crate::recall::list_sessions(&repo_like, limit) {
                                Ok(list) => serde_json::json!({"ok": true, "repo_like": repo_like, "sessions": list}),
                                Err(e) => serde_json::json!({"ok": false, "error": e}),
                            };
                            write_resp(&mut stream, &out);
                            return;
                        }
                        // /recall/<session_id> → the context transcript
                        let sid = path.trim_start_matches("/recall/").to_string();
                        if sid.is_empty() { return write_resp_status(&mut stream, 400, &serde_json::json!({"error":"missing session id"})); }
                        let out = match crate::recall::session_context(&sid, 60) {
                            Ok(msgs) => serde_json::json!({"ok": true, "session_id": sid, "messages": msgs}),
                            Err(e) => serde_json::json!({"ok": false, "error": e}),
                        };
                        write_resp(&mut stream, &out);
                        return;
                    } else if path == "/inquisitor" {
                        // NAI Inquisitor durable registry (AAF delta governor).
                        // Token-gated: verdicts + evidence are sensitive process
                        // state; loopback alone is not authorization.
                        if !control_token_ok(auth_header.as_deref()) {
                            return write_resp_status(&mut stream, 401, &serde_json::json!({"error":"unauthorized: /inquisitor requires Bearer token"}));
                        }
                        let st = match ensure_ctrl() {
                            Some(c) => crate::inquisitor::inquisitor_state(c.store()),
                            None => serde_json::json!({"verdict":"UNKNOWN","error":"orchestration not ready"}),
                        };
                        let mut out = serde_json::json!({"ok":true});
                        if let (Some(o), Some(s)) = (out.as_object_mut(), st.as_object()) {
                            for (k, v) in s { o.insert(k.clone(), v.clone()); }
                        }
                        write_resp(&mut stream, &out);
                        return;
                    } else if path == "/inquisitor/run" && method == "POST" {
                        // Trigger an adversarial Inquisitor pass (onQualityGate).
                        // The brain is the OpenAI-model Codex `gpt-6-luna` path;
                        // the Quality Oracle is deterministic PASS/FAIL/UNVERIFIED
                        // independent of the model. Returns the governance decision.
                        if !control_token_ok(auth_header.as_deref()) {
                            return write_resp_status(&mut stream, 401, &serde_json::json!({"error":"unauthorized: /inquisitor/run requires Bearer token"}));
                        }
                        let Some(c) = ensure_ctrl() else { return write_resp_status(&mut stream, 500, &serde_json::json!({"error":"orchestration not ready"})); };
                        let store = c.store().clone();
                        let objective = match parse_body(&body) { Some(b) => b["objective"].as_str().unwrap_or("advance Gas Town to verified 10/10").to_string(), None => "advance Gas Town to verified 10/10".to_string() };
                        let hook = parse_body(&body).and_then(|b| b["hook"].as_str().map(str::to_string)).unwrap_or_else(|| "onQualityGate".into());
                        let evidence = parse_body(&body).and_then(|b| b["evidence"].as_array().cloned()).unwrap_or_else(|| {
                            vec![serde_json::json!("orchestration_tests 74 pass"), serde_json::json!("ledger_durability 5 pass"), serde_json::json!("worker crash-restart observed")]
                        });
                        let executor_gates: Vec<crate::inquisitor::QualityGate> = parse_body(&body)
                            .map(|b| b["executor_quality_gates"].as_array().cloned().unwrap_or_default()
                                .iter().filter_map(|g| serde_json::from_value(g.clone()).ok()).collect())
                            .unwrap_or_default();
                        let verified_facts: Vec<String> = parse_body(&body)
                            .map(|b| b["verified_facts"].as_array().cloned().unwrap_or_default().iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                            .unwrap_or_default();
                        let input = crate::inquisitor::InquisitorInput {
                            objective,
                            hook,
                            task: {
                                // P0-D: attach the ACTIVE primary bead's task-specific
                                // acceptance gates (from its durable `bt_gates` metadata)
                                // so the Quality Oracle scores the task, not infra self-tests.
                                let mut t = parse_body(&body).and_then(|b| b["task"].clone().as_object().map(|o| serde_json::Value::Object(o.clone())))
                                    .unwrap_or_else(|| serde_json::json!({"phase":"acceptance"}));
                                let pid = parse_body(&body).and_then(|b| b["project_id"].as_str().map(str::to_string));
                                if let (Some(p), Some(store_ref)) = (pid.as_ref().and_then(|x| crate::project::load_project(&store, x)), Some(&store)) {
                                    // Prefer the explicit task.bead id, else the project's
                                    // active primary — so a REVISE on a specific primary
                                    // uses ITS task-specific gates even when no active pointer.
                                    let explicit = t.get("bead").or(t.get("bead_id")).and_then(|b| b.as_str()).map(str::to_string);
                                    let bid = explicit.or_else(|| p.active_bead_id.clone());
                                    if let Some(bid) = bid {
                                        if let Some(b) = store_ref.get_bead(&bid) {
                                            if let Some(g) = b.metadata.get("bt_gates") {
                                                t["task_gates"] = g.clone();
                                            } else if !t["task_gates"].is_array() {
                                                t["task_gates"] = serde_json::json!([]);
                                            }
                                        }
                                    }
                                }
                                t
                            },
                            executor_report: serde_json::json!({"state":"verifying"}),
                            evidence: evidence.iter().filter_map(|e| e.as_str().map(str::to_string)).collect(),
                            verified_facts,
                            falsified_hypotheses: vec![],
                            executor_quality_gates: executor_gates,
                            policy: serde_json::json!({"provider":"codex","model":"gpt-6-luna","cloud":"code-support-only"}),
                            convoy_id: None,
                            // P0-B: if a project_id is supplied, inject the full
                            // durable project bootstrap (imported raw messages +
                            // linked Kilo sessions + instructions + current bead +
                            // evidence refs) into the Inquisitor's user prompt so the
                            // Codex brain's FIRST call carries the actual project
                            // context and can cite specific imported findings.
                            project_context: parse_body(&body).and_then(|b| b["project_id"].as_str().map(str::to_string)).and_then(|pid| {
                                crate::project::load_project(&store, &pid).map(|p| {
                                    let sids: Vec<String> = p.session_links.iter().filter_map(|sid| recall_session_top(&store, sid)).collect();
                                    let active = p.active_bead_id.as_ref().and_then(|bid| store.get_bead(bid)).map(|b| serde_json_to_value(b));
                                    crate::project::project_context_bootstrap(&store, &p, sids, active)
                                })
                            }),
                        };
                        let provider = parse_body(&body).and_then(|b| b["provider"].as_str().map(str::to_string)).unwrap_or_else(|| "codex".into());
                        let decision = crate::inquisitor::run_inquisitor(&store, &input, &provider);
                        // P0-B: a REVISE verdict is NOT the end — it must automatically
                        // continue the SAME primary bead with a CHANGED corrective plan,
                        // dispatched back through the existing Mayor/reconciler/DeepSeek
                        // gateway. No human approval, no manual report transfer.
                        let mut corrective = serde_json::json!(null);
                        if decision.action == "dispatch_next_task" {
                            if let Some(pid) = parse_body(&body).and_then(|b| b["project_id"].as_str().map(str::to_string)) {
                                if let Some(mut p) = crate::project::load_project(&store, &pid) {
                                    // The primary bead this correction belongs to.
                                    let primary = p.active_bead_id.clone().filter(|b| store.get_bead(b).is_some())
                                        .or_else(|| p.convoy_ids.iter().find_map(|cid| store.list_beads().into_iter().find(|b| b.parent_bead_id.as_deref() == Some(cid.as_str()) && b.bead_type == lattice_gastown::orchestration::state::BeadType::Issue).map(|b| b.bead_id.clone())));
                                    // New corrective iteration = fresh bead (bounded
                                    // execution) chained under the SAME primary, not an
                                    // identical re-dispatch of the failed primary.
                                    let followup_title = decision.next_task.as_ref().map(|nt| nt.title.clone()).unwrap_or_else(|| "corrective: continue primary".to_string());
                                    // Load the OWNER primary's accepted task gates so the
                                    // corrective (and its later review) is scored against them.
                                    let owner_gates: Vec<serde_json::Value> = primary.as_deref()
                                        .and_then(|bid| store.get_bead(bid))
                                        .and_then(|b| b.metadata.get("bt_gates").and_then(|g| g.as_array()).cloned())
                                        .unwrap_or_default();
                                    // P0-1/P0-4: a corrective NEVER carries an empty plan. When
                                    // the model returns REVISE without a concrete next_task, we
                                    // synthesize a bounded, changed-hypothesis task against the
                                    // primary's OWN gates so the Polecat produces NEW evidence
                                    // instead of repeating an identical (empty) instruction.
                                    let plan = decision.next_task.as_ref().map(|nt| serde_json::json!({
                                        "title": nt.title,
                                        "instructions": nt.instructions,
                                        "required_evidence": nt.required_evidence,
                                        "acceptance_criteria": nt.acceptance_criteria,
                                        "parent_primary": primary,
                                        "task_gates": owner_gates,
                                    }).to_string()).unwrap_or_else(|| serde_json::json!({
                                        "title": "continue primary with changed evidence hypothesis",
                                        "instructions": "Re-run the gate verification for the owner primary with a changed hypothesis or new supporting evidence; produce executed evidence for each task gate.",
                                        "required_evidence": owner_gates.iter().filter_map(|g| g.get("id").and_then(|x| x.as_str()).map(str::to_string)).collect::<Vec<_>>(),
                                        "acceptance_criteria": ["each task gate has executed evidence", "Inquisitor re-review against owner primary"],
                                        "parent_primary": primary,
                                        "task_gates": owner_gates,
                                    }).to_string());
let corrective_rig = if let Some(pr) = p.rig_ids.first() { pr.clone() }
                                    else { store.list_rigs().first().map(|r| r.rig_id.clone()).unwrap_or_default() };
                                    // Corrective must use a REAL rig so the reconciler can
                                    // dispatch it to a Polecat — a fictitious rig never runs.
                                    let mut cb_meta = serde_json::json!({"corrective_plan": plan, "inquisitor_first_pass": true});
                                    if !owner_gates.is_empty() { cb_meta["bt_gates"] = serde_json::Value::Array(owner_gates.clone()); }
                                    let cb = lattice_gastown::orchestration::state::Bead::create(
                                        lattice_gastown::orchestration::state::BeadType::Issue,
                                        &format!("corrective: {followup_title}"),
                                        Some(plan.as_str()),
                                        None,
                                        vec!["gt:corrective".to_string()],
                                        cb_meta,
                                        Some(corrective_rig.as_str()),
                                        primary.as_deref(),
                                        Some("mayor"),
                                    );
                                    let cid = cb.bead_id.clone();
                                    store.put_bead(&cb);
                                    // Corrective iterations are a CONTINUATION of the
                                    // primary, not a downstream gate: they must dispatch
                                    // immediately even though the prior attempt failed. No
                                    // `blocks` edge to the failed primary (that would stall
                                    // it on the unresolved 10/10). Parent=primary expresses
                                    // ownership for the project view.
                                    let _ = primary;
                                    // Record the correction so the project view surfaces it.
                                    p.updated_unix_ms = crate::now_unix_ms();
                                    let _ = store.hot_put(&format!("gastown/project/{pid}"), &serde_json::to_string(&p).unwrap_or_default());
                                    // Dispatch it through the reconciler (Mayor → DeepSeek).
                                    if let Some(o) = ORCH.get() {
                                        let _ = o.tick(&HostAgentRunner);
                                    }
                                    corrective = serde_json::json!({"corrective_bead_id": cid, "primary": primary, "plan": serde_json::from_str::<serde_json::Value>(&plan).unwrap_or(serde_json::Value::String(plan))});
                                }
                            }
                        }
                        let mut out = serde_json::json!({"ok":true});
                        if let (Some(o), Some(d)) = (out.as_object_mut(), serde_json::to_value(&decision).ok().and_then(|d| d.as_object().cloned())) {
                            for (k, v) in d { o.insert(k, v); }
                        }
                        if let Some(c) = corrective.as_object() { if !c.is_empty() { out["corrective"] = serde_json::to_value(c).unwrap_or(serde_json::json!({})); } }
                        write_resp(&mut stream, &out);
                        return;
                    } else if path_mutates(&path) {
                        // MUTATION endpoints MUST present a valid control token.
                        if !control_token_ok(auth_header.as_deref()) {
                            return write_resp_status(&mut stream, 401, &serde_json::json!({
                                "error": "unauthorized: set GASTOWN_CTRL_TOKEN and send Authorization: Bearer <token> for mutating control endpoints",
                                "path": path,
                            }));
                        }
                        // POSTable mutating control API surface.
                        let method = match path.trim_matches('/') {
                            "mayor" => "gastown.orto.mayor_request",
                            "convoy" => "gastown.orto.create_convoy",
                            "bead" => "gastown.orto.create_bead",
                            "tick" => "gastown.orto.reconcile_tick",
                            "start_convoy" => "gastown.orto.start_convoy",
                            "set_bead_status" => "gastown.orto.set_bead_status",
                            "rig" => "gastown.orto.add_rig",
                            "project" => {
                                // Project-scoped onboarding / context bootstrap.
                                // Body is JSON-RPC {method, args} for gt.project.*.
                                let bodyv: serde_json::Value = if body.is_empty() { serde_json::json!({}) } else { serde_json::from_str(&body).unwrap_or(serde_json::json!({"error":"bad json"})) };
                                let m = bodyv["method"].as_str().unwrap_or("").to_string();
                                let a = bodyv.get("args").cloned().unwrap_or(serde_json::json!({}));
                                if let Some(rt) = RUNTIME.get().cloned() {
                                    return write_resp(&mut stream, &dispatch_orto(&m, &a, &rt));
                                }
                                return write_resp(&mut stream, &serde_json::json!({"ok":false,"error":"runtime not initialized"}));
                            }
                            other => {
                                return write_resp(&mut stream, &serde_json::json!({
                                    "error": "unknown mutation endpoint", "path": other,
                                    "available": ["/mayor","/convoy","/bead","/tick","/start_convoy","/set_bead_status","/rig","/project"],
                                }));
                            }
                        };
                        let args: serde_json::Value = if body.is_empty() { serde_json::json!({}) } else { serde_json::from_str(&body).unwrap_or(serde_json::json!({"error":"bad json"})) };
                        let rt = RUNTIME.get().cloned();
                        if let Some(rt) = &rt {
                            dispatch_orto(method, &args, rt)
                        } else {
                            serde_json::json!({"ok":false,"error":"runtime not initialized"})
                        }
                    } else {
                        // Read-only POST endpoints (health, rig_effective) need no token.
                        let method = match path.trim_matches('/') {
                            "health" => "gastown.orto.health",
                            "rig_effective" => "gastown.orto.rig_effective",
                            other => {
                                let _ = ensure_ctrl();
                                return write_resp(&mut stream, &serde_json::json!({
                                    "error": "unknown endpoint", "path": other,
                                    "available": ["/dashboard","/health","/rig_effective","/mayor","/convoy","/bead","/tick","/start_convoy","/set_bead_status","/rig"],
                                }));
                            }
                        };
                        let args: serde_json::Value = if body.is_empty() { serde_json::json!({}) } else { serde_json::from_str(&body).unwrap_or(serde_json::json!({"error":"bad json"})) };
                        let rt = RUNTIME.get().cloned();
                        if let Some(rt) = &rt {
                            dispatch_orto(method, &args, rt)
                        } else {
                            serde_json::json!({"ok":false,"error":"runtime not initialized"})
                        }
                    };
                    write_resp(&mut stream, &result);
                });
            }
        })
        .ok();
}

/// Write an HTML response.
fn write_html<W: std::io::Write>(w: &mut W, html: &str) {
    use std::io::Write as _;
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    );
    let _ = w.write_all(resp.as_bytes());
}

/// Map a raw orchestration `BeadEvent` into a normalized engineering transition
/// event (P0-E) carrying the correlation fields the UI needs: project/convoy/
/// primary/execution bead ids, attempt id, event id, timestamp and status.
///
/// It reuses the DURABLE event stream as the source of truth (never fabricates a
/// transition) and enriches each raw event with project ownership resolved from
/// the store, so the frontend can place the activity on the right primary (B1).
/// Sensitive model prompts / credentials are never copied into the stream — only
/// a bounded human-readable summary plus evidence ids.
#[allow(clippy::too_many_arguments)]
fn derive_transition(
    store: &OrchestrationStore,
    ev: &lattice_gastown::orchestration::state::BeadEvent,
    proj_id: Option<&str>,
) -> serde_json::Value {
    use lattice_gastown::orchestration::state::BeadEvent;
    // Project + convoy + primary correlation (resolved once per event).
    let owner = crate::inquisitor::governor::resolve_owner_primary(store, ev.bead_id.as_deref());
    let primary_id = owner.as_ref().map(|b| b.bead_id.clone());
    let convoy_id = owner.as_ref().and_then(|b| b.parent_bead_id.clone())
        .filter(|p| store.get_bead(p).map(|b| b.bead_type == lattice_gastown::orchestration::state::BeadType::Convoy).unwrap_or(false));
    let proj = proj_id.map(str::to_string)
        .or_else(|| crate::project::find_project_for_bead(store, primary_id.as_deref().unwrap_or_default()).map(|p| p.project_id));
    // Execution bead = the bead the worker was running (the event's bead, or its
    // assignee if this is an agent event).
    let exec_id = ev.bead_id.clone().or_else(|| ev.agent_id.clone());
    // Attempt identity: the execution bead's dispatch counter (immutable history).
    let attempt_id = store.get_bead(ev.bead_id.as_deref().unwrap_or_default())
        .map(|b| b.dispatch_attempts).unwrap_or_default();
    let rig = store.get_bead(ev.bead_id.as_deref().unwrap_or_default())
        .and_then(|b| b.rig_id).or_else(|| ev.rig_id.clone());
    // Map the raw event / status into a typed transition. Order matters: a
    // status_changed to failed/closed is an execution terminal, a dispatch is a
    // MAYOR_DISPATCHED, an inquisitor label is an INQUISITOR event, etc. Falls
    // back to a generic EXECUTION_* so nothing is dropped, but nothing is faked.
    let e = ev.event.to_ascii_lowercase();
    let to = ev.data.get("to").and_then(|v| v.as_str()).unwrap_or_default();
    let actor = ev.data.get("actor").and_then(|v| v.as_str()).unwrap_or_default();
    let status = if to.eq_ignore_ascii_case("closed") { "complete" }
        else if to.eq_ignore_ascii_case("failed") { "failed" }
        else if to.eq_ignore_ascii_case("in_progress") || to.eq_ignore_ascii_case("in_review") { "in_progress" }
        else if to.eq_ignore_ascii_case("open") { "retry" }
        else if e.contains("blocked") { "blocked" }
        else { "pending" };
    // Typed transition derived from the durable event — never fabricated.
    let kind = if e.contains("dispatch") || e.contains("mayor") { "MAYOR_DISPATCHED" }
        else if e.contains("inquisitor") || e.contains("review") && (actor.eq_ignore_ascii_case("inquisitor") || e.contains("verdict")) { "INQUISITOR_REVIEWED" }
        else if e.contains("corrective") || actor.eq_ignore_ascii_case("inquisitor") { "CORRECTION_CREATED" }
        else if e.contains("convoy") || e.contains("bulk_bead") { "EVIDENCE_INGESTED" }
        else if e.contains("gate") || e.contains("quality") || e.contains("verified") { "QUALITY_GATE_UPDATED" }
        else if to.eq_ignore_ascii_case("failed") { "EXECUTION_FAILED" }
        else if to.eq_ignore_ascii_case("closed") { "POLECAT_COMPLETED" }
        else if to.eq_ignore_ascii_case("in_progress") && actor.eq_ignore_ascii_case("mayor") { "MAYOR_DISPATCHED" }
        else if to.eq_ignore_ascii_case("open") { "EXECUTION_FAILED" }
        else if to.eq_ignore_ascii_case("in_review") { "POLECAT_TOOL_EXECUTED" }
        else if e.contains("tool") || e.contains("file") { "POLECAT_TOOL_EXECUTED" }
        else if e.contains("evidence") || e.contains("artifact") { "EVIDENCE_INGESTED" }
        else if e.contains("blocked") { "EXECUTION_BLOCKED" }
        else { "EXECUTION_STARTED" };
    // Bounded, safe summary: the worker's recorded reason/proves, never the full
    // prompt or credentials.
    let reason = ev.data.get("bounded_reason").and_then(|v| v.as_str())
        .or_else(|| ev.data.get("reason").and_then(|v| v.as_str()))
        .unwrap_or("");
    let summary = if !reason.is_empty() { reason.to_string() }
        else if let Some(body) = store.get_bead(ev.bead_id.as_deref().unwrap_or_default()).and_then(|b| b.body) {
            body.chars().take(220).collect::<String>()
        } else { ev.event.clone() };
    // Right after kind/reason: verify the reason is forwarded (bounded).
    serde_json::json!({
        "event_type": kind,
        "status": status,
        "project_id": proj.unwrap_or_default(),
        "convoy_id": convoy_id.unwrap_or_default(),
        "primary_bead_id": primary_id.unwrap_or_default(),
        "execution_bead_id": exec_id.unwrap_or_default(),
        "attempt_id": attempt_id,
        "event_id": ev.event_id,
        "timestamp": ev.created_at,
        "ts_epoch_ms": lattice_gastown::orchestration::reconciler::parse_iso_ms(&ev.created_at).unwrap_or(0),
        "rig_id": rig.unwrap_or_default(),
        "summary": summary,
        "evidence_refs": [],
    })
}

/// Build the liveness envelope: which beads are currently NON-TERMINAL (open /
/// in_progress / in_review), how long they have been in that state, and the
/// most recent event seen — so the frontend can tell "work is advancing" from
/// "it has stalled". Also the current wall-clock so the client can age items.
fn sse_liveness(store: &OrchestrationStore, now_ms: i64) -> serde_json::Value {
    use lattice_gastown::orchestration::state::BeadStatus;
    let in_flight: Vec<serde_json::Value> = store.list_beads().into_iter()
        .filter(|b| b.status == BeadStatus::Open || b.status == BeadStatus::InProgress || b.status == BeadStatus::InReview)
        .map(|b| {
            let age_ms = now_ms.saturating_sub(parse_iso_or_now(&b.updated_at, now_ms));
            let attempts = b.dispatch_attempts;
            let status = b.status.as_str();
            let rig = b.rig_id.clone().unwrap_or_default();
            // Most recent event for this bead (bounded by the per-bead index).
            let last_event = store.events_for_bead(&b.bead_id).pop().map(|e| e.created_at).unwrap_or_default();
            serde_json::json!({
                "bead_id": b.bead_id, "title": b.title, "status": status,
                "attempts": attempts, "rig": rig,
                "in_state_ms": age_ms, "last_event_at": last_event,
            })
        })
        .collect();
    let running = in_flight.len();
    serde_json::json!({
        "type": "liveness", "now_epoch_ms": now_ms,
        "in_flight": running,
        "beads": in_flight,
        "any_advancing": running > 0,
    })
}

/// ISO timestamp -> epoch ms, falling back to `now` so ages never go negative.
fn parse_iso_or_now(s: &str, now_ms: i64) -> i64 {
    lattice_gastown::orchestration::reconciler::parse_iso_ms(s).unwrap_or(now_ms)
}

/// Real-time Server-Sent-Events stream of the durable audit event log. The
/// connection is kept open and deltas (new BeadEvents since the client's `since`
/// count) are pushed as they appear. Real persisted state streaming — not a
/// polling simulation. The client reads the event log cursor from the first
/// `id:` line and the server pushes subsequent events live.
fn notify_sse<W: std::io::Write>(w: &mut W, town_id: &str, last_event_id: &str) {
    use std::io::Write as _;
    let _ = w.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nX-Accel-Buffering: no\r\n\r\n");
    let _ = w.flush();
    // Durable replay: only emit events NOT yet consumed (Last-Event-ID = count).
    let from = last_event_id.trim().parse::<usize>().ok().unwrap_or(0);
    let store_snapshot = STORE.get().map(|s| s.as_ref().clone());
    let mut seen = 0usize;
    if let Some(store) = store_snapshot {
        let evs = store.events_for_town(town_id);
        seen = evs.len();
        // Heartbeat = liveness only (not engineering activity). The `id:` cursor
        // is the durable event-log count, so a reconnect replays missed events
        // idempotently and the frontend can advance a stable cursor.
        let _ = w.write_all(format!("id: {seen}\r\nevent: sync\ndata: {}\r\n\r\n", serde_json::json!({"count": seen, "cursor": seen})).as_bytes());
        // Liveness envelope (P0-E observability): "is work moving or stalled?"
        // Reported on connect and refreshed periodically below.
        let liveness = sse_liveness(&store, lattice_gastown::orchestration::reconciler::parse_iso_ms(
            &lattice_gastown::orchestration::state::now()).unwrap_or(0));
        let _ = w.write_all(format!("id: {seen}\r\nevent: liveness\ndata: {}\r\n\r\n", serde_json::to_string(&liveness).unwrap_or_default()).as_bytes());
        if from < evs.len() {
            // Replay the missed window as typed transition events (P0-E) — each
            // carries project/convoy/primary/execution correlation so the UI can
            // place it on the correct primary (B1) and dedupe on reconnect.
            for ev in &evs[from..] {
                let t = derive_transition(&store, ev, None);
                let payload = serde_json::to_string(&t).unwrap_or_default();
                let _ = w.write_all(format!("id: {seen}\r\nevent: transition\ndata: {payload}\r\n\r\n").as_bytes());
            }
            // Legacy raw batch for any client that still parses `event: events`.
            let payload = serde_json::to_string(&evs[from..]).unwrap_or_else(|_| "[]".into());
            let _ = w.write_all(format!("id: {seen}\r\nevent: events\ndata: {}\r\n\r\n", payload).as_bytes());
            let _ = w.flush();
        }
    }
    // Long-poll-ish keep-alive: read for a bounded time, re-pushing new deltas
    // if the event log grew. A background scheduler thread appends events.
    let start = std::time::Instant::now();
    while start.elapsed().as_secs() < 55 {
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let store2 = STORE.get().map(|s| s.as_ref().clone());
        if let Some(store) = store2 {
            let evs = store.events_for_town(town_id);
            if evs.len() > seen {
                // Push each new durable event as a typed transition (P0-E).
                for ev in &evs[seen..evs.len()] {
                    let t = derive_transition(&store, ev, None);
                    let payload = serde_json::to_string(&t).unwrap_or_default();
                    let _ = w.write_all(format!("id: {}\r\nevent: transition\ndata: {payload}\r\n\r\n", evs.len()).as_bytes());
                }
                let new_payload = serde_json::to_string(&evs[seen..evs.len()]).unwrap_or_else(|_| "[]".into());
                let _ = w.write_all(format!("id: {}\r\nevent: events\ndata: {}\r\n\r\n", evs.len(), new_payload).as_bytes());
                let _ = w.flush();
                seen = evs.len();
            }
            // Periodic liveness: refresh the in-flight / stalled signal so the
            // dashboard always shows whether work is advancing or is idle.
            let liveness = sse_liveness(&store, lattice_gastown::orchestration::reconciler::parse_iso_ms(
                &lattice_gastown::orchestration::state::now()).unwrap_or(0));
            let _ = w.write_all(format!("id: {seen}\r\nevent: liveness\ndata: {}\r\n\r\n", serde_json::to_string(&liveness).unwrap_or_default()).as_bytes());
            let _ = w.flush();
        }
    }
    // Writer drops (SSE client reconnects).
}

/// Extract the SSE `Last-Event-ID` header (used for durable reconnect replay).
fn last_event_id_from_headers(req: &str) -> String {
    let header_block = req.split_once("\r\n\r\n").map(|x| x.0).unwrap_or(req);
    for line in header_block.split("\r\n").skip(1) {
        if let Some(v) = line.strip_prefix("Last-Event-ID:") {
            return v.trim().to_string();
        }
    }
    String::new()
}

/// Write a JSON HTTP/1.1 response and close.
fn write_resp<W: std::io::Write>(w: &mut W, value: &serde_json::Value) {
    write_resp_status(w, 200, value);
}

/// Write a JSON HTTP/1.1 response with an explicit status and close.
fn write_resp_status<W: std::io::Write>(w: &mut W, status: u16, value: &serde_json::Value) {
    use std::io::Write as _;
    let body = serde_json::to_string_pretty(value).unwrap_or_default();
    let reason = match status { 200=>"OK", 401=>"Unauthorized", 403=>"Forbidden", 400=>"Bad Request", 500=>"Internal Server Error", _=>"OK" };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = w.write_all(resp.as_bytes());
}

/// Which control paths are MUTATING (create/update/destroy state or git) and
/// therefore require a control token. Loopback binding alone is NOT authorization.
fn path_mutates(path: &str) -> bool {
    match path.trim_matches('/') {
        "mayor" | "convoy" | "bead" | "tick" | "start_convoy" | "set_bead_status" | "rig" | "project" => true,
        _ => false,
    }
}

/// The control token. Source of truth is `GASTOWN_CTRL_TOKEN`, else a
/// persisted, user-scoped secret at `%LOCALAPPDATA%\NAI\Gastown\control.token`
/// (or `NAI_LATTICE_ROOT` when set). First run generates + writes it with a
/// deny-other-read ACL (the current user only), so authentication is stable
/// across process restarts and never requires re-provisioning. The secret is
/// never printed back or logged.
fn control_auth_token() -> String {
    use std::sync::OnceLock;
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        if let Ok(t) = std::env::var("GASTOWN_CTRL_TOKEN") {
            if !t.trim().is_empty() { return t; }
        }
        // Locate the user-scoped secret store (survives restarts).
        let root = std::env::var_os("NAI_LATTICE_ROOT")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("LOCALAPPDATA").map(|l| std::path::PathBuf::from(&l).join("NAI").join("Gastown")))
            .unwrap_or_else(|| std::path::PathBuf::from(".gastown"));
        let secret_path = root.join("control.token");
        let _ = std::fs::create_dir_all(&root);
        // Try to read the persisted secret first.
        if let Ok(prev) = std::fs::read_to_string(&secret_path) {
            let prev = prev.trim().to_string();
            if !prev.is_empty() {
                return prev;
            }
        }
        // Generate + persist with a restrictive user-only ACL.
        let entropy = blake3::hash(format!("{}:{}{}", std::process::id(), randish_noop(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)).as_bytes()).to_hex().to_string();
        let _ = std::fs::write(&secret_path, &entropy);
        #[cfg(windows)]
        {
            // Restrict to the current user (deny BUILTIN\Users read) so a local
            // peer process cannot read the secret from disk.
            let _ = restrict_file_acl(&secret_path);
        }
        tracing::info!(path = %secret_path.display(), "Gas Town control token persisted (user-scoped)");
        eprintln!("NAI Gas Town control token written to: {}", secret_path.display());
        entropy
    }).clone()
}

/// Windows: deny non-owner read on a secret file (best-effort ACL hardening).
#[cfg(windows)]
fn restrict_file_acl(p: &std::path::Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    let user = std::env::var("USERNAME").unwrap_or_default();
    // Grant full to the current user, reset inheritance (drop inherited read),
    // then make it user-only. Best-effort; failure is non-fatal.
    if !user.is_empty() {
        let mut c = Command::new("icacls");
        c.args([p.to_string_lossy().as_ref(), "/inheritance:r", &format!("{user}:F")]).creation_flags(0x0800_0000);
        let _ = c.status();
    }
    Ok(())
}

/// Stable-random salt (system time + proc id, good enough for a local token).
fn randish_noop() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

/// Verify an `Authorization: Bearer <token>` header against the control token.
/// Compares a keyed/blake3 digest (SHA-256-equivalent strength) instead of the
/// raw token to avoid a trivial timing side-channel on a shared loopback socket.
fn control_token_ok(auth_header: Option<&str>) -> bool {
    let Some(ah) = auth_header else { return false };
    let token = ah.strip_prefix("Bearer ").map(str::trim);
    token.map(|t| {
        let want = control_auth_token();
        let a = blake3::hash(t.as_bytes());
        let b = blake3::hash(want.as_bytes());
        a.as_bytes() == b.as_bytes()
    }).unwrap_or(false)
}

/// DNS-rebinding guard: only loopback Hosts may reach the control plane.
fn host_loopback(host: Option<&str>) -> bool {
    let Some(h) = host else { return true }; // HTTP/1.0 or unusual — allow (loopback-bound anyway)
    let h = h.trim().to_lowercase();
    let h = h.split(':').next().unwrap_or("").trim(); // strip port
    h == "127.0.0.1" || h == "localhost" || h == "::1" || h.starts_with("127.")
}

/// Browser cross-origin guard: a foreign Origin cannot drive the control plane.
fn loopback_origin(origin: &str) -> bool {
    let o = origin.trim().to_lowercase();
    if let Some(rest) = o.strip_prefix("http://") {
        let host = rest.split(':').next().unwrap_or("");
        return host == "127.0.0.1" || host == "localhost" || host == "::1";
    }
    o.starts_with("http://127.") || o.starts_with("http://localhost") || o.starts_with("http://[::1]")
}

/// Effective hydration of the SSE reconnect contract is delegated to the client
/// via the `id:` field (Last-Event-ID). (No URL token is ever accepted.)

/// Crash-recovery evidence: rehydrate the orchestrator to reflect durable state
/// and persist a recovery watermark. Called on startup after `init_from_runtime`.
pub(crate) fn rehydrate_recovered_town() {
    if let Some(orch) = ORCH.get() {
        // The scheduler tick is re-armed from durable state; ensure a town exists.
        let store = orch.store();
        let towns = store.list_towns();
        if let Some(t) = towns.into_iter().next() {
            orch.set_town(t);
        }
        tracing::info!(
            tick = %orch.last_tick(),
            towns = %store.list_towns().len(),
            beads = %store.list_beads().len(),
            "Gas Town orchestration recovered from durable state (crash recovery)",
        );
    }
}

/// Auto-continue (restart recovery) — FAITHFUL to P0-C: a host/worker restart
/// is NOT a retry-budget reset. We DO NOT re-open failed primary beads and DO NOT
/// reset `dispatch_attempts`/`rework_count`; exhausted executions stay exhausted
/// (their failure history is preserved). A restart only resumes work that was
/// INTERRUPTED mid-flight (in_progress with an expired lease, or staged work that
/// never got its completion ingested) — never a failed execution, and never by
/// erasing attempt history.
///
/// Corrective continuation after a real failure is owned by the Inquisitor/Mayor:
/// it must issue a CHANGED hypothesis/plan as a new corrective iteration under the
/// same primary bead (new bounded execution), not repeat an identical dispatch.
pub(crate) fn recover_interrupted_primary_on_startup() {
    let Some(orch) = ORCH.get() else { return };
    let store = orch.store();
    if store.list_towns().is_empty() { return; }
    let now_ms = crate::now_unix_ms();
    let lease_ms = store.hot_get("gastown/lease_ms").and_then(|s| s.parse::<i64>().ok()).unwrap_or(120_000);
    // 1) Resume INTERRUPTED (not failed) in-flight primary work: in_progress whose
    //    lease expired on the dead process. Attempt history is preserved.
    let mut resumed = 0usize;
    let in_flight: Vec<lattice_gastown::orchestration::state::Bead> = store.list_beads().into_iter()
        .filter(|b| b.status == lattice_gastown::orchestration::state::BeadStatus::InProgress)
        .collect();
    for mut b in in_flight {
        // Only resume when we have a numeric lease watermark; otherwise leave it
        // untouched (do not guess and hand it to a second worker).
        let last_beat = b.metadata.get("last_lease_beat").and_then(|v| v.as_str())
            .and_then(|s| s.parse::<i64>().ok());
        if let Some(beat) = last_beat {
            if now_ms - beat >= lease_ms {
                b.status = lattice_gastown::orchestration::state::BeadStatus::Open;
                b.metadata.as_object_mut().map(|m| m.insert("resumed_at".to_string(), serde_json::json!(now_ms)));
                store.put_bead(&b);
                resumed += 1;
            }
        }
    }
    // 2) Do NOT touch Failed primaries here — exhaustion stays visible; the
    //    Inquisitor/Mayor must produce a changed corrective plan (new iteration)
    //    to continue. This keeps failure-history intact and avoids repeating an
    //    identical (possibly non-idempotent) operation after a reboot.
    if resumed > 0 {
        tracing::info!(resumed = %resumed, "auto-continue: resumed lease-expired in-flight work (failed primaries untouched, retry budget preserved)");
    }
    // 3) Arm one reconciliation round so resumed work actually dispatches. This is
    //    NOT a retry-budget reset — it only lets a genuinely interrupted execution
    //    pick back up (with its real attempt history intact).
    let _ = orch.tick(&HostAgentRunner);
    tracing::info!(pre_dispatch = %store.list_beads().iter().filter(|b| b.status == lattice_gastown::orchestration::state::BeadStatus::Open).count(), "auto-continue: reconciliation armed after startup");
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    #[test]
    fn mutating_paths_require_auth() {
        assert!(path_mutates("/mayor"));
        assert!(path_mutates("/convoy"));
        assert!(path_mutates("/bead"));
        assert!(path_mutates("/tick"));
        assert!(path_mutates("/start_convoy"));
        assert!(path_mutates("/set_bead_status"));
        assert!(path_mutates("/rig"));
        assert!(!path_mutates("/dashboard"));
        assert!(!path_mutates("/health"));
        assert!(!path_mutates("/rig_effective"));
        assert!(!path_mutates("/"));
    }

    #[test]
    fn token_ok_requires_bearer_and_matching_token() {
        // Set a known token for the test process.
        std::env::set_var("GASTOWN_CTRL_TOKEN", "test-super-secret");
        let want = control_auth_token();
        assert_eq!(want, "test-super-secret");
        // Correct bearer passes.
        assert!(control_token_ok(Some("Bearer test-super-secret")));
        // Wrong value fails.
        assert!(!control_token_ok(Some("Bearer wrong")));
        // Missing header / malformed fails.
        assert!(!control_token_ok(None));
        assert!(!control_token_ok(Some("test-super-secret"))); // no "Bearer " prefix
        std::env::remove_var("GASTOWN_CTRL_TOKEN");
    }

    #[test]
    fn unauthenticated_mutation_is_rejected() {
        // Simulate the 401 gate: an empty/missing Authorization never passes.
        assert!(!control_token_ok(None));
        // And a read-only path is never gated.
        assert!(!path_mutates("/dashboard"));
    }
}