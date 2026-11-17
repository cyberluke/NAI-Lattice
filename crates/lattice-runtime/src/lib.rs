//! NAI Lattice runtime (Lattice spec §6, §7, §8).
//!
//! Wires the core subsystems — canonical ledger, hot store, CAS, AAF resolver
//! — into a single object the host process (and CLI) can drive, and implements
//! the NAF/1 minimum-capability dispatch used by both the named-pipe host and
//! the in-process/test path.

use std::path::PathBuf;
use std::sync::Arc;

use lattice_aaf::AafResolver;
use lattice_cas::object::ObjectStore;
use lattice_core::{CanonicalEvent, Durability, EventId, GlobalId};
use lattice_hotstore::{HotKey, HotValue, HotStore};
use lattice_ledger::Ledger;
use tracing::info;

pub mod collection;

use collection::CollectionStore;

/// The NAF/1 capability names this runtime ACTUALLY serves (kept in lockstep
/// with the `match` in `dispatch_async`). Truthful advertisement: clients must
/// only be told about methods that exist, so capability negotiation and §19
/// snapshots reflect reality rather than a spec floor.
const RUNTIME_SERVED_CAPABILITIES: &[&str] = &[
    "runtime.health",
    "runtime.describe",
    "capabilities.list",
    "event.append",
    "event.append_batch",
    "event.read_range",
    "artifact.put",
    "artifact.open",
    "artifact.verify",
    "entity.get",
    // Collection projection + transaction + lease surfaces (Lattice
    // `collection.v1`/`transaction.v1`/`lease.v1`) power Mastra canonical
    // persistence (`LatticeMastraStore`) instead of LibSQL.
    "collection.schema.ensure",
    "collection.find_one",
    "collection.find_many",
    "collection.count",
    "collection.insert",
    "collection.upsert",
    "collection.update_many",
    "collection.delete_many",
    "collection.update_atomic",
    "transaction.begin",
    "transaction.commit",
    "transaction.rollback",
    "lease.claim",
    "lease.renew",
    "lease.release",
    "query.vector",
    "semantic.index",
    "query.search",
    "query.federated",
];

