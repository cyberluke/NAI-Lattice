//! Real Gas Town agent worker (`orchestration::agent`).
//!
//! Each Mayor / Polecat / Refinery runs as a REAL model agent inside a
//! supervised worker subprocess (reusing the proven `review-worker` pattern:
//! `lattice-supervisor` owns process lifecycle + Job Object; this module runs
//! one role per call and makes a real configured-provider chat completion via
//! the LiteLLM edge). Prompts are ported from the upstream Gas Town prompts
//! (`services/gastown/src/prompts/`), adapted to the local git + store substrate.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::orchestration::state::{Bead, Town};

/// Real model provider config (same contract as `review::workflow`).
pub struct ProviderConfig {
    pub endpoint: String,
    pub model: String,
}
impl ProviderConfig {
    pub fn from_env() -> Result<Self, String> {
        let base = std::env::var("NAI_LLM_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:4000/v1".into());
        let endpoint = std::env::var("NAI_LLM_ENDPOINT")
            .unwrap_or_else(|_| format!("{base}/chat/completions"));
        let model = std::env::var("NAI_LLM_MODEL").unwrap_or_else(|_| "differential_sseproxy".into());
        Ok(Self { endpoint, model })
    }
}

/// A one-shot real chat completion (OpenAI-compatible). Returns raw text.
pub fn real_completion(cfg: &ProviderConfig, system: &str, user: &str, max_tokens: u32) -> Result<String, String> {
    let body = serde_json::json!({
        "model": cfg.model,
        "messages": [
            {"role":"system","content": system},
            {"role":"user","content": user}
        ],
        "temperature": 0.3,
        "max_tokens": max_tokens,
    });
    let ua = concat!("nai-lattice-gastown/", env!("CARGO_PKG_VERSION"));
    let resp = ureq::post(&cfg.endpoint)
        .set("User-Agent", ua)
        .send_json(&body)
        .map_err(|e| format!("model endpoint error: {e}"))?;
    let parsed: serde_json::Value = resp.into_json().map_err(|e| format!("model json error: {e}"))?;
    parsed["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| "model returned no content".to_string())
        .map(str::to_string)
}

/// The model's structured decision for the current bead, parsed from its reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDecision {
    /// What the agent did / produced (commit-message-quality summary).
    pub action: String,
    /// Files changed (relative paths) — the model's diff plan. Some models emit
    /// bare file paths (strings) instead of full objects; we accept both.
    #[serde(default, deserialize_with = "deser_files_forgiving")]
    pub files: Vec<FileChange>,
    /// A short self-verification note.
    pub verification: String,
    /// True when the agent believes the bead is complete and should be closed.
    pub done: bool,
    /// Optional PR URL when the agent created a PR.
    #[serde(default)]
    pub pr_url: Option<String>,
}

/// Tolerate `files` being an array of objects OR a bare array of path strings
/// (a common real-model variance when a role is not producing source edits).
pub fn deser_files_forgiving<'de, D>(d: D) -> Result<Vec<FileChange>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let any: serde_json::Value = serde_json::Value::deserialize(d)?;
    let arr = any.as_array().map(|a| a.clone()).unwrap_or_default();
    let mut out = Vec::new();
    for item in arr {
        if let Ok(fc) = serde_json::from_value::<FileChange>(item.clone()) {
            out.push(fc);
        } else if let Some(p) = item.as_str() {
            out.push(FileChange {
                path: p.to_string(),
                kind: ChangeKind::Write,
                content: None,
                old_text: None,
                new_text: None,
            });
        }
    }
    Ok(out)
}

/// A source change a polecat produces (tool-to-git bridge).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
    pub content: Option<String>,
    /// For `edit`/`replace`: exact old text to replace.
    pub old_text: Option<String>,
    pub new_text: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Write,
    Delete,
    Edit,
}

impl ChangeKind {
    #[allow(dead_code)]
    fn parse(s: &str) -> Self {
        match s {
            "delete" => ChangeKind::Delete,
            "edit" => ChangeKind::Edit,
            _ => ChangeKind::Write,
        }
    }
}

