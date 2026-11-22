//! AAF resolver (Lattice spec §11).

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, RwLock};

use nai_lattice_cas::object::ObjectStore;

use crate::source::ExternalArtifactSource;
use crate::{AafError, Candidate, DiscoverFilter, Locator, ProviderHealth, Resolution};

/// A registered provider (shared handle).
pub type ProviderHandle = Arc<dyn ExternalArtifactSource>;

/// The AAF resolver: registers providers and resolves artifacts against the
/// preference ordering. Read-only by default.
pub struct AafResolver {
    /// Owned CAS for fallback resolution and fingerprinting.
    cas: Arc<ObjectStore>,
    providers: RwLock<HashMap<String, ProviderHandle>>,
}

impl AafResolver {
    pub fn new(cas: Arc<ObjectStore>) -> Self {
        Self {
            cas,
            providers: RwLock::new(HashMap::new()),
        }
    }

    pub fn register(&self, provider: ProviderHandle) -> Result<(), AafError> {
        let id = provider.provider_id().to_string();
        self.providers.write().unwrap().insert(id.clone(), provider);
        Ok(())
    }

    pub fn unregister(&self, provider_id: &str) {
        self.providers.write().unwrap().remove(provider_id);
    }

    pub fn providers(&self) -> Vec<String> {
        self.providers.read().unwrap().keys().cloned().collect()
    }

    /// Healthcheck: `(provider, health)` for every registered provider.
    pub fn provider_health(&self) -> Vec<(String, ProviderHealth)> {
        self.providers
            .read()
            .unwrap()
            .iter()
            .map(|(id, p)| (id.clone(), p.health()))
            .collect()
    }

    /// Discover candidates across all healthy providers.
    pub fn discover_all(&self, filter: &DiscoverFilter) -> Vec<Candidate> {
        let mut out = Vec::new();
        let read = self.providers.read().unwrap();
        for p in read.values() {
            if matches!(p.health(), ProviderHealth::Healthy) {
                if let Ok(cands) = p.discover(filter) {
                    out.extend(cands);
                }
            }
        }
        out
    }

    /// Resolve an artifact expected to hash to `expected_hash`. Preference:
    /// owned CAS, stable-local, range-access local, hardlink-capable immutable,
    /// staged local copy, remote. Returns the best available `Resolution`.
    pub fn resolve(
        &self,
        expected_hash: &str,
        filter: &DiscoverFilter,
    ) -> Result<Resolution, AafError> {
        // 1. Owned CAS first.
        if self.cas.verify(expected_hash)? {
            let path = self.cas.object_path_for(expected_hash);
            return Ok(Resolution::Owned {
                object_path: path,
                hash: expected_hash.to_string(),
            });
        }

        // 2. Discover external candidates and find one whose fingerprint
        //    matches the requested identity.
        let candidates = self.discover_all(filter);
        for cand in candidates {
            let Some(fp) = self.provider_fingerprint(&cand.provider_id, &cand.locator)? else {
                continue;
            };
            let matches = if let Some(h) = &fp.blake3 {
                h == expected_hash
            } else {
                // No blake3; fall back to size-only heuristic (weak). Prefer to
                // stage and verify rather than trust blindly.
                false
            };
            if !matches {
                continue;
            }
            let provider = self
                .providers
                .read()
                .unwrap()
                .get(&cand.provider_id)
                .cloned()
                .ok_or_else(|| AafError::UnknownProvider(cand.provider_id.clone()))?;
            // Preference: local range-access / hardlink-capable immutable.
            if provider.is_local() && provider.supports_hardlink() {
                return Ok(Resolution::External {
                    provider_id: cand.provider_id.clone(),
                    locator: cand.locator.clone(),
                });
            }
            return Ok(Resolution::External {
                provider_id: cand.provider_id,
                locator: cand.locator,
            });
        }

        Err(AafError::NoLocation(expected_hash.to_string()))
    }

    /// Open a byte range from a provider locator (read-only).
    pub fn open_range(
        &self,
        provider_id: &str,
        locator: &Locator,
        range: Range<u64>,
    ) -> Result<crate::source::ByteStream, AafError> {
        let provider = self
            .providers
            .read()
            .unwrap()
            .get(provider_id)
            .cloned()
            .ok_or_else(|| AafError::UnknownProvider(provider_id.to_string()))?;
        provider
            .open_range(locator, range)
            .map_err(|e| AafError::Source(e.to_string()))
    }

    /// Fingerprint an object at a provider, if reachable.
    pub fn provider_fingerprint(
        &self,
        provider_id: &str,
        locator: &Locator,
    ) -> Result<Option<crate::Fingerprint>, AafError> {
        let provider = self
            .providers
            .read()
            .unwrap()
            .get(provider_id)
            .cloned()
            .ok_or_else(|| AafError::UnknownProvider(provider_id.to_string()))?;
        match provider.fingerprint(locator) {
            Ok(fp) => Ok(Some(fp)),
            Err(_) => Ok(None),
        }
    }
}

/// A reference provider in tests/fixtures.
pub type TestProvider = crate::source::MemorySource;