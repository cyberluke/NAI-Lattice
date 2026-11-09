//! Embedded semantic vector store (Lattice spec §10/§13 "query.vector").
//!
//! In-process Qdrant replacement: an HNSW approximate nearest-neighbour index
//! (hnsw_rs) keyed by external string ids, with cosine similarity. Persists the
//! id→vector catalog as JSON and rebuilds the HNSW on open — no mmap lifetime
//! coupling, so the store is `'static` and trivially shareable behind `Arc`.
//!
//! Durable relational identity stays in the ledger/hot-store; this store owns
//! only the semantic vectors (exactly what Qdrant held).

use std::sync::{Mutex, RwLock};

use hnsw_rs::prelude::*;
use serde::{Deserialize, Serialize};

/// Errors from the semantic store.
#[derive(Debug, thiserror::Error)]
pub enum VectorError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("dimension mismatch: index {index_dim}, given {given_dim}")]
    Dimension { index_dim: usize, given_dim: usize },
    #[error("hnsw: {0}")]
    Hnsw(String),
    #[error("index at capacity {0}")]
    Full(usize),
    #[error("id not found: {0}")]
    NotFound(String),
    #[error("serialize: {0}")]
    Serialize(String),
}

/// One search hit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectorHit {
    pub id: String,
    /// Cosine similarity in [0,1]; 1 = identical.
    pub score: f32,
}

/// Persisted catalog: stable id → dense vector (dimension = all same).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub dim: usize,
    pub points: Vec<(String, Vec<f32>)>,
}

/// The semantic vector store. Shareable behind `Arc`; reads are concurrent,
/// writes serialize on the internal `Mutex`.
pub struct SemanticStore {
    index: RwLock<Hnsw<'static, f32, DistCosine>>,
    catalog: Mutex<Catalog>,
    m: usize,
    ef_construction: usize,
    ef_search: usize,
}

impl SemanticStore {
    /// Create an empty in-memory store.
    pub fn in_memory(dim: usize) -> Self {
        Self::new(dim, Catalog { dim, points: Vec::new() })
    }