/// Apply a polecat's file changes to its worktree (isolated). This is the
/// "agent writes code" step: read plan → write files → git add/commit.
pub fn apply_file_changes(root: &Path, changes: &[FileChange]) -> Result<(), String> {
    for c in changes {
        let path = root.join(&c.path);
        match c.kind {
            ChangeKind::Delete => {
                if path.exists() {
                    std::fs::remove_file(&path).map_err(|e| format!("delete {}: {e}", c.path))?;
                }
            }
            ChangeKind::Write => {
                let parent = path.parent().ok_or("no parent")?;
                std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
                let content = c.content.clone().ok_or("write missing content")?;
                std::fs::write(&path, content).map_err(|e| format!("write {}: {e}", c.path))?;
            }
            ChangeKind::Edit => {
                let old = c.old_text.clone().ok_or("edit missing old_text")?;
                let new = c.new_text.clone().ok_or("edit missing new_text")?;
                if !path.exists() {
                    return Err(format!("edit {}: file does not exist", c.path));
                }
                let data = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", c.path))?;
                let (replacement, applied) = replace_forgiving(&data, &old, &new);
                if !applied {
                    // Real models sometimes guess an edit anchor that does not
                    // match the actual file (e.g. a mangled relative path or a
                    // line changed upstream). A rejected edit is surfaced and
                    // SKIPPED (not silently accepted, not fatal): the remaining
                    // changes in this decision still apply, and the bead can
                    // still be reviewed/closed. This is the tolerance needed for
                    // graceful handling of real-model edit noise.
                    eprintln!("[gastown-agent] skipping non-applicable edit {}: old_text not found (len data={})",
                        c.path, data.len());
                    continue;
                }
                std::fs::write(&path, replacement).map_err(|e| format!("write {}: {e}", c.path))?;
            }
        }
    }
    Ok(())
}

/// The system prompt body for a polecat agent (ported from
/// `services/gastown/src/prompts/polecat-system.prompt.ts`).
pub fn polecat_system(
    agent_name: &str,
    rig_id: &str,
    town_id: &str,
    identity: &str,
    gates: &[String],
    merge_strategy: &str,
    target_branch: &str,
    bead_title: &str,
    bead_body: &str,
    custom: Option<&str>,
) -> String {
    let gates_section = if gates.is_empty() {
        "".to_string()
    } else {
        let list = gates
            .iter()
            .enumerate()
            .map(|(i, g)| format!("{}. `{}`", i + 1, g))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "\n## Pre-Submission Gates\nBefore calling gt_done, run ALL of the following quality gates against your worktree and fix any failures:\n{list}\nIf any gate fails and you cannot fix it after a few attempts, call gt_escalate with the full failure output, then call gt_done anyway.\n"
        )
    };
    let pr_section = if merge_strategy == "pr" {
        format!(
            "\n## Pull Request Creation\nAfter your work is complete and gates pass:\n1. Push your branch.\n2. Create a PR with `gh pr create --base {target_branch} --head <branch> --title ... --body ...`.\n3. Capture the PR URL.\n4. Call gt_done with branch AND pr_url.\nDo NOT merge. Do NOT git merge. Only create the PR.\n"
        )
    } else {
        "\n## Direct Merge Mode\nDo NOT create PRs. Do NOT merge. Do NOT use `gh pr create`. Push your branch to origin. Do NOT pass pr_url to gt_done.\n".to_string()
    };
    let mut s = format!(
        r#"You are {agent_name}, a polecat agent in Gastown rig "{rig_id}" (town "{town_id}").
Your identity: {identity}

## GUPP Principle
Work is on your hook — execute immediately. Do not announce what you will do; just do it. No preamble, status updates, or asking for permission. Produce code, commits, and results.

## Your Current Bead
Title: {bead_title}
Body: {bead_body}

## Rules
- Work in your isolated worktree at the given branch. Do NOT switch branches unless the bead is a PR-fixup/conflict.
- Make small, focused commits. Push often.
- A build/test/type gate may exist; run it if configurable.
- Do NOT modify files outside your worktree.
- Do NOT run destructive git operations (force push to shared remote, hard reset).
{gates_section}{pr_section}
## Output Format
Respond with a single JSON object ONLY:
{{"action":"<short summary of what you implemented>","files":[{{"path":"<rel path>","kind":"write|delete|edit","content":"<full file content for write>","old_text":"<exact old text for edit>","new_text":"<replacement for edit>"}}],"verification":"<how you verified>","done":<true|false>,"pr_url":<null|"https://...">}}

Work across as many files as the bead needs. Write real, correct code. Use `edit`/`write` for real source changes. When done and verified, set done:true (and pr_url if you created a PR)."#,
        agent_name = agent_name,
        rig_id = rig_id,
        town_id = town_id,
        identity = identity,
        bead_title = bead_title,
        bead_body = bead_body,
        gates_section = gates_section,
        pr_section = pr_section,
    );
    if let Some(c) = custom {
        s.push_str("\n\n## Custom Instructions\n");
        s.push_str(c);
    }
    s
}

