//! Shared storage lifecycle policy for NAI Lattice.
//!
//! This crate is the single source of truth for three things that used to be
//! duplicated (and drifting) between the CLI, the runtime NAF surface and the
//! dashboard summary:
//!
//! 1. **Inventory sizing** — the `storage` payload frozen in
//!    `docs/DASHBOARD_API.md` §1 (`storage_status`).
//! 2. **Safe prune planning** — stale rotated logs/tmp, expired backups and
//!    audit rotation only. The canonical ledger, CAS, derived stores (hot /
//!    search / vector), read-model files, `active.json`, `runtime/` and the
//!    maintenance lease are never planned (`safe_prune_plan`).
//! 3. **Applying a prune plan** — reparse-point-safe deletion, audit rotation
//!    and the shared `maintenance.audit.jsonl` record (`apply_safe_prune`).
//!
//! The policy is deliberately conservative:
//!
//! * The newest `*.log` file is treated as the log the host is currently
//!   writing and is skipped. Rotated logs, `*.tmp` and `*.partial` files older
//!   than `logs_days` are eligible.
//! * The audit file is rotated (old JSONL records dropped, unparseable lines
//!   kept), never deleted.
//! * A directory is only planned as a backup when it lives under
//!   `<base>/backups` and its mtime is older than `backup_days`.
//!
//! `storage_status` reads `lastPrune` from the last successful `prune` record
//! in `<base>/maintenance.audit.jsonl`. Free-space probing is a Windows-only
//! convenience; when it is unavailable the three byte fields are `null` and
//! pressure is `"unknown"` (never a synthesized zero).

pub mod pins;

use std::io::{BufRead, BufReader, Write as _};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub use pins::{PinError, PinRecord, PinStore, PINS_FILE};

/// Lifecycle audit ledger written next to the runtime root.
pub const AUDIT_FILE: &str = "maintenance.audit.jsonl";
/// Backup collection directory next to the runtime root.
pub const BACKUP_DIR: &str = "backups";

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
/// Never budget less than 512 MiB, even on a nearly full volume.
const MIN_BUDGET_BYTES: u64 = 512 * MIB;
/// Reserve at least 20 GiB (or 15% of free space, whichever is larger).
const MIN_RESERVE_BYTES: u64 = 20 * GIB;
const ELEVATED_PERCENT: u128 = 90;
const DAY_MS: u64 = 86_400_000;
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Runtime root plus its lifecycle base directory.
///
/// In the canonical layout `root` is `<base>/v1`, but the pair is explicit so a
/// caller that owns a custom root (tests, alternate installs) still resolves
/// the lifecycle files consistently.
#[derive(Clone, Debug)]
pub struct Paths {
    /// `<base>` — lifecycle state: audit ledger, policy, backups, runtimes.
    pub base: PathBuf,
    /// `<root>` — canonical runtime data.
    pub root: PathBuf,
}

impl Paths {
    /// `base` is `root`'s parent. A root without a parent keeps an empty base
    /// (never the root itself), so lifecycle files can never land inside the
    /// protected data tree by accident.
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let base = root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        Self { base, root }
    }

    /// `NAI_LATTICE_ROOT` when set; otherwise `%LOCALAPPDATA%\NAI\Lattice\v1`
    /// (`.` when the environment is missing, matching [`RuntimeConfig`]'s
    /// default). [`RuntimeConfig`]: https://docs.rs/nai-lattice-runtime
    pub fn from_env() -> Self {
        if let Some(root) = std::env::var_os("NAI_LATTICE_ROOT") {
            return Self::from_root(PathBuf::from(root));
        }
        let local = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self::from_root(local.join("NAI").join("Lattice").join("v1"))
    }
}

/// Retention policy shared by every surface.
#[derive(Clone, Debug)]
pub struct StorageConfig {
    /// Rotated log / temp files older than this many days are eligible.
    pub logs_days: u32,
    /// Audit records older than this many days are rotated out.
    pub audit_days: u32,
    /// Backup directories older than this many days are eligible.
    pub backup_days: u32,
    /// Superseded runtime versions to keep (retention display only; the shared
    /// safe plan never deletes runtimes).
    pub runtime_keep: usize,
}

impl Default for StorageConfig {
    fn default() -> Self {
        // Matches the CLI's `MaintenanceConfig::default()`.
        Self {
            logs_days: 30,
            audit_days: 90,
            backup_days: 14,
            runtime_keep: 2,
        }
    }
}

/// One planned, non-destructive-by-contract prune action.
#[derive(Clone, Debug)]
pub struct PruneItem {
    /// Absolute path to delete (or the audit file to rotate).
    pub path: PathBuf,
    /// Measured bytes (directory sizes for backups, record bytes for audit).
    pub bytes: u64,
    /// Human-readable reason, stable for CLI/dashboard display.
    pub reason: &'static str,
    /// `logs`, `backups`, `audit` (shared policy) or `runtimes` (the CLI's
    /// opt-in superseded-runtime pruning, confined to `<base>/runtime`).
    pub class: &'static str,
}

/// Outcome of [`apply_safe_prune`] / [`apply_prune_items`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PruneReport {
    /// Number of deleted paths (audit rotation is reported separately and is
    /// not a path deletion).
    pub items: usize,
    /// Bytes actually reclaimed (deleted files plus dropped audit records).
    pub bytes_freed: u64,
    /// True when nothing was changed and no audit record was appended.
    pub dry_run: bool,
    /// True when a `prune` audit record was appended.
    pub audit_appended: bool,
}

/// Automatic reclaim gate derived from a `storage_status` payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReclaimDecision {
    /// Do nothing, with a stable human-readable reason.
    Skip(&'static str),
    /// Pressure warrants the bounded safe prune.
    Prune,
}

/// Decide whether automatic maintenance may reclaim.
///
/// This is a pure function of the measured pressure: `elevated` and `critical`
/// prune; `normal` skips; anything missing, malformed or `unknown` skips
/// (`pressure unknown`) so a scheduled task fails closed instead of guessing.
pub fn reclaim_decision(status: &serde_json::Value) -> ReclaimDecision {
    match status.get("pressure").and_then(serde_json::Value::as_str) {
        Some("elevated") | Some("critical") => ReclaimDecision::Prune,
        Some("normal") => ReclaimDecision::Skip("pressure normal"),
        _ => ReclaimDecision::Skip("pressure unknown"),
    }
}

