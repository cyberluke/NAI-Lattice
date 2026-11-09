//! HotStore trait (Lattice spec §13).
//!
//! The hot store owns current mutable state (entity state, projections,
//! artifact location catalog, provider catalog, checkpoints, idempotency,
//! counters, runtime config, short-lived coordination). It does **not** own
//! canonical user truth — that lives in the ledger + CAS.
//!
//! Adaptors: `lattice-hotstore-surrealkv` (production), `lattice-hotstore-fjall`.
//! Switching hot stores must not change NAF/1 or any product code.

use std::fmt::Debug;

/// A namespaced byte key (opaque; the opaque data includes the keyspace).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HotKey(pub Vec<u8>);

/// A byte value with an optional large-journal sidecar (opaque).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotValue(pub Vec<u8>);

impl From<Vec<u8>> for HotKey {
    fn from(v: Vec<u8>) -> Self {
        Self(v)
    }
}
impl From<Vec<u8>> for HotValue {
    fn from(v: Vec<u8>) -> Self {
        Self(v)
    }
}

/// Read/write error.
#[derive(Debug, thiserror::Error)]
pub enum HotStoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend: {0}")]
    Backend(String),
    #[error("key not found: {0:?}")]
    NotFound(HotKey),
    #[error("serialization: {0}")]
    Serialize(String),
    #[error("range: {0}")]
    Range(String),
}

/// Implementation of the hot KV abstraction. Type-erased enough that product
/// code never depends on a specific backend.
///
/// `Sync` (`&self` methods) because hot-store reads/writes are short and
/// backends (SurrealKV, Fjall) are synchronous underneath; the caller wraps in
/// `spawn_blocking` when necessary.
pub trait HotStore: Send + Sync + Debug {
    /// Get a value by key. `None` when absent.
    fn get(&self, key: &HotKey) -> Result<Option<HotValue>, HotStoreError>;

    /// Put a value (insert or overwrite).
    fn put(&self, key: &HotKey, value: &HotValue) -> Result<(), HotStoreError>;

    /// Delete a key, returning whether it existed.
    fn delete(&self, key: &HotKey) -> Result<bool, HotStoreError>;

    /// Whether the key exists.
    fn exists(&self, key: &HotKey) -> Result<bool, HotStoreError>;

    /// Increment a counter at `key`, returning the new value.
    fn increment(&self, key: &HotKey) -> Result<u64, HotStoreError> {
        let cur = self
            .get(key)?
            .map(|v| -> Result<u64, HotStoreError> {
                let bytes = v.0;
                if bytes.is_empty() {
                    Ok(0u64)
                } else {
                    Ok(u64::from_le_bytes(bytes[..8].try_into().map_err(|_| {
                        HotStoreError::Serialize("counter not u64le".into())
                    })?))
                }
            })
            .transpose()?
            .unwrap_or(0);
        let next = cur + 1;
        self.put(key, &HotValue(next.to_le_bytes().to_vec()))?;
        Ok(next)
    }

    /// List keys with a prefix (used for keyspace iteration, e.g. all entities).
    fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<HotKey>, HotStoreError>;

    /// Flush durable state to the durability boundary when supported.
    fn flush(&self) -> Result<(), HotStoreError> {
        Ok(())
    }
}

/// Convenience: a strongly-typed keyspace over `HotStore` allowing the spec's
/// keyspace shapes (`entity/<type>/<id>`, `projection/<id>/checkpoint`, ...).
#[derive(Debug, Clone, Copy)]
pub struct Keyspace<'a, S: HotStore>(pub &'a S);

impl<'a, S: HotStore> Keyspace<'a, S> {
    pub fn new(store: &'a S) -> Self {
        Self(store)
    }

    /// Build a namespaced byte key from token segments.
    pub fn key<I, T>(segments: I) -> Vec<u8>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<[u8]>,
    {
        let mut out = Vec::new();
        for (i, seg) in segments.into_iter().enumerate() {
            if i > 0 {
                out.push(b'/');
            }
            out.extend_from_slice(seg.as_ref());
        }
        out
    }

    pub fn get<I, T>(&self, segments: I) -> Result<Option<HotValue>, HotStoreError>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<[u8]>,
    {
        self.0.get(&HotKey(Self::key(segments)))
    }

    pub fn put<I, T, V>(&self, segments: I, value: V) -> Result<(), HotStoreError>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<[u8]>,
        V: Into<HotValue>,
    {
        self.0.put(&HotKey(Self::key(segments)), &value.into())
    }

    /// JSON-encode a value into the keyspace.
    pub fn put_json<I, T, V>(&self, segments: I, value: &V) -> Result<(), HotStoreError>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<[u8]>,
        V: serde::Serialize,
    {
        let json = serde_json::to_vec(value).map_err(|e| HotStoreError::Serialize(e.to_string()))?;
        self.put(segments, HotValue(json))
    }

    /// Read + JSON-decode a value.
    pub fn get_json<I, T, V>(&self, segments: I) -> Result<Option<V>, HotStoreError>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<[u8]>,
        V: serde::de::DeserializeOwned,
    {
        match self.get(segments)? {
            Some(v) => serde_json::from_slice(&v.0)
                .map(Some)
                .map_err(|e| HotStoreError::Serialize(e.to_string())),
            None => Ok(None),
        }
    }

    pub fn delete<I, T>(&self, segments: I) -> Result<bool, HotStoreError>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<[u8]>,
    {
        self.0.delete(&HotKey(Self::key(segments)))
    }
}

/// Atomic read-modify-write helper over any `HotStore`. Backends guarantee
/// per-key atomicity; multi-key transactions are backend-optional.
pub fn atomic_update<S, U, E>(store: &S, key: &HotKey, f: U) -> Result<(), HotStoreError>
where
    S: HotStore,
    U: FnOnce(Option<HotValue>) -> Result<HotValue, E>,
    E: Into<HotStoreError>,
{
    let prev = store.get(key)?;
    let next = f(prev).map_err(Into::into)?;
    store.put(key, &next)
}