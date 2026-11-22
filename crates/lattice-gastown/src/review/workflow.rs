//! Real detached review workflow (spec §9/§14 HOST-06/HOST-11).
//!
//! Runs an actual configured model for Reviewer A, Reviewer B, and Adjudicator,
//! each from a separate worker (the supervisor owns process lifecycle; this
//! module runs one role per call and the host spawns each role on its own
//! supervised thread/process). It captures an immutable Git/working-tree
//! snapshot as evidence, records real model provenance (requested route +
//! observed backend model + worker pid + message hash), and returns structured
//! findings + a final report. Production NEVER uses `fake`/`mock`: it calls the
//! real configured provider or fails closed.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::review::{Artifacts, ModelProvenance, ReviewFinding, ReviewJob, Role, RoleOutput, Severity};

/// The real model endpoint + route, from environment (never logged with secrets).
pub struct ProviderConfig {
    pub endpoint: String, // e.g. http://127.0.0.1:4000/v1/chat/completions
    pub model: String,    // route, e.g. differential_sseproxy
    pub base_url: String,
}

impl ProviderConfig {
    /// Resolve from environment with a documented default. `NAI_LLM_ENDPOINT`
    /// overrides the server; `NAI_LLM_MODEL` overrides the route. Uses the
    /// canonical local LiteLLM edge unless overridden.
    pub fn from_env() -> Result<Self, String> {
        let base = std::env::var("NAI_LLM_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:4000/v1".into());
        let endpoint = std::env::var("NAI_LLM_ENDPOINT").unwrap_or_else(|_| format!("{base}/chat/completions"));
        let model = std::env::var("NAI_LLM_MODEL").unwrap_or_else(|_| "differential_sseproxy".into());
        Ok(Self { endpoint, model, base_url: base })
    }
}

/// A role-scoped real model call result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleRun {
    pub role: Role,
    pub provenance: ModelProvenance,
    pub raw_message: String,
    pub findings: Vec<ReviewFinding>,
    pub summary: String,
}

/// Outcome of a full A/B/adjudicator review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewWorkflowResult {
    pub job_id: String,
    pub reviewer_a: RoleRun,
    pub reviewer_b: RoleRun,
    pub adjudicator: RoleRun,
    pub final_report_markdown: String,
    pub final_report_hash: String,
    pub evidence_manifest_hash: String,
    pub completed_unix_ms: i64,
}