/// Real byte size of a file or directory tree. A missing path is `0`. Reparse
/// points are counted as `0` and never traversed, so a junction cannot inflate
/// the inventory or create a cycle.
pub fn directory_size(path: &Path) -> u64 {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if is_reparse_point(&md) {
        return 0;
    }
    if !md.is_dir() {
        return md.len();
    }
    let Ok(rd) = std::fs::read_dir(path) else {
        return 0;
    };
    rd.flatten().map(|e| directory_size(&e.path())).sum()
}

/// The frozen `docs/DASHBOARD_API.md` §1 `storage` payload.
///
/// `usedBytes` is the managed footprint: the runtime root plus the base-level
/// backups and audit ledger. Free-space failure is represented as `null`
/// fields and `pressure: "unknown"`.
pub fn storage_status(paths: &Paths, cfg: &StorageConfig) -> serde_json::Value {
    let free = free_space_bytes(&probe_dir(paths));
    storage_status_with_free(paths, cfg, free)
}

/// [`storage_status`] with an injected free-space value. Used by unit tests and
/// diagnostics on hosts without a probeable volume.
#[doc(hidden)]
pub fn storage_status_with_free(
    paths: &Paths,
    cfg: &StorageConfig,
    free_bytes: Option<u64>,
) -> serde_json::Value {
    let classes = ClassSizes::measure(paths);
    let audit_bytes = file_size(&audit_path(paths));
    let used_bytes = directory_size(&paths.root)
        .saturating_add(directory_size(&paths.base.join(BACKUP_DIR)))
        .saturating_add(audit_bytes);
    let budget = derive_budget(free_bytes);
    serde_json::json!({
        "schema": 1,
        "root": paths.root.display().to_string(),
        "base": paths.base.display().to_string(),
        "budgetBytes": budget.map(|b| b.budget),
        "usedBytes": used_bytes,
        "freeBytes": free_bytes,
        "reserveBytes": budget.map(|b| b.reserve),
        "pressure": pressure(used_bytes, budget.map(|b| b.budget)),
        "classes": {
            "ledger": classes.ledger,
            "cas": classes.cas,
            "hot": classes.hot,
            "search": classes.search,
            "vector": classes.vector,
            "readModels": classes.read_models,
            "logs": classes.logs,
            "backups": classes.backups,
            "audit": classes.audit,
        },
        "retention": {
            "logsDays": cfg.logs_days,
            "auditDays": cfg.audit_days,
            "backupDays": cfg.backup_days,
            "runtimeKeep": cfg.runtime_keep,
        },
        // Additive optional field (schema stays 1): pin count + pinned versions
        // so a dashboard can explain why a runtime was not pruned. A corrupt
        // pins file reports its error instead of pretending there are no pins.
        "pins": pins_status(paths),
        "lastPrune": last_prune(paths),
    })
}

/// Pin inventory for the storage status payload.
fn pins_status(paths: &Paths) -> serde_json::Value {
    match PinStore::new(&paths.base).list() {
        Ok(pins) => serde_json::json!({
            "count": pins.len(),
            "versions": pins.iter().map(|pin| pin.version.clone()).collect::<Vec<_>>(),
        }),
        Err(error) => serde_json::json!({
            "count": serde_json::Value::Null,
            "versions": Vec::<String>::new(),
            "error": error.to_string(),
        }),
    }
}

/// Plan only the actions the shared policy considers safe. The canonical
/// ledger/CAS/derived stores/read models/lease are never included.
pub fn safe_prune_plan(paths: &Paths, cfg: &StorageConfig) -> Vec<PruneItem> {
    let mut items = Vec::new();
    let now = now_ms();
    let logs_cutoff = cutoff_ms(now, cfg.logs_days);

    let log_entries = direct_entries(&paths.root, &["log", "tmp", "partial"]);
    // The newest `*.log` is the file the host is currently writing: deleting it
    // on Windows would fail anyway, and on POSIX it would truncate live logs.
    let current_log = log_entries
        .iter()
        .filter(|entry| entry.extension == "log")
        .filter_map(|entry| entry.mtime.map(|mtime| (mtime, entry.path.clone())))
        .max()
        .map(|(_, path)| path);
    for entry in &log_entries {
        if Some(&entry.path) == current_log.as_ref() {
            continue;
        }
        // An unreadable mtime is NOT evidence of age (fail closed, matching the
        // CLI's `older_than` behaviour).
        let Some(mtime) = entry.mtime else {
            continue;
        };
        if mtime >= logs_cutoff || is_protected(paths, &entry.path) {
            continue;
        }
        items.push(PruneItem {
            path: entry.path.clone(),
            bytes: entry.bytes,
            reason: "stale log/temp",
            class: "logs",
        });
    }

    let backups = paths.base.join(BACKUP_DIR);
    if let Ok(rd) = std::fs::read_dir(&backups) {
        let backups_cutoff = cutoff_ms(now, cfg.backup_days);
        for entry in rd.flatten() {
            let path = entry.path();
            let Ok(md) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !path.is_dir() || is_protected(paths, &path) {
                continue;
            }
            // An unreadable mtime is not evidence of age.
            let Some(mtime) = mtime_ms(&md) else {
                continue;
            };
            if mtime >= backups_cutoff {
                continue;
            }
            items.push(PruneItem {
                path: path.clone(),
                bytes: directory_size(&path),
                reason: "expired backup",
                class: "backups",
            });
        }
    }

    // Audit rotation is planned separately from deletion: apply rewrites the
    // file and keeps every unparseable line.
    if let Some(rotation) = audit_rotation_preview(paths, cfg.audit_days) {
        if rotation.dropped_bytes > 0 {
            items.push(PruneItem {
                path: audit_path(paths),
                bytes: rotation.dropped_bytes,
                reason: "audit rotation",
                class: "audit",
            });
        }
    }

    items
}

