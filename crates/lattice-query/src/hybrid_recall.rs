//! Hybrid recall (spec §9): lexical (Tantivy) + vector (LUSI embeddings) →
//! reciprocal-rank fusion → optional LUSI rerank → resolve original records from
//! Parquet.
//!
//! Pipeline (spec §9 "Hybrid RAG"):
//! ```text
//! source/workflow/split filters
//!   ├── lattice-search lexical recall (topK=…)
//!   └── lattice-embeddings LUSI query embedding → vector recall (topK=…)
//! candidate union → reciprocal rank fusion → LUSI rerank (topN)
//! resolve original records from Parquet (lakehouse) → context / structured result
//! ```
//!
//! This is deliberately **not** the existing `query.federated` in `lattice-runtime`
//! (which stayed compatible for app launchers); it is the dataset retrieval layer in
//! `lattice-query`.

use nai_lattice_lakehouse::manifest::NormalizedRecordV1;
use nai_lattice_search::DocumentIndex;

/// Errors from hybrid recall.
#[derive(Debug, thiserror::Error)]
pub enum RecallError {
    #[error("search: {0}")]
    Search(#[from] nai_lattice_search::SearchError),
    #[error("embeddings: {0}")]
    Embeddings(#[from] nai_lattice_embeddings::EmbeddingError),
    #[error("vector: {0}")]
    Vector(#[from] nai_lattice_vector::VectorError),
    #[error("no indexed documents / embeddings populated")]
    Empty,
}

/// Default recall parameters (spec §9 starting configuration for benchmarking).
pub const LEXICAL_TOPK: usize = 60;
pub const VECTOR_TOPK: usize = 60;
pub const RERANK_TOPN: usize = 40;

/// Reciprocal-rank fusion: combine two id→rank maps weighted by k.
fn reciprocal_rank_fusion(
    lexical: &[(String, f32)],
    vector: &[(String, f32)],
    k: f32,
) -> Vec<(String, f32)> {
    let mut scores: std::collections::HashMap<String, f32> = Default::default();
    for (i, (id, _)) in lexical.iter().enumerate() {
        *scores.entry(id.clone()).or_default() += 1.0 / (k + (i as f32) + 1.0);
    }
    for (i, (id, _)) in vector.iter().enumerate() {
        *scores.entry(id.clone()).or_default() += 1.0 / (k + (i as f32) + 1.0);
    }
    let mut out: Vec<(String, f32)> = scores.into_iter().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out
}

/// A fully-resolved hybrid recall hit with its original record.
#[derive(Debug, Clone)]
pub struct HybridHit {
    pub record_id: String,
    pub score: f32,
    pub record: NormalizedRecordV1,
}

/// Hybrid recall over an indexed source. `lexical_queries` are run through
/// `lattice-search`; `vector` uses the LUSI gateway embeddings + a semantic store.
///
/// Callers build the index + semantic store once (see helpers) and pass them here.
pub fn hybrid_recall(
    search: &DocumentIndex,
    vectors: &nai_lattice_vector::SemanticStore,
    query: &str,
    namespace: &str,
    top_k: usize,
    resolve: &dyn Fn(&str) -> Option<NormalizedRecordV1>,
) -> Result<Vec<HybridHit>, RecallError> {
    // 1. Lexical recall (namespace-scoped via a prefix filter in the query body).
    let lexical = search
        .search(&format!("{query} {namespace}"), LEXICAL_TOPK.max(1))?
        .into_iter()
        .map(|h| (h.id, h.score))
        .collect::<Vec<_>>();

    if lexical.is_empty() {
        return Err(RecallError::Empty);
    }

    // 2. Vector recall via LUSI query embedding.
    let lusi = nai_lattice_embeddings::LusiFederated::at_default_url();
    let vector_hits = lusi.semantic_query(vectors, query, VECTOR_TOPK.max(1))?;
    let vector = vector_hits
        .iter()
        .map(|v| (v.id.clone(), v.score))
        .collect::<Vec<_>>();

    // 3. Reciprocal-rank fusion.
    let fused = reciprocal_rank_fusion(&lexical, &vector, 60.0);

    // 4. Optional rerank (top N) — only when we have a non-trivial candidate set
    //    and the LUSI rerank endpoint adds signal. Resolve records + document text.
    let candidates: Vec<&(String, f32)> = fused.iter().take(RERANK_TOPN.max(1)).collect();
    let mut docs: Vec<(String, String)> = Vec::new();
    let mut by_id: std::collections::HashMap<String, &(String, f32)> = Default::default();
    for c in &candidates {
        if let Some(rec) = resolve(&c.0) {
            docs.push((c.0.clone(), searchable_text(&rec)));
            by_id.insert(c.0.clone(), c);
        }
    }
    let reranked = if docs.len() > 1 {
        lusi.rerank(query, &docs.iter().map(|(i, d)| (i.as_str(), d.as_str())).collect::<Vec<_>>(), RERANK_TOPN).unwrap_or_default()
    } else {
        docs.iter().map(|(i, _)| nai_lattice_embeddings::RerankHit { id: i.clone(), index: 0, score: 1.0 }).collect()
    };

    // 5. Resolve full records in reranked order, tacking on the fused score.
    let mut out = Vec::with_capacity(RERANK_TOPN);
    for hit in reranked {
        let Some(rec) = resolve(&hit.id) else { continue };
        let fused_score = by_id.get(&hit.id).map(|c| c.1).unwrap_or(hit.score);
        out.push(HybridHit {
            record_id: hit.id,
            score: fused_score,
            record: rec,
        });
        if out.len() >= top_k {
            break;
        }
    }
    Ok(out)
}

/// Build a searchable text body for a record (prompt + both responses for a
/// creative preference pair — never losing the rejected side).
pub fn searchable_text(r: &NormalizedRecordV1) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(p) = r.payload.get("prompt") {
        if let Some(s) = p.as_str() {
            parts.push(s.to_string());
        }
    }
    if let Some(c) = r.payload.pointer("/choice/chosen/response") {
        if let Some(s) = c.as_str() {
            parts.push(s.to_string());
        }
    }
    if let Some(c) = r.payload.pointer("/choice/rejected/response") {
        if let Some(s) = c.as_str() {
            parts.push(s.to_string());
        }
    }
    // Fall back to serializing the whole payload.
    if parts.is_empty() {
        parts.push(serde_json::to_string(&r.payload).unwrap_or_default());
    }
    parts.join("\n")
}

/// Index a batch of records into `search` (id + body) under a namespace. Used by
/// the caller to prepare the lexical half of hybrid recall.
pub fn index_records(
    search: &DocumentIndex,
    records: &[NormalizedRecordV1],
    namespace: &str,
) -> Result<(), RecallError> {
    for r in records {
        search.put(&r.id, &r.kind, &searchable_text(r), namespace)?;
    }
    search.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_ranks_union_highest() {
        let lex = vec![("a".to_string(), 1.0), ("b".to_string(), 0.9)];
        let vec = vec![("c".to_string(), 0.8), ("a".to_string(), 0.7)];
        let fused = reciprocal_rank_fusion(&lex, &vec, 60.0);
        // "a" appears in both -> highest RRF score.
        assert_eq!(fused[0].0, "a");
    }

    #[test]
    fn searchable_text_preserves_both_responses() {
        let r = NormalizedRecordV1 {
            id: "p1".into(),
            source_id: "writing-preference".into(),
            source_revision: "r".into(),
            kind: "creative-preference-pair".into(),
            split: "train".into(),
            language: Some("en".into()),
            raw_artifact_ref: "x".into(),
            normalized_hash: "h".into(),
            payload: serde_json::json!({
                "prompt": "make a hook",
                "choice": {"chosen": {"response": "chosen text"}, "rejected": {"response": "rejected text"}}
            }),
            evidence: vec![],
            relations: vec![],
        };
        let t = searchable_text(&r);
        assert!(t.contains("chosen text"));
        assert!(t.contains("rejected text"));
        assert!(t.contains("make a hook"));
    }
}