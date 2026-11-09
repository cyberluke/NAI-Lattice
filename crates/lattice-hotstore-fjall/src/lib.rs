//! Fjall-backed `HotStore` adapter (Lattice spec §13).
//!
//! Implements the same trait as the SurrealKV adapter so switching hot stores
//! does not change NAF/1 or any product code. Compiled behind a feature for
//! benchmark/comparison; SurrealKV remains the production default.

use std::fmt;
use std::path::PathBuf;

use fjall::{Database, Keyspace, KeyspaceCreateOptions, PersistMode};
use lattice_hotstore::{HotKey, HotStore, HotStoreError, HotValue};

/// Fjall hot-store adapter. Uses a single logical keyspace for all keyspaces.
pub struct FjallHotStore {
    db: Database,
    defaults: Keyspace,
}

impl fmt::Debug for FjallHotStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FjallHotStore").finish()
    }
}

/// Options for opening a Fjall hot store.
#[derive(Debug, Clone)]
pub struct FjallOptions {
    pub path: PathBuf,
    pub keyspace_name: String,
}

impl FjallOptions {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            keyspace_name: "default".to_string(),
        }
    }
}

impl FjallHotStore {
    /// Open (or create) the Fjall database + default keyspace.
    pub fn open(opts: FjallOptions) -> Result<Self, HotStoreError> {
        std::fs::create_dir_all(&opts.path)
            .map_err(|e| HotStoreError::Io(e))?;
        let db = Database::builder(&opts.path)
            .open()
            .map_err(|e| HotStoreError::Backend(format!("{e}")))?;
        let defaults = db
            .keyspace(&opts.keyspace_name, KeyspaceCreateOptions::default)
            .map_err(|e| HotStoreError::Backend(format!("{e}")))?;
        Ok(Self { db, defaults })
    }
}

impl HotStore for FjallHotStore {
    fn get(&self, key: &HotKey) -> Result<Option<HotValue>, HotStoreError> {
        self.defaults
            .get(&key.0)
            .map(|v| v.map(|iv| HotValue(iv.to_vec())))
            .map_err(|e| HotStoreError::Backend(format!("{e}")))
    }

    fn put(&self, key: &HotKey, value: &HotValue) -> Result<(), HotStoreError> {
        self.defaults
            .insert(key.0.to_vec(), value.0.to_vec())
            .map_err(|e| HotStoreError::Backend(format!("{e}")))
    }

    fn delete(&self, key: &HotKey) -> Result<bool, HotStoreError> {
        let exists = self.exists(key)?;
        self.defaults
            .remove(key.0.clone())
            .map_err(|e| HotStoreError::Backend(format!("{e}")))?;
        Ok(exists)
    }

    fn exists(&self, key: &HotKey) -> Result<bool, HotStoreError> {
        self.defaults
            .contains_key(&key.0)
            .map_err(|e| HotStoreError::Backend(format!("{e}")))
    }

    fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<HotKey>, HotStoreError> {
        let mut keys = Vec::new();
        for guard in self.defaults.prefix(prefix) {
            let k = guard
                .key()
                .map_err(|e| HotStoreError::Backend(format!("{e}")))?;
            keys.push(HotKey(k.to_vec()));
        }
        Ok(keys)
    }

    fn flush(&self) -> Result<(), HotStoreError> {
        self.db
            .persist(PersistMode::SyncAll)
            .map_err(|e| HotStoreError::Backend(format!("{e}")))
    }
}