//! Reproducible knowledge packs (spec §9 `KnowledgePackV1`).
//!
//! A knowledge pack is what the LLM actually receives under a 512K-context regime:
//! **the full selected records**, not approximate embeddings. For a human preference
//! dataset this means both the chosen and rejected responses arrive together, plus
//! the original prompt and the human score gap. Packs are deterministically hashed so
//! the same manifest/params produce the same pack (`PACK_REPRODUCIBILITY` gate) and
//! so test ground truth can be withheld (`BENCHMARK_ISOLATION` gate) rather than
//! leaking into generative prompts.

use serde::Serialize;
use serde_json::Value;

use lattice_lakehouse::manifest::{NormalizedRecordV1, Relation};

/// One evidence item within a pack, mapping against the spec's `evidenceClass`.
#[derive(Debug, Clone, Serialize)]
pub struct PackEvidence {
    pub record_id: String,
    pub source_id: String,
    pub source_revision: String,
    pub kind: String,
    pub original_content: Value,
    pub evidence_class: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_range_ms: Option<(u64, u64)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_refs: Option<Vec<String>>,
}

/// A comparison edge inside a pack (spec `comparisons`).
#[derive(Debug, Clone, Serialize)]
pub struct PackComparison {
    pub left_id: String,
    pub right_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
}

/// The reproducible knowledge pack (`KnowledgePackV1`).
#[derive(Debug, Clone, Serialize)]
pub struct KnowledgePackV1 {
    pub workflow: String,
    pub task: String,
    pub source_manifest_hashes: Vec<String>,
    pub evidence: Vec<PackEvidence>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub comparisons: Vec<PackComparison>,
    pub constraints: Vec<String>,
    pub withheld_evaluation_record_ids: Vec<String>,
    pub token_count: u64,
    pub pack_hash: String,
}

/// A trivial, deterministic tokenizer: whitespace-token estimate; good enough for
/// reproducible pack budgeting without pulling in a model tokenizer. Callers can
/// override with the target model's real tokenizer for production budgeting.
pub fn count_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    // Approx: 1 token ≈ 4 chars for a rough 512K budget, then min 1.
    ((text.chars().count() as u64).saturating_add(3)) / 4
}

/// Build a reproducible knowledge pack from a set of full records.
///
/// `prefers` is a function over `record.relations` that returns `Some(rejected_id)`
/// when the record is a preference pair (we attach the edge in `comparisons`),
/// preserving the rejected side — never dropping the "worse" response.
pub fn build_pack(
    workflow: &str,
    task: &str,
    source_manifest_hashes: Vec<String>,
    records: &[NormalizedRecordV1],
    constraints: Vec<String>,
) -> KnowledgePackV1 {
    let mut evidence = Vec::with_capacity(records.len());
    let mut comparisons = Vec::new();
    let mut token_total = 0u64;

    for r in records {
        // Detect a preference edge from this record's relations.
        if let Some(rel) = r.relations.iter().find(|rel: &&Relation| rel.kind == "prefers-chosen-over-rejected") {
            let rejected_id = rel.target_id.clone();
            comparisons.push(PackComparison {
                left_id: r.id.clone(),
                right_id: rejected_id.clone(),
                preferred_id: Some(r.id.clone()),
                context_id: Some(r.raw_artifact_ref.clone()),
            });
        }
        let text = hybrid_text(r);
        token_total += count_tokens(&text);
        evidence.push(PackEvidence {
            record_id: r.id.clone(),
            source_id: r.source_id.clone(),
            source_revision: r.source_revision.clone(),
            kind: r.kind.clone(),
            original_content: r.payload.clone(),
            evidence_class: human_or_derived(&r.kind).to_string(),
            annotations: Some(Value::Null), // adapters may attach annotations later
            time_range_ms: None,
            artifact_refs: (!r.raw_artifact_ref.is_empty()).then(|| vec![r.raw_artifact_ref.clone()]),
        });
    }

    let mut pack = KnowledgePackV1 {
        workflow: workflow.to_string(),
        task: task.to_string(),
        source_manifest_hashes,
        evidence,
        comparisons,
        constraints,
        withheld_evaluation_record_ids: Vec::new(),
        token_count: token_total,
        pack_hash: String::new(),
    };
    pack.pack_hash = pack_hash(&pack);
    pack
}

