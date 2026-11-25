//! Deterministic ground-truth validator for review findings (CTO nuance #2).
//!
//! A production adjudication pipeline must not take a reviewer's word that a
//! finding is real. Given an immutable source snapshot (maps path -> line count
//! and optional snippet index), this validator machine-classifies each finding's
//! cited evidence into `VERIFIED / UNVERIFIED / HALLUCINATED` BEFORE adjudication:
//!   - file must exist in the snapshot index;
//!   - the cited line range must be inside the file's real line count;
//!   - when a snippet is cited, the exact snippet must appear in the file;
//!   - any finding citing a file/lines/snippet that do not exist => HALLUCINATED.
//!
//! This is the same check that rejected the adversarial reviewer's fabricated
//! `event_handler.ts` / `session_store.ts` findings during the self-audit, now
//! formalized and reusable so the ADJUDICATOR runs it for every role.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A bounded snapshot index: for each real source file, its line count, and an
/// optional content hash so entire-tree or per-file integrity can be asserted.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SnapshotIndex {
    pub files: HashMap<String, FileRef>,
    /// Content hash of the whole snapshot bundle (immutable evidence pointer).
    pub bundle_hash: String,
    /// The filesystem root the snapshot was taken from. `verify` resolves
    /// repo-relative paths against this root to machine-check line ranges AND
    /// exact snippet content (the authoritative evidence source), falling back
    /// to known source bases when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRef {
    /// Canonical repo-relative path.
    pub path: String,
    pub line_count: usize,
    pub sha256: String,
}

/// The machine verdict attached to ONE cited evidence — NOT a verdict on the
/// reviewer's overall conclusion. `Verified` only means the cited evidence
/// exists and matches the snapshot; whether that evidence actually implies the
/// finding (e.g. "this code causes a race") is decided by the Adjudicator with
/// additional evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidenceVerdict {
    /// The cited file exists, the line range is in bounds AND any cited snippet
    /// appears within that exact declared line range. This is evidence-existence
    /// only, never proof of the finding's claim.
    Verified,
    /// Evidence not machine-checkable (symbol/endpoint token, or file real but
    /// snippet/line mismatch). The Adjudicator must decide with more evidence.
    Unverified,
    /// The cited file does not exist in the snapshot. Strong rejection signal.
    Hallucinated,
}

impl EvidenceVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            EvidenceVerdict::Verified => "verified",
            EvidenceVerdict::Unverified => "unverified",
            EvidenceVerdict::Hallucinated => "hallucinated",
        }
    }
}

/// The cited evidence a reviewer attached to a finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CitedEvidence {
    /// Repo-relative file path (or a symbol like `crate::mod::fn`, or a pipe name).
    pub path: String,
    /// Optional 1-based inclusive line range.
    pub lines: Option<(u32, u32)>,
    /// Optional exact snippet the finding claims exists in the file.
    pub snippet: Option<String>,
}

impl SnapshotIndex {
    /// Classify a reviewer-cited evidence deterministically against the snapshot.
    ///
    /// Rules:
    ///  - path not in index  -> Hallucinated (unless it is a known symbol/endpoint
    ///    token, recorded in `known_symbols`).
    ///  - path in index but lines out of range or snippet not found in file
    ///    content -> Unverified (file real but the specific evidence is wrong).
    ///  - exact match (line range within bounds AND snippet present when cited)
    ///    -> Verified.
    pub fn verify(&self, ev: &CitedEvidence, known_symbols: &[&str]) -> EvidenceVerdict {
        let norm = ev.path.replace('\\', "/").to_lowercase();
        let file = self.files.get(&norm);
        let is_symbol = known_symbols.iter().any(|s| norm == s.to_lowercase());
        if file.is_none() {
            return if is_symbol || norm.is_empty() {
                EvidenceVerdict::Unverified // symbol/endpoint ref: real but not a verifiable file line
            } else {
                EvidenceVerdict::Hallucinated
            };
        }
        let file = file.expect("guarded: file is Some here");
        // File exists. Check line range bounds; a snippet (when cited) must be
        // found WITHIN that exact declared line range, not merely anywhere in
        // the file — otherwise a finding could cite wrong evidence and still
        // get `Verified`.
        let (lo, hi) = match ev.lines {
            Some((a, b)) => {
                if a == 0 || b < a || (b as usize) > file.line_count {
                    return EvidenceVerdict::Unverified;
                }
                (a, b)
            }
            None => (1, file.line_count as u32),
        };
        if let Some(snip) = &ev.snippet {
            if !snip.trim().is_empty() && !self.snippet_in_snapshot_root(&file.path, snip, lo, hi) {
                return EvidenceVerdict::Unverified;
            }
        }
        EvidenceVerdict::Verified
    }

