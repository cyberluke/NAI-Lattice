//! SurrealKV-backed `HotStore` (production hot store, Lattice spec §13).
//!
//! SurrealKV's commit pipeline needs a Tokio reactor. This adapter owns a
//! dedicated runtime on its own worker thread, fully decoupled from any caller
//! runtime (sync or async). Every operation is shipped to that thread and run
//! with `block_on`, so callers never need a runtime and can never hit
//! "blocking inside a runtime". `Durability::Immediate` is used for `Durable`
//! semantics; the default commit is `Eventual` (batched) which suits
//! `BatchedDurable`.

use std::fmt;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;

use lattice_hotstore::{HotKey, HotStore, HotStoreError, HotValue};
use surrealkv::{Durability, LSMIterator, Tree, TreeBuilder};

/// SurrealKV hot-store adapter.
pub struct SurrealKvHotStore {
    worker: Arc<Worker>,
    /// When true every commit requests `Durability::Immediate`.
    durable: bool,
}

struct Worker {
    rt: tokio::runtime::Runtime,
    tree: Arc<Tree>,
}

impl fmt::Debug for SurrealKvHotStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SurrealKvHotStore")
            .field("durable", &self.durable)
            .finish()
    }
}

/// Options for opening a SurrealKV hot store.
#[derive(Debug, Clone)]
pub struct SurrealKvOptions {
    pub path: PathBuf,
    pub durable: bool,
}

impl SurrealKvOptions {
    pub fn new(path: PathBuf) -> Self {
        Self { path, durable: false }
    }
}

impl SurrealKvHotStore {
    /// Open (or create) the SurrealKV tree on a dedicated worker thread.
    pub fn open(opts: SurrealKvOptions) -> Result<Self, HotStoreError> {
        std::fs::create_dir_all(&opts.path).map_err(HotStoreError::Io)?;

        // Cross a thread boundary so we never create a runtime while another
        // runtime is active on the caller thread. Build the tree there.
        let path = opts.path.clone();
        let (tx, rx): (mpsc::Sender<Result<Worker, String>>, mpsc::Receiver<Result<Worker, String>>) =
            mpsc::channel();
        std::thread::scope(move |scope| {
            scope.spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = tx.send(Err(format!("rt: {e}")));
                        return;
                    }
                };
                let tree = rt.block_on(async move {
                    TreeBuilder::new()
                        .with_path(path)
                        .build_async()
                        .await
                        .map_err(|e| format!("{e}"))
                });
                match tree {
                    Ok(tree) => {
                        let _ = tx.send(Ok(Worker {
                            rt,
                            tree: Arc::new(tree),
                        }));
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            });
        });
        let worker = rx
            .recv()
            .map_err(|e| HotStoreError::Backend(format!("open channel: {e}")))?
            .map_err(HotStoreError::Backend)?;

        Ok(Self {
            worker: Arc::new(worker),
            durable: opts.durable,
        })
    }
}

impl SurrealKvHotStore {
    /// Run an async `op` on a dedicated thread using the worker runtime, so
    /// callers that are themselves inside another Tokio runtime never conflict.
    #[inline]
    fn on_worker<F, Fut, T>(&self, op: F) -> Result<T, HotStoreError>
    where
        F: FnOnce(Arc<Tree>) -> Fut + Send,
        Fut: std::future::Future<Output = Result<T, surrealkv::Error>> + Send,
        T: Send,
    {
        let worker = self.worker.clone();
        let out = std::thread::scope(|s| {
            s.spawn(move || {
                let tree = worker.tree.clone();
                worker.rt.block_on(async move { op(tree).await })
            })
            .join()
            .expect("worker thread")
        });
        out.map_err(|e: surrealkv::Error| HotStoreError::Backend(format!("{e}")))
    }
}

impl HotStore for SurrealKvHotStore {
    fn get(&self, key: &HotKey) -> Result<Option<HotValue>, HotStoreError> {
        let k = key.0.clone();
        self.on_worker(move |tree| async move {
            let tx = tree.begin()?;
            let v = tx.get(k)?;
            Ok(v.map(|v| HotValue(v.to_vec())))
        })
    }

    fn put(&self, key: &HotKey, value: &HotValue) -> Result<(), HotStoreError> {
        let k = key.0.clone();
        let v = value.0.clone();
        let durable = self.durable;
        self.on_worker(move |tree| async move {
            let mut tx = tree.begin()?;
            if durable {
                tx.set_durability(Durability::Immediate);
            }
            tx.set(k, v)?;
            tx.commit().await?;
            Ok(())
        })
    }

    fn delete(&self, key: &HotKey) -> Result<bool, HotStoreError> {
        let k = key.0.clone();
        self.on_worker(move |tree| async move {
            let exists = tree.begin()?.get(k.clone())?.is_some();
            let mut tx = tree.begin()?;
            tx.delete(k)?;
            tx.commit().await?;
            Ok(exists)
        })
    }

    fn exists(&self, key: &HotKey) -> Result<bool, HotStoreError> {
        let k = key.0.clone();
        self.on_worker(move |tree| async move {
            let tx = tree.begin()?;
            Ok(tx.get(k)?.is_some())
        })
    }

    fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<HotKey>, HotStoreError> {
        let start = prefix.to_vec();
        let end = prefix_end(prefix);
        self.on_worker(move |tree| async move {
            let tx = tree.begin()?;
            let mut keys = Vec::new();
            let mut iter = tx.range(start, end)?;
            let mut ok = iter.seek_first()?;
            while ok {
                keys.push(HotKey(iter.key().user_key().to_vec()));
                ok = iter.next()?;
            }
            Ok(keys)
        })
    }

    fn flush(&self) -> Result<(), HotStoreError> {
        Ok(())
    }
}

/// Compute the exclusive upper bound for `[prefix .. prefix+1)` scanning.
fn prefix_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    for i in (0..end.len()).rev() {
        if end[i] < 0xFF {
            end[i] += 1;
            end.truncate(i + 1);
            return end;
        }
    }
    vec![0xFF; prefix.len() + 1]
}