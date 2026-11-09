//! Embedded fuzzy full-text search index (nai-lattice "query.search").
//!
//! Backed by Tantivy. Documents are minimal (`id`, `namespace`, `title`,
//! `body`) so launcher/files/dock can index application metadata, descriptors,
//! email subjects, etc. and run fuzzy + phrase queries locally.

use std::path::Path;
use std::sync::Mutex;

use tantivy::collector::TopDocs;
use tantivy::query::{FuzzyTermQuery, QueryParser, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value};
use tantivy::TantivyDocument;
use tantivy::{doc, Index, IndexWriter, Term};

/// Errors from the search index.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tantivy: {0}")]
    Tantivy(#[from] tantivy::TantivyError),
    #[error("query: {0}")]
    Query(String),
}

/// One search hit.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchHit {
    pub id: String,
    pub score: f32,
    pub snippet: String,
}

/// The fuzzy full-text index, shareable behind `Arc` (reads concurrent,
/// writes serialize on the internal `Mutex<IndexWriter>`).
pub struct DocumentIndex {
    index: Index,
    id: Field,
    namespace: Field,
    title: Field,
    body: Field,
    writer: Mutex<IndexWriter>,
}

/// Builds the Tantivy schema: `id` (indexed for exact/fuzzy id match),
/// `namespace` (indexed + stored), `title`/`body` (text, boosted + stored).
fn build_schema() -> (Schema, Field, Field, Field, Field) {
    let mut sb = Schema::builder();
    let text = TextOptions::default()
        .set_indexing_options(TextFieldIndexing::default().set_tokenizer("default"))
        .set_stored();
    // `id` is indexed raw (un-tokenized) so exact term/lookup matches work.
    let raw = TextOptions::default()
        .set_indexing_options(TextFieldIndexing::default().set_tokenizer("raw"))
        .set_stored();
    let id = sb.add_text_field("id", raw);
    let namespace = sb.add_text_field("namespace", text.clone());
    let title_field = sb.add_text_field("title", text.clone());
    let body = sb.add_text_field("body", text);
    (sb.build(), id, namespace, title_field, body)
}

impl DocumentIndex {
    /// Create an in-memory index with the Lattice schema.
    pub fn in_memory() -> Result<Self, SearchError> {
        let (schema, id, namespace, title, body) = build_schema();
        let index = Index::create_in_ram(schema); // returns Index directly
        let writer: IndexWriter = index.writer(50_000_000)?;
        Ok(Self { index, id, namespace, title, body, writer: Mutex::new(writer) })
    }

    /// Open (or create) a persistent index at `path`.
    pub fn open(path: &Path) -> Result<Self, SearchError> {
        std::fs::create_dir_all(path)?;
        let index = if path.join("meta.json").exists() {
            Index::open_in_dir(path)?
        } else {
            let (schema, ..) = build_schema();
            Index::create_in_dir(path, schema)?
        };
        let id = index.schema().get_field("id").map_err(|e| SearchError::Query(e.to_string()))?;
        let namespace = index.schema().get_field("namespace").map_err(|e| SearchError::Query(e.to_string()))?;
        let title = index.schema().get_field("title").map_err(|e| SearchError::Query(e.to_string()))?;
        let body = index.schema().get_field("body").map_err(|e| SearchError::Query(e.to_string()))?;
        let writer: IndexWriter = index.writer(50_000_000)?;
        Ok(Self { index, id, namespace, title, body, writer: Mutex::new(writer) })
    }

    /// Index (replace by id) one document.
    pub fn put(&self, id: &str, title: &str, body: &str, namespace: &str) -> Result<(), SearchError> {
        let mut w = self.writer.lock().unwrap();
        w.delete_term(Term::from_field_text(self.id, id));
        w.add_document(doc!(
            self.id => id,
            self.namespace => namespace,
            self.title => title,
            self.body => body,
        ))?;
        w.commit()?;
        Ok(())
    }