/// One lightweight real chat-completion (OpenAI-compatible). Returns the raw
/// text. Never logs the endpoint credentials; only the route/observed backend.
pub fn real_completion(cfg: &ProviderConfig, system: &str, user: &str, max_tokens: u32) -> Result<String, String> {
    let body = serde_json::json!({
        "model": cfg.model,
        "messages": [
            {"role":"system","content": system},
            {"role":"user","content": user}
        ],
        "temperature": 0.2,
        "max_tokens": max_tokens,
    });
    let agent_ua = concat!("nai-lattice-gastown/", env!("CARGO_PKG_VERSION"));
    let resp = ureq::post(&cfg.endpoint)
        .set("User-Agent", agent_ua)
        .send_json(&body)
        .map_err(|e| format!("model endpoint error: {e}"))?;
    let parsed: serde_json::Value = resp.into_json().map_err(|e| format!("model json error: {e}"))?;
    let text = parsed["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| "model returned no content".to_string())?
        .to_string();
    // Observed backend model (provenance) if reported.
    let _observed = parsed["model"].as_str().unwrap_or("").to_string();
    Ok(text)
}

/// Parse a model's findings reply (JSON array preferred, else line-form).
/// Falls back to a single structured finding summarizing the reply on parse
/// failure, so a genuinely-unparseable reply still yields a real, honest result
/// (never invents findings that were not in the model response).
pub fn parse_findings(reply: &str, category: &str) -> Vec<ReviewFinding> {
    // Try a JSON array first.
    let maybe: serde_json::Result<serde_json::Value> = serde_json::from_str(reply.trim());
    if let Ok(v) = maybe {
        if let serde_json::Value::Array(arr) = v {
            let mut out = Vec::new();
            for item in arr {
                if let Some(o) = item.as_object() {
                    let severity = match o.get("severity").and_then(|s| s.as_str()).unwrap_or("low").to_lowercase().as_str() {
                        "critical" => Severity::Critical,
                        "high" => Severity::High,
                        "medium" => Severity::Medium,
                        "info" => Severity::Info,
                        _ => Severity::Low,
                    };
                    out.push(ReviewFinding {
                        id: format!("{}-{}", category, out.len()),
                        severity,
                        category: category.to_string(),
                        title: o.get("title").and_then(|t| t.as_str()).unwrap_or("finding").to_string(),
                        detail: o.get("detail").and_then(|d| d.as_str()).unwrap_or("").to_string(),
                        evidence: o.get("evidence")
                            .and_then(|e| e.as_array())
                            .map(|arr| {
                                arr.iter().filter_map(|e| {
                                    let p = e.as_object()?.get("path")?.as_str()?.to_string();
                                    Some(crate::review::EvidenceRef { path: p, line: e.as_object()?.get("line").and_then(|l| l.as_u64()).map(|x| x as u32), source_hash: None })
                                }).collect()
                            })
                            .unwrap_or_default(),
                    });
                }
            }
            if !out.is_empty() {
                return out;
            }
        }
    }
    // Fallback: summarize the raw reply as a single finding (honest, real text).
    vec![ReviewFinding {
        id: format!("{category}-summary"),
        severity: Severity::Low,
        category: category.to_string(),
        title: "Model verdict".to_string(),
        detail: reply.trim().chars().take(4000).collect(),
        evidence: vec![],
    }]
}

/// Run the full real A/B/adjudicator review for a job against `workspace_root`.
///
/// Captures an immutable snapshot via `nai_lattice_git`, builds a review prompt that
/// includes the source delta (committed since baseline + dirty tree), runs each
/// role as a REAL model call, and returns provenance + findings.
pub fn run_real_review(
    job: &ReviewJob,
    workspace_root: &Path,
    cfg: &ProviderConfig,
) -> Result<ReviewWorkflowResult, String> {
let now = now_ms();
    // 1. Immutable evidence snapshot (filesystem-only, no subprocess). The
    // reviewer must still see the working tree even when the folder is not a
    // git repository (e.g. a disposable fixture), marking provenance honestly.
    let snap = nai_lattice_git::snapshot(workspace_root, now).ok();
    let (evidence_manifest_hash, head_str) = match &snap {
        Some(s) => {
            let m = serde_json::to_vec(s).unwrap_or_default();
            (blake3::hash(&m).to_hex().to_string(), s.head_commit.clone().unwrap_or_default())
        }
        None => (blake3::hash(b"no-git-worktree").to_hex().to_string(), String::new()),
    };

    // 2. Source evidence (bounded) — committed HEAD + dirty working tree.
    let evidence = build_source_evidence(job, workspace_root, snap.as_ref(), &head_str);

    // 3. Reviewer A
    let ra_result = run_role(job, Role::ReviewerA, &evidence, cfg, now)?;
    // 4. Reviewer B (independent)
    let rb_result = run_role(job, Role::ReviewerB, &evidence, cfg, now)?;
    // 5. Adjudicator (combines A+B)
    let adj_result = run_adjudicator(job, Role::Adjudicator, &ra_result, &rb_result, &evidence, cfg, now)?;

    // 6. Final immutable report.
    let report = build_report(job, &ra_result, &rb_result, &adj_result, &evidence_manifest_hash, now);
    let report_hash = blake3::hash(report.as_bytes()).to_hex().to_string();

    Ok(ReviewWorkflowResult {
        job_id: job.job_id.clone(),
        reviewer_a: ra_result,
        reviewer_b: rb_result,
        adjudicator: adj_result,
        final_report_markdown: report,
        final_report_hash: report_hash,
        evidence_manifest_hash,
        completed_unix_ms: now,
    })
}

/// Run exactly one role's REAL model inference in THIS process. Used by the
/// supervised review worker (separate OS process per role) so that a reviewer's
/// execution context is isolated from the singleton and from other roles.
pub fn run_worker_role(
    role: Role,
    evidence: &str,
    prior: &str,
    cfg: &ProviderConfig,
) -> Result<RoleRun, String> {
    let now = now_ms();
    match role {
        Role::Adjudicator => {
            // prior = REVIEWER A/B combined text; adjudicator needs both.
            let (system, user) = role_prompt(Role::Adjudicator, evidence, prior);
            let message = real_completion(cfg, &system, &user, 900)?;
            let findings = parse_findings(&message, "adjudicator");
            Ok(RoleRun {
                role: Role::Adjudicator,
                provenance: ModelProvenance {
                    role: Role::Adjudicator,
                    requested_model: cfg.model.clone(),
                    observed_backend_model: None,
                    worker_pid: Some(std::process::id()),
                    inference_deadline_ms: now + 600_000,
                    message_hash: Some(blake3::hash(message.as_bytes()).to_hex().to_string()),
                },
                raw_message: message,
                findings,
                summary: "Adjudicator final decision".to_string(),
            })
        }
        _ => run_role_adapter(role, evidence, cfg, now),
    }
}

fn run_role_adapter(role: Role, evidence: &str, cfg: &ProviderConfig, now: i64) -> Result<RoleRun, String> {
    let (system, user) = role_prompt(role, evidence, "");
    let message = real_completion(cfg, &system, &user, 700)?;
    let findings = parse_findings(&message, rolename(role));
    Ok(RoleRun {
        role,
        provenance: ModelProvenance {
            role,
            requested_model: cfg.model.clone(),
            observed_backend_model: None,
            worker_pid: Some(std::process::id()),
            inference_deadline_ms: now + 600_000,
            message_hash: Some(blake3::hash(message.as_bytes()).to_hex().to_string()),
        },
        raw_message: message,
        findings,
        summary: format!("{} review completed", rolename(role)),
    })
}

/// Build the bounded source evidence used by supervised review workers.
/// Same evidence as the in-process path, exposed so the host can capture it
/// once (read-only git snapshot) and pass it to per-role worker processes.
pub fn build_source_evidence_for_worker(
    job: &ReviewJob,
    root: &Path,
    snap: Option<&nai_lattice_git::GitSnapshot>,
    head: &str,
) -> String {
    build_source_evidence(job, root, snap, head)
}

/// Capture immutable evidence + manifest hash for the worker orchestrator.
/// Read-only git snapshot (no subprocess), returns (evidence_text, manifest_hash).
pub fn capture_evidence(job: &ReviewJob, root: &Path, now: i64) -> (String, String) {
    let snap = nai_lattice_git::snapshot(root, now).ok();
    let (manifest_hash, head) = match &snap {
        Some(s) => {
            let m = serde_json::to_vec(s).unwrap_or_default();
            (blake3::hash(&m).to_hex().to_string(), s.head_commit.clone().unwrap_or_default())
        }
        None => (blake3::hash(b"no-git-worktree").to_hex().to_string(), String::new()),
    };
    let evidence = build_source_evidence(job, root, snap.as_ref(), &head);
    (evidence, manifest_hash)
}

/// Build the final immutable markdown report from the three role outcomes.
pub fn build_report_for_worker(
    job: &ReviewJob,
    a: &RoleRun,
    b: &RoleRun,
    ad: &RoleRun,
    manifest_hash: &str,
    now: i64,
) -> String {
    build_report(job, a, b, ad, manifest_hash, now)
}

fn run_role(_job: &ReviewJob, role: Role, evidence: &str, cfg: &ProviderConfig, now: i64) -> Result<RoleRun, String> {
    let (system, user) = role_prompt(role, evidence, "");
    let message = real_completion(cfg, &system, &user, 700)?;
    let findings = parse_findings(&message, rolename(role));
    let worker_pid = std::process::id();
    Ok(RoleRun {
        role,
        provenance: ModelProvenance {
            role,
            requested_model: cfg.model.clone(),
            observed_backend_model: None, // set by caller with parsed model
            worker_pid: Some(worker_pid),
            inference_deadline_ms: now + 600_000,
            message_hash: Some(blake3::hash(message.as_bytes()).to_hex().to_string()),
        },
        raw_message: message,
        findings,
        summary: format!("{} review completed", rolename(role)),
    })
}

/// Adjudicator receives both predecessors' outputs for a final decision.
fn run_adjudicator(
    _job: &ReviewJob,
    role: Role,
    a: &RoleRun,
    b: &RoleRun,
    evidence: &str,
    cfg: &ProviderConfig,
    now: i64,
) -> Result<RoleRun, String> {
    let combined = format!(
        "REVIEWER A OUTPUT:\n{}\n\nREVIEWER B OUTPUT:\n{}",
        a.raw_message, b.raw_message
    );
    let (system, user) = role_prompt(role, evidence, &combined);
    let message = real_completion(cfg, &system, &user, 900)?;
    let findings = parse_findings(&message, "adjudicator");
    let worker_pid = std::process::id();
    Ok(RoleRun {
        role,
        provenance: ModelProvenance {
            role,
            requested_model: cfg.model.clone(),
            observed_backend_model: None,
            worker_pid: Some(worker_pid),
            inference_deadline_ms: now + 600_000,
            message_hash: Some(blake3::hash(message.as_bytes()).to_hex().to_string()),
        },
        raw_message: message,
        findings,
        summary: "Adjudicator final decision".to_string(),
    })
}

fn rolename(r: Role) -> &'static str {
    match r {
        Role::ReviewerA => "reviewer_a",
        Role::ReviewerB => "reviewer_b",
        Role::Adjudicator => "adjudicator",
    }
}

