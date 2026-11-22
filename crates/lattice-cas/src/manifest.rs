//! Artifact manifest persistence (Lattice spec §10 field list, §11 external
//! locations). Manifests are durable JSON keyed by whole-object hash.

use std::path::{Path, PathBuf};

use nai_lattice_core::{
    ArtifactLocation, ArtifactManifest, Blake3Hash, ChunkManifest, LocationHealth,
    StabilityClass, VerificationState,
};

use crate::CasError;

/// Persists/reads `ArtifactManifest` under `<manifests_root>/<first2>/<rest>.json`.
pub struct ManifestStore {
    root: PathBuf,
}

impl ManifestStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn path_for(&self, hash: &str) -> PathBuf {
        let (a, rest) = hash.split_at(2);
        self.root.join(a).join(format!("{rest}.json"))
    }

    pub fn create(
        &self,
        whole_hash: &str,
        size: u64,
        media_type: &str,
        logical_type: &str,
        provenance: &str,
        chunk_manifest: Option<ChunkManifest>,
        owned_locations: Vec<ArtifactLocation>,
        external_locations: Vec<ArtifactLocation>,
    ) -> Result<ArtifactManifest, CasError> {
        Ok(ArtifactManifest {
            artifact_id: whole_hash.to_string(),
            whole_hash: Blake3Hash(whole_hash.to_string()),
            size,
            media_type: media_type.to_string(),
            logical_type: logical_type.to_string(),
            created_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64,
            provenance: provenance.to_string(),
            chunk_manifest,
            owned_locations,
            external_locations,
            verification_state: VerificationState::Pending,
            last_verified_at: None,
        })
    }

    pub fn write(&self, hash: &str, manifest: &ArtifactManifest) -> Result<(), CasError> {
        let path = self.path_for(hash);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(manifest)
            .map_err(|e| CasError::Manifest(e.to_string()))?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    pub fn read(&self, hash: &str) -> Result<Option<ArtifactManifest>, CasError> {
        let path = self.path_for(hash);
        if !path.exists() {
            return Ok(None);
        }
        let data = std::fs::read(&path)?;
        let manifest: ArtifactManifest = serde_json::from_slice(&data)
            .map_err(|e| CasError::Manifest(e.to_string()))?;
        Ok(Some(manifest))
    }

    pub fn update<F>(&self, hash: &str, f: F) -> Result<ArtifactManifest, CasError>
    where
        F: FnOnce(&mut ArtifactManifest),
    {
        let mut manifest = self.read(hash)?.ok_or_else(|| {
            CasError::NotFound(format!("manifest for {hash}"))
        })?;
        f(&mut manifest);
        self.write(hash, &manifest)?;
        Ok(manifest)
    }

    /// Append an external AAF location to an existing manifest (§11).
    pub fn add_external_location(
        &self,
        hash: &str,
        provider_id: &str,
        opaque_locator: &str,
        size: u64,
        stability: StabilityClass,
        random_access_capability: bool,
        cost_class: u8,
    ) -> Result<(), CasError> {
        self.update(hash, |m| {
            m.external_locations.push(ArtifactLocation {
                provider_id: provider_id.to_string(),
                opaque_locator: opaque_locator.to_string(),
                expected_hash: Blake3Hash(hash.to_string()),
                size,
                stability,
                random_access_capability,
                cost_class,
                owned: false,
                last_verified: None,
                health: LocationHealth::Unknown,
            });
        })?;
        Ok(())
    }

    pub fn mark_verified(&self, hash: &str, state: VerificationState) -> Result<(), CasError> {
        self.update(hash, |m| {
            m.verification_state = state;
            m.last_verified_at = Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            );
        })?;
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}