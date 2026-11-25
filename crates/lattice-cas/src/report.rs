//! Read-only CAS dedup / integrity report (S03 partial, honest).
//!
//! The owned CAS is addressed by **whole-object BLAKE3**, so byte-identical
//! content is already deduplicated by address: the object path *is* the hash.
//! This report inventories that address space and the index sidecars; it does
//! not rewrite objects, manifests or chunk layouts.
//!
//! Chunk-level content-defined dedup is **not** implemented. The current
//! `ChunkManifest` is optional metadata produced by `identity_chunk_manifest`
//! (a single identity chunk); there is no chunk CAS. Adding CDC dedup would be
//! a CAS/chunk format migration, not a report change, and is deliberately out
//! of scope here.
//!
//! What is checked (cheap, index-level):
//!
//! * every object file is at `<objects>/<first2>/<62 hex>`;
//! * every manifest file is at `<manifests>/<first2>/<62 hex>.json` and parses
//!   as an `ArtifactManifest` whose `whole_hash`/`artifact_id` match its path;
//! * a manifest whose object is missing;
//! * an object whose manifest is missing;
//! * manifest size vs object size.
//!
//! Object **content** is not re-hashed here; `ObjectStore::put` confirms the
//! BLAKE3 on write and `ObjectStore::verify` does it on demand. The report says
//! so instead of implying a full scrub.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use nai_lattice_core::ArtifactManifest;

use crate::CasError;

/// Report schema version.
pub const CAS_REPORT_SCHEMA: u32 = 1;
/// Cap on each diagnostic list so a damaged store cannot produce an unbounded
/// JSON payload.
const MAX_LIST_ENTRIES: usize = 1000;

/// One index entry that cannot be trusted.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CorruptIndexEntry {
    /// Path relative to the CAS root.
    pub path: String,
    /// Why the entry was rejected.
    pub error: String,
}

/// Inventory of one owned CAS root.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CasReport {
    pub schema: u32,
    /// CAS root as provided (display form).
    pub root: String,
    /// Object files physically present with a valid hash path.
    pub objects: u64,
    /// Sum of object file sizes.
    pub total_bytes: u64,
    /// Distinct object hashes (equal to `objects` for a healthy sharded store).
    pub unique_hashes: u64,
    /// Parsed manifests.
    pub manifests: u64,
    /// Manifests whose object file is missing.
    pub missing_objects: Vec<String>,
    /// Objects with no manifest sidecar.
    pub missing_manifests: Vec<String>,
    /// Malformed/unparseable/mismatched index entries.
    pub corrupt_index: Vec<CorruptIndexEntry>,
    /// True when any diagnostic list was capped at [`MAX_LIST_ENTRIES`].
    pub lists_truncated: bool,
    /// Identity model: `whole-object-blake3`.
    pub dedup: String,
    /// Always false today; CDC dedup would need a chunk-format migration.
    pub chunk_level_dedup: bool,
    /// Honest scope note.
    pub note: String,
}