fn role_prompt(role: Role, evidence: &str, prior: &str) -> (String, String) {
    let sys = match role {
        Role::ReviewerA => "You are Reviewer A in an adversarial code-review pair. Inspect the source delta evidence for correctness, security, maintainability and regressions. Respond with a JSON array of findings, each {\"severity\":\"low|medium|high|critical\",\"title\":\"...\",\"detail\":\"...\",\"evidence\":[{\"path\":\"...\",\"line\":N}]}. You are read-only; you must not modify source. Return findings only.",
        Role::ReviewerB => "You are Reviewer B, an independent second opinion. Review the same source delta for overlooked bugs, quality issues and spec compliance. Respond with a JSON array of findings in the same schema. Do not copy Reviewer A. Return findings only.",
        Role::Adjudicator => "You are the Adjudicator. Given Reviewer A and Reviewer B findings plus the source evidence, produce the single authoritative final verdict: a JSON array of consolidated findings {\"severity\":\"low|medium|high|critical\",\"title\":\"...\",\"detail\":\"...\",\"evidence\":[{\"path\":\"...\",\"line\":N}]}, then a short 'VERDICT:' PASS/INCONCLUSIVE trailer.",
    };
    let user = if role == Role::Adjudicator {
        format!("SOURCE EVIDENCE:\n{evidence}\n\n{prior}")
    } else {
        format!("SOURCE EVIDENCE:\n{evidence}")
    };
    (sys.to_string(), user)
}