/// Plan and apply the shared safe prune. `dry_run` changes nothing and appends
/// no audit record. A real run deletes plan items, rotates the audit file, then
/// appends the same `prune` JSONL record shape the CLI uses.
pub fn apply_safe_prune(
    paths: &Paths,
    cfg: &StorageConfig,
    dry_run: bool,
) -> Result<PruneReport, String> {
    let plan = safe_prune_plan(paths, cfg);
    apply_prune_items(paths, cfg, &plan, dry_run, None)
}

/// Apply a caller-merged plan (shared safety + audit writer).
///
/// The CLI adds its opt-in superseded-runtime items to a [`safe_prune_plan`]
/// and applies the merged list through this function so deletion, audit
/// rotation and the audit record stay single-sourced. `extra_detail` is merged
/// into the audit record's `detail` object (the CLI uses it to preserve
/// `host_stopped` and the full policy object).
///
/// Refuses any path the shared policy protects, even if the caller supplied it.
pub fn apply_prune_items(
    paths: &Paths,
    cfg: &StorageConfig,
    items: &[PruneItem],
    dry_run: bool,
    extra_detail: Option<&serde_json::Value>,
) -> Result<PruneReport, String> {
    // Runtime pins fail closed: a corrupt/unreadable pins file means no runtime
    // version can be proven unpinned, so the whole runtime class is refused.
    let pins = if items.iter().any(|item| item.class == "runtimes") {
        Some(PinStore::new(&paths.base).list().map_err(|error| {
            format!(
                "refusing runtime prune: the pins file cannot be read ({error}); \
                 no version can be proven unpinned"
            )
        })?)
    } else {
        None
    };
    for item in items {
        if item.class == "audit" {
            if !same_path(&item.path, &audit_path(paths)) {
                return Err(format!(
                    "refusing audit rotation of {}: not the lifecycle audit ledger",
                    item.path.display()
                ));
            }
        } else if item.class == "runtimes" {
            // Reserved for the CLI's explicit `--prune-runtimes`: only a direct
            // child of `<base>/runtime`, never the directory itself, and never
            // the version `active.json` currently points at.
            let runtime_root = paths.base.join("runtime");
            if same_path(&item.path, &runtime_root)
                || !path_starts_with(&item.path, &runtime_root)
            {
                return Err(format!(
                    "refusing runtime prune of {}: not under {}",
                    item.path.display(),
                    runtime_root.display()
                ));
            }
            let Some(active) = active_runtime_version(paths) else {
                return Err(format!(
                    "refusing runtime prune of {}: {} is missing or has no `version`; \
                     the active runtime cannot be protected",
                    item.path.display(),
                    paths.base.join("active.json").display()
                ));
            };
            if item.path.file_name().and_then(|name| name.to_str()) == Some(active.as_str()) {
                return Err(format!(
                    "refusing runtime prune of {}: it is the active runtime",
                    item.path.display()
                ));
            }
            // A pinned version is never deletable, even if a caller planned it.
            let pinned = pins.as_ref().and_then(|pins| {
                item.path
                    .strip_prefix(&runtime_root)
                    .ok()
                    .and_then(|relative| relative.components().next())
                    .and_then(|component| component.as_os_str().to_str())
                    .and_then(|version| {
                        pins.iter()
                            .find(|pin| pin.version == version)
                            .map(|pin| (version.to_string(), pin.clone()))
                    })
            });
            if let Some((version, pin)) = pinned {
                return Err(format!(
                    "refusing runtime prune of {}: version {} is pinned ({})",
                    item.path.display(),
                    version,
                    pin.describe()
                ));
            }
        } else if is_protected(paths, &item.path) {
            return Err(format!(
                "refusing to prune protected path {}",
                item.path.display()
            ));
        }
    }

    if dry_run {
        let deleted_items = items.iter().filter(|item| item.class != "audit").count();
        let bytes_freed = items.iter().map(|item| item.bytes).sum();
        return Ok(PruneReport {
            items: deleted_items,
            bytes_freed,
            dry_run: true,
            audit_appended: false,
        });
    }

    let mut deleted = 0usize;
    let mut bytes_freed = 0u64;
    let mut audit_removed = 0usize;
    for item in items {
        if item.class == "audit" {
            let rotation = rotate_audit(paths, cfg.audit_days)?;
            bytes_freed = bytes_freed.saturating_add(rotation.dropped_bytes);
            audit_removed += rotation.dropped_records;
        } else {
            bytes_freed = bytes_freed.saturating_add(remove_tree_safe(&item.path)?);
            deleted += 1;
        }
    }

    let mut detail = serde_json::json!({
        "items": deleted,
        "bytes": bytes_freed,
        "audit_records_removed": audit_removed,
        "config": config_json(cfg),
    });
    if let Some(extra) = extra_detail.and_then(serde_json::Value::as_object) {
        if let Some(detail) = detail.as_object_mut() {
            for (key, value) in extra {
                detail.insert(key.clone(), value.clone());
            }
        }
    }
    let record = serde_json::json!({
        "ts_ms": now_ms(),
        "ts_utc": now_iso(),
        "action": "prune",
        "result": "ok",
        "cli_version": env!("CARGO_PKG_VERSION"),
        "detail": detail,
    });
    append_audit(paths, &record)?;

    Ok(PruneReport {
        items: deleted,
        bytes_freed,
        dry_run: false,
        audit_appended: true,
    })
}

// ---------------------------------------------------------------------------
// Inventory internals
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct ClassSizes {
    ledger: u64,
    cas: u64,
    hot: u64,
    search: u64,
    vector: u64,
    read_models: u64,
    logs: u64,
    backups: u64,
    audit: u64,
}

impl ClassSizes {
    fn measure(paths: &Paths) -> Self {
        let read_models = file_size(&paths.root.join("read-model.json"))
            .saturating_add(file_size(&paths.root.join("multi-dim-index.json")));
        let logs = direct_entries(&paths.root, &["log", "tmp", "partial"])
            .iter()
            .map(|entry| entry.bytes)
            .sum();
        Self {
            ledger: directory_size(&paths.root.join("ledger")),
            cas: directory_size(&paths.root.join("cas")),
            hot: directory_size(&paths.root.join("hot")),
            search: directory_size(&paths.root.join("search")),
            vector: directory_size(&paths.root.join("vector")),
            read_models,
            logs,
            backups: directory_size(&paths.base.join(BACKUP_DIR)),
            audit: file_size(&audit_path(paths)),
        }
    }
}