    /// Search a free-text query (phrase / boolean) across title + body.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let mut parser = QueryParser::for_index(&self.index, vec![self.title, self.body]);
        parser.set_field_boost(self.title, 2.0);
        let parsed = parser
            .parse_query(query)
            .map_err(|e| SearchError::Query(e.to_string()))?;
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let top: Vec<(f32, tantivy::DocAddress)> =
            searcher.search(&parsed, &TopDocs::with_limit(limit.max(1)).order_by_score())?;
        Ok(hits_from(top, &searcher, self.id, self.body))
    }

    /// Pure fuzzy-term search: matches a term with edit distance 1 across
    /// title/body/id (launcher-style "ftz" / "Coslculator" fuzzy app-match).
    pub fn search_fuzzy(&self, term: &str, limit: usize) -> Result<Vec<SearchHit>, SearchError> {
        if term.trim().is_empty() {
            return Ok(Vec::new());
        }
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let mut fused: Vec<SearchHit> = Vec::new();
        for field in [self.title, self.body, self.id] {
            let fq = FuzzyTermQuery::new(Term::from_field_text(field, term), 1, true);
            let top: Vec<(f32, tantivy::DocAddress)> =
                searcher.search(&fq, &TopDocs::with_limit(limit.max(1)).order_by_score())?;
            for (score, addr) in top {
                let idv = doc_id(&searcher, addr, self.id);
                if fused.iter().any(|h: &SearchHit| h.id == idv) {
                    continue;
                }
                let doc: TantivyDocument = searcher.doc(addr)?;
                let snippet = doc
                    .get_first(self.title)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .chars()
                    .take(120)
                    .collect::<String>();
                fused.push(SearchHit { id: idv, score, snippet });
            }
        }
        fused.sort_by(|a, b| b.score.total_cmp(&a.score));
        fused.truncate(limit.max(1));
        Ok(fused)
    }

    /// Exact id lookup (used for `entity.resolve`-style hits).
    pub fn get_by_id(&self, id: &str) -> Result<Option<SearchHit>, SearchError> {
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let tq = TermQuery::new(Term::from_field_text(self.id, id), IndexRecordOption::Basic);
        let top: Vec<(f32, tantivy::DocAddress)> = searcher.search(&tq, &TopDocs::with_limit(1).order_by_score())?;
        if top.is_empty() {
            return Ok(None);
        }
        Ok(hits_from(top, &searcher, self.id, self.body).into_iter().next())
    }

    pub fn delete(&self, id: &str) -> Result<(), SearchError> {
        let mut w = self.writer.lock().unwrap();
        w.delete_term(Term::from_field_text(self.id, id));
        w.commit()?;
        Ok(())
    }

    /// Commit any pending writes.
    pub fn commit(&self) -> Result<(), SearchError> {
        self.writer.lock().unwrap().commit()?;
        Ok(())
    }
}

fn doc_id(searcher: &tantivy::Searcher, addr: tantivy::DocAddress, id_field: Field) -> String {
    let doc: Result<TantivyDocument, _> = searcher.doc(addr);
    doc.ok()
        .and_then(|d| d.get_first(id_field).map(tantivy::schema::OwnedValue::from))
        .and_then(owned_str)
        .unwrap_or_default()
}

/// Extract an owned string from an `OwnedValue` (the fields are stored text).
fn owned_str(v: tantivy::schema::OwnedValue) -> Option<String> {
    match v {
        tantivy::schema::OwnedValue::Str(s) => Some(s),
        _ => None,
    }
}

fn hits_from(
    top: Vec<(f32, tantivy::DocAddress)>,
    searcher: &tantivy::Searcher,
    id_field: Field,
    body_field: Field,
) -> Vec<SearchHit> {
    top.into_iter()
        .filter_map(|(score, addr)| {
            let doc: TantivyDocument = searcher.doc(addr).ok()?;
            let id = doc
                .get_first(id_field)
                .map(tantivy::schema::OwnedValue::from)
                .and_then(owned_str)?;
            let snippet = doc
                .get_first(body_field)
                .map(tantivy::schema::OwnedValue::from)
                .and_then(owned_str)
                .unwrap_or_default()
                .chars()
                .take(160)
                .collect::<String>();
            Some(SearchHit { id, score, snippet })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_and_queries_fuzzy() {
        let idx = DocumentIndex::in_memory().unwrap();
        idx.put("app/calc", "Calculator", "perform arithmetic and math", "apps").unwrap();
        idx.put("app/editor", "Code Editor", "edit source code with syntax", "apps").unwrap();
        idx.put("app/browser", "Web Browser", "navigate the web pages", "apps").unwrap();
        idx.commit().unwrap();

        let hits = idx.search("arithmetic", 10).unwrap();
        assert_eq!(hits[0].id, "app/calc");

        let fuzzy = idx.search_fuzzy("edtor", 10).unwrap();
        assert_eq!(fuzzy[0].id, "app/editor");
    }

    #[test]
    fn persists_and_deletes_and_exact_id() {
        let dir = tempfile::tempdir().unwrap();
        let idx = DocumentIndex::open(dir.path()).unwrap();
        idx.put("doc/1", "Quarterly Report", "q3 numbers and trends", "files").unwrap();
        idx.commit().unwrap();
        // Release the writer lock before reopening the same directory.
        drop(idx);

        let reloaded = DocumentIndex::open(dir.path()).unwrap();
        assert_eq!(reloaded.search("quarterly", 5).unwrap()[0].id, "doc/1");
        assert!(reloaded.get_by_id("doc/1").unwrap().is_some());

        reloaded.delete("doc/1").unwrap();
        assert!(reloaded.search("quarterly", 5).unwrap().is_empty());
    }

    #[test]
    fn empty_and_blank_queries() {
        let idx = DocumentIndex::in_memory().unwrap();
        assert!(idx.search("", 5).unwrap().is_empty());
        assert!(idx.search_fuzzy("   ", 5).unwrap().is_empty());
    }
}