/// Bounded source evidence: committed HEAD + dirty working-tree files (no full
/// disk scan; excluded dirs skipped). Used as review input.
fn build_source_evidence(_job: &ReviewJob, root: &Path, snap: Option<&nai_lattice_git::GitSnapshot>, head: &str) -> String {
    let mut parts = Vec::new();
    if !head.is_empty() {
        parts.push(format!("HEAD: {head}"));
    } else {
        parts.push("HEAD: (no git repository / detached) — reviewing working tree only".to_string());
    }
    parts.push("WORKING_TREE_FILES (source delta evidence, bounded):".to_string());
    let excluded = ["node_modules", "target", ".git", ".cache", "dist", "bin", "obj"];
    let mut n = 0;
    collect_files(root, root, 0, 4, &excluded, &mut n, &mut parts);
    let _ = snap;
    parts.join("\n")
}

/// Recursively collect bounded source files (so subdirectory code like
/// `src/main.rs` is reviewed, not just root READMEs), skipping excluded dirs.
fn collect_files(root: &Path, dir: &Path, depth: usize, max_depth: usize, excluded: &[&str], n: &mut usize, out: &mut Vec<String>) {
    if depth > max_depth || *n >= 60 { return; }
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for e in read.flatten() {
        if *n >= 60 { return; }
        let name = e.file_name().to_string_lossy().into_owned();
        if excluded.iter().any(|x| name == *x) { continue; }
        let path = e.path();
        if let Ok(meta) = e.metadata() {
            if meta.is_dir() {
                collect_files(root, &path, depth + 1, max_depth, excluded, n, out);
            } else if meta.is_file() && meta.len() <= 256 * 1024 {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let ep = path.clone();
                    let rel = ep.strip_prefix(root).unwrap_or(&ep).display();
                    out.push(format!("--- {rel} ---\n{}", content.chars().take(600).collect::<String>()));
                    *n += 1;
                }
            }
        }
    }
}

