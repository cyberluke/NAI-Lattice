//! Runtime pins (R04 / NAIOS-007).
//!
//! A pin protects one runtime version from the opt-in `prune --prune-runtimes`
//! path. The store is a small JSON document at `<base>/pins.json`:
//!
//! ```json
//! {"schema":1,"pins":[{"id":"…","version":"0.1.1","reason":"rollback",
//!   "created_at_unix_ms":1790000000000,"created_by":"nai-lattice-cli"}]}
//! ```
//!
//! Invariants:
//!
//! * One pin per version: [`PinStore::pin`] is idempotent per version and
//!   returns the existing record instead of duplicating it.
//! * Writes are atomic: a sibling temp file is written, fsynced, then renamed
//!   over `pins.json`.
//! * A missing file means "no pins". A corrupt/unreadable file is an error that
//!   **fails closed for pruning**: no runtime may be deleted while the pins file
//!   cannot be parsed, because an unpinned version cannot be proven.
//! * A corrupt file is never silently overwritten by [`PinStore::pin`] or
//!   [`PinStore::unpin_*`]; the operator must repair or remove it first.
//! * Listing still works well enough to *report* the error (the caller gets the
//!   typed error, not a panic).

use std::fmt;
use std::path::{Path, PathBuf};

/// Lifecycle file name next to the runtime root.
pub const PINS_FILE: &str = "pins.json";
/// Schema version written by this module.
pub const PINS_SCHEMA: u32 = 1;

/// One runtime pin.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PinRecord {
    /// Stable record id (unique per pin event, not per version).
    pub id: String,
    /// Runtime version directory name (for example `0.1.1`).
    pub version: String,
    /// Operator-supplied reason, stable for display.
    #[serde(default)]
    pub reason: String,
    /// Creation time (Unix epoch milliseconds).
    pub created_at_unix_ms: u64,
    /// Actor that created the pin.
    #[serde(default)]
    pub created_by: String,
}

impl PinRecord {
    /// Short human-readable explanation used in prune output and refusals.
    pub fn describe(&self) -> String {
        if self.reason.is_empty() {
            format!("{} pinned by {}", self.version, self.created_by)
        } else {
            format!("{} pinned: {}", self.version, self.reason)
        }
    }
}

/// Pin store failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PinError {
    /// Filesystem failure (read/write/rename).
    Io(String),
    /// The pins file exists but is not a valid schema-1 document, or a stored
    /// version is unusable.
    Corrupt(String),
    /// Invalid caller input (empty or path-like version).
    Invalid(String),
}