#[derive(Debug, Clone)]
struct DirectEntry {
    path: PathBuf,
    extension: String,
    /// `None` when the filesystem cannot report a modification time.
    mtime: Option<u64>,
    bytes: u64,
}

/// Direct child files (not directories) under `dir` whose extension is one of
/// `extensions`. Directory sizes are never included here.
fn direct_entries(dir: &Path, extensions: &[&str]) -> Vec<DirectEntry> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        let Ok(md) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if md.is_dir() {
            continue;
        }
        let extension = path
            .extension()
            .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if !extensions.iter().any(|candidate| *candidate == extension) {
            continue;
        }
        out.push(DirectEntry {
            path,
            extension,
            mtime: mtime_ms(&md),
            bytes: if is_reparse_point(&md) { 0 } else { md.len() },
        });
    }
    out
}

fn config_json(cfg: &StorageConfig) -> serde_json::Value {
    serde_json::json!({
        "logs_days": cfg.logs_days,
        "audit_days": cfg.audit_days,
        "backup_days": cfg.backup_days,
        "runtime_keep": cfg.runtime_keep,
    })
}

// ---------------------------------------------------------------------------
// Budget math
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Budget {
    budget: u64,
    reserve: u64,
}

fn derive_budget(free_bytes: Option<u64>) -> Option<Budget> {
    let free = free_bytes?;
    let percent = (free as u128 * 15 / 100).min(u64::MAX as u128) as u64;
    let reserve = percent.max(MIN_RESERVE_BYTES);
    let budget = free.saturating_sub(reserve).max(MIN_BUDGET_BYTES);
    Some(Budget { budget, reserve })
}

fn pressure(used_bytes: u64, budget_bytes: Option<u64>) -> &'static str {
    let Some(budget) = budget_bytes else {
        return "unknown";
    };
    if used_bytes > budget {
        return "critical";
    }
    if used_bytes as u128 * 100 >= budget as u128 * ELEVATED_PERCENT {
        return "elevated";
    }
    "normal"
}

#[cfg(windows)]
fn free_space_bytes(dir: &Path) -> Option<u64> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let directory = HSTRING::from(dir.as_os_str().to_string_lossy().as_ref());
    let mut available = 0u64;
    // SAFETY: `directory` is a valid NUL-terminated HSTRING for the duration of
    // the call and `available` is a valid out-pointer.
    unsafe { GetDiskFreeSpaceExW(&directory, Some(&mut available), None, None) }
        .ok()
        .map(|_| available)
}

#[cfg(not(windows))]
fn free_space_bytes(_dir: &Path) -> Option<u64> {
    None
}