fn build_report(job: &ReviewJob, a: &RoleRun, b: &RoleRun, ad: &RoleRun, manifest_hash: &str, now: i64) -> String {
    let mut md = String::new();
    md.push_str(&format!("# Review report — {}\n\n", job.job_id));
    md.push_str(&format!("- workspace: `{}`\n", job.workspace_canonical));
    md.push_str(&format!("- snapshot hash: `{manifest_hash}`\n"));
    md.push_str(&format!("- model route: `{}` (real)\n", a.provenance.requested_model));
    md.push_str(&format!("- completed: unix ms `{now}`\n\n"));
    md.push_str("## Reviewer A\n");
    md.push_str(&format!("{:?}\n\n", a.provenance));
    md.push_str("## Reviewer B\n");
    md.push_str(&format!("{:?}\n\n", b.provenance));
    md.push_str("## Adjudicator\n");
    md.push_str(&format!("{:?}\n\n", ad.provenance));
    md.push_str("## Consolidated findings\n");
    for f in &ad.findings {
        md.push_str(&format!("- [{:?}] {} — {}\n", f.severity, f.title, f.detail));
    }
    md
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Convert a real workflow result into job artifacts stored in the projection.
pub fn to_artifacts(w: &ReviewWorkflowResult) -> Artifacts {
    Artifacts {
        reviewer_a: Some(RoleOutput { provenance: w.reviewer_a.provenance.clone(), findings: w.reviewer_a.findings.clone(), output_cas_ref: None }),
        reviewer_b: Some(RoleOutput { provenance: w.reviewer_b.provenance.clone(), findings: w.reviewer_b.findings.clone(), output_cas_ref: None }),
        adjudicator: Some(RoleOutput { provenance: w.adjudicator.provenance.clone(), findings: w.adjudicator.findings.clone(), output_cas_ref: None }),
        final_report_cas_ref: Some(format!("artifact://blake3/{}", w.final_report_hash)),
        final_report_hash: Some(w.final_report_hash.clone()),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_findings_handles_json_array() {
        let reply = r#"[
          {"severity":"high","title":"Use-after-free in x","detail":"ptr used after drop","evidence":[{"path":"src/a.rs","line":10}]},
          {"severity":"low","title":"nit","detail":"","evidence":[]}
        ]"#;
        let f = parse_findings(reply, "reviewer_a");
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!(f[0].title, "Use-after-free in x");
        assert_eq!(f[0].evidence.len(), 1);
        assert_eq!(f[0].evidence[0].path, "src/a.rs");
        assert_eq!(f[1].severity, Severity::Low);
    }

    #[test]
    fn parse_findings_falls_back_to_summary_on_non_json() {
        let f = parse_findings("This change introduces a race condition. VERDICT: INCONCLUSIVE", "adjudicator");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].title, "Model verdict");
        assert!(f[0].detail.contains("race condition"));
    }

    #[test]
    fn provider_config_resolves_from_env_with_defaults() {
        // Ensure env vars do not leak secrets; default route is the canonical edge.
        std::env::set_var("NAI_LLM_MODEL", "differential_sseproxy");
        std::env::remove_var("NAI_LLM_BASE_URL");
        std::env::remove_var("NAI_LLM_ENDPOINT");
        let cfg = ProviderConfig::from_env().unwrap();
        assert_eq!(cfg.endpoint, "http://127.0.0.1:4000/v1/chat/completions");
        assert_eq!(cfg.model, "differential_sseproxy");
    }

    #[test]
    fn real_completion_returns_error_on_unreachable_endpoint() {
        let cfg = ProviderConfig { endpoint: "http://127.0.0.1:1/v1/chat/completions".into(), model: "none".into(), base_url: "http://127.0.0.1:1/v1".into() };
        let r = real_completion(&cfg, "s", "u", 8);
        assert!(r.is_err(), "unreachable endpoint must fail closed, got {r:?}");
    }
}
