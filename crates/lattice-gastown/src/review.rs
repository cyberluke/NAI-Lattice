//! Review pipeline domain: `ReviewJob`, findings, provenance, adjudication
//! contracts, coverage (spec §9, §14). Storage is the canonical ledger/CAS; the
//! scheduler + worker supervision come from shared NAI-Lattice crates.

pub mod workflow;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Healthy severity scale.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity { Info, Low, Medium, High, Critical }

/// A single review finding with provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub id: String,
    pub severity: Severity,
    pub category: String,
    pub title: String,
    pub detail: String,
    /// Exact file/line evidence from the immutable snapshot.
    pub evidence: Vec<EvidenceRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub path: String,
    pub line: Option<u32>,
    pub source_hash: Option<String>,
}

/// Role identity for provenance (never a brittle substring match).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role { ReviewerA, ReviewerB, Adjudicator }

/// Provenance of one model run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProvenance {
    pub role: Role,
    /// Requested model from explicit policy.
    pub requested_model: String,
    /// Observed backend model where available (may differ from requested).
    pub observed_backend_model: Option<String>,
    pub worker_pid: Option<u32>,
    pub inference_deadline_ms: i64,
    /// BLAKE3 of the raw model message when kept in a scoped snapshot.
    pub message_hash: Option<String>,
}

/// Job state (all transitions persisted in the canonical ledger).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum JobState {
    Queued,
    ReviewerA,
    ReviewerB,
    Adjudicating,
    Completed,
    Failed,
    Canceled,
}

/// The server-owned review job. Idempotency identity (spec §8):
/// `(workspace canonical, task epoch, source manifest hash, review policy version)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewJob {
    pub job_id: String,
    pub workspace_canonical: String,
    pub epoch: Option<crate::TaskEpochId>,
    pub source_manifest_hash: String,
    pub review_policy_version: u32,
    pub state: JobState,
    pub created_unix_ms: i64,
    pub artifacts: Artifacts,
    pub attempts: u32,
}

/// Immutable artifacts produced per job.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Artifacts {
    pub reviewer_a: Option<RoleOutput>,
    pub reviewer_b: Option<RoleOutput>,
    pub adjudicator: Option<RoleOutput>,
    /// Final immutable report CAS ref.
    pub final_report_cas_ref: Option<String>,
    pub final_report_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleOutput {
    pub provenance: ModelProvenance,
    pub findings: Vec<ReviewFinding>,
    /// CAS ref for the immutable message output when large.
    pub output_cas_ref: Option<String>,
}

/// Review policy — a single documented source for deadlines/resources.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewPolicy {
    pub version: u32,
    /// Quiet period before a task is review-eligible.
    pub quiet_period_ms: i64,
    pub snapshot_timeout_ms: i64,
    pub per_worker_inference_deadline_ms: i64,
    pub termination_grace_ms: i64,
    pub max_queue_size: usize,
    pub per_model_concurrency: usize,
    pub max_retries: u32,
}

impl Default for ReviewPolicy {
    fn default() -> Self {
        Self {
            version: 1,
            quiet_period_ms: 5_000,
            snapshot_timeout_ms: 30_000,
            per_worker_inference_deadline_ms: 600_000,
            termination_grace_ms: 5_000,
            max_queue_size: 512,
            per_model_concurrency: 2,
            max_retries: 3,
        }
    }
}

impl ReviewJob {
    /// Construct a review job from a NAF `gastown.review.enqueue_explicit`
    /// args object. Missing required fields return an error string.
    pub fn from_json(args: &serde_json::Value) -> Result<Self, String> {
        let job_id = args.get("job_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if job_id.is_empty() {
            return Err("job_id required".into());
        }
        let workspace = args.get("workspace_canonical").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let manifest = args.get("source_manifest_hash").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let epoch = args.get("epoch").and_then(|v| v.as_object()).map(|o| crate::TaskEpochId {
            host_runtime_id: o.get("host_runtime_id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            session_id: o.get("session_id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            turn_id: o.get("turn_id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        });
        Ok(ReviewJob {
            job_id,
            workspace_canonical: workspace,
            epoch,
            source_manifest_hash: manifest,
            review_policy_version: args.get("review_policy_version").and_then(|v| v.as_u64()).unwrap_or(1) as u32,
            state: JobState::Queued,
            created_unix_ms: args.get("created_unix_ms").and_then(|v| v.as_i64()).unwrap_or_default(),
            artifacts: Artifacts::default(),
            attempts: 0,
        })
    }

    /// The idempotency identity key (spec §8).
    pub fn idempotency_key(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.workspace_canonical,
            self.epoch.as_ref().map(|e| format!("{}:{}:{}", e.host_runtime_id, e.session_id, e.turn_id)).unwrap_or_default(),
            self.source_manifest_hash,
            self.review_policy_version,
        )
    }

    /// Coverage computed from the union of reviewer findings per source file.
    pub fn coverage(files: &[String], findings: &[ReviewFinding]) -> HashMap<String, u64> {
        let mut m: HashMap<String, u64> = files.iter().map(|f| (f.clone(), 0)).collect();
        for f in findings {
            for ev in &f.evidence {
                let n = m.entry(ev.path.clone()).or_insert(0);
                *n += 1;
            }
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_key_stable_and_distinct_per_epoch() {
        let a = ReviewJob {
            job_id: "j1".into(),
            workspace_canonical: "W".into(),
            epoch: Some(crate::TaskEpochId { host_runtime_id: "h".into(), session_id: "s".into(), turn_id: "t1".into() }),
            source_manifest_hash: "m".into(),
            review_policy_version: 1,
            state: JobState::Queued,
            created_unix_ms: 1,
            artifacts: Artifacts::default(),
            attempts: 0,
        };
        let b = ReviewJob {
            epoch: Some(crate::TaskEpochId { host_runtime_id: "h".into(), session_id: "s".into(), turn_id: "t2".into() }),
            ..a.clone()
        };
        assert_eq!(a.idempotency_key(), a.idempotency_key());
        assert_ne!(a.idempotency_key(), b.idempotency_key(), "two prompts not same task");
    }

    #[test]
    fn coverage_counts_file_evidence() {
        let files = vec!["a.rs".to_string(), "b.rs".to_string()];
        let findings = vec![ReviewFinding {
            id: "f1".into(),
            severity: Severity::High,
            category: "bug".into(),
            title: "t".into(),
            detail: "d".into(),
            evidence: vec![EvidenceRef { path: "a.rs".into(), line: Some(1), source_hash: None }],
        }];
        let cov = ReviewJob::coverage(&files, &findings);
        assert_eq!(cov["a.rs"], 1);
        assert_eq!(cov["b.rs"], 0);
    }
}