/// Runtime configuration.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Per-user data root (default `%LOCALAPPDATA%\NAI\Lattice\v1`).
    pub root: PathBuf,
    /// Ledger target segment size.
    pub ledger_segment_bytes: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        // Optional env override lets an isolated accept/acceptance singleton run
        // against a disposable root instead of the live per-user fabric root.
        if let Some(root) = std::env::var_os("NAI_LATTICE_ROOT") {
            return Self { root: PathBuf::from(root), ledger_segment_bytes: 128 * 1024 * 1024 };
        }
        let local = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            root: local.join("NAI").join("Lattice").join("v1"),
            ledger_segment_bytes: 128 * 1024 * 1024,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("ledger: {0}")]
    Ledger(#[from] lattice_ledger::LedgerError),
    #[error("cas: {0}")]
    Cas(#[from] lattice_cas::CasError),
    #[error("hotstore: {0}")]
    HotStore(#[from] lattice_hotstore::HotStoreError),
    #[error("aaf: {0}")]
    Aaf(#[from] lattice_aaf::AafError),
    #[error("method not found: {0}")]
    MethodNotFound(String),
    #[error("bad args for {method}: {detail}")]
    BadArgs { method: String, detail: String },
    #[error("durability downgrade for {0:?}")]
    DurabilityUnavailable(Durability),
    #[error("backend: {0}")]
    Backend(String),
    #[error("vector store: {0}")]
    Vector(#[from] lattice_vector::VectorError),
    #[error("search index: {0}")]
    Search(#[from] lattice_search::SearchError),
}

impl From<String> for RuntimeError {
    fn from(e: String) -> Self {
        RuntimeError::Backend(e)
    }
}

/// The assembled Lattice runtime.
pub struct LatticeRuntime {
    pub config: RuntimeConfig,
    pub ledger: Arc<Ledger>,
    pub cas: Arc<ObjectStore>,
    pub hot: Arc<dyn HotStore>,
    pub aaf: Arc<AafResolver>,
    pub instance_id: String,
    /// Semantic HNSW vector store (Qdrant-equivalent recall).
    pub vector_store: lattice_vector::SemanticStore,
    /// Fuzzy full-text index.
    pub search_index: lattice_search::DocumentIndex,
    /// Collection projection store (Mastra canonical persistence surface),
    /// guarded by a single lock for serialized single-writer semantics.
    pub collections: tokio::sync::Mutex<CollectionStore>,
    /// Internal async executor owned by the runtime. Async work runs on this
    /// runtime from a dedicated thread so callers never see a runtime conflict.
    rt: tokio::runtime::Runtime,
}

impl LatticeRuntime {
    /// Open the full stack (ledger + hotstore + CAS + AAF) at `config`.
    pub fn open(config: RuntimeConfig) -> Result<Self, RuntimeError> {
        let local = &config.root;

        // Canonical ledger (opened with crash recovery + projection catch-up).
        let ledger_cfg = lattice_ledger::LedgerConfig {
            root: local.clone(),
            target_segment_bytes: config.ledger_segment_bytes,
            batched_latency_ms: 25,
        };
        let ledger = Arc::new(Ledger::open(ledger_cfg)?);
        // BatchedDurable frames are flushed through the ledger's OWN persistent
        // writer via `Ledger::flush_batched` — one authoritative write path
        // (LEDGER-002/003). No external flush hook is installed.

        // Content-addressed object store.
        let cas = Arc::new(ObjectStore::new(local.join("cas"))?);

        // SurrealKV hot store (production adapter).
        let sk_opts = lattice_hotstore_surrealkv::SurrealKvOptions::new(local.join("hot"));
        let hot: Arc<dyn HotStore> =
            Arc::new(lattice_hotstore_surrealkv::SurrealKvHotStore::open(sk_opts)?);

        // AAF resolver over the owned CAS.
        let aaf = Arc::new(AafResolver::new(cas.clone()));

        // Semantic HNSW vector store (Qdrant-equivalent recall).
        let vector_store = lattice_vector::SemanticStore::open(
            &local.join("vector"),
            "semantic",
            1024, // Qwen3-Embedding default dim; callers upsert at runtime.
        )?;

        // Fuzzy full-text index (Tantivy).
        let search_index = lattice_search::DocumentIndex::open(&local.join("search"))?;

        let instance_id = format!("nai-lattice-{}", uuid::Uuid::now_v7().simple());

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| RuntimeError::Backend(format!("rt: {e}")))?;

        info!(instance = %instance_id, "Lattice runtime opened");
        Ok(Self {
            config,
            ledger,
            cas,
            hot,
            aaf,
            instance_id,
            vector_store,
            search_index,
            collections: tokio::sync::Mutex::new(CollectionStore::new()),
            rt,
        })
    }

    /// Run an async body on the runtime's own executor from a dedicated thread,
    /// so a caller that is itself inside a Tokio runtime never conflicts.
    #[inline]
    fn on_rt<F, Fut, T>(&self, f: F) -> T
    where
        F: FnOnce() -> Fut + Send,
        Fut: std::future::Future<Output = T>,
        T: Send,
    {
        match tokio::runtime::Handle::try_current() {
            // Not inside a caller runtime: block directly on ours.
            Err(_) => self.rt.block_on(f()),
            // Inside a caller runtime: run on a dedicated thread with our runtime.
            Ok(_) => {
                let rt = self.rt.handle().clone();
                std::thread::scope(|s| {
                    s.spawn(move || rt.block_on(f())).join().expect("rt thread")
                })
            }
        }
    }

    /// Health snapshot (spec §8 `runtime.health`). Synchronous.
    pub fn health(&self) -> serde_json::Value {
        self.on_rt(|| async { self.health_async().await })
    }

    async fn health_async(&self) -> serde_json::Value {
        let last = self.ledger.last_sequence().await;
        serde_json::json!({
            "state": "healthy",
            "runtime_instance_id": self.instance_id,
            "ledger_high_water_mark": last,
            "provider_health": self.aaf.provider_health(),
            "hot_store": "surreal",
        })
    }

    /// The NAF/1 capabilities this runtime actually serves via `dispatch_async`.
    /// This is TRUTHFUL capability advertisement (§19): a capability is only
    /// listed here once a `dispatch_async` arm implements it. The protocol's
    /// `MINIMUM_CAPABILITIES` is a spec floor, NOT a claim of implementation —
    /// advertising a method we do not serve would let Mastra/other clients
    /// negotiate a phantom surface. Keep these two lists in lockstep with the
    /// `match` in `dispatch_async`.
    pub fn capabilities(&self) -> Vec<&'static str> {
        RUNTIME_SERVED_CAPABILITIES.to_vec()
    }

    /// Flush any pending group-committed (batched) ledger frames to the
    /// durability boundary. Safe to call from any thread.
    pub fn flush_hotstore(&self) {
        // Flush pending BatchedDurable ledger frames through the ledger's own
        // persistent writer (one authoritative write path) via the runtime's
        // internal executor.
        let _ = self.rt.block_on(self.ledger.flush_batched());
        let _ = self.hot.flush();
    }

    /// Dispatch one NAF method synchronously. Runs the async body on this
    /// runtime's own executor from a safe thread context.
    pub fn dispatch(
        &self,
        method: &str,
        args_json: &[u8],
        producer: &str,
        idempotency_key: Option<&str>,
    ) -> Result<Vec<u8>, RuntimeError> {
        let method = method.to_string();
        let args_json = args_json.to_vec();
        let producer = producer.to_string();
        let idempotency_key = idempotency_key.map(str::to_string);
        self.on_rt(move || async move {
            self.dispatch_async(&method, &args_json, &producer, idempotency_key.as_deref())
                .await
        })
    }

    /// Async dispatch body.
    async fn dispatch_async(
        &self,
        method: &str,
        args_json: &[u8],
        producer: &str,
        idempotency_key: Option<&str>,
    ) -> Result<Vec<u8>, RuntimeError> {
        let args: serde_json::Value = serde_json::from_slice(args_json)
            .map_err(|e| RuntimeError::BadArgs {
                method: method.to_string(),
                detail: e.to_string(),
            })?;
        match method {
            "runtime.health" => Ok(serde_json::to_vec(&self.health_async().await).unwrap()),
            "runtime.describe" => Ok(serde_json::to_vec(&serde_json::json!({
                "name": "NAI Ambient Fabric",
                "naf_version": 1,
                "storage_schema_version": 1,
                "runtime_instance_id": self.instance_id,
                "root": self.config.root.display().to_string(),
            }))
            .unwrap()),
            "capabilities.list" => Ok(serde_json::to_vec(
                &self.capabilities().iter().map(|s| *s).collect::<Vec<_>>(),
            )
            .unwrap()),
            "event.append" => self.on_event_append(&args, producer, idempotency_key).await,
            // Read back the canonical ledger. The capability contract advertises
            // `event.read_range` (MINIMUM_CAPABILITIES) but the committed runtime
            // lacked it — this closes that gap (AAF: the read delta that was
            // already declared). `event.scan` is an alias for a full-list scan.
            "event.read_range" | "event.range" => self.on_event_range(&args).await,
            // `event.append_batch` — deterministic per-item appends under the
            // same capability. Each item may carry its own idempotency key so
            // replay never duplicates an individual event (NAF-009).
            "event.append_batch" => self.on_event_append_batch(&args, producer, idempotency_key).await,
            "artifact.put" => self.on_artifact_put(&args).await,
            "artifact.open" => self.on_artifact_open(&args).await,
            "artifact.verify" => self.on_artifact_verify(&args).await,
            "entity.get" => self.on_entity_get(&args).await,
            "query.vector" => Self::on_query_vector(&self.vector_store, &args),
            "semantic.index" => Self::on_semantic_index(&self.vector_store, &self.search_index, &args),
            "query.search" => Self::on_query_search(&self.search_index, &args),
            "query.federated" => Self::on_query_federated(&self.search_index, &self.vector_store, &args),
            // Collection projection surface (`collection.v1`), powering Mastra
            // canonical persistence. These are transactional + single-writer.
            "collection.schema.ensure" => self.on_collection_schema_ensure(&args).await,
            "collection.find_one" => self.on_collection_find_one(&args).await,
            "collection.find_many" => self.on_collection_find_many(&args).await,
            "collection.count" => self.on_collection_count(&args).await,
            "collection.insert" => self.on_collection_upsert(&args, false).await,
            "collection.upsert" => self.on_collection_upsert(&args, true).await,
            "collection.update_many" => self.on_collection_update_many(&args).await,
            "collection.delete_many" => self.on_collection_delete_many(&args).await,
            "collection.update_atomic" => self.on_collection_update_atomic(&args).await,
            // Transaction sessions (`transaction.v1`).
            "transaction.begin" => Ok(serde_json::to_vec(&self.on_tx_begin().await).unwrap()),
            "transaction.rollback" => self.on_tx_rollback(&args).await,
            "transaction.commit" => self.on_tx_commit(&args).await,
            // Workflow run ownership / fencing (`lease.v1`).
            "lease.claim" => self.on_lease_claim(&args).await,
            "lease.renew" => self.on_lease_renew(&args).await,
            "lease.release" => self.on_lease_release(&args).await,
            other => Err(RuntimeError::MethodNotFound(other.to_string())),
        }
    }

    /// `query.vector { vector: [f32], limit }` — HNSW kNN cosine search.
    fn on_query_vector(store: &lattice_vector::SemanticStore, args: &serde_json::Value) -> Result<Vec<u8>, RuntimeError> {
        let vector = args
            .get("vector")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.iter().map(|x| x.as_f64()).collect::<Option<Vec<_>>>())
            .map(|v| v.into_iter().map(|x| x as f32).collect::<Vec<f32>>())
            .ok_or_else(|| RuntimeError::BadArgs {
                method: "query.vector".into(),
                detail: "missing vector".into(),
            })?;
        let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(10) as usize;
        let hits = store.search(&vector, limit)?;
        Ok(serde_json::to_vec(&serde_json::json!({ "hits": hits })).unwrap())
    }

    /// `semantic.index { id, vector, title?, body? }` — upsert a vector (and
    /// optionally index its text) for launcher/files semantic + fuzzy recall.
    fn on_semantic_index(
        store: &lattice_vector::SemanticStore,
        search: &lattice_search::DocumentIndex,
        args: &serde_json::Value,
    ) -> Result<Vec<u8>, RuntimeError> {
        let id = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RuntimeError::BadArgs {
                method: "semantic.index".into(),
                detail: "missing id".into(),
            })?
            .to_string();
        if let Some(vector) = args.get("vector").and_then(|v| v.as_array()) {
            let v: Vec<f32> = vector
                .iter()
                .map(|x| x.as_f64().unwrap_or(0.0) as f32)
                .collect();
            store.upsert(&id, &v)?;
        }
        if let Some(title) = args.get("title").and_then(|v| v.as_str()) {
            let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");
            let namespace = args.get("namespace").and_then(|v| v.as_str()).unwrap_or("default");
            search.put(&id, title, body, namespace)?;
        }
        Ok(serde_json::to_vec(&serde_json::json!({ "id": id, "indexed": true })).unwrap())
    }

    /// `query.search { query, limit, fuzzy? }` — fuzzy full-text search.
    fn on_query_search(index: &lattice_search::DocumentIndex, args: &serde_json::Value) -> Result<Vec<u8>, RuntimeError> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RuntimeError::BadArgs {
                method: "query.search".into(),
                detail: "missing query".into(),
            })?;
        let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(10) as usize;
        let fuzzy = args.get("fuzzy").and_then(|f| f.as_bool()).unwrap_or(false);
        let hits = if fuzzy {
            index.search_fuzzy(query, limit)?
        } else {
            index.search(query, limit)?
        };
        Ok(serde_json::to_vec(&serde_json::json!({ "hits": hits })).unwrap())
    }

    /// `query.federated { query, limit, vector? }` — fuses fuzzy text hits and
    /// optional semantic vector hits into one ranked list (AAF federation of
    /// the LUSI-leveraged recall planes).
    fn on_query_federated(
        search: &lattice_search::DocumentIndex,
        vector: &lattice_vector::SemanticStore,
        args: &serde_json::Value,
    ) -> Result<Vec<u8>, RuntimeError> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(10) as usize;
        let mut fused: Vec<serde_json::Value> = Vec::new();
        if !query.is_empty() {
            for h in search.search(query, limit)? {
                fused.push(serde_json::json!({
                    "id": h.id, "score": h.score, "kind": "text", "snippet": h.snippet,
                }));
            }
            for h in search.search_fuzzy(query, limit)? {
                if !fused.iter().any(|x| x["id"] == h.id) {
                    fused.push(serde_json::json!({
                        "id": h.id, "score": h.score, "kind": "fuzzy", "snippet": h.snippet,
                    }));
                }
            }
        }
        if let Some(vec) = args.get("vector").and_then(|v| v.as_array()) {
            let v: Vec<f32> = vec.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect();
            if !v.is_empty() {
                for h in vector.search(&v, limit)? {
                    if !fused.iter().any(|x| x["id"] == h.id) {
                        fused.push(serde_json::json!({
                            "id": h.id, "score": h.score, "kind": "vector",
                        }));
                    }
                }
            }
        }
        fused.sort_by(|a, b| b["score"].as_f64().unwrap_or(0.0).total_cmp(&a["score"].as_f64().unwrap_or(0.0)));
        fused.truncate(limit);
        Ok(serde_json::to_vec(&serde_json::json!({ "fused": fused })).unwrap())
    }

    async fn on_event_append(
        &self,
        args: &serde_json::Value,
        producer: &str,
        idem: Option<&str>,
    ) -> Result<Vec<u8>, RuntimeError> {
        let event = build_event_from_json(args, producer)?;
        // ProjectionOnly default unless the caller opts into Durable.
        let durability = match args.get("durability").and_then(|d| d.as_str()) {
            Some("durable") => Durability::Durable,
            Some("batched") => Durability::BatchedDurable,
            _ => Durability::ProjectionOnly,
        };
        if let Some(key) = idem {
            let idem_key = HotKey(format!("idempotency/{producer}/{key}").into_bytes());
            if self.hot.exists(&idem_key)? {
                // Retry with the same idempotency key returns the prior event id.
                let prior = self.hot.get(&idem_key)?.map(|v| String::from_utf8_lossy(&v.0).to_string());
                return Ok(serde_json::to_vec(&serde_json::json!({
                    "event_id": prior,
                    "duplicated": true
                }))
                .unwrap());
            }
        }
        let event_id = self.ledger.append(event, durability).await?;
        if let Some(key) = idem {
            self.hot.put(
                &HotKey(format!("idempotency/{producer}/{key}").into_bytes()),
                &HotValue(event_id.to_string().into_bytes()),
            )?;
        }
        Ok(serde_json::to_vec(&serde_json::json!({
            "event_id": event_id.to_string(),
            "duplicated": false
        }))
        .unwrap())
    }

    /// `event.append_batch { events: [...] }` — append several canonical events
    /// in one call. Each item mirrors `event.append` args and MAY carry its own
    /// `idempotency_key` (note: the single key here applies to the whole batch
    /// envelope; per-item keys are preferred for exact dedup). Commits happen
    /// per-item through the same `ledger.append` path, so ordering is the order
    /// of the array and each append gets a deterministic runtime_sequence.
    async fn on_event_append_batch(
        &self,
        args: &serde_json::Value,
        producer: &str,
        _batch_idem: Option<&str>,
    ) -> Result<Vec<u8>, RuntimeError> {
        let events = args.get("events").and_then(|e| e.as_array()).ok_or_else(|| {
            RuntimeError::BadArgs { method: "event.append_batch".into(), detail: "missing events array".into() }
        })?;
        let mut ids = Vec::with_capacity(events.len());
        for item in events.iter() {
            // Envelope default durability mirrors `event.append` (ProjectionOnly unless
            // the caller opts into Durable / Batched).
            let durability = match item.get("durability").and_then(|d| d.as_str()) {
                Some("durable") => Durability::Durable,
                Some("batched") => Durability::BatchedDurable,
                _ => Durability::ProjectionOnly,
            };
            let ev = build_event_from_json(item, producer)?;
            let idem = item.get("idempotency_key").and_then(|k| k.as_str());
            if let Some(key) = idem {
                let ik = HotKey(format!("idempotency/{producer}/{key}").into_bytes());
                if self.hot.exists(&ik)? {
                    let prior = self
                        .hot
                        .get(&ik)?
                        .map(|v| String::from_utf8_lossy(&v.0).to_string());
                    ids.push(serde_json::json!({
                        "event_id": prior.unwrap_or_default(),
                        "duplicated": true,
                        "order": ids.len(),
                    }));
                    continue;
                }
            }
            let event_id = self.ledger.append(ev, durability).await?;
            if let Some(key) = idem {
                self.hot.put(
                    &HotKey(format!("idempotency/{producer}/{key}").into_bytes()),
                    &HotValue(event_id.to_string().into_bytes()),
                )?;
            }
            ids.push(serde_json::json!({
                "event_id": event_id.to_string(),
                "duplicated": false,
                "order": ids.len(),
            }));
        }
        Ok(serde_json::to_vec(&serde_json::json!({ "appended": ids })).unwrap())
    }

    /// Append a durable canonical mutation delta to the ledger and, only on the
    /// durable ACK, apply it to the collection projection. Returns the event id.
    /// This is THE commit point: the ledger append is durable (fsync) and the
    /// projection is only applied afterwards, so a crash never leaves a half-applied
    /// mutation and the ledger is the authoritative source of truth.
    async fn commit_mutation(
        &self,
        collection: &str,
        delta: &serde_json::Value,
        producer: &str,
        _idempotency_key: Option<&str>,
    ) -> Result<serde_json::Value, RuntimeError> {
        let ev = build_event_from_json(
            &serde_json::json!({
                "namespace": format!("nai.mastra.collection.{collection}"),
                "event_type": "schema.mutation",
                "entity_id": format!("nai.mastra.collection.{collection}"),
                "payload": delta,
            }),
            producer,
        )?;
        // Force Durable regardless of the event default: collection mutations that
        // back Mastra state must be fsynced before we acknowledge them. `append`
        // consumes the event to build/append the frame; we only need the returned
        // event id for the ACK.
        let event_id = self.ledger.append(ev, Durability::Durable).await?;
        // Apply to the projection now that the durable ACK is in.
        {
            let mut store = self.collections.lock().await;
            store.apply_delta(collection, delta)?;
        }
        Ok(serde_json::json!({
            "event_id": event_id.to_string(),
            "collection": collection,
            "acknowledged": true,
        }))
    }

    /// `collection.schema.ensure { schema: {...} }` — register/additively evolve
    /// a Mastra collection schema.
    async fn on_collection_schema_ensure(&self, args: &serde_json::Value) -> Result<Vec<u8>, RuntimeError> {
        let schema = args
            .get("schema")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        if schema.is_null() {
            // Allow passing the schema fields at the top level (`{name, columns,...}`).
            let mut store = self.collections.lock().await;
            let out = store.ensure_schema(args)?;
            return Ok(serde_json::to_vec(&out).unwrap());
        }
        let mut store = self.collections.lock().await;
        let out = store.ensure_schema(&schema)?;
        Ok(serde_json::to_vec(&out).unwrap())
    }

    /// `collection.schema.ensure` with the top-level shape also matches arity.
    async fn on_collection_find_one(&self, args: &serde_json::Value) -> Result<Vec<u8>, RuntimeError> {
        let collection = args.get("collection").and_then(|c| c.as_str()).ok_or_else(|| {
            RuntimeError::BadArgs { method: "collection.find_one".into(), detail: "missing collection".into() }
        })?;
        let where_json = args.get("where").cloned().unwrap_or(serde_json::json!({}));
        let store = self.collections.lock().await;
        let row = store.find_one(collection, &where_json)?;
        Ok(serde_json::to_vec(&serde_json::json!({ "row": row })).unwrap())
    }

    /// `collection.find_many { collection, where?, order_by?, limit? }`.
    async fn on_collection_find_many(&self, args: &serde_json::Value) -> Result<Vec<u8>, RuntimeError> {
        let collection = args.get("collection").and_then(|c| c.as_str()).ok_or_else(|| {
            RuntimeError::BadArgs { method: "collection.find_many".into(), detail: "missing collection".into() }
        })?;
        let where_json = args.get("where").cloned().unwrap_or(serde_json::json!({}));
        let order_by = args.get("order_by").cloned().unwrap_or(serde_json::json!([]));
        let limit = args.get("limit").and_then(|l| l.as_u64()).map(|l| l as usize);
        let store = self.collections.lock().await;
        let rows = store.find_many(collection, &where_json, &order_by, limit)?;
        Ok(serde_json::to_vec(&serde_json::json!({ "rows": rows })).unwrap())
    }

    /// `collection.count { collection, where? }`.
    async fn on_collection_count(&self, args: &serde_json::Value) -> Result<Vec<u8>, RuntimeError> {
        let collection = args.get("collection").and_then(|c| c.as_str()).ok_or_else(|| {
            RuntimeError::BadArgs { method: "collection.count".into(), detail: "missing collection".into() }
        })?;
        let where_json = args.get("where").cloned().unwrap_or(serde_json::json!({}));
        let store = self.collections.lock().await;
        let count = store.count(collection, &where_json)?;
        Ok(serde_json::to_vec(&serde_json::json!({ "count": count })).unwrap())
    }

    /// `collection.insert` / `collection.upsert { collection, row, conflict_keys? }`.
    async fn on_collection_upsert(&self, args: &serde_json::Value, is_upsert: bool) -> Result<Vec<u8>, RuntimeError> {
        let collection = args.get("collection").and_then(|c| c.as_str()).ok_or_else(|| {
            RuntimeError::BadArgs { method: "collection.upsert".into(), detail: "missing collection".into() }
        })?;
        let row = args.get("row").cloned().filter(|v| !v.is_null()).ok_or_else(|| {
            RuntimeError::BadArgs { method: "collection.upsert".into(), detail: "missing row".into() }
        })?;
        let conflict: Vec<String> = args
            .get("conflict_keys")
            .and_then(|c| c.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let tx_id = args.get("tx_id").and_then(|t| t.as_str()).map(str::to_string);
        let mut store = self.collections.lock().await;
        let delta = store.plan_upsert(collection, &row, &conflict)?;
        if let Some(tx) = tx_id {
            store.stage(&tx, collection, delta)?;
            return Ok(serde_json::to_vec(&serde_json::json!({ "staged": true, "tx_id": tx })).unwrap());
        }
        let _ = is_upsert;
        drop(store);
        let out = self
            .commit_mutation(collection, &delta, "nai.npilot.mastra", None)
            .await?;
        Ok(serde_json::to_vec(&out).unwrap())
    }

    /// `collection.update_many { collection, where?, set }`.
    async fn on_collection_update_many(&self, args: &serde_json::Value) -> Result<Vec<u8>, RuntimeError> {
        let collection = args.get("collection").and_then(|c| c.as_str()).ok_or_else(|| {
            RuntimeError::BadArgs { method: "collection.update_many".into(), detail: "missing collection".into() }
        })?;
        let where_json = args.get("where").cloned().unwrap_or(serde_json::json!({}));
        let set = args.get("set").cloned();
        let tx_id = args.get("tx_id").and_then(|t| t.as_str()).map(str::to_string);
        let mut store = self.collections.lock().await;
        let delta = store.plan_update_many(collection, &where_json, set.as_ref())?;
        if let Some(tx) = tx_id {
            store.stage(&tx, collection, delta)?;
            return Ok(serde_json::to_vec(&serde_json::json!({ "staged": true, "tx_id": tx })).unwrap());
        }
        drop(store);
        let out = self
            .commit_mutation(collection, &delta, "nai.npilot.mastra", None)
            .await?;
        Ok(serde_json::to_vec(&out).unwrap())
    }

    /// `collection.delete_many { collection, where? }`.
    async fn on_collection_delete_many(&self, args: &serde_json::Value) -> Result<Vec<u8>, RuntimeError> {
        let collection = args.get("collection").and_then(|c| c.as_str()).ok_or_else(|| {
            RuntimeError::BadArgs { method: "collection.delete_many".into(), detail: "missing collection".into() }
        })?;
        let where_json = args.get("where").cloned().unwrap_or(serde_json::json!({}));
        let tx_id = args.get("tx_id").and_then(|t| t.as_str()).map(str::to_string);
        let mut store = self.collections.lock().await;
        let delta = store.plan_update_many(collection, &where_json, None)?;
        if let Some(tx) = tx_id {
            store.stage(&tx, collection, delta)?;
            return Ok(serde_json::to_vec(&serde_json::json!({ "staged": true, "tx_id": tx })).unwrap());
        }
        drop(store);
        let out = self
            .commit_mutation(collection, &delta, "nai.npilot.mastra", None)
            .await?;
        Ok(serde_json::to_vec(&out).unwrap())
    }

    /// `collection.update_atomic { collection, where?, set }`.
    /// True read-modify-write: the collection lock is held across plan + durable
    /// commit + projection apply, so a concurrent `update_atomic` (or any other
    /// collection writer) on the same row can never interleave and lose an update.
    /// This is the same single-writer isolation Mastra's LibSQL backend documents.
    async fn on_collection_update_atomic(&self, args: &serde_json::Value) -> Result<Vec<u8>, RuntimeError> {
        let collection = args.get("collection").and_then(|c| c.as_str()).ok_or_else(|| {
            RuntimeError::BadArgs { method: "collection.update_atomic".into(), detail: "missing collection".into() }
        })?;
        let where_json = args.get("where").cloned().unwrap_or(serde_json::json!({}));
        // The reducer is expressed declaratively: `set` columns (the caller computed
        // the delta from a `findOne` under an estimate; the host re-reads under the
        // lock and applies exactly the supplied columns).
        let set = args.get("set").cloned().unwrap_or(serde_json::json!({}));
        // Hold the single collection lock across the whole RMW: plan, durable
        // ledger append, and projection apply. No other collection writer can
        // interleave on the same row, so no update is ever lost.
        let mut store = self.collections.lock().await;
        let delta = store.plan_update_many(collection, &where_json, Some(&set))?;
        let ev = build_event_from_json(
            &serde_json::json!({
                "namespace": format!("nai.mastra.collection.{collection}"),
                "event_type": "schema.mutation",
                "entity_id": format!("nai.mastra.collection.{collection}"),
                "payload": delta,
            }),
            "nai.npilot.mastra",
        )?;
        let event_id = self.ledger.append(ev, Durability::Durable).await?;
        store.apply_delta(collection, &delta)?;
        drop(store);
        Ok(serde_json::to_vec(&serde_json::json!({
            "event_id": event_id.to_string(),
            "collection": collection,
            "acknowledged": true,
        }))
        .unwrap())
    }

    /// `transaction.begin {}` → `{ tx_id }`.
    async fn on_tx_begin(&self) -> serde_json::Value {
        let mut store = self.collections.lock().await;
        let tx_id = store.begin();
        serde_json::json!({ "tx_id": tx_id })
    }

    /// `transaction.rollback { tx_id }`.
    async fn on_tx_rollback(&self, args: &serde_json::Value) -> Result<Vec<u8>, RuntimeError> {
        let tx_id = args.get("tx_id").and_then(|t| t.as_str()).ok_or_else(|| {
            RuntimeError::BadArgs { method: "transaction.rollback".into(), detail: "missing tx_id".into() }
        })?;
        let mut store = self.collections.lock().await;
        store.rollback(tx_id)?;
        Ok(serde_json::to_vec(&serde_json::json!({ "rolled_back": true })).unwrap())
    }

    /// `transaction.commit { tx_id }` — atomically apply all staged deltas as one
    /// durable ledger event, then apply to the projections.
    async fn on_tx_commit(&self, args: &serde_json::Value) -> Result<Vec<u8>, RuntimeError> {
        let tx_id = args.get("tx_id").and_then(|t| t.as_str()).ok_or_else(|| {
            RuntimeError::BadArgs { method: "transaction.commit".into(), detail: "missing tx_id".into() }
        })?;
        let mut store = self.collections.lock().await;
        let deltas = store.commit_plan(tx_id)?;
        if deltas.is_empty() {
            return Ok(serde_json::to_vec(&serde_json::json!({ "committed": true, "events": [] })).unwrap());
        }
        // Group all deltas into one canonical "tx.commit" ledger event.
        let groups: std::collections::BTreeMap<String, Vec<&serde_json::Value>> = {
            let mut m: std::collections::BTreeMap<String, Vec<&serde_json::Value>> = std::collections::BTreeMap::new();
            for (c, d) in &deltas {
                m.entry(c.clone()).or_default().push(d);
            }
            m
        };
        let payload = serde_json::json!({ "collections": groups.iter().map(|(c, ds)| {
            serde_json::json!({ "collection": c, "deltas": ds })
        }).collect::<Vec<_>>() });
        // Build + append one durable event carrying the whole transaction.
        let ev = build_event_from_json(
            &serde_json::json!({
                "namespace": "nai.mastra.tx",
                "event_type": "tx.commit",
                "entity_id": format!("nai.mastra.tx.{tx_id}"),
                "payload": payload,
            }),
            "nai.npilot.mastra",
        )?;
        let event_id = self.ledger.append(ev, Durability::Durable).await?;
        // Apply each delta to the projection after the durable ACK.
        for (c, d) in &deltas {
            store.apply_delta(c, d)?;
        }
        Ok(serde_json::to_vec(&serde_json::json!({
            "committed": true,
            "event_id": event_id.to_string(),
            "tx_id": tx_id,
        }))
        .unwrap())
    }

    /// `lease.claim { lease_id, owner_id, ttl_ms, now_ms? }`.
    async fn on_lease_claim(&self, args: &serde_json::Value) -> Result<Vec<u8>, RuntimeError> {
        let lease_id = args.get("lease_id").and_then(|x| x.as_str()).ok_or_else(|| {
            RuntimeError::BadArgs { method: "lease.claim".into(), detail: "missing lease_id".into() }
        })?;
        let owner_id = args.get("owner_id").and_then(|x| x.as_str()).ok_or_else(|| {
            RuntimeError::BadArgs { method: "lease.claim".into(), detail: "missing owner_id".into() }
        })?;
        let ttl_ms = args.get("ttl_ms").and_then(|x| x.as_i64()).unwrap_or(30_000);
        let now_ms = args.get("now_ms").and_then(|x| x.as_i64()).unwrap_or_else(now_ms);
        let mut store = self.collections.lock().await;
        let out = store.lease_claim(lease_id, owner_id, ttl_ms, now_ms)?;
        Ok(serde_json::to_vec(&out).unwrap())
    }

    /// `lease.renew { lease_id, owner_id, ttl_ms, now_ms? }`.
    async fn on_lease_renew(&self, args: &serde_json::Value) -> Result<Vec<u8>, RuntimeError> {
        let lease_id = args.get("lease_id").and_then(|x| x.as_str()).ok_or_else(|| {
            RuntimeError::BadArgs { method: "lease.renew".into(), detail: "missing lease_id".into() }
        })?;
        let owner_id = args.get("owner_id").and_then(|x| x.as_str()).ok_or_else(|| {
            RuntimeError::BadArgs { method: "lease.renew".into(), detail: "missing owner_id".into() }
        })?;
        let ttl_ms = args.get("ttl_ms").and_then(|x| x.as_i64()).unwrap_or(30_000);
        let now_ms = args.get("now_ms").and_then(|x| x.as_i64()).unwrap_or_else(now_ms);
        let mut store = self.collections.lock().await;
        let out = store.lease_renew(lease_id, owner_id, ttl_ms, now_ms)?;
        Ok(serde_json::to_vec(&out).unwrap())
    }

    /// `lease.release { lease_id, owner_id }`.
    async fn on_lease_release(&self, args: &serde_json::Value) -> Result<Vec<u8>, RuntimeError> {
        let lease_id = args.get("lease_id").and_then(|x| x.as_str()).ok_or_else(|| {
            RuntimeError::BadArgs { method: "lease.release".into(), detail: "missing lease_id".into() }
        })?;
        let owner_id = args.get("owner_id").and_then(|x| x.as_str()).ok_or_else(|| {
            RuntimeError::BadArgs { method: "lease.release".into(), detail: "missing owner_id".into() }
        })?;
        let mut store = self.collections.lock().await;
        let out = store.lease_release(lease_id, owner_id)?;
        Ok(serde_json::to_vec(&out).unwrap())
    }

    /// `event.range { after?, limit?, namespace?, event_type?, entity_id?,
    /// correlation_id? }` — fabric re-read over the canonical ledger. Returns
    /// events in ascending runtime_sequence order. This is how projections
    /// rebuild and how clients verify durable imports (§8/§9 re-read delta).
    async fn on_event_range(&self, args: &serde_json::Value) -> Result<Vec<u8>, RuntimeError> {
        use lattice_ledger::EventScanFilter;
        let tail = args.get("tail").and_then(|v| v.as_bool()).unwrap_or(false);
        let requested_limit = args.get("limit")
            .and_then(|l| l.as_u64()).unwrap_or(200).clamp(1, 10_000);
        let filter = EventScanFilter {
            after: args.get("after").and_then(|a| a.as_u64()),
            // A tail request asks for LAST N, not FIRST N. The ledger scan is
            // ascending, so only the runtime-facing projection applies the
            // reverse boundary. The ledger itself remains canonical.
            limit: if tail { None } else { Some(requested_limit) },
            namespace: args.get("namespace").and_then(|n| n.as_str()).map(str::to_string),
            event_type: args.get("event_type").and_then(|e| e.as_str()).map(str::to_string),
            entity_id: args.get("entity_id").and_then(|e| e.as_str()).map(str::to_string),
            correlation_id: args.get("correlation_id").and_then(|c| c.as_str()).map(str::to_string),
        };
        let mut events = self.ledger.scan_events(&filter)?;
        if tail {
            let skip = events.len().saturating_sub(requested_limit as usize);
            events.drain(..skip);
        }
        let out: Vec<serde_json::Value> = events
            .into_iter()
            .map(|ev| {
                serde_json::json!({
                    "runtime_sequence": ev.runtime_sequence,
                    "event_id": ev.event_id.to_string(),
                    "timestamp_utc_ms": ev.timestamp_utc_ms,
                    "namespace": ev.namespace,
                    "entity_id": ev.entity_id.as_ref().map(|g| g.uri.clone()),
                    "event_type": ev.event_type,
                    "correlation_id": ev.correlation_id,
                    "causation_id": ev.causation_id,
                    "producer_app": ev.producer_app,
                    "payload": serde_json::from_slice::<serde_json::Value>(&ev.payload_bytes)
                        .unwrap_or(serde_json::Value::Null),
                })
            })
            .collect();
        let high = out.len();
        let _ = high;
        Ok(serde_json::to_vec(&serde_json::json!({
            "events": out,
            "next_after": out.last().and_then(|e| e["runtime_sequence"].as_u64()),
        }))
        .unwrap())
    }

    async fn on_artifact_put(&self, args: &serde_json::Value) -> Result<Vec<u8>, RuntimeError> {
        let data_b64 = args
            .get("data")
            .and_then(|d| d.as_str())
            .ok_or_else(|| RuntimeError::BadArgs {
                method: "artifact.put".into(),
                detail: "missing data".into(),
            })?;
        let data = base64_decode(data_b64)
            .map_err(|e| RuntimeError::BadArgs { method: "artifact.put".into(), detail: e })?;
        let media_type = args.get("media_type").and_then(|m| m.as_str()).unwrap_or("application/octet-stream");
        let logical_type = args.get("logical_type").and_then(|m| m.as_str()).unwrap_or("blob");
        let hash = self.cas.put(&data, media_type, logical_type, "naf:artifact.put")?;
        Ok(serde_json::to_vec(&serde_json::json!({
            "artifact_uri": format!("artifact://blake3/{}", hash.as_hex()),
            "deduplicated": false,
        }))
        .unwrap())
    }

    async fn on_artifact_open(&self, args: &serde_json::Value) -> Result<Vec<u8>, RuntimeError> {
        let uri = args.get("artifact_uri").and_then(|u| u.as_str()).ok_or_else(|| {
            RuntimeError::BadArgs { method: "artifact.open".into(), detail: "missing artifact_uri".into() }
        })?;
        let hash = uri
            .strip_prefix("artifact://blake3/")
            .ok_or_else(|| RuntimeError::BadArgs {
                method: "artifact.open".into(),
                detail: "not an artifact://blake3 URI".into(),
            })?;
        let opened = self.cas.open(hash)?;
        let data = std::fs::read(&opened.owned_path)?;
        Ok(serde_json::to_vec(&serde_json::json!({
            "artifact_uri": uri,
            "data": base64_encode(&data),
            "size": data.len(),
        }))
        .unwrap())
    }

    async fn on_artifact_verify(&self, args: &serde_json::Value) -> Result<Vec<u8>, RuntimeError> {
        let hash = args
            .get("expected_hash")
            .and_then(|h| h.as_str())
            .ok_or_else(|| RuntimeError::BadArgs {
                method: "artifact.verify".into(),
                detail: "missing expected_hash".into(),
            })?;
        let ok = self.cas.verify(hash)?;
        Ok(serde_json::to_vec(&serde_json::json!({ "verified": ok })).unwrap())
    }

    async fn on_entity_get(&self, args: &serde_json::Value) -> Result<Vec<u8>, RuntimeError> {
        let uri = args.get("uri").and_then(|u| u.as_str()).ok_or_else(|| {
            RuntimeError::BadArgs { method: "entity.get".into(), detail: "missing uri".into() }
        })?;
        let key = HotKey(uri.as_bytes().to_vec());
        match self.hot.get(&key)? {
            Some(v) => Ok(serde_json::to_vec(&serde_json::json!({
                "uri": uri,
                "state": serde_json::from_slice::<serde_json::Value>(&v.0).unwrap_or(serde_json::Value::Null),
            }))
            .unwrap()),
            None => Ok(serde_json::to_vec(&serde_json::json!({ "uri": uri, "state": null })).unwrap()),
        }
    }
}