    /// Open (or create) a persisted store from `dir/<name>.json`.
    pub fn open(dir: &std::path::Path, name: &str, dim: usize) -> Result<Self, VectorError> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{name}.json"));
        let catalog = if path.exists() {
            serde_json::from_slice(&std::fs::read(&path)?)
                .map_err(|e| VectorError::Serialize(e.to_string()))?
        } else {
            Catalog { dim, points: Vec::new() }
        };
        Ok(Self::new(dim, catalog))
    }

    fn new(_dim: usize, catalog: Catalog) -> Self {
        // Build an empty HNSW (points indexed lazily via `rebuild_locked`).
        let s = Self {
            index: RwLock::new(Hnsw::<f32, DistCosine>::new(16, 4096, 8, 200, DistCosine {})),
            catalog: Mutex::new(catalog),
            m: 16,
            ef_construction: 200,
            ef_search: 100,
        };
        let pts = s.catalog.lock().unwrap().points.clone();
        // If the catalog already has points (e.g. fresh open from disk), index them.
        if !pts.is_empty() {
            s.rebuild_locked(&pts);
        }
        s
    }

    pub fn dim(&self) -> usize {
        self.catalog.lock().unwrap().dim
    }
    pub fn len(&self) -> usize {
        self.catalog.lock().unwrap().points.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn check_dim(&self, v: &[f32]) -> Result<(), VectorError> {
        let dim = self.dim();
        if v.len() != dim {
            return Err(VectorError::Dimension { index_dim: dim, given_dim: v.len() });
        }
        Ok(())
    }

    /// Upsert a vector for `id` (insert or replace).
    pub fn upsert(&self, id: &str, vector: &[f32]) -> Result<(), VectorError> {
        self.check_dim(vector)?;
        let mut cat = self.catalog.lock().unwrap();
        if let Some(slot) = cat.points.iter_mut().find(|(existing, _)| existing == id) {
            slot.1 = vector.to_vec();
            // Clear old HNSW contribution by rebuilding from full catalog.
            self.rebuild_locked(&cat.points);
            return Ok(());
        }
        if cat.points.len() >= cat_capacity(&cat) {
            return Err(VectorError::Full(cat.points.len()));
        }
        cat.points.push((id.to_string(), vector.to_vec()));
        self.rebuild_locked(&cat.points);
        Ok(())
    }

    /// Bulk upsert (used by index rebuild).
    pub fn upsert_many(&self, points: &[(&str, Vec<f32>)]) -> Result<(), VectorError> {
        for (id, v) in points {
            self.upsert(id, v)?;
        }
        Ok(())
    }

    /// Search the `k` nearest neighbours by cosine similarity (highest first).
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>, VectorError> {
        self.check_dim(query)?;
        let cat = self.catalog.lock().unwrap();
        if cat.points.is_empty() {
            return Ok(Vec::new());
        }
        let index = self.index.read().unwrap();
        let neighbours = index.search(query, k.max(1), self.ef_search.max(1));
        let mut hits = Vec::with_capacity(neighbours.len());
        for n in &neighbours {
            let Some((id, _)) = cat.points.get(n.d_id) else {
                continue;
            };
            // DistCosine distance = 1 - cos_sim, so similarity = 1 - distance.
            let sim = (1.0 - n.distance).clamp(0.0, 1.0);
            hits.push(VectorHit { id: id.clone(), score: sim });
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        Ok(hits)
    }

    /// Remove an id.
    pub fn remove(&self, id: &str) -> Result<bool, VectorError> {
        let mut cat = self.catalog.lock().unwrap();
        let before = cat.points.len();
        cat.points.retain(|(existing, _)| existing != id);
        let removed = cat.points.len() != before;
        if removed {
            self.rebuild_locked(&cat.points);
        }
        Ok(removed)
    }

    /// Persist the catalog (rebuilds happen on next open).
    pub fn persist(&self, dir: &std::path::Path, name: &str) -> Result<(), VectorError> {
        std::fs::create_dir_all(dir)?;
        let cat = self.catalog.lock().unwrap().clone();
        let json = serde_json::to_vec_pretty(&cat).map_err(|e| VectorError::Serialize(e.to_string()))?;
        std::fs::write(dir.join(format!("{name}.json")), json)?;
        Ok(())
    }

    /// Clear all points.
    pub fn clear(&self) -> Result<(), VectorError> {
        let mut cat = self.catalog.lock().unwrap();
        cat.points.clear();
        self.rebuild_locked(&cat.points);
        Ok(())
    }

    /// Snapshot of the catalog (for observability).
    pub fn snapshot(&self) -> Catalog {
        self.catalog.lock().unwrap().clone()
    }

    /// Rebuild the HNSW index from `points` (used after every mutation).
    fn rebuild_locked(&self, points: &[(String, Vec<f32>)]) {
        let cap = points.len().max(64);
        let nb_layer = 16usize.min(((cap as f32).ln().max(1.0).trunc()) as usize).max(1);
        let index = Hnsw::<f32, DistCosine>::new(self.m, cap.max(4096), nb_layer, self.ef_construction, DistCosine {});
        for (i, (_, v)) in points.iter().enumerate() {
            index.insert((v.as_slice(), i));
        }
        *self.index.write().unwrap() = index;
    }
}

fn cat_capacity(_cat: &Catalog) -> usize {
    4096
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_and_retrieves_semantically() {
        let store = SemanticStore::in_memory(4);
        store.upsert("entity://file/a", &[1., 0., 0., 0.]).unwrap();
        store.upsert("entity://file/b", &[0., 1., 0., 0.]).unwrap();
        store.upsert("entity://file/c", &[0., 0., 1., 0.]).unwrap();

        let hits = store.search(&[0.99, 0.01, 0., 0.], 3).unwrap();
        assert_eq!(hits[0].id, "entity://file/a");
        // Ordered by descending similarity.
        assert!(hits[0].score >= hits[1].score && hits[1].score >= hits[2].score);
        assert_eq!(store.len(), 3);
    }

    #[test]
    fn rejects_dimension_mismatch_and_is_empty() {
        let store = SemanticStore::in_memory(4);
        assert!(store.search(&[1., 2.], 1).is_err());
        assert!(store.upsert("x", &[1., 2.]).is_err());
        assert!(store.search(&[0., 0., 0., 0.], 3).unwrap().is_empty());
    }

    #[test]
    fn persists_and_reloads_and_upserts_replace() {
        let dir = tempfile::tempdir().unwrap();
        let store = SemanticStore::open(dir.path(), "semantic", 3).unwrap();
        store.upsert("entity://app/calc", &[1., 0., 0.]).unwrap();
        store.upsert("entity://app/editor", &[0., 1., 0.]).unwrap();
        store.persist(dir.path(), "semantic").unwrap();
        assert_eq!(store.search(&[1., 0.01, 0.], 2).unwrap()[0].id, "entity://app/calc");

        let reloaded = SemanticStore::open(dir.path(), "semantic", 3).unwrap();
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded.search(&[1., 0.01, 0.], 2).unwrap()[0].id, "entity://app/calc");

        // Upsert (replace) the vector for calc to point at editor.
        reloaded.upsert("entity://app/calc", &[0., 1., 0.]).unwrap();
        let hits = reloaded.search(&[1., 0., 0.], 2).unwrap();
        assert_eq!(hits[0].id, "entity://app/editor");
    }

    #[test]
    fn removes() {
        let store = SemanticStore::in_memory(3);
        store.upsert("k1", &[1., 0., 0.]).unwrap();
        store.upsert("k2", &[0., 1., 0.]).unwrap();
        assert!(store.remove("k1").unwrap());
        assert!(!store.remove("k1").unwrap());
        assert_eq!(store.len(), 1);
    }
}