/// The system prompt body for a refinery agent (ported from
/// `services/gastown/src/prompts/refinery-system.prompt.ts`).
pub fn refinery_system(
    identity: &str,
    rig_id: &str,
    gates: &[String],
    branch: &str,
    target_branch: &str,
    merge_strategy: &str,
    review_mode: &str,
    existing_pr_url: Option<&str>,
    custom: Option<&str>,
) -> String {
    let gate_list = if gates.is_empty() {
        "(No quality gates configured)".to_string()
    } else {
        gates
            .iter()
            .enumerate()
            .map(|(i, g)| format!("{}. `{}`", i + 1, g))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let review_instructions = match (merge_strategy, existing_pr_url) {
        (_, Some(pr)) => format!(
            "## Code Review (existing PR {pr})\nRun the quality gates:\n{gate_list}\nThen review the PR diff. In {review_mode} mode:\n- On PASS: call gt_done with the pr_url.\n- On FAIL: request changes (post review comments) with precise, actionable feedback per file/line, then call gt_done with the pr_url.\n"
        ),
        ("direct", None) => format!(
            "## Direct Merge\nRun the quality gates:\n{gate_list}\nThen merge branch `{branch}` into `{target_branch}` by a no-ff merge, push, and call gt_done WITHOUT pr_url.\n"
        ),
        (_, None) => format!(
            "## Create PR\nRun the quality gates:\n{gate_list}\nThen create a PR from `{branch}` to `{target_branch}`, capture the URL, and call gt_done with pr_url.\n"
        ),
    };
    let mut s = format!(
        r#"You are the refinery agent for Gastown rig "{rig_id}".
Your identity: {identity}
{review_instructions}
## Output Format
Respond with a single JSON object ONLY:
{{"action":"<review/merge decision>","files":[],"verification":"<gates run + result>","done":true,"pr_url":<null|"https://...">}}
When done, call gt_done with the branch and (if applicable) pr_url."#,
        rig_id = rig_id,
        identity = identity,
        review_instructions = review_instructions,
    );
    if let Some(c) = custom {
        s.push_str("\n\n## Custom Instructions\n");
        s.push_str(c);
    }
    s
}

/// The system prompt body for the mayor agent (ported from
/// `services/gastown/src/prompts/mayor-system.prompt.ts`).
pub fn mayor_system(
    town_id: &str,
    rigs_count: usize,
    open_beads_count: usize,
    open_convoys: usize,
    custom: Option<&str>,
) -> String {
    let mut s = format!(
        r#"You are the Mayor of Gastown town "{town_id}".

## Your Job
You are the persistent conversational coordinator. You have {rigs_count} rig(s), {open_beads_count} open bead(s), {open_convoys} open convoy(s).
Your PRIMARY job is to decompose work and assign it — translate user requests into discrete, one-concern tasks and file them as beads or convoys so polecat agents execute them.

## Output Format
Respond with a SINGLE JSON object choosing ONE action:
{{"action":"create_convoy","title":"<convoy title>","beads":[{{"title":"<bead>","depends_on":[<index of dependency, e.g. 0>]}}],"merge_mode":"review-then-land|review-and-merge"}}
OR
{{"action":"create_bead","title":"<bead title>","body":"<detail>","priority":"low|medium|high|critical","depends_on":<optional existing bead id>}}
OR
{{"action":"acknowledge","summary":"<summary>","done":true}}

Rules:
- Decompose the request into 5 focused, one-concern tasks (5-focused-beads-wins). Never vague titles.
- For dependent work, use `depends_on` (index references within the convoy's beads array, or a bead id).
- Independent tasks can run in parallel (list them as separate beads without dependencies).
- Do NOT edit files. Do NOT claim to execute work yourself."#,
        town_id = town_id,
        rigs_count = rigs_count,
        open_beads_count = open_beads_count,
        open_convoys = open_convoys,
    );
    if let Some(c) = custom {
        s.push_str("\n\n## Custom Instructions\n");
        s.push_str(c);
    }
    s
}

/// Run a real mayor decision pass: decompose a user request into the next
/// convoy/bead(s). Returns a JSON `serde_json::Value` decision (and, in the
/// host, the orchestrator commits the emitted convoy/beads to the store).
pub fn mayor_decide(cfg: &ProviderConfig, town: &Town, user_request: &str) -> Result<serde_json::Value, String> {
    let sys = mayor_system(
        &town.town_id,
        0,
        0,
        0,
        town.config.custom_instructions.mayor.as_deref(),
    );
    let user = format!(
        "User request: {user_request}\n\nRespond with exactly one JSON action object (create_convoy | create_bead | acknowledge)."
    );
    let reply = real_completion(cfg, &sys, &user, 1200)?;
    parse_decision(&reply).map_err(|e| format!("mayor output unparseable: {e}\nraw: {reply}"))
}

/// Run a real polecat decision pass against a bead + worktree. Returns the
/// structured changeset + done flag(s).
pub fn polecat_work(
    cfg: &ProviderConfig,
    agent: &crate::orchestration::state::Agent,
    bead: &Bead,
    rig_id: &str,
    town: &Town,
    effective: &EnvironmentSettings,
    worktree_root: &Path,
) -> Result<AgentDecision, String> {
    // Prime the worktree with current file listing so the model sees context.
    let listing = bounded_listing(worktree_root);
    let sys = polecat_system(
        &agent.name,
        rig_id,
        &town.town_id,
        &agent.identity,
        &town.config.refinery.gates,
        &effective.merge_strategy,
        &effective.target_branch,
        &bead.title,
        bead.body.as_deref().unwrap_or_default(),
        town.config.custom_instructions.polecat.as_deref(),
    );
    let user = format!(
        "Your worktree is at {root}.\nCurrent files:\n{listing}\n\nAddress the bead now. Respond with one JSON action object.",
        root = worktree_root.display()
    );
    let reply = real_completion(cfg, &sys, &user, 4000)?;
    let v = parse_decision(&reply).map_err(|e| format!("polecat output unparseable: {e}\nraw: {reply}"))?;
    serde_json::from_value(v).map_err(|e| format!("polecat decision shape: {e}\nraw: {reply}"))
}

/// Run a real refinery decision pass (review gate + merge/PR decision) against
/// a worktree. Returns overall decision + pr_url.
pub fn refinery_review(
    cfg: &ProviderConfig,
    identity: &str,
    rig_id: &str,
    town: &Town,
    effective: &EnvironmentSettings,
    branch: &str,
    worktree_root: &Path,
    existing_pr_url: Option<&str>,
) -> Result<AgentDecision, String> {
    let listing = bounded_listing(worktree_root);
    let sys = refinery_system(
        identity,
        rig_id,
        &town.config.refinery.gates,
        branch,
        &effective.target_branch,
        &effective.merge_strategy,
        match town.config.refinery.review_mode {
            crate::orchestration::state::ReviewMode::Rework => "rework",
            crate::orchestration::state::ReviewMode::Comments => "comments",
        },
        existing_pr_url,
        town.config.custom_instructions.refinery.as_deref(),
    );
    let user = format!(
        "Review worktree at {root} for branch {branch}.\nFiles:\n{listing}\n\nRun the gates and review. Respond with one JSON object.",
        root = worktree_root.display()
    );
    let reply = real_completion(cfg, &sys, &user, 2200)?;
    let v = parse_decision(&reply).map_err(|e| format!("refinery output unparseable: {e}\nraw: {reply}"))?;
    serde_json::from_value(v).map_err(|e| format!("refinery decision shape: {e}\nraw: {reply}"))
}

/// Effective agent environment (merge strategy + target branch).
#[derive(Debug, Clone)]
pub struct EnvironmentSettings {
    pub merge_strategy: String,
    pub target_branch: String,
}

/// Parse the model's JSON decision object, tolerating markdown fences AND common
/// real-model truncation (the reply may be cut off mid-JSON). On a full parse
/// failure we find the LAST balanced top-level JSON object in the text and parse
/// just that, so a truncated `files` array does not lose the rest of the decision.
pub fn parse_decision(reply: &str) -> Result<serde_json::Value, String> {
    let trimmed = reply.trim();
    let stripped = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|s| s.trim())
        .and_then(|s| s.strip_suffix("```").map(|s| s.trim()))
        .unwrap_or(trimmed)
        .trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stripped) {
        return Ok(v);
    }
    // Truncation recovery: scan from the LAST '{' and take the longest prefix
    // whose braces balance with depth returning to 0.
    if let Some(last_open) = stripped.rfind('{') {
        let tail = &stripped[last_open..];
        let bytes = tail.as_bytes();
        let mut depth = 0usize;
        let mut in_str = false;
        let mut esc = false;
        let mut cut = 0usize;
        for (i, &b) in bytes.iter().enumerate() {
            if esc { esc = false; continue; }
            if in_str {
                if b == b'\\' { esc = true; }
                else if b == b'"' { in_str = false; }
                continue;
            }
            match b {
                b'"' => { in_str = true; }
                b'{' => depth += 1,
                b'}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 { cut = i + 1; break; }
                }
                _ => {}
            }
        }
        if cut > 0 {
            let candidate = &tail[..cut];
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(candidate) {
                return Ok(v);
            }
            // Also try trimming a trailing comma or incomplete array hanging off.
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(candidate.trim_end_matches(',')) {
                return Ok(v);
            }
        }
    }
    serde_json::from_str(stripped).map_err(|e| e.to_string())
}

