//! WritingPreferenceBench adapter (spec §C2).
//!
//! Acquired real schema (verified against `WP_bench_english-1.json`):
//! ```json
//! {
//!   "prompt": "...",
//!   "prompt_id": "<uuid>",
//!   "tag": "...",
//!   "chosen": { "response": "...", "score": 3, "model": "...", "completion_tokens": N,
//!               "prompt_tokens": N, "word_len": N },
//!   "rejected": { "response": "...", "score": 1, "model": "...", ... }
//! }
//! ```
//!
//! Web/repo: `m-a-p/Writing-Preference-Bench`, 1 800 preference pairs (3 files ×
//! 600). The value of the dataset is the **pairs**: the rejected response must never
//! be dropped (spec C2: "Nikdy neztratit horší odpověď").
//!
//! [`WritingPreferenceAdapter::parse`] reads the raw JSON array(s) and produces one
//! `NormalizedRecordV1` per pair with:
//! - `id` = `prompt_id` (source-stable identity),
//! - `payload` = `{ prompt, tag, chosen: {...}, rejected: {...} }` (both responses),
//! - a `prefers-chosen-over-rejected` relation linking the two response record ids.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::adapters::AdapterError;
use crate::manifest::{NormalizedRecordV1, Relation};

/// The raw single-pair schema as published by WritingPreferenceBench.
#[derive(Debug, Deserialize)]
pub struct RawPair {
    pub prompt: String,
    #[serde(rename = "prompt_id")]
    pub prompt_id: String,
    pub tag: String,
    pub chosen: RawResponse,
    pub rejected: RawResponse,
}

#[derive(Debug, Deserialize)]
pub struct RawResponse {
    pub response: String,
    pub score: i64,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub word_len: Option<u64>,
}

/// Adapter for `m-a-p/Writing-Preference-Bench`.
#[derive(Default)]
pub struct WritingPreferenceAdapter {
    pub adapter_version: String,
}

impl WritingPreferenceAdapter {
    pub fn new() -> Self {
        Self {
            adapter_version: "writing-preference/1.0.0".into(),
        }
    }

    /// Parse a directory of `WP_bench_*.json` files into normalized records.
    /// Every array element is one preference pair. `split` defaults to "train"
    /// (the bench is released as a training set). `source_revision` is the pinned
    /// Git/HF revision.
    pub fn parse_dir(
        &self,
        dir: &Path,
        source_id: &str,
        source_revision: &str,
    ) -> Result<Vec<NormalizedRecordV1>, AdapterError> {
        let mut out = Vec::new();
        let mut files: Vec<PathBuf> = fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .map(|x| x == "json")
                    .unwrap_or(false)
            })
            // Only the WP_bench data files, in a stable order (README/.gitattributes
            // are not data).
            .filter(|p| p.file_name().map(|f| f.to_string_lossy().starts_with("WP_bench")) == Some(true))
            .collect();
        files.sort();
        for f in files {
            let file_name = f.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let language = if file_name.contains("chinese") { "zh" } else { "en" };
            let raw = fs::read_to_string(&f).map_err(|e| AdapterError::Source(e.to_string()))?;
            let pairs: Vec<RawPair> = serde_json::from_str(&raw)
                .map_err(|e| AdapterError::Json(file_name.clone(), e.to_string()))?;
            for pair in pairs {
                out.push(self.normalize(source_id, source_revision, language, &file_name, pair));
            }
        }
        Ok(out)
    }

    fn normalize(
        &self,
        source_id: &str,
        source_revision: &str,
        language: &str,
        raw_file: &str,
        p: RawPair,
    ) -> NormalizedRecordV1 {
        let chosen_id = format!("{}/chosen", p.prompt_id);
        let rejected_id = format!("{}/rejected", p.prompt_id);
        let relations = vec![Relation {
            kind: "prefers-chosen-over-rejected".into(),
            target_id: rejected_id.clone(),
        }];
        let payload = serde_json::json!({
            "prompt": p.prompt,
            "tag": p.tag,
            "choice": {
                "chosen": {
                    "response": p.chosen.response,
                    "score": p.chosen.score,
                    "model": p.chosen.model,
                    "completion_tokens": p.chosen.completion_tokens,
                    "prompt_tokens": p.chosen.prompt_tokens,
                    "word_len": p.chosen.word_len,
                },
                "rejected": {
                    "response": p.rejected.response,
                    "score": p.rejected.score,
                    "model": p.rejected.model,
                    "completion_tokens": p.rejected.completion_tokens,
                    "prompt_tokens": p.rejected.prompt_tokens,
                    "word_len": p.rejected.word_len,
                },
            },
            "chosenId": chosen_id,
            "rejectedId": rejected_id,
        });
        NormalizedRecordV1 {
            id: p.prompt_id.clone(),
            source_id: source_id.to_string(),
            source_revision: source_revision.to_string(),
            kind: "creative-preference-pair".into(),
            split: "train".into(),
            language: Some(language.to_string()),
            raw_artifact_ref: raw_file.to_string(),
            normalized_hash: String::new(),
            payload,
            evidence: vec![],
            relations,
        }
        .with_computed_hash()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_acquired_bench_files() {
        // Guard: only run against the real, git-pinned acquisition dir.
        let root = std::path::Path::new("D:\\NAI\\DataLake\\raw\\writing-preference");
        if !root.join("WP_bench_english-1.json").exists() {
            eprintln!("SKIP: writing-preference not acquired at D:\\NAI\\DataLake\\raw\\writing-preference");
            return;
        }
        let adapter = WritingPreferenceAdapter::new();
        let records = adapter
            .parse_dir(root, "writing-preference", "676d7114ce37ad5d8eff5b39f3648b855730bf77")
            .unwrap();
        assert_eq!(records.len(), 1800, "expected 1800 total preference pairs");
        // Stable identity + both responses must be preserved for the first pair.
        let first = &records[0];
        assert!(!first.id.is_empty());
        let chosen = first.payload["choice"]["chosen"]["response"].as_str().unwrap();
        let rejected = first.payload["choice"]["rejected"]["response"].as_str().unwrap();
        assert!(!chosen.is_empty());
        assert!(!rejected.is_empty());
        assert_ne!(chosen, rejected, "chosen must differ from rejected");
        // Relations link the pair.
        assert_eq!(first.relations[0].kind, "prefers-chosen-over-rejected");
        // Deterministic: re-parsing yields the same normalized_hash set.
        let again = adapter
            .parse_dir(root, "writing-preference", "676d7114ce37ad5d8eff5b39f3648b855730bf77")
            .unwrap();
        let h1: Vec<_> = records.iter().map(|r| r.normalized_hash.clone()).collect();
        let h2: Vec<_> = again.iter().map(|r| r.normalized_hash.clone()).collect();
        assert_eq!(h1, h2);
    }
}