impl fmt::Display for PinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PinError::Io(message)
            | PinError::Corrupt(message)
            | PinError::Invalid(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PinError {}

#[derive(serde::Serialize, serde::Deserialize)]
struct PinsDocument {
    schema: u32,
    #[serde(default)]
    pins: Vec<PinRecord>,
}

/// A pins document rooted at one lifecycle base directory.
#[derive(Clone, Debug)]
pub struct PinStore {
    path: PathBuf,
}

impl PinStore {
    /// `<base>/pins.json`.
    pub fn new(base: &Path) -> Self {
        Self {
            path: base.join(PINS_FILE),
        }
    }

    /// Explicit file path (tests / alternate layouts).
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// All pins, oldest first. A missing file is an empty list; a corrupt or
    /// unreadable file is an error (never silently "no pins").
    pub fn list(&self) -> Result<Vec<PinRecord>, PinError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(PinError::Io(format!(
                    "read {}: {error}",
                    self.path.display()
                )))
            }
        };
        let document: PinsDocument = serde_json::from_str(&text).map_err(|error| {
            PinError::Corrupt(format!(
                "parse {}: {error}; pruning is refused until the pins file is repaired",
                self.path.display()
            ))
        })?;
        if document.schema != PINS_SCHEMA {
            return Err(PinError::Corrupt(format!(
                "{}: unsupported pins schema {} (expected {PINS_SCHEMA}); pruning is refused \
                 until the pins file is repaired",
                self.path.display(),
                document.schema
            )));
        }
        Ok(document.pins)
    }

    /// True when `version` has at least one pin. I/O/parse failures propagate.
    pub fn is_pinned(&self, version: &str) -> Result<bool, PinError> {
        Ok(self
            .list()?
            .iter()
            .any(|record| record.version == version))
    }

    /// Pin a version. Idempotent per version: an existing pin is returned
    /// unchanged and the file is not rewritten.
    pub fn pin(&self, version: &str, reason: &str, by: &str) -> Result<PinRecord, PinError> {
        validate_version(version)?;
        let mut pins = self.list()?;
        if let Some(existing) = pins.iter().find(|record| record.version == version) {
            return Ok(existing.clone());
        }
        let record = PinRecord {
            id: new_pin_id(version),
            version: version.to_string(),
            reason: reason.to_string(),
            created_at_unix_ms: now_ms(),
            created_by: by.to_string(),
        };
        pins.push(record.clone());
        self.write(&pins)?;
        Ok(record)
    }

    /// Remove every pin for `version`. Returns true when something was removed.
    pub fn unpin_version(&self, version: &str) -> Result<bool, PinError> {
        validate_version(version)?;
        let mut pins = self.list()?;
        let before = pins.len();
        pins.retain(|record| record.version != version);
        if pins.len() == before {
            return Ok(false);
        }
        self.write(&pins)?;
        Ok(true)
    }

    /// Remove one pin by id. Returns true when something was removed.
    pub fn unpin_id(&self, id: &str) -> Result<bool, PinError> {
        if id.trim().is_empty() {
            return Err(PinError::Invalid("pin id must not be empty".into()));
        }
        let mut pins = self.list()?;
        let before = pins.len();
        pins.retain(|record| record.id != id);
        if pins.len() == before {
            return Ok(false);
        }
        self.write(&pins)?;
        Ok(true)
    }

    /// Atomic temp + fsync + rename. Never leaves a partial `pins.json`.
    fn write(&self, pins: &[PinRecord]) -> Result<(), PinError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| PinError::Io(format!("create {}: {error}", parent.display())))?;
        }
        let document = PinsDocument {
            schema: PINS_SCHEMA,
            pins: pins.to_vec(),
        };
        let text = serde_json::to_string_pretty(&document)
            .map_err(|error| PinError::Corrupt(format!("serialize pins: {error}")))?;
        let temp = self.path.with_file_name(format!(
            ".{PINS_FILE}.tmp-{}-{}",
            std::process::id(),
            now_ms()
        ));
        {
            use std::io::Write as _;
            let mut file = std::fs::File::create(&temp)
                .map_err(|error| PinError::Io(format!("create {}: {error}", temp.display())))?;
            file.write_all(text.as_bytes())
                .map_err(|error| PinError::Io(format!("write {}: {error}", temp.display())))?;
            file.write_all(b"\n")
                .map_err(|error| PinError::Io(format!("write {}: {error}", temp.display())))?;
            file.sync_all()
                .map_err(|error| PinError::Io(format!("sync {}: {error}", temp.display())))?;
        }
        // Read back before publishing: a truncated/failed temp must never become
        // the pins document.
        let read_back = std::fs::read_to_string(&temp)
            .map_err(|error| PinError::Io(format!("read back {}: {error}", temp.display())))?;
        if serde_json::from_str::<PinsDocument>(&read_back).is_err() {
            let _ = std::fs::remove_file(&temp);
            return Err(PinError::Io(format!(
                "read back {}: pins document did not round-trip",
                temp.display()
            )));
        }
        std::fs::rename(&temp, &self.path).map_err(|error| {
            let _ = std::fs::remove_file(&temp);
            PinError::Io(format!(
                "replace {} with {}: {error}",
                self.path.display(),
                temp.display()
            ))
        })?;
        sync_parent(&self.path);
        Ok(())
    }
}

/// Reject versions that could escape `<base>/runtime` or cannot name a runtime.
fn validate_version(version: &str) -> Result<(), PinError> {
    let weight = version.trim();
    if weight.is_empty() {
        return Err(PinError::Invalid("pin version must not be empty".into()));
    }
    if weight != version {
        return Err(PinError::Invalid(format!(
            "invalid pin version `{version}`: leading/trailing whitespace is not a version"
        )));
    }
    if weight.len() > 128 {
        return Err(PinError::Invalid(
            "pin version is longer than 128 characters".into(),
        ));
    }
    if weight == "." || weight == ".." || weight.contains(['/', '\\']) {
        return Err(PinError::Invalid(format!(
            "invalid pin version `{version}`: a version must be a single directory name"
        )));
    }
    Ok(())
}