/// Inventory a CAS root (`<root>/objects`, `<root>/manifests`).
///
/// A missing root is an empty report, not an error. Unreadable directories and
/// malformed entries are reported, never guessed away.
pub fn cas_report(root: &Path) -> Result<CasReport, CasError> {
    let mut objects: BTreeMap<String, u64> = BTreeMap::new();
    let mut corrupt_index = Vec::new();
    let mut truncated = false;

    let objects_dir = root.join("objects");
    if objects_dir.is_dir() {
        scan_sharded_files(&objects_dir, &mut |relative, absolute| {
            let hash = hash_from_object_relative(relative);
            let Some(hash) = hash else {
                push_bounded(
                    &mut corrupt_index,
                    CorruptIndexEntry {
                        path: format!("objects/{}", relative.display()),
                        error: "object file does not live at <first2>/<62 hex>".into(),
                    },
                    &mut truncated,
                );
                return;
            };
            match std::fs::metadata(absolute) {
                Ok(metadata) if metadata.is_file() => {
                    objects.insert(hash, metadata.len());
                }
                Ok(_) => push_bounded(
                    &mut corrupt_index,
                    CorruptIndexEntry {
                        path: format!("objects/{}", relative.display()),
                        error: "object path is not a regular file".into(),
                    },
                    &mut truncated,
                ),
                Err(error) => push_bounded(
                    &mut corrupt_index,
                    CorruptIndexEntry {
                        path: format!("objects/{}", relative.display()),
                        error: format!("stat object: {error}"),
                    },
                    &mut truncated,
                ),
            }
        })?;
    }

    let mut manifests: BTreeMap<String, ArtifactManifest> = BTreeMap::new();
    let manifests_dir = root.join("manifests");
    if manifests_dir.is_dir() {
        scan_sharded_files(&manifests_dir, &mut |relative, absolute| {
            let hash = hash_from_manifest_relative(relative);
            let Some(hash) = hash else {
                push_bounded(
                    &mut corrupt_index,
                    CorruptIndexEntry {
                        path: format!("manifests/{}", relative.display()),
                        error: "manifest does not live at <first2>/<62 hex>.json".into(),
                    },
                    &mut truncated,
                );
                return;
            };
            let bytes = match std::fs::read(absolute) {
                Ok(bytes) => bytes,
                Err(error) => {
                    push_bounded(
                        &mut corrupt_index,
                        CorruptIndexEntry {
                            path: format!("manifests/{}", relative.display()),
                            error: format!("read manifest: {error}"),
                        },
                        &mut truncated,
                    );
                    return;
                }
            };
            let manifest: ArtifactManifest = match serde_json::from_slice(&bytes) {
                Ok(manifest) => manifest,
                Err(error) => {
                    push_bounded(
                        &mut corrupt_index,
                        CorruptIndexEntry {
                            path: format!("manifests/{}", relative.display()),
                            error: format!("parse manifest: {error}"),
                        },
                        &mut truncated,
                    );
                    return;
                }
            };
            if manifest.whole_hash.0 != hash || manifest.artifact_id != hash {
                push_bounded(
                    &mut corrupt_index,
                    CorruptIndexEntry {
                        path: format!("manifests/{}", relative.display()),
                        error: format!(
                            "manifest identity does not match its path (whole_hash {}, artifact_id {})",
                            manifest.whole_hash.0, manifest.artifact_id
                        ),
                    },
                    &mut truncated,
                );
                return;
            }
            manifests.insert(hash, manifest);
        })?;
    }

    let mut missing_objects = Vec::new();
    for (hash, manifest) in &manifests {
        match objects.get(hash) {
            Some(size) if manifest.size != *size => push_bounded(
                &mut corrupt_index,
                CorruptIndexEntry {
                    path: format!("manifests/{}/{}...json", &hash[..2], &hash[2..]),
                    error: format!(
                        "manifest size {} != object size {size}",
                        manifest.size
                    ),
                },
                &mut truncated,
            ),
            Some(_) => {}
            None => push_bounded(&mut missing_objects, hash.clone(), &mut truncated),
        }
    }
    let mut missing_manifests = Vec::new();
    for hash in objects.keys() {
        if !manifests.contains_key(hash) {
            push_bounded(&mut missing_manifests, hash.clone(), &mut truncated);
        }
    }
    missing_objects.sort();
    missing_manifests.sort();

    let total_bytes = objects.values().sum();
    let unique_hashes = objects.keys().collect::<BTreeSet<_>>().len() as u64;

    Ok(CasReport {
        schema: CAS_REPORT_SCHEMA,
        root: root.display().to_string(),
        objects: objects.len() as u64,
        total_bytes,
        unique_hashes,
        manifests: manifests.len() as u64,
        missing_objects,
        missing_manifests,
        corrupt_index,
        lists_truncated: truncated,
        dedup: "whole-object-blake3".into(),
        chunk_level_dedup: false,
        note: "Objects are addressed by whole-object BLAKE3, so byte-identical content is stored \
               once. Object bytes are not re-hashed by this read-only inventory; put/open verify \
               on demand. Chunk-level CDC dedup requires a CAS/chunk format migration and is not \
               implemented."
            .into(),
    })
}

/// Walk `<shard>/<file>` exactly one level deep. Reparse points are skipped so
/// a junction cannot make the inventory traverse an arbitrary tree.
fn scan_sharded_files(
    dir: &Path,
    visit: &mut dyn FnMut(&Path, &Path),
) -> Result<(), CasError> {
    for shard in std::fs::read_dir(dir)?.flatten() {
        let shard_path = shard.path();
        let Ok(metadata) = std::fs::symlink_metadata(&shard_path) else {
            continue;
        };
        if is_reparse_point(&metadata) || !metadata.is_dir() {
            continue;
        }
        let shard_name = shard.file_name();
        for entry in std::fs::read_dir(&shard_path)?.flatten() {
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if is_reparse_point(&metadata) || metadata.is_dir() {
                continue;
            }
            // Skip in-flight temp files (they are never published objects).
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with('.')
            {
                continue;
            }
            let relative = Path::new(&shard_name).join(entry.file_name());
            visit(&relative, &path);
        }
    }
    Ok(())
}

fn hash_from_object_relative(relative: &Path) -> Option<String> {
    let shard = relative.components().next()?.as_os_str().to_str()?;
    let file = relative.components().nth(1)?.as_os_str().to_str()?;
    let hash = format!("{shard}{file}");
    is_lower_hex_64(&hash).then_some(hash)
}

fn hash_from_manifest_relative(relative: &Path) -> Option<String> {
    let shard = relative.components().next()?.as_os_str().to_str()?;
    let file = relative.components().nth(1)?.as_os_str().to_str()?;
    let stem = file.strip_suffix(".json")?;
    let hash = format!("{shard}{stem}");
    is_lower_hex_64(&hash).then_some(hash)
}

