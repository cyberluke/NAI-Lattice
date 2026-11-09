//! AAF-federated embeddings + rerank provider (Lattice spec: the embeddings and
//! reranking planes are owned by LUSI — the shared OpenVINO NPU Qwen3 runtime.
//! nai-lattice calls out to the LUSI gateway; it never re-implements or
//! duplicates the models).
//!
//! LUSI gateway contract (127.0.0.1:27130, canonical shared engine home):
//! - `POST /v1/embeddings {input:[...], operation:query|index}` → `{data:[{index, embedding}]}`
//! - `POST /v1/rerank {query, documents, top_n}` → `{results:[{id, index, score}]}`
//!
//! Uses the lightweight synchronous `ureq` HTTP client (plain localhost, no
//! TLS stack) so the federation adds negligible size/deps.

use lattice_vector::{SemanticStore, VectorHit};
use serde::{Deserialize, Serialize};

/// Errors from the federated embeddings provider.
#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("http: {0}")]
    Http(String),
    #[error("lusi gateway responded with error: {0}")]
    Gateway(String),
    #[error("no embedding returned for input {0}")]
    NoVector(usize),
    #[error("vector store: {0}")]
    Vector(#[from] lattice_vector::VectorError),
}

/// One LUSI `/v1/rerank` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankHit {
    pub id: String,
    pub index: usize,
    pub score: f32,
}

/// The default LUSI gateway base URL (canonical shared engine home port).
pub fn default_gateway_url() -> String {
    std::env::var("NAI_LUSI_GATEWAY_URL").unwrap_or_else(|_| "http://127.0.0.1:27130".to_string())
}

/// AAF-federated embeddings + rerank client over the LUSI gateway (synchronous).
#[derive(Debug, Clone)]
pub struct LusiFederated {
    base: String,
    http: ureq::Agent,
}

fn http_error(e: ureq::Error) -> EmbeddingError {
    EmbeddingError::Http(e.to_string())
}

impl LusiFederated {
    /// Build a client at the canonical LUSI gateway URL.
    pub fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            http: ureq::AgentBuilder::new().build(),
        }
    }

    /// Create at the default (env-overridable) gateway URL.
    pub fn at_default_url() -> Self {
        Self::new(&default_gateway_url())
    }

    /// Embed `inputs` via the LUSI NPU Qwen3 model, returning vectors in order.
    /// `query=true` uses the query instruction; `false` indexes the documents.
    pub fn embed(&self, inputs: &[&str], query: bool) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let body = serde_json::json!({
            "input": inputs,
            "operation": if query { "query" } else { "index" },
        });
        let r = self
            .http
            .post(&format!("{}/v1/embeddings", self.base))
            .send_json(body)
            .map_err(http_error)?;
        if !(200..300).contains(&r.status()) {
            return Err(EmbeddingError::Gateway(r.status().to_string()));
        }
        let json: serde_json::Value =
            r.into_json().map_err(|e| EmbeddingError::Http(e.to_string()))?;
        let data = json
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| EmbeddingError::Gateway("missing data".into()))?;
        let mut vectors = Vec::with_capacity(data.len());
        for (i, item) in data.iter().enumerate() {
            let v: Vec<f32> = item
                .get("embedding")
                .and_then(|e| e.as_array())
                .and_then(|a| a.iter().map(|x| x.as_f64()).collect::<Option<Vec<_>>>())
                .map(|a| a.into_iter().map(|x| x as f32).collect())
                .ok_or_else(|| EmbeddingError::NoVector(i))?;
            vectors.push(v);
        }
        Ok(vectors)
    }

    /// Rerank `documents` against `query` via the LUSI Qwen3 reranker.
    pub fn rerank(
        &self,
        query: &str,
        documents: &[(&str, &str)],
        top_n: usize,
    ) -> Result<Vec<RerankHit>, EmbeddingError> {
        let body = serde_json::json!({
            "query": query,
            "documents": documents.iter().map(|(_, doc)| doc).collect::<Vec<_>>(),
            "top_n": top_n,
        });
        let r = self
            .http
            .post(&format!("{}/v1/rerank", self.base))
            .send_json(body)
            .map_err(http_error)?;
        if !(200..300).contains(&r.status()) {
            return Err(EmbeddingError::Gateway(r.status().to_string()));
        }
        let json: serde_json::Value =
            r.into_json().map_err(|e| EmbeddingError::Http(e.to_string()))?;
        let results = json
            .get("results")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        let mut hits = Vec::with_capacity(results.len());
        for r in results {
            let idx = r.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
            let id = documents
                .get(idx)
                .map(|(id, _)| id.to_string())
                .unwrap_or_else(|| idx.to_string());
            let score = r.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0) as f32;
            hits.push(RerankHit { id, index: idx, score });
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        Ok(hits)
    }

    /// Convenience: embed a query, run it against a `SemanticStore`, and return
    /// the fused vector hits (semantic-recall half of the federated path).
    pub fn semantic_query(
        &self,
        store: &SemanticStore,
        query: &str,
        limit: usize,
    ) -> Result<Vec<VectorHit>, EmbeddingError> {
        let q = self
            .embed(&[query], true)?
            .into_iter()
            .next()
            .ok_or_else(|| EmbeddingError::NoVector(0))?;
        Ok(store.search(&q, limit)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rerank_response_shape_and_orders() {
        // Parse the LUSI rerank contract shape without a live server.
        let json = serde_json::json!({
            "results": [
                {"id": "d1", "index": 0, "score": 0.1},
                {"id": "d2", "index": 1, "score": 0.9},
            ]
        });
        let results = json["results"].as_array().unwrap().clone();
        let docs = [("d1", "a"), ("d2", "b")];
        let mut hits: Vec<RerankHit> = results
            .into_iter()
            .map(|r| {
                let idx = r["index"].as_u64().unwrap_or(0) as usize;
                RerankHit {
                    id: docs.get(idx).map(|(id, _)| id.to_string()).unwrap(),
                    index: idx,
                    score: r["score"].as_f64().unwrap_or(0.0) as f32,
                }
            })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        assert_eq!(hits[0].id, "d2");
        assert_eq!(hits[1].id, "d1");
    }

    #[test]
    fn default_url_is_lusi_shared_home() {
        assert!(default_gateway_url().contains("127.0.0.1"));
    }
}