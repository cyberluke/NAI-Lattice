//! NAI Lattice `lattice-query` — structural SQL-over-Parquet, hybrid recall, and
//! reproducible knowledge packs over federated datasets.
//!
//! This is the query layer of the Tutty Knowledge Federation blueprint
//! (`docs/data_ingest_workflows.md` §7): it operates over the canonical Parquet
//! snapshots produced by [`lattice-lakehouse`] and implements the retrieval classes
//! enumerated in §9:
//! - **structural** — SQL/Parquet filters, projections and aggregations for ratings,
//!   strategies, conversion logs and UI actions;
//! - **hybrid recall** — lexical (Tantivy) + vector (LUSI embeddings) →
//!   reciprocal-rank fusion → rerank → resolve original records from Parquet;
//! - **knowledge packs** — the reproducible, full-record context packs an LLM
//!   actually receives under a 512K context regime.

pub mod dataset_sql;
pub mod hybrid_recall;
pub mod knowledge_pack;

pub use dataset_sql::{aggregate_by, count, query_structural, StructFilter, StructResult};
pub use hybrid_recall::{
    hybrid_recall, index_records, searchable_text, HybridHit, RERANK_TOPN, LEXICAL_TOPK,
    VECTOR_TOPK,
};
pub use knowledge_pack::{build_pack, count_tokens, pack_hash, KnowledgePackV1, PackComparison, PackEvidence};