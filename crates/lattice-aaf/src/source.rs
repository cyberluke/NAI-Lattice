//! External artifact source provider contract (Lattice spec §11).

use std::io::Error as IoError;
use std::ops::Range;
use std::path::PathBuf;

use crate::{Candidate, DiscoverFilter, Fingerprint, Locator, ProviderHealth};

/// An object-capable byte stream with an explicit ending.
pub type ByteStream = Box<dyn std::io::Read + Send>;

/// Provider contract for a read-only external artifact source.
pub trait ExternalArtifactSource: Send + Sync {
    /// A stable, unique provider id (e.g. `naibrowser-opfs`, `naicache-retained`).
    fn provider_id(&self) -> &str;

    /// Enumerate candidate artifacts matching `filter`.
    fn discover(&self, filter: &DiscoverFilter) -> Result<Vec<Candidate>, IoError>;

    /// Compute (or return known) content fingerprint for `locator`.
    fn fingerprint(&self, locator: &Locator) -> Result<Fingerprint, IoError>;

    /// Open a byte range of the object.
    fn open_range(&self, locator: &Locator, range: Range<u64>) -> Result<ByteStream, IoError>;

    /// Report provider health.
    fn health(&self) -> ProviderHealth;

    /// When true, the provider's source is immutable and safe to reflink/hardlink.
    fn supports_hardlink(&self) -> bool {
        false
    }

    /// Whether the provider is local (avoids remote cost class).
    fn is_local(&self) -> bool {
        true
    }
}

/// A simple on-disk directory source: `root/<logical_type>/<file>`.
pub struct DirectorySource {
    pub id: String,
    pub root: PathBuf,
}

impl DirectorySource {
    pub fn new(id: &str, root: PathBuf) -> Self {
        Self {
            id: id.to_string(),
            root,
        }
    }
}

impl ExternalArtifactSource for DirectorySource {
    fn provider_id(&self) -> &str {
        &self.id
    }

    fn discover(&self, filter: &DiscoverFilter) -> Result<Vec<Candidate>, IoError> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.path().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let logical = entry
                .path()
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if let Some(lt) = &filter.logical_type {
                if &logical != lt {
                    continue;
                }
            }
            let size = entry.metadata()?.len();
            out.push(Candidate {
                provider_id: self.id.clone(),
                locator: Locator(name),
                fingerprint: Fingerprint { blake3: None, size },
                media_type_hint: None,
                logical_type_hint: Some(logical),
                provenance: format!("{}:{}", self.id, entry.path().display()),
            });
        }
        Ok(out)
    }

    fn fingerprint(&self, locator: &Locator) -> Result<Fingerprint, IoError> {
        let path = self.root.join(&locator.0);
        let meta = std::fs::metadata(&path)?;
        Ok(Fingerprint {
            blake3: None,
            size: meta.len(),
        })
    }

    fn open_range(&self, locator: &Locator, range: Range<u64>) -> Result<ByteStream, IoError> {
        use std::io::{Read, Seek, SeekFrom};
        let path = self.root.join(&locator.0);
        let mut f = std::fs::File::open(&path)?;
        f.seek(SeekFrom::Start(range.start))?;
        let len = range.end.saturating_sub(range.start) as usize;
        let mut buf = vec![0u8; len];
        let mut read = 0usize;
        while read < buf.len() {
            let n = f.read(&mut buf[read..])?;
            if n == 0 {
                break;
            }
            read += n;
        }
        buf.truncate(read);
        Ok(Box::new(std::io::Cursor::new(buf)))
    }

    fn health(&self) -> ProviderHealth {
        if self.root.exists() {
            ProviderHealth::Healthy
        } else {
            ProviderHealth::Unavailable {
                detail: format!("directory missing: {}", self.root.display()),
            }
        }
    }

    fn supports_hardlink(&self) -> bool {
        true
    }
}

/// A pure in-memory source (used in tests / fixtures).
pub struct MemorySource {
    pub id: String,
    pub objects: std::collections::HashMap<String, Vec<u8>>,
}

impl ExternalArtifactSource for MemorySource {
    fn provider_id(&self) -> &str {
        &self.id
    }
    fn discover(&self, _filter: &DiscoverFilter) -> Result<Vec<Candidate>, IoError> {
        Ok(self
            .objects
            .iter()
            .map(|(k, v)| Candidate {
                provider_id: self.id.clone(),
                locator: Locator(k.clone()),
                fingerprint: Fingerprint {
                    blake3: Some(blake3::hash(v).to_hex().to_string()),
                    size: v.len() as u64,
                },
                media_type_hint: None,
                logical_type_hint: None,
                provenance: format!("memory:{}", k),
            })
            .collect())
    }
    fn fingerprint(&self, locator: &Locator) -> Result<Fingerprint, IoError> {
        let v = self
            .objects
            .get(&locator.0)
            .ok_or_else(|| IoError::new(std::io::ErrorKind::NotFound, "no such object"))?;
        Ok(Fingerprint {
            blake3: Some(blake3::hash(v).to_hex().to_string()),
            size: v.len() as u64,
        })
    }
    fn open_range(&self, locator: &Locator, range: Range<u64>) -> Result<ByteStream, IoError> {
        let v = self
            .objects
            .get(&locator.0)
            .ok_or_else(|| IoError::new(std::io::ErrorKind::NotFound, "no such object"))?;
        let data = v[range.start as usize..range.end as usize].to_vec();
        Ok(Box::new(std::io::Cursor::new(data)))
    }
    fn health(&self) -> ProviderHealth {
        ProviderHealth::Healthy
    }
    fn supports_hardlink(&self) -> bool {
        false
    }
}