/// Prefer the runtime root; fall back to its nearest existing ancestor so the
/// probe works before the first host start.
fn probe_dir(paths: &Paths) -> PathBuf {
    if paths.root.exists() {
        return paths.root.clone();
    }
    if paths.base.exists() {
        return paths.base.clone();
    }
    let mut candidate = paths.root.as_path();
    while let Some(parent) = candidate.parent() {
        if parent.exists() {
            return parent.to_path_buf();
        }
        candidate = parent;
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

// ---------------------------------------------------------------------------
// Audit ledger
// ---------------------------------------------------------------------------

/// Version recorded in `<base>/active.json` (the shared runtime pointer).
fn active_runtime_version(paths: &Paths) -> Option<String> {
    let text = std::fs::read_to_string(paths.base.join("active.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|version| !version.is_empty())
}

fn audit_path(paths: &Paths) -> PathBuf {
    paths.base.join(AUDIT_FILE)
}

fn last_prune(paths: &Paths) -> serde_json::Value {
    let Ok(file) = std::fs::File::open(audit_path(paths)) else {
        return serde_json::Value::Null;
    };
    let mut last = serde_json::Value::Null;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("action").and_then(serde_json::Value::as_str) != Some("prune")
            || value.get("result").and_then(serde_json::Value::as_str) != Some("ok")
        {
            continue;
        }
        let Some(run_unix_ms) = value.get("ts_ms").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        let bytes_freed = value
            .get("detail")
            .and_then(|detail| detail.get("bytes"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        last = serde_json::json!({
            "runUnixMs": run_unix_ms,
            "bytesFreed": bytes_freed,
        });
    }
    last
}

fn append_audit(paths: &Paths, record: &serde_json::Value) -> Result<(), String> {
    let line = serde_json::to_string(record).map_err(|e| format!("serialize audit record: {e}"))?;
    let path = audit_path(paths);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create audit dir: {e}"))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    file.write_all(format!("{line}\n").as_bytes())
        .map_err(|e| format!("append {}: {e}", path.display()))
}

#[derive(Debug)]
struct AuditRotation {
    dropped_bytes: u64,
    dropped_records: usize,
    kept: String,
}

fn audit_rotation_preview(paths: &Paths, days: u32) -> Option<AuditRotation> {
    let text = std::fs::read_to_string(audit_path(paths)).ok()?;
    let cutoff = cutoff_ms(now_ms(), days);
    let mut kept = String::new();
    let mut dropped_bytes = 0u64;
    let mut dropped_records = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let drop = match serde_json::from_str::<serde_json::Value>(line) {
            Ok(value) => value
                .get("ts_ms")
                .and_then(serde_json::Value::as_u64)
                .map(|ts| ts < cutoff)
                .unwrap_or(false),
            Err(_) => false,
        };
        if drop {
            dropped_records += 1;
            dropped_bytes += line.len() as u64 + 1;
        } else {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    if dropped_records == 0 {
        None
    } else {
        Some(AuditRotation {
            dropped_bytes,
            dropped_records,
            kept,
        })
    }
}

#[derive(Debug)]
struct RotationResult {
    dropped_bytes: u64,
    dropped_records: usize,
}

fn rotate_audit(paths: &Paths, days: u32) -> Result<RotationResult, String> {
    let Some(rotation) = audit_rotation_preview(paths, days) else {
        return Ok(RotationResult {
            dropped_bytes: 0,
            dropped_records: 0,
        });
    };
    let path = audit_path(paths);
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, &rotation.kept).map_err(|e| format!("write audit tmp: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("replace audit: {e}"))?;
    Ok(RotationResult {
        dropped_bytes: rotation.dropped_bytes,
        dropped_records: rotation.dropped_records,
    })
}

// ---------------------------------------------------------------------------
// Filesystem safety
// ---------------------------------------------------------------------------

fn file_size(path: &Path) -> u64 {
    std::fs::symlink_metadata(path)
        .map(|md| {
            if md.is_dir() || is_reparse_point(&md) {
                0
            } else {
                md.len()
            }
        })
        .unwrap_or(0)
}

fn mtime_ms(md: &std::fs::Metadata) -> Option<u64> {
    md.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
}

fn cutoff_ms(now: u64, days: u32) -> u64 {
    now.saturating_sub((days as u64).saturating_mul(DAY_MS))
}

fn is_reparse_point(md: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        md.file_type().is_symlink()
    }
}

/// Remove a file or directory without following junctions/symlinks. Returns the
/// bytes reclaimed (0 when the path is already gone).
fn remove_tree_safe(path: &Path) -> Result<u64, String> {
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("stat {}: {e}", path.display())),
    };
    let bytes = if md.is_dir() { directory_size(path) } else { md.len() };
    if is_reparse_point(&md) {
        if md.is_dir() {
            std::fs::remove_dir(path)
                .map_err(|e| format!("remove junction {}: {e}", path.display()))?;
        } else {
            std::fs::remove_file(path)
                .map_err(|e| format!("remove link {}: {e}", path.display()))?;
        }
    } else if md.is_dir() {
        std::fs::remove_dir_all(path).map_err(|e| format!("remove {}: {e}", path.display()))?;
    } else {
        std::fs::remove_file(path).map_err(|e| format!("remove {}: {e}", path.display()))?;
    }
    Ok(bytes)
}

/// Canonical paths the shared policy must never delete or rotate.
fn is_protected(paths: &Paths, path: &Path) -> bool {
    if same_path(path, &paths.root) || same_path(path, &paths.base) {
        return true;
    }
    let protected_files = [
        paths.root.join("read-model.json"),
        paths.root.join("multi-dim-index.json"),
        paths.root.join("maintenance.lock.json"),
    ];
    if protected_files.iter().any(|file| same_path(path, file)) {
        return true;
    }
    // The lease also has staging siblings (`maintenance.lock.json.tmp`, ...).
    if path
        .file_name()
        .map(|name| name.to_string_lossy().starts_with("maintenance.lock.json"))
        .unwrap_or(false)
    {
        return true;
    }
    let protected_dirs = [
        paths.root.join("ledger"),
        paths.root.join("cas"),
        paths.root.join("hot"),
        paths.root.join("search"),
        paths.root.join("vector"),
        paths.base.join("runtime"),
    ];
    if protected_dirs
        .iter()
        .any(|dir| path_starts_with(path, dir))
    {
        return true;
    }
    same_path(path, &paths.base.join("active.json"))
}

/// Resolve `.`/`..` lexically and make the path absolute (no I/O).
fn normalize_absolute(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn normalized_component(component: Component<'_>) -> String {
    let text = component.as_os_str().to_string_lossy().replace('/', "\\");
    #[cfg(windows)]
    {
        text.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        text
    }
}

fn path_starts_with(path: &Path, base: &Path) -> bool {
    let path = normalize_absolute(path);
    let base = normalize_absolute(base);
    let path_parts: Vec<String> = path.components().map(normalized_component).collect();
    let base_parts: Vec<String> = base.components().map(normalized_component).collect();
    base_parts.len() <= path_parts.len() && path_parts[..base_parts.len()] == base_parts[..]
}

fn same_path(a: &Path, b: &Path) -> bool {
    path_starts_with(a, b) && path_starts_with(b, a)
}

// ---------------------------------------------------------------------------
// UTC formatting (no date dependency)
// ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn iso_from_ms(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn now_iso() -> String {
    iso_from_ms(now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_paths() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("v1");
        std::fs::create_dir_all(&root).expect("create root");
        let paths = Paths::from_root(root);
        (dir, paths)
    }

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    fn age(path: &Path, days: u64) {
        let when = SystemTime::now() - Duration::from_secs(days * 86_400);
        // Dev-only helper: `std::fs::File::set_modified` cannot open a
        // directory on Windows, and `filetime` can.
        #[allow(deprecated)]
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(when)).unwrap();
    }

    #[test]
    fn class_sizes_are_real_and_missing_classes_are_zero() {
        let (_dir, paths) = temp_paths();
        write(&paths.root.join("ledger").join("a.bin"), &[1u8; 100]);
        write(&paths.root.join("cas").join("b.bin"), &[2u8; 200]);
        write(&paths.root.join("hot").join("c.bin"), &[3u8; 300]);
        write(&paths.root.join("search").join("d.bin"), &[4u8; 400]);
        write(&paths.root.join("vector").join("e.bin"), &[5u8; 500]);
        write(&paths.root.join("read-model.json"), &[6u8; 600]);
        write(&paths.root.join("multi-dim-index.json"), &[7u8; 700]);
        write(&paths.root.join("host.log"), &[8u8; 800]);
        write(&paths.base.join("backups").join("b1").join("f.bin"), &[9u8; 900]);
        write(&paths.base.join(AUDIT_FILE), &[10u8; 1000]);

        let status = storage_status_with_free(&paths, &StorageConfig::default(), Some(100 * GIB));
        assert_eq!(status["schema"], 1);
        assert_eq!(status["classes"]["ledger"], 100);
        assert_eq!(status["classes"]["cas"], 200);
        assert_eq!(status["classes"]["hot"], 300);
        assert_eq!(status["classes"]["search"], 400);
        assert_eq!(status["classes"]["vector"], 500);
        assert_eq!(status["classes"]["readModels"], 1300);
        assert_eq!(status["classes"]["logs"], 800);
        assert_eq!(status["classes"]["backups"], 900);
        assert_eq!(status["classes"]["audit"], 1000);
        let used = status["usedBytes"].as_u64().unwrap();
        assert_eq!(
            used,
            directory_size(&paths.root)
                + directory_size(&paths.base.join(BACKUP_DIR))
                + 1000
        );
        assert_eq!(status["retention"]["logsDays"], 30);
        assert_eq!(status["retention"]["auditDays"], 90);
        assert_eq!(status["retention"]["backupDays"], 14);
        assert_eq!(status["retention"]["runtimeKeep"], 2);
        assert!(status["lastPrune"].is_null());

        let other = tempfile::tempdir().expect("second tempdir");
        let missing = storage_status_with_free(
            &Paths::from_root(other.path().join("v1")),
            &StorageConfig::default(),
            None,
        );
        for class in [
            "ledger", "cas", "hot", "search", "vector", "readModels", "logs", "backups",
            "audit",
        ] {
            assert_eq!(missing["classes"][class], 0, "missing {class} must be 0");
        }
        assert!(missing["freeBytes"].is_null());
        assert!(missing["budgetBytes"].is_null());
        assert!(missing["reserveBytes"].is_null());
        assert_eq!(missing["pressure"], "unknown");
    }

    #[test]
    fn budget_and_pressure_follow_the_contract() {
        assert_eq!(derive_budget(None), None);
        assert_eq!(
            derive_budget(Some(100 * GIB)),
            Some(Budget {
                budget: 80 * GIB,
                reserve: 20 * GIB
            })
        );
        assert_eq!(
            derive_budget(Some(1000 * GIB)),
            Some(Budget {
                budget: 850 * GIB,
                reserve: 150 * GIB
            })
        );
        assert_eq!(
            derive_budget(Some(10 * GIB)),
            Some(Budget {
                budget: MIN_BUDGET_BYTES,
                reserve: 20 * GIB
            })
        );
        assert_eq!(
            derive_budget(Some(0)),
            Some(Budget {
                budget: MIN_BUDGET_BYTES,
                reserve: 20 * GIB
            })
        );

        assert_eq!(pressure(10, None), "unknown");
        assert_eq!(pressure(81, Some(80)), "critical");
        assert_eq!(pressure(80, Some(80)), "elevated");
        assert_eq!(pressure(90, Some(100)), "elevated");
        assert_eq!(pressure(89, Some(100)), "normal");
        assert_eq!(pressure(10, Some(100)), "normal");
    }

    #[test]
    fn reclaim_decision_is_pressure_gated_and_fails_closed() {
        let decision = |pressure: &str| reclaim_decision(&serde_json::json!({ "pressure": pressure }));
        assert_eq!(
            decision("normal"),
            ReclaimDecision::Skip("pressure normal")
        );
        assert_eq!(
            decision("unknown"),
            ReclaimDecision::Skip("pressure unknown")
        );
        assert_eq!(decision("elevated"), ReclaimDecision::Prune);
        assert_eq!(decision("critical"), ReclaimDecision::Prune);
        // Missing or malformed pressure is never a licence to mutate.
        assert_eq!(
            reclaim_decision(&serde_json::json!({})),
            ReclaimDecision::Skip("pressure unknown")
        );
        assert_eq!(
            reclaim_decision(&serde_json::json!({ "pressure": 90 })),
            ReclaimDecision::Skip("pressure unknown")
        );
        assert_eq!(
            reclaim_decision(&serde_json::json!({ "pressure": "Normal" })),
            ReclaimDecision::Skip("pressure unknown")
        );

        // The decision agrees with the real status payload at both extremes.
        let (_dir, paths) = temp_paths();
        let normal =
            storage_status_with_free(&paths, &StorageConfig::default(), Some(100 * GIB));
        assert_eq!(normal["pressure"], "normal");
        assert_eq!(
            reclaim_decision(&normal),
            ReclaimDecision::Skip("pressure normal")
        );
    }

    #[test]
    fn safe_plan_never_includes_canonical_paths() {
        let (_dir, paths) = temp_paths();
        write(&paths.root.join("ledger").join("seg.bin"), b"ledger");
        write(&paths.root.join("cas").join("obj.bin"), b"cas");
        write(&paths.root.join("hot").join("kv.bin"), b"hot");
        write(&paths.root.join("search").join("idx.bin"), b"search");
        write(&paths.root.join("vector").join("hnsw.bin"), b"vector");
        write(&paths.root.join("read-model.json"), b"{}");
        write(&paths.root.join("multi-dim-index.json"), b"{}");
        write(&paths.root.join("maintenance.lock.json"), b"lease");
        write(&paths.base.join("active.json"), b"{}");
        write(&paths.base.join("runtime").join("0.1.1").join("host.exe"), b"exe");

        // Decoy directory with a log extension: must never be planned.
        std::fs::create_dir_all(paths.root.join("ledger.log")).unwrap();
        age(&paths.root.join("ledger.log"), 90);

        write(&paths.root.join("current.log"), b"current");
        write(&paths.root.join("old.log"), b"old");
        write(&paths.root.join("scratch.tmp"), b"tmp");
        age(&paths.root.join("old.log"), 60);
        age(&paths.root.join("scratch.tmp"), 60);
        std::fs::create_dir_all(paths.base.join(BACKUP_DIR).join("old")).unwrap();
        write(&paths.base.join(BACKUP_DIR).join("old").join("f"), b"old");
        age(&paths.base.join(BACKUP_DIR).join("old"), 30);
        std::fs::create_dir_all(paths.base.join(BACKUP_DIR).join("new")).unwrap();
        write(&paths.base.join(AUDIT_FILE), b"{\"ts_ms\":0,\"action\":\"prune\",\"result\":\"ok\"}\n");

        let plan = safe_prune_plan(&paths, &StorageConfig::default());
        let paths_in_plan: Vec<String> = plan
            .iter()
            .map(|item| item.path.display().to_string())
            .collect();
        assert!(paths_in_plan.iter().any(|p| p.ends_with("old.log")));
        assert!(paths_in_plan.iter().any(|p| p.ends_with("scratch.tmp")));
        assert!(paths_in_plan.iter().any(|p| p.ends_with("old")));
        assert!(paths_in_plan.iter().any(|p| p.ends_with(AUDIT_FILE)));
        assert!(
            !paths_in_plan.iter().any(|p| p.ends_with("current.log")),
            "the newest log is the live log and must be skipped"
        );
        assert!(
            !paths_in_plan.iter().any(|p| p.ends_with("ledger.log")),
            "a directory with a .log name must never be planned"
        );
        for protected in [
            "ledger",
            "cas",
            "hot",
            "search",
            "vector",
            "read-model.json",
            "multi-dim-index.json",
            "active.json",
            "maintenance.lock.json",
        ] {
            assert!(
                !paths_in_plan
                    .iter()
                    .any(|p| p.to_lowercase().contains(&protected.to_lowercase())),
                "safe plan must never include {protected}: {paths_in_plan:?}"
            );
        }
        assert!(!paths_in_plan.iter().any(|p| p.contains("runtime")));
    }

    #[test]
    fn dry_run_mutates_nothing_and_appends_no_audit() {
        let (_dir, paths) = temp_paths();
        write(&paths.root.join("current.log"), b"current");
        write(&paths.root.join("old.log"), b"stale");
        age(&paths.root.join("old.log"), 60);
        std::fs::create_dir_all(paths.base.join(BACKUP_DIR).join("old")).unwrap();
        age(&paths.base.join(BACKUP_DIR).join("old"), 30);
        let audit = b"{\"ts_ms\":0,\"action\":\"prune\",\"result\":\"ok\"}\n";
        write(&paths.base.join(AUDIT_FILE), audit);

        let report = apply_safe_prune(&paths, &StorageConfig::default(), true).unwrap();
        assert!(report.dry_run);
        assert!(!report.audit_appended);
        assert_eq!(report.items, 2, "one log plus one backup");
        assert!(report.bytes_freed > 0);
        assert!(paths.root.join("old.log").exists());
        assert!(paths.base.join(BACKUP_DIR).join("old").exists());
        assert_eq!(std::fs::read(paths.base.join(AUDIT_FILE)).unwrap(), audit);
    }

    #[test]
    fn apply_deletes_safe_items_and_appends_the_shared_audit_record() {
        let (_dir, paths) = temp_paths();
        write(&paths.root.join("ledger").join("keep.bin"), b"ledger");
        write(&paths.root.join("cas").join("keep.bin"), b"cas");
        write(&paths.root.join("current.log"), b"current");
        write(&paths.root.join("old.log"), b"stale");
        age(&paths.root.join("old.log"), 60);
        std::fs::create_dir_all(paths.base.join(BACKUP_DIR).join("old")).unwrap();
        write(&paths.base.join(BACKUP_DIR).join("old").join("f"), b"old");
        age(&paths.base.join(BACKUP_DIR).join("old"), 30);
        write(
            &paths.base.join(AUDIT_FILE),
            b"{\"ts_ms\":0,\"action\":\"prune\",\"result\":\"ok\"}\n{not json}\n",
        );

        let report = apply_safe_prune(&paths, &StorageConfig::default(), false).unwrap();
        assert!(!report.dry_run);
        assert!(report.audit_appended);
        assert_eq!(report.items, 2);
        assert!(!paths.root.join("old.log").exists());
        assert!(paths.root.join("current.log").exists());
        assert!(!paths.base.join(BACKUP_DIR).join("old").exists());
        assert!(paths.root.join("ledger").join("keep.bin").exists());
        assert!(paths.root.join("cas").join("keep.bin").exists());

        let audit = std::fs::read_to_string(paths.base.join(AUDIT_FILE)).unwrap();
        assert!(audit.contains("{not json}"), "unparseable evidence is kept");
        let last: serde_json::Value =
            serde_json::from_str(audit.lines().last().unwrap()).unwrap();
        assert_eq!(last["action"], "prune");
        assert_eq!(last["result"], "ok");
        assert_eq!(last["detail"]["items"], report.items);
        assert_eq!(last["detail"]["bytes"], report.bytes_freed);
        assert_eq!(last["detail"]["audit_records_removed"], 1);
        assert!(last["cli_version"].is_string());

        let status = storage_status_with_free(&paths, &StorageConfig::default(), Some(100 * GIB));
        assert_eq!(status["lastPrune"]["bytesFreed"], report.bytes_freed);
        assert!(status["lastPrune"]["runUnixMs"].as_u64().unwrap() > 0);

        // Nothing safe remains: the live log is skipped, backups are gone and the
        // audit only carries current records.
        assert!(safe_prune_plan(&paths, &StorageConfig::default()).is_empty());
    }

    #[test]
    fn last_prune_uses_the_latest_successful_record() {
        let (_dir, paths) = temp_paths();
        let lines = [
            r#"{"ts_ms":111,"action":"prune","result":"ok","detail":{"bytes":10}}"#,
            r#"{"ts_ms":222,"action":"prune","result":"error","detail":{"bytes":99}}"#,
            r#"{"ts_ms":333,"action":"backup","result":"ok","detail":{"bytes":88}}"#,
            r#"{"ts_ms":444,"action":"prune","result":"ok","detail":{"bytes":77}}"#,
        ];
        write(&paths.base.join(AUDIT_FILE), format!("{}\n", lines.join("\n")).as_bytes());
        let status = storage_status_with_free(&paths, &StorageConfig::default(), None);
        assert_eq!(status["lastPrune"]["runUnixMs"], 444);
        assert_eq!(status["lastPrune"]["bytesFreed"], 77);
    }

    #[test]
    fn apply_refuses_protected_paths_supplied_by_a_caller() {
        let (_dir, paths) = temp_paths();
        write(&paths.root.join("ledger").join("seg.bin"), b"ledger");
        let plan = vec![PruneItem {
            path: paths.root.join("ledger").join("seg.bin"),
            bytes: 6,
            reason: "caller mistake",
            class: "logs",
        }];
        let error = apply_prune_items(&paths, &StorageConfig::default(), &plan, false, None)
            .expect_err("protected path must be refused");
        assert!(error.contains("protected"), "{error}");
        assert!(paths.root.join("ledger").join("seg.bin").exists());
    }

    #[test]
    fn runtime_class_is_confined_and_never_the_active_version() {
        let (_dir, paths) = temp_paths();
        let old = paths.base.join("runtime").join("0.1.1");
        let active = paths.base.join("runtime").join("0.1.2");
        write(&old.join("host.exe"), b"old");
        write(&active.join("host.exe"), b"active");
        write(&paths.base.join("active.json"), br#"{"version":"0.1.2"}"#);
        let cfg = StorageConfig::default();
        let item = |path: PathBuf| PruneItem {
            path,
            bytes: 3,
            reason: "superseded runtime",
            class: "runtimes",
        };

        // The active version is never deletable, even when a caller asks.
        let error = apply_prune_items(&paths, &cfg, &[item(active.clone())], false, None)
            .expect_err("active runtime must be refused");
        assert!(error.contains("active runtime"), "{error}");
        assert!(active.join("host.exe").exists());

        // The class cannot be used to smuggle another protected path.
        let error = apply_prune_items(
            &paths,
            &cfg,
            &[item(paths.root.join("ledger").join("x.bin"))],
            false,
            None,
        )
        .expect_err("runtimes class is confined to <base>/runtime");
        assert!(error.contains("not under"), "{error}");

        // A caller-supplied stale runtime is applied through the shared path.
        let report = apply_prune_items(&paths, &cfg, &[item(old.clone())], false, None).unwrap();
        assert_eq!(report.items, 1);
        assert!(!old.exists());
        assert!(active.join("host.exe").exists());

        // Without a readable active pointer, runtime deletion fails closed.
        std::fs::remove_file(paths.base.join("active.json")).unwrap();
        write(&paths.base.join("runtime").join("0.1.3").join("host.exe"), b"next");
        let error = apply_prune_items(
            &paths,
            &cfg,
            &[item(paths.base.join("runtime").join("0.1.3"))],
            false,
            None,
        )
        .expect_err("missing active.json must fail closed");
        assert!(error.contains("active.json"), "{error}");
    }

    #[test]
    fn pinned_runtime_is_refused_by_the_shared_apply_path() {
        let (_dir, paths) = temp_paths();
        let old = paths.base.join("runtime").join("0.1.1");
        let active = paths.base.join("runtime").join("0.1.2");
        write(&old.join("host.exe"), b"old");
        write(&active.join("host.exe"), b"active");
        write(&paths.base.join("active.json"), br#"{"version":"0.1.2"}"#);
        PinStore::new(&paths.base)
            .pin("0.1.1", "keep rollback", "test")
            .unwrap();
        let item = PruneItem {
            path: old.clone(),
            bytes: 3,
            reason: "superseded runtime",
            class: "runtimes",
        };

        // A dry run is still refused: planning a pinned delete is a bug.
        for dry_run in [true, false] {
            let error = apply_prune_items(
                &paths,
                &StorageConfig::default(),
                std::slice::from_ref(&item),
                dry_run,
                None,
            )
            .expect_err("a pinned runtime must never be planned or applied");
            assert!(error.contains("pinned"), "{error}");
            assert!(error.contains("keep rollback"), "{error}");
            assert!(old.join("host.exe").exists());
        }

        // Unpin -> the same item applies.
        PinStore::new(&paths.base)
            .unpin_version("0.1.1")
            .unwrap();
        let report = apply_prune_items(
            &paths,
            &StorageConfig::default(),
            &[item],
            false,
            None,
        )
        .unwrap();
        assert_eq!(report.items, 1);
        assert!(!old.exists());
        assert!(active.join("host.exe").exists());
    }

    #[test]
    fn corrupt_pins_file_fails_closed_for_runtime_pruning_only() {
        let (_dir, paths) = temp_paths();
        let old = paths.base.join("runtime").join("0.1.1");
        write(&old.join("host.exe"), b"old");
        write(&paths.base.join("active.json"), br#"{"version":"0.1.2"}"#);
        write(&paths.base.join(PINS_FILE), b"{broken");
        let runtime_item = PruneItem {
            path: old.clone(),
            bytes: 3,
            reason: "superseded runtime",
            class: "runtimes",
        };
        let error = apply_prune_items(
            &paths,
            &StorageConfig::default(),
            &[runtime_item],
            true,
            None,
        )
        .expect_err("a corrupt pins file must fail closed for runtime pruning");
        assert!(error.contains("pins file"), "{error}");
        assert!(old.join("host.exe").exists());

        // Non-runtime classes are unaffected: the shared safe plan still applies.
        // Keep a live (newest) log so the rotated one is not mistaken for it.
        write(&paths.root.join("current.log"), b"live");
        write(&paths.root.join("old.log"), b"stale");
        age(&paths.root.join("old.log"), 60);
        let plan = safe_prune_plan(&paths, &StorageConfig::default());
        assert!(plan.iter().any(|item| item.reason == "stale log/temp"));
        let report = apply_safe_prune(&paths, &StorageConfig::default(), false).unwrap();
        assert!(!paths.root.join("old.log").exists());
        assert!(report.audit_appended);
    }

    #[test]
    fn storage_status_exposes_pins_and_reports_a_corrupt_store() {
        let (_dir, paths) = temp_paths();
        let status = storage_status_with_free(&paths, &StorageConfig::default(), None);
        assert_eq!(status["pins"]["count"], 0);
        assert_eq!(status["pins"]["versions"].as_array().unwrap().len(), 0);

        PinStore::new(&paths.base)
            .pin("0.1.1", "rollback", "test")
            .unwrap();
        let status = storage_status_with_free(&paths, &StorageConfig::default(), None);
        assert_eq!(status["pins"]["count"], 1);
        assert_eq!(status["pins"]["versions"][0], "0.1.1");
        assert!(status["pins"].get("error").is_none());

        write(&paths.base.join(PINS_FILE), b"{broken");
        let status = storage_status_with_free(&paths, &StorageConfig::default(), None);
        assert!(status["pins"]["count"].is_null());
        assert!(status["pins"]["error"]
            .as_str()
            .unwrap()
            .contains("parse"));
        // The frozen schema/classes shape is unchanged.
        assert_eq!(status["schema"], 1);
        for class in [
            "ledger", "cas", "hot", "search", "vector", "readModels", "logs", "backups",
            "audit",
        ] {
            assert!(status["classes"][class].is_u64(), "classes.{class}");
        }
    }
}