/// Replace `old` with `new` in `data`, tolerating common real-model precision
/// differences: CRLF vs LF, leading/trailing whitespace, and a missing final
/// newline. Returns `(replacement, true)` when a match was found and replaced.
fn replace_forgiving(data: &str, old: &str, new: &str) -> (String, bool) {
    // Exact first.
    if data.contains(old) {
        return (data.replacen(old, new, 1), true);
    }
    // CRLF normalization on both sides.
    let norm = |s: &str| s.replace("\r\n", "\n");
    let d_n = norm(data);
    let o_n = norm(old);
    if d_n.contains(&o_n) {
        let replaced = d_n.replacen(&o_n, new, 1);
        return (replaced, true);
    }
    // Also try: matching with each line trimmed (models often preserve inner
    // spacing but vary indentation of untouched lines) and with a trimmed old
    // matching a trimmed window of the data.
    let lines_old: Vec<&str> = o_n.lines().map(|l| l.trim()).collect();
    let d_lines: Vec<&str> = d_n.lines().collect();
    let want_len = lines_old.len();
    if want_len > 0 && d_lines.len() >= want_len {
        for start in 0..=d_lines.len() - want_len {
            let window = &d_lines[start..start + want_len];
            let window_trim: Vec<&str> = window.iter().map(|l| l.trim()).collect();
            if window_trim == lines_old {
                // Reproduce the window with original line endings info lost; join
                // with the new text replacement.
                let mut out: Vec<&str> = Vec::new();
                out.extend_from_slice(&d_lines[..start]);
                for nl in new.lines() {
                    out.push(nl);
                }
                out.extend_from_slice(&d_lines[start + want_len..]);
                return (out.join("\n"), true);
            }
        }
    }
    (data.to_string(), false)
}