/// The canonical searchable text of a record used both for token estimate and for
/// the "full context" representation (kept deterministic).
fn hybrid_text(r: &NormalizedRecordV1) -> String {
    // Reuse the same shape used in hybrid recall — full payload, both responses.
    if let Some(p) = r.payload.get("prompt") {
        let mut s = p.as_str().unwrap_or_default().to_string();
        if let Some(c) = r.payload.pointer("/choice/chosen/response") {
            s += "\nCHOSEN:\n";
            s += c.as_str().unwrap_or_default();
        }
        if let Some(rj) = r.payload.pointer("/choice/rejected/response") {
            s += "\nREJECTED:\n";
            s += rj.as_str().unwrap_or_default();
        }
        return s;
    }
    serde_json::to_string(&r.payload).unwrap_or_default()
}

/// Deterministic BLAKE3 of the canonical pack JSON (reproducibility gate).
pub fn pack_hash(pack: &KnowledgePackV1) -> String {
    // Hash only the user-visible content (not the hash field itself) so it is stable.
    let mut h = blake3::Hasher::new();
    h.update(pack.workflow.as_bytes());
    h.update(pack.task.as_bytes());
    for hh in &pack.source_manifest_hashes {
        h.update(hh.as_bytes());
    }
    if let Ok(json) = serde_json::to_vec(&pack.evidence) {
        h.update(&json);
    }
    if let Ok(json) = serde_json::to_vec(&pack.comparisons) {
        h.update(&json);
    }
    h.finalize().to_hex().to_string()
}

fn human_or_derived(kind: &str) -> &'static str {
    if kind.contains("preference") || kind.contains("rating") || kind.contains("human") {
        "human"
    } else {
        "derived"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lattice_lakehouse::manifest::NormalizedRecordV1;

    fn pair(id: &str, chosen_score: i64, rejected_score: i64) -> NormalizedRecordV1 {
        NormalizedRecordV1 {
            id: id.into(),
            source_id: "writing-preference".into(),
            source_revision: "676d7114ce37ad5d8eff5b39f3648b855730bf77".into(),
            kind: "creative-preference-pair".into(),
            split: "train".into(),
            language: Some("en".into()),
            raw_artifact_ref: "WP_bench_english-1.json".into(),
            normalized_hash: format!("h-{id}"),
            payload: serde_json::json!({
                "prompt": "Write a catchy sales hook",
                "choice": {
                    "chosen": {"response": "chosen hook: Buy now", "score": chosen_score},
                    "rejected": {"response": "rejected hook: Meh", "score": rejected_score}
                }
            }),
            evidence: vec![],
            relations: vec![Relation { kind: "prefers-chosen-over-rejected".into(), target_id: format!("{id}/rejected") }],
        }
    }

    #[test]
    fn pack_preserves_both_sides_and_is_reproducible() {
        let records = vec![pair("p1", 3, 1), pair("p2", 5, 2)];
        let p = build_pack("creative", "rankPair", vec!["m1".into()], &records, vec![]);
        // Comparison edges hold both sides; preferred is the chosen id.
        assert_eq!(p.comparisons.len(), 2);
        assert_eq!(p.comparisons[0].left_id, "p1");
        assert_eq!(p.comparisons[0].right_id, "p1/rejected");
        assert_eq!(p.comparisons[0].preferred_id.as_deref(), Some("p1"));
        // Evidence includes original content with both responses.
        assert_eq!(p.evidence.len(), 2);
        assert!(p.evidence[0].original_content.pointer("/choice/rejected/response").is_some());
        // Repro: same inputs -> same hash.
        let p2 = build_pack("creative", "rankPair", vec!["m1".into()], &records, vec![]);
        assert_eq!(p.pack_hash, p2.pack_hash);
        // Hash is content-sensitive.
        let p3 = build_pack("creative", "rankPair", vec!["m2".into()], &records, vec![]);
        assert_ne!(p.pack_hash, p3.pack_hash);
    }

    #[test]
    fn token_count_is_deterministic() {
        // "alpha beta gamma delta" = 22 chars -> (22+3)/4 = 6.
        assert_eq!(count_tokens("alpha beta gamma delta"), 6);
        let t = count_tokens("");
        assert_eq!(t, 0);
    }
}