    /// Does the exact snippet appear within lines [lo, hi] of `rel` as it exists
    /// at the snapshot's real root (authoritative), else the known source bases?
    fn snippet_in_snapshot_root(&self, rel: &str, snippet: &str, lo: u32, hi: u32) -> bool {
        let mut bases: Vec<std::path::PathBuf> = Vec::new();
        if let Some(root) = &self.root {
            bases.push(root.clone());
        }
        const HB: &[&str] = &["D:\\_SATIN_AI\\NAI-Lattice", "C:\\src\\kilo-gastown-local"];
        for b in HB {
            bases.push(std::path::PathBuf::from(b));
        }
        for base in bases {
            let p = base.join(rel.trim_start_matches(['/', '\\']));
            if let Ok(content) = std::fs::read_to_string(&p) {
                let lines: Vec<&str> = content.lines().collect();
                let needle = snippet.trim();
                let (a, b) = (lo.saturating_sub(1) as usize, (hi as usize).min(lines.len()));
                if a >= b {
                    continue;
                }
                // 1) The exact snippet may be a multi-line block spanning the range:
                //    join [a,b) with '\n' and look for the needle as a contiguous
                //    substring within that exact range.
                let joined = lines[a..b].join("\n");
                if joined.contains(needle) {
                    return true;
                }
                // 2) Or the needle may appear within a single line of the range.
                for idx in a..b {
                    if lines.get(idx).map(|l| l.contains(needle)).unwrap_or(false) {
                        return true;
                    }
                }
                return false; // file found at this base → authoritative (even if not matching)
            }
        }
        false
    }
}

/// A bounded snapshot index: for each real source file, its line count, and an
/// optional content hash so entire-tree or per-file integrity can be asserted.

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_index() -> SnapshotIndex {
        use std::collections::HashMap;
        let mut files = HashMap::new();
        let lc = real_line_count("apps/nai-fabric-host/src/main.rs");
        files.insert(
            "apps/nai-fabric-host/src/main.rs".into(),
            FileRef { path: "apps/nai-fabric-host/src/main.rs".into(), line_count: lc, sha256: "x".into() },
        );
        SnapshotIndex { files, bundle_hash: "b".into(), root: None }
    }

    /// Real 1-based line number where `needle` appears in `rel` (from any of the
    /// known source bases), or a large fallback so the "out of range" checks
    /// still work when the file isn't found.
    fn line_containing(rel: &str, needle: &str) -> u32 {
        const BASES: &[&str] = &["D:\\_SATIN_AI\\NAI-Lattice", "C:\\src\\kilo-gastown-local"];
        for base in BASES {
            let p = std::path::Path::new(base).join(rel);
            if let Ok(c) = std::fs::read_to_string(&p) {
                for (i, l) in c.lines().enumerate() {
                    if l.contains(needle) {
                        return i as u32 + 1;
                    }
                }
            }
        }
        1_000_000
    }

    fn real_line_count(rel: &str) -> usize {
        const BASES: &[&str] = &["D:\\_SATIN_AI\\NAI-Lattice", "C:\\src\\kilo-gastown-local"];
        for base in BASES {
            let p = std::path::Path::new(base).join(rel);
            if let Ok(c) = std::fs::read_to_string(&p) {
                return c.lines().count();
            }
        }
        1_000_000
    }

    #[test]
    fn snippet_outside_cited_range_is_unverified() {
        // `run_worker_main()` genuinely appears in main.rs. Citing lines 1..10
        // with that snippet: the snippet IS in the file but NOT within the cited
        // range => must be UNVERIFIED (not Verified).
        let idx = sample_index();
        let ev = CitedEvidence {
            path: "apps/nai-fabric-host/src/main.rs".into(),
            lines: Some((1, 10)),
            snippet: Some("fn run_worker_main()".into()),
        };
        assert_eq!(idx.verify(&ev, &[]), EvidenceVerdict::Unverified,
            "snippet exists in file but outside the cited line range => not Verified");
    }

    #[test]
    fn snippet_in_cited_range_is_verified_even_if_also_elsewhere() {
        let idx = sample_index();
        // Locate the actual line where `run_worker_main()` appears in main.rs
        // so this test is robust to unrelated edits that shift line numbers (the
        // semantic is "snippet present within the cited range => Verified").
        let line = line_containing("apps/nai-fabric-host/src/main.rs", "fn run_worker_main()");
        let ev = CitedEvidence {
            path: "apps/nai-fabric-host/src/main.rs".into(),
            lines: Some((line, line)),
            snippet: Some("fn run_worker_main()".into()),
        };
        assert_eq!(idx.verify(&ev, &[]), EvidenceVerdict::Verified,
            "snippet present within the cited line range => Verified (existence only)");
    }

    #[test]
    fn unknown_file_is_hallucinated() {
        let idx = sample_index();
        let ev = CitedEvidence { path: "src/event_handler.ts".into(), lines: Some((142, 142)), snippet: None };
        assert_eq!(idx.verify(&ev, &[]), EvidenceVerdict::Hallucinated);
    }

    #[test]
    fn real_file_out_of_line_range_is_unverified() {
        let idx = sample_index();
        let ev = CitedEvidence { path: "apps/nai-fabric-host/src/main.rs".into(), lines: Some((99999, 100000)), snippet: None };
        assert_eq!(idx.verify(&ev, &[]), EvidenceVerdict::Unverified);
    }

    #[test]
    fn real_file_valid_range_snippet_is_verified() {
        let idx = sample_index();
        let line = line_containing("apps/nai-fabric-host/src/main.rs", "fn run_worker_main()");
        let ev = CitedEvidence {
            path: "apps/nai-fabric-host/src/main.rs".into(),
            lines: Some((line, line)),
            snippet: Some("fn run_worker_main()".into()),
        };
        assert_eq!(idx.verify(&ev, &[]), EvidenceVerdict::Verified);
    }

    #[test]
    fn symbol_token_is_unverified_not_hallucinated() {
        let idx = sample_index();
        let ev = CitedEvidence { path: "nai_lattice_supervisor::child::run".into(), lines: None, snippet: None };
        assert_eq!(idx.verify(&ev, &["nai_lattice_supervisor::child::run"]), EvidenceVerdict::Unverified);
    }
}