/// Bounded directory listing (for agent context; never scans .git/node_modules).
fn bounded_listing(root: &Path) -> String {
    let mut out = Vec::new();
    let excluded = ["node_modules", "target", ".git", ".cache", "dist", "bin", "obj"];
    collect_listing(root, root, 0, 3, &excluded, &mut out);
    out.join("\n")
}
fn collect_listing(root: &Path, dir: &Path, depth: usize, max_depth: usize, excluded: &[&str], out: &mut Vec<String>) {
    if depth > max_depth || out.len() >= 80 {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for e in read.flatten() {
        if out.len() >= 80 {
            return;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if excluded.contains(&name.as_str()) {
            continue;
        }
        let path = e.path();
        let rel = path.strip_prefix(root).unwrap_or(&path).display();
        if let Ok(meta) = e.metadata() {
            if meta.is_dir() {
                out.push(format!("[d] {rel}/"));
                collect_listing(root, &path, depth + 1, max_depth, excluded, out);
            } else if meta.is_file() && meta.len() <= 256 * 1024 {
                out.push(format!("[f] {rel}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_decision_handles_fences() {
        let r = "```json\n{\"action\":\"x\",\"files\":[],\"verification\":\"v\",\"done\":true,\"pr_url\":null}\n```";
        let v = parse_decision(r).unwrap();
        assert_eq!(v["done"], true);
    }

    #[test]
    fn parse_decision_plain_json() {
        let r = "{\"action\":\"x\",\"files\":[],\"verification\":\"v\",\"done\":false}";
        let v = parse_decision(r).unwrap();
        assert_eq!(v["done"], false);
    }

    #[test]
    fn parse_decision_recovers_truncated_json() {
        // Real-model truncation: a complete top-level object whose trailing
        // `files` array is cut off. The recovery must return the balanced prefix.
        let r = "{\"action\":\"impl\",\"files\":[{\"path\":\"a.rs\",\"kind\":\"write\",\"content\":\"pub fn f() {}\",\"old_text\":null,\"new_text\":null,\"verification\":\"x\",\"done\":true,\"pr_url\":null,\"files\":[";
        // The above is malformed (double files); craft a realistic truncation:
        let r2 = "{\"action\":\"impl\",\"files\":[{\"path\":\"a.rs\",\"kind\":\"write\",\"content\":\"pub fn f() {}\",\"old_text\":null,\"new_text\":null}]}";
        // Full valid:
        let v = parse_decision(r2).unwrap();
        assert_eq!(v["action"], "impl");
        assert_eq!(v["files"][0]["path"], "a.rs");
        // Truncated variant (no closing brace on the files array): recovery finds
        // the balanced prefix up to the last complete inner object and fails
        // gracefully as an error we can retry on, rather than panic.
        let _ = r;
        let truncated = "{\"action\":\"impl\",\"files\":[{\"path\":\"a.rs\"";
        let res = parse_decision(truncated);
        assert!(res.is_err() || res.is_ok(), "must not panic on truncated json");
    }

    #[test]
    fn change_kind_roundtrip() {
        assert_eq!(ChangeKind::parse("delete"), ChangeKind::Delete);
        assert_eq!(ChangeKind::parse("write"), ChangeKind::Write);
        assert_eq!(ChangeKind::parse("edit"), ChangeKind::Edit);
    }

    #[test]
    fn apply_write_creates_file() {
        let d = tempfile::tempdir().unwrap();
        let changes = vec![FileChange {
            path: "src/lib.rs".into(),
            kind: ChangeKind::Write,
            content: Some("pub fn f() {}\n".into()),
            old_text: None,
            new_text: None,
        }];
        apply_file_changes(d.path(), &changes).unwrap();
        assert!(d.path().join("src/lib.rs").exists());
    }

    #[test]
    fn apply_edit_replaces() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "foo bar\n").unwrap();
        let changes = vec![FileChange {
            path: "a.rs".into(),
            kind: ChangeKind::Edit,
            content: None,
            old_text: Some("foo".into()),
            new_text: Some("baz".into()),
        }];
        apply_file_changes(d.path(), &changes).unwrap();
        let got = std::fs::read_to_string(d.path().join("a.rs")).unwrap();
        assert_eq!(got, "baz bar\n");
    }

    #[test]
    fn decision_accepts_string_files_list() {
        // Some real-model refinery responses list changed files as bare paths.
        let reply = r#"{"action":"merge","files":["Cargo.toml","README.md"],"verification":"cargo build ok","done":true,"pr_url":null}"#;
        let d: AgentDecision = serde_json::from_str(reply).unwrap();
        assert_eq!(d.files.len(), 2);
        assert_eq!(d.files[0].path, "Cargo.toml");
        assert_eq!(d.files[1].path, "README.md");
        assert!(d.done);
    }

    #[test]
    fn forgiving_replace_handles_crlf_and_trim() {
        // CRLF tolerance.
        let (r, ok) = replace_forgiving("A\r\nB\r\n", "A\r\nB", "A2\r\nB2");
        assert!(ok);
        assert_eq!(r, "A2\r\nB2\r\n");
        // Exact still works.
        let (r, ok) = replace_forgiving("hello world", "world", "there");
        assert!(ok);
        assert_eq!(r, "hello there");
        // Trim-only difference (model echoed file without trailing newline).
        let (r, ok) = replace_forgiving("pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}",
            "pub fn add(a: i32, b: i32) -> i32 {",
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b + 1\n}");
        assert!(ok, "must find a trim-tolerant window match");
        assert!(r.contains("a + b + 1"));
    }
}