fn is_lower_hex_64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn push_bounded<T>(list: &mut Vec<T>, value: T, truncated: &mut bool) {
    if list.len() < MAX_LIST_ENTRIES {
        list.push(value);
    } else {
        *truncated = true;
    }
}

fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObjectStore;

    fn setup() -> (tempfile::TempDir, std::path::PathBuf, ObjectStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("cas");
        let store = ObjectStore::new(root.clone()).expect("object store");
        (dir, root, store)
    }

    fn manifest_path(root: &Path, hash: &str) -> std::path::PathBuf {
        root.join("manifests").join(&hash[..2]).join(format!("{}.json", &hash[2..]))
    }

    #[test]
    fn whole_object_addresses_dedup_identical_content() {
        let (_dir, root, store) = setup();
        let a = store.put(b"same bytes", "text/plain", "note", "test").unwrap();
        let b = store.put(b"same bytes", "text/plain", "note", "test").unwrap();
        assert_eq!(a, b, "whole-object addressing must dedup content");
        let c = store.put(b"other bytes", "text/plain", "note", "test").unwrap();
        assert_ne!(a, c);

        let report = cas_report(&root).unwrap();
        assert_eq!(report.schema, 1);
        assert_eq!(report.objects, 2);
        assert_eq!(report.unique_hashes, 2);
        assert_eq!(report.manifests, 2);
        assert_eq!(report.total_bytes, 10 + 11);
        assert!(report.missing_objects.is_empty());
        assert!(report.missing_manifests.is_empty());
        assert!(report.corrupt_index.is_empty());
        assert_eq!(report.dedup, "whole-object-blake3");
        assert!(!report.chunk_level_dedup);
        assert!(!report.lists_truncated);
    }

    #[test]
    fn missing_objects_and_manifests_are_reported() {
        let (_dir, root, store) = setup();
        let a = store.put(b"aaa", "text/plain", "note", "test").unwrap().0;
        let b = store.put(b"bbb", "text/plain", "note", "test").unwrap().0;
        // A manifest whose object vanished.
        std::fs::remove_file(root.join("objects").join(&a[..2]).join(&a[2..])).unwrap();
        // An orphan object with no manifest.
        let orphan = "ab".repeat(32);
        std::fs::create_dir_all(root.join("objects").join(&orphan[..2])).unwrap();
        std::fs::write(
            root.join("objects").join(&orphan[..2]).join(&orphan[2..]),
            b"orphan",
        )
        .unwrap();

        let report = cas_report(&root).unwrap();
        assert_eq!(report.missing_objects, vec![a]);
        assert!(report.missing_manifests.contains(&orphan));
        // `b` is healthy and stays out of both lists.
        assert!(!report.missing_objects.contains(&b));
        assert!(!report.missing_manifests.contains(&b));
    }

    #[test]
    fn corrupt_manifest_and_size_mismatch_are_reported() {
        let (_dir, root, store) = setup();
        let a = store.put(b"12345", "text/plain", "note", "test").unwrap().0;
        let b = store.put(b"67890", "text/plain", "note", "test").unwrap().0;
        std::fs::write(manifest_path(&root, &a), b"{not json").unwrap();
        // Size lie: rewrite b's manifest with the wrong size.
        let mut manifest: ArtifactManifest =
            store.manifest_store().read(&b).unwrap().expect("manifest");
        manifest.size = 999;
        store.manifest_store().write(&b, &manifest).unwrap();

        let report = cas_report(&root).unwrap();
        assert!(report
            .corrupt_index
            .iter()
            .any(|entry| entry.path.contains(&a[..2]) && entry.error.contains("parse")));
        assert!(report
            .corrupt_index
            .iter()
            .any(|entry| entry.error.contains("manifest size 999")));
    }

    #[test]
    fn malformed_address_names_are_reported_not_counted() {
        let (_dir, root, store) = setup();
        store.put(b"x", "text/plain", "note", "test").unwrap();
        std::fs::create_dir_all(root.join("objects").join("zz")).unwrap();
        std::fs::write(root.join("objects").join("zz").join("nope"), b"?").unwrap();
        let report = cas_report(&root).unwrap();
        assert_eq!(report.objects, 1);
        assert!(report
            .corrupt_index
            .iter()
            .any(|entry| entry.path.contains("zz") && entry.error.contains("first2")));
    }

    #[test]
    fn missing_root_is_an_empty_report() {
        let dir = tempfile::tempdir().unwrap();
        let report = cas_report(&dir.path().join("nope")).unwrap();
        assert_eq!(report.objects, 0);
        assert_eq!(report.total_bytes, 0);
        assert_eq!(report.unique_hashes, 0);
        assert_eq!(report.manifests, 0);
        assert!(report.corrupt_index.is_empty());
    }
}
