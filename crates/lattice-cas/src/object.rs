//! Owned CAS byte store (Lattice spec §10).
//!
//! Path layout: `<root>/cas/objects/<first2>/<rest>`. Whole-object BLAKE3 is
//! the canonical identity; the path prefix is just sharding for the filesystem.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use lattice_core::{ArtifactManifest, Blake3Hash, ChunkDescriptor, ChunkManifest};

use crate::{CasError, OpenedArtifact, RangeRead};

/// Owned object store rooted at `<cas_root>`.
pub struct ObjectStore {
    cas_root: PathBuf,
    /// After `put`, a manifest writer is expected to record the artifact.
    manifest: crate::manifest::ManifestStore,
}

impl ObjectStore {
    pub fn new(cas_root: PathBuf) -> std::io::Result<Self> {
        create_shard_dirs(&cas_root)?;
        let manifest = crate::manifest::ManifestStore::new(cas_root.join("manifests"));
        Ok(Self { cas_root, manifest })
    }

    /// The on-disk path for a whole-object hash (sharded).
    pub fn object_path_for(&self, hash: &str) -> PathBuf {
        let (a, b) = split_hash(hash);
        self.cas_root.join("objects").join(a).join(b)
    }

    /// Store bytes, confirm the BLAKE3 whole-object hash, write to the sharded
    /// path, and persist a manifest. Returns the confirmed hash.
    pub fn put(
        &self,
        bytes: &[u8],
        media_type: &str,
        logical_type: &str,
        provenance: &str,
    ) -> Result<Blake3Hash, CasError> {
        let digest = blake3::hash(bytes);
        let hash = digest.to_hex().to_string();
        let (a, b) = split_hash(&hash);
        let dir = self.cas_root.join("objects").join(a);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(b);

        // Write to a temp file then atomic rename -> crash-safe.
        let tmp = dir.join(format!(".tmp-{}", std::process::id()));
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        if path.exists() {
            // Already present: verify identity before reuse.
            let existing = std::fs::read(&path)?;
            if existing != bytes {
                return Err(CasError::HashMismatch {
                    expected: hash,
                    got: blake3::hash(&existing).to_hex().to_string(),
                });
            }
            let _ = std::fs::remove_file(&tmp);
        } else {
            std::fs::rename(&tmp, &path)?;
        }

        let manifest = self.manifest.create(
            &hash,
            bytes.len() as u64,
            media_type,
            logical_type,
            provenance,
            None,
            vec![],
            vec![],
        )?;
        self.manifest.write(&hash, &manifest)?;
        Ok(Blake3Hash(hash))
    }

    /// Confirm an existing object's whole-object hash matches `expected`.
    pub fn verify(&self, expected: &str) -> Result<bool, CasError> {
        let path = self.object_path_for(expected);
        if !path.exists() {
            return Ok(false);
        }
        let mut f = std::fs::File::open(&path)?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let got = hasher.finalize().to_hex().to_string();
        Ok(got == expected)
    }

    /// Open an artifact by hash: returns manifest + owned superseded path.
    pub fn open(&self, hash: &str) -> Result<OpenedArtifact, CasError> {
        let path = self.object_path_for(hash);
        if !path.exists() {
            return Err(CasError::NotFound(hash.to_string()));
        }
        let manifest = self.manifest.read(hash)?.ok_or_else(|| {
            CasError::NotFound(format!("manifest for {hash}"))
        })?;
        let size = std::fs::metadata(&path)?.len();
        Ok(OpenedArtifact {
            manifest,
            owned_path: path,
            size,
        })
    }

    /// Read a byte range of an owned object.
    pub fn open_range(&self, hash: &str, start: u64, end: u64) -> Result<RangeRead, CasError> {
        let path = self.object_path_for(hash);
        if !path.exists() {
            return Err(CasError::NotFound(hash.to_string()));
        }
        let size = std::fs::metadata(&path)?.len();
        if start > size || end > size || start >= end {
            return Err(CasError::RangeOutOfBounds { start, end, size });
        }
        use std::io::{Seek, SeekFrom};
        let mut f = std::fs::File::open(&path)?;
        f.seek(SeekFrom::Start(start))?;
        let len = (end - start) as usize;
        let mut data = vec![0u8; len];
        f.read_exact(&mut data)?;
        Ok(RangeRead { data, start, end })
    }

    pub fn root(&self) -> &Path {
        &self.cas_root
    }

    pub fn manifest_store(&self) -> &crate::manifest::ManifestStore {
        &self.manifest
    }
}

/// Split a 64-hex hash into `(first2, rest)`.
fn split_hash(hash: &str) -> (String, String) {
    let (a, rest) = hash.split_at(2);
    (a.to_string(), rest.to_string())
}

fn create_shard_dirs(root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root.join("objects"))?;
    // Pre-create all 256 shard dirs to avoid per-write create_dir_all.
    for i in 0u8..=255 {
        std::fs::create_dir_all(root.join("objects").join(format!("{i:02x}")))?;
    }
    std::fs::create_dir_all(root.join("manifests"))?;
    Ok(())
}

/// True when the artifact exceeds the FastCDC chunking threshold (spec §10:
/// optional content-defined chunk manifest for files > 64 MiB).
pub fn should_chunk(manifest: &ArtifactManifest) -> bool {
    manifest.size > 64 * 1024 * 1024
}

/// Produce a trivial single-chunk descriptor (identity chunk) as a placeholder
/// until FastCDC is wired. Whole-object BLAKE3 remains canonical identity.
pub fn identity_chunk_manifest(hash: &str, size: u64) -> ChunkManifest {
    ChunkManifest {
        chunk_size_min: 1024 * 1024,
        chunk_size_avg: 4 * 1024 * 1024,
        chunk_size_max: 16 * 1024 * 1024,
        chunks: vec![ChunkDescriptor {
            offset: 0,
            length: size,
            hash: Blake3Hash(hash.to_string()),
        }],
    }
}