fn new_pin_id(version: &str) -> String {
    format!("pin-{version}-{:x}-{}", now_ms(), std::process::id())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(directory) = std::fs::File::open(parent) {
            let _ = directory.sync_all();
        }
    }
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, PinStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = PinStore::new(dir.path());
        (dir, store)
    }

    #[test]
    fn missing_file_is_an_empty_list() {
        let (_dir, store) = store();
        assert_eq!(store.list().unwrap(), Vec::new());
        assert!(!store.is_pinned("0.1.2").unwrap());
        assert!(store.path().ends_with(PINS_FILE));
    }

    #[test]
    fn pin_is_idempotent_per_version_and_writes_schema_one() {
        let (_dir, store) = store();
        let first = store.pin("0.1.1", "rollback", "cli").unwrap();
        let second = store.pin("0.1.1", "ignored duplicate", "other").unwrap();
        assert_eq!(first, second);
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(store.is_pinned("0.1.1").unwrap());

        let text = std::fs::read_to_string(store.path()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["schema"], 1);
        assert_eq!(value["pins"][0]["version"], "0.1.1");
        assert_eq!(value["pins"][0]["reason"], "rollback");
        assert_eq!(value["pins"][0]["created_by"], "cli");
        assert!(value["pins"][0]["created_at_unix_ms"].as_u64().unwrap() > 0);
        assert!(value["pins"][0]["id"].as_str().unwrap().contains("0.1.1"));
        assert!(
            !store.path().with_extension("json.tmp").exists(),
            "atomic write must not leave a staging file"
        );
    }

    #[test]
    fn unpin_by_version_and_by_id() {
        let (_dir, store) = store();
        let a = store.pin("0.1.1", "a", "cli").unwrap();
        let b = store.pin("0.1.2", "b", "cli").unwrap();
        assert!(store.unpin_version("0.1.1").unwrap());
        assert!(!store.unpin_version("0.1.1").unwrap());
        assert_eq!(store.list().unwrap(), vec![b.clone()]);
        assert!(store.unpin_id(&b.id).unwrap());
        assert!(!store.unpin_id(&b.id).unwrap());
        assert!(store.list().unwrap().is_empty());
        // An id that never existed is a no-op, not an error.
        assert!(!store.unpin_id(&a.id).unwrap());
    }

    #[test]
    fn corrupt_file_fails_closed_but_list_reports_the_error() {
        let (dir, store) = store();
        std::fs::write(store.path(), b"{not json").unwrap();
        match store.list() {
            Err(PinError::Corrupt(message)) => {
                assert!(message.contains("pruning is refused"), "{message}");
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
        // Mutations must not clobber evidence.
        assert!(matches!(
            store.pin("0.1.1", "r", "cli"),
            Err(PinError::Corrupt(_))
        ));
        assert_eq!(std::fs::read(store.path()).unwrap(), b"{not json");
        assert!(!dir.path().join(format!(".{PINS_FILE}.tmp-1")).exists());
    }

    #[test]
    fn unsupported_schema_is_corrupt() {
        let (_dir, store) = store();
        std::fs::write(store.path(), br#"{"schema":2,"pins":[]}"#).unwrap();
        assert!(matches!(store.list(), Err(PinError::Corrupt(_))));
    }

    #[test]
    fn versions_that_could_escape_the_runtime_tree_are_rejected() {
        let (_dir, store) = store();
        for bad in ["", "   ", " 0.1.1", "0.1.1 ", ".", "..", "../0.1.1", "a/b", "a\\b"] {
            assert!(
                matches!(store.pin(bad, "r", "cli"), Err(PinError::Invalid(_))),
                "version {bad:?} must be rejected"
            );
        }
        assert!(store.list().unwrap().is_empty());
    }
}