/// Build a `CanonicalEvent` from NAF JSON event args.
fn build_event_from_json(args: &serde_json::Value, producer: &str) -> Result<CanonicalEvent, RuntimeError> {
    let entity_id = args
        .get("entity_id")
        .and_then(|e| e.as_str())
        .and_then(GlobalId::from_uri);
    let plain = CanonicalEvent {
        event_id: EventId::uuid_v7(),
        runtime_sequence: 0,
        timestamp_utc_ms: args
            .get("timestamp_utc_ms")
            .and_then(|t| t.as_i64())
            .unwrap_or_else(now_ms),
        device_id: uuid::Uuid::now_v7(),
        producer_app: args
            .get("producer_app")
            .and_then(|p| p.as_str())
            .unwrap_or(producer)
            .to_string(),
        producer_version: args.get("producer_version").and_then(|p| p.as_str()).unwrap_or("0").to_string(),
        namespace: args.get("namespace").and_then(|n| n.as_str()).unwrap_or("default").to_string(),
        entity_id,
        event_type: args
            .get("event_type")
            .and_then(|e| e.as_str())
            .ok_or_else(|| RuntimeError::BadArgs {
                method: "event.append".into(),
                detail: "missing event_type".into(),
            })?
            .to_string(),
        schema_version: args.get("schema_version").and_then(|s| s.as_u64()).unwrap_or(1) as u32,
        correlation_id: args.get("correlation_id").and_then(|c| c.as_str()).map(str::to_string),
        causation_id: args.get("causation_id").and_then(|c| c.as_str()).map(str::to_string),
        payload_type: args.get("payload_type").and_then(|p| p.as_str()).unwrap_or("application/json").to_string(),
        payload_bytes: args
            .get("payload")
            .map(|p| serde_json::to_vec(p).unwrap_or_default())
            .unwrap_or_default(),
        artifact_refs: args
            .get("artifact_refs")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .filter_map(GlobalId::from_uri)
                    .collect()
            })
            .unwrap_or_default(),
        previous_hash: None,
        frame_hash: None,
    };
    Ok(plain)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| e.to_string())
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}