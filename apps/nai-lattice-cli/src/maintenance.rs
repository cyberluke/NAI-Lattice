//! Enterprise lifecycle maintenance for the per-user Lattice fabric.
//!
//! The lifecycle is deliberately split by blast radius:
//!
//! * `prune`  — bounded, scheduled housekeeping: rotated host logs, the
//!              maintenance audit, old backups, superseded runtime versions and
//!              stale temp files. Never touches the canonical ledger or CAS.
//!              `--auto` gates the whole run on the shared storage pressure:
//!              normal/unknown is a successful no-op (nothing touched, no audit
//!              record); elevated/critical runs the same safe path.
//! * `purge`  — explicit operator reset by scope:
//!              `derived` (rebuildable projections: hot/search/vector/read models),
//!              `all` (every file under the runtime root, canonical ledger included),
//!              `legacy` (old Helldesk release artifacts).
//! * `backup` / `restore` — an audit-grade safety net for `purge --scope all`.
//!
//! Every mutating action is blocked while the fabric host is running unless the
//! command explicitly stops it (`--stop`), in which case the host is resurrected
//! afterwards unless `--no-restart`. Destructive actions require `--yes`;
//! `--dry-run` prints the exact plan without touching the disk. Each accepted
//! action appends a JSONL record to `<base>/maintenance.audit.jsonl`, which
//! survives a full data purge.
//!
//! Paths come from the canonical `RuntimeConfig` (honors `NAI_LATTICE_ROOT`),
//! so maintenance cannot be pointed at an arbitrary tree by accident.
//!
//! Usage:
//!   nai-lattice-cli maintenance status [--json]
//!   nai-lattice-cli maintenance stop [--timeout <secs>]
//!   nai-lattice-cli maintenance start [--host-exe <path>] [--timeout <secs>]
//!   nai-lattice-cli maintenance prune [--scheduled] [--auto] [--dry-run] [--yes] [--stop]
//!                                    [--no-restart] [--logs-days N] [--audit-days N]
//!                                    [--backup-days N] [--runtime-keep N] [--prune-runtimes] [--json]
//!   nai-lattice-cli maintenance purge --scope derived|all|legacy
//!                                    [--backup|--no-backup] [--dry-run] [--yes]
//!                                    [--stop] [--no-restart]
//!   nai-lattice-cli maintenance backup [--to <dir>]
//!   nai-lattice-cli maintenance restore --from <dir> [--yes] [--stop]
//!   nai-lattice-cli maintenance audit [--limit N]
//!   nai-lattice-cli maintenance config [--init] [--json]

use std::fmt;
use std::io::{Read as _, Write as _};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nai_lattice_ledger::compression::{
    self, CompressionOptions, SealedCodec, COMPRESSION_DICT_FILE, DEFAULT_BLOCK_BYTES,
};
use nai_lattice_runtime::lifecycle::{
    AcquireOutcome, ExtendOutcome, MaintenanceLock, ReleaseOutcome, MAINTENANCE_LOCK_FILE,
};
use nai_lattice_runtime::RuntimeConfig;
use nai_lattice_storage::{
    apply_prune_items, directory_size, reclaim_decision, safe_prune_plan, storage_status,
    Paths as StoragePaths, PinError, PinRecord, PinStore, PruneItem, ReclaimDecision,
    StorageConfig, PINS_FILE,
};

pub const EXIT_OK: u8 = 0;
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_HOST_BUSY: u8 = 3;
pub const EXIT_UNSAFE: u8 = 4;
pub const EXIT_IO: u8 = 5;

const AUDIT_FILE: &str = "maintenance.audit.jsonl";
const CONFIG_FILE: &str = "maintenance.json";
const BACKUP_DIR: &str = "backups";
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// Owner recorded in the cross-process maintenance lease.
const LEASE_OWNER: &str = "nai-lattice-cli";
/// Lease lifetime covering one command. The guard renews it immediately before
/// the long mutation; an operator can extend it further if needed.
const LEASE_TTL_MS: u64 = 15 * 60 * 1000;

#[cfg(windows)]
const ERROR_PIPE_BUSY: i32 = 231;
#[cfg(windows)]
const ERROR_FILE_NOT_FOUND: i32 = 2;
#[cfg(windows)]
const ERROR_PATH_NOT_FOUND: i32 = 3;

/// Typed command failure; the variant decides the process exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CmdError {
    /// Missing or invalid option / subcommand.
    Usage(String),
    /// Refusal that protects data, shared state or the shared runtime.
    Unsafe(String),
    /// The fabric host is busy, unreachable, or serves a different root.
    HostBusy(String),
    /// Filesystem or other I/O failure.
    Io(String),
}

impl CmdError {
    /// Documented process exit code for this failure class.
    pub fn exit_code(&self) -> u8 {
        match self {
            CmdError::Usage(_) => EXIT_USAGE,
            CmdError::Unsafe(_) => EXIT_UNSAFE,
            CmdError::HostBusy(_) => EXIT_HOST_BUSY,
            CmdError::Io(_) => EXIT_IO,
        }
    }
}

impl fmt::Display for CmdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CmdError::Usage(m)
            | CmdError::Unsafe(m)
            | CmdError::HostBusy(m)
            | CmdError::Io(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for CmdError {}

/// Plain helper errors default to I/O; call sites use the explicit variants for
/// usage and safety refusals.
impl From<String> for CmdError {
    fn from(value: String) -> Self {
        CmdError::Io(value)
    }
}

impl From<&str> for CmdError {
    fn from(value: &str) -> Self {
        CmdError::Io(value.to_string())
    }
}

/// Retention policy. Stored next to the runtime root (not inside it) so a
/// `purge --scope all` cannot erase the operator's policy.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MaintenanceConfig {
    /// Host log files (`*.log` directly under the runtime root) older than this.
    pub logs_days: u64,
    /// Maintenance audit records older than this.
    pub audit_days: u64,
    /// Backup directories older than this.
    pub backup_days: u64,
    /// Superseded runtime versions to keep in `<base>/runtime` (active excluded).
    pub runtime_keep: usize,
    /// Opt-in: prune non-active runtime versions. Off by default because
    /// `PRODUCT_BUNDLING.md` requires shared-runtime collection to be
    /// reference-counted across installed products, and no such registry exists
    /// yet — an operator must confirm before this is safe.
    #[serde(default)]
    pub prune_runtimes: bool,
    /// Reserved for the scheduled task: whether daily pruning is expected.
    pub auto_prune: bool,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            logs_days: 30,
            audit_days: 90,
            backup_days: 14,
            runtime_keep: 2,
            prune_runtimes: false,
            auto_prune: true,
        }
    }
}

struct Paths {
    /// `<base>` = `%LOCALAPPDATA%\NAI\Lattice` (lifecycle state, audit, backups, runtimes).
    base: PathBuf,
    /// `<root>` = `<base>\v1` (canonical runtime data).
    root: PathBuf,
}

fn paths() -> Result<Paths, String> {
    let root = RuntimeConfig::default().root;
    let base = root
        .parent()
        .ok_or_else(|| format!("runtime root has no parent: {}", root.display()))?
        .to_path_buf();
    Ok(Paths { base, root })
}

/// The CLI's root/base pair in the shared storage crate's representation.
fn storage_paths(paths: &Paths) -> StoragePaths {
    StoragePaths {
        base: paths.base.clone(),
        root: paths.root.clone(),
    }
}

/// CLI retention policy → the shared crate's policy. CLI days are parsed as
/// `u64`; the shared contract is `u32`, so an absurd value saturates instead of
/// wrapping.
fn storage_config(cfg: &MaintenanceConfig) -> StorageConfig {
    StorageConfig {
        logs_days: cfg.logs_days.min(u32::MAX as u64) as u32,
        audit_days: cfg.audit_days.min(u32::MAX as u64) as u32,
        backup_days: cfg.backup_days.min(u32::MAX as u64) as u32,
        runtime_keep: cfg.runtime_keep,
    }
}

fn config_path(paths: &Paths) -> PathBuf {
    paths.base.join(CONFIG_FILE)
}

fn audit_path(paths: &Paths) -> PathBuf {
    paths.base.join(AUDIT_FILE)
}

fn load_config(paths: &Paths) -> MaintenanceConfig {
    std::fs::read_to_string(config_path(paths))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_config(paths: &Paths, cfg: &MaintenanceConfig) -> Result<(), String> {
    let text = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(config_path(paths), text).map_err(|e| format!("write config: {e}"))
}

// ---------------------------------------------------------------------------
// Time helpers (UTC, no external date crate)
// ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
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

/// `YYYY-MM-DDTHH:MM:SSZ` for a Unix millisecond timestamp.
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

/// Filesystem-safe sortable stamp, e.g. `20261124-184205`.
fn stamp() -> String {
    let iso = now_iso();
    iso.replace(['-', ':'], "").replace('T', "-").trim_end_matches('Z').to_string()
}

// ---------------------------------------------------------------------------
// Audit ledger
// ---------------------------------------------------------------------------

fn append_audit(paths: &Paths, record: serde_json::Value) {
    let line = match serde_json::to_string(&record) {
        Ok(l) => l,
        Err(_) => return,
    };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(audit_path(paths)) {
        let _ = writeln!(f, "{line}");
    }
}

fn audit_record(action: &str, detail: serde_json::Value, result: &str) -> serde_json::Value {
    serde_json::json!({
        "ts_ms": now_ms(),
        "ts_utc": now_iso(),
        "action": action,
        "result": result,
        "cli_version": env!("CARGO_PKG_VERSION"),
        "detail": detail,
    })
}

fn read_audit_tail(paths: &Paths, limit: usize) -> Vec<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(audit_path(paths)) else { return Vec::new() };
    let mut out: Vec<serde_json::Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    if out.len() > limit {
        out = out.split_off(out.len() - limit);
    }
    out
}

/// Drops audit records older than `days`, keeping unparseable lines (never
/// silently discard evidence). Atomic replace.
fn rotate_audit(paths: &Paths, days: u64, dry_run: bool) -> Result<(usize, usize), String> {
    let path = audit_path(paths);
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok((0, 0)) };
    let cutoff = now_ms().saturating_sub(days.saturating_mul(86_400_000));
    let mut kept = String::new();
    let mut kept_n = 0usize;
    let mut dropped_n = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let drop = match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => v.get("ts_ms").and_then(|t| t.as_u64()).map(|t| t < cutoff).unwrap_or(false),
            Err(_) => false,
        };
        if drop {
            dropped_n += 1;
        } else {
            kept.push_str(line);
            kept.push('\n');
            kept_n += 1;
        }
    }
    if !dry_run && dropped_n > 0 {
        let tmp = path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, kept).map_err(|e| format!("write audit tmp: {e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("replace audit: {e}"))?;
    }
    Ok((kept_n, dropped_n))
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

fn dir_size(path: &Path) -> u64 {
    let Ok(md) = std::fs::symlink_metadata(path) else { return 0 };
    if !md.is_dir() {
        return md.len();
    }
    let Ok(rd) = std::fs::read_dir(path) else { return 0 };
    rd.flatten().map(|e| dir_size(&e.path())).sum()
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
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

/// Remove a file or directory without following junctions/symlinks: a reparse
/// point is deleted as a link, never by recursing into its target.
fn remove_tree_safe(path: &Path) -> Result<u64, String> {
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("stat {}: {e}", path.display())),
    };
    let bytes = if md.is_dir() { dir_size(path) } else { md.len() };
    if is_reparse_point(&md) {
        if md.is_dir() {
            std::fs::remove_dir(path).map_err(|e| format!("remove junction {}: {e}", path.display()))?;
        } else {
            std::fs::remove_file(path).map_err(|e| format!("remove link {}: {e}", path.display()))?;
        }
    } else if md.is_dir() {
        std::fs::remove_dir_all(path).map_err(|e| format!("remove {}: {e}", path.display()))?;
    } else {
        std::fs::remove_file(path).map_err(|e| format!("remove {}: {e}", path.display()))?;
    }
    Ok(bytes)
}

fn copy_tree(src: &Path, dst: &Path) -> Result<(u64, u64), String> {
    let md = std::fs::symlink_metadata(src).map_err(|e| format!("stat {}: {e}", src.display()))?;
    if is_reparse_point(&md) {
        return Ok((0, 0)); // never traverse links into a backup
    }
    if !md.is_dir() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        std::fs::copy(src, dst).map_err(|e| format!("copy {}: {e}", src.display()))?;
        return Ok((1, md.len()));
    }
    std::fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    let mut files = 0u64;
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))?.flatten() {
        let (f, b) = copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        files += f;
        bytes += b;
    }
    Ok((files, bytes))
}

fn file_mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok()?.modified().ok()
}

// ---------------------------------------------------------------------------
// Path containment (lexical, no filesystem existence required)
// ---------------------------------------------------------------------------

/// Resolve `.`/`..` lexically and make the path absolute. Performs no I/O, so a
/// destination that does not exist yet is still checked.
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
                // Never climb above the root/prefix on an absolute path.
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

/// `path` is `base` or below it, compared lexically and case-insensitively on
/// Windows (component-wise, so `C:\data2` is not inside `C:\data`).
fn path_starts_with(path: &Path, base: &Path) -> bool {
    let path = normalize_absolute(path);
    let base = normalize_absolute(base);
    let path_parts: Vec<String> = path.components().map(normalized_component).collect();
    let base_parts: Vec<String> = base.components().map(normalized_component).collect();
    base_parts.len() <= path_parts.len() && path_parts[..base_parts.len()] == base_parts[..]
}

/// Either path contains the other.
fn paths_overlap(a: &Path, b: &Path) -> bool {
    path_starts_with(a, b) || path_starts_with(b, a)
}

/// Same path, preferring the filesystem's canonical form when both exist.
fn same_path(a: &Path, b: &Path) -> bool {
    let a = a.canonicalize().unwrap_or_else(|_| normalize_absolute(a));
    let b = b.canonicalize().unwrap_or_else(|_| normalize_absolute(b));
    path_starts_with(&a, &b) && path_starts_with(&b, &a)
}

/// True for the maintenance lease and its staging artifacts. The lease must
/// survive purge/restore and must never be copied back over the live lease.
fn is_lease_entry(path: &Path) -> bool {
    path.file_name()
        .map(|name| name.to_string_lossy().starts_with(MAINTENANCE_LOCK_FILE))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Host control (NAF)
// ---------------------------------------------------------------------------

struct HostEndpoint {
    pid: Option<u32>,
    pipe: String,
    instance_id: Option<String>,
}

fn endpoint(paths: &Paths) -> Option<HostEndpoint> {
    let text = std::fs::read_to_string(paths.root.join("endpoint.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(HostEndpoint {
        pid: v.get("pid").and_then(|p| p.as_u64()).map(|p| p as u32),
        pipe: v
            .get("canonical_pipe")
            .and_then(|p| p.as_str())
            .or_else(|| v.get("legacy_alias").and_then(|p| p.as_str()))
            .unwrap_or("")
            .to_string(),
        instance_id: v.get("instance_id").and_then(|i| i.as_str()).map(|s| s.to_string()),
    })
}

fn active_runtime(paths: &Paths) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(paths.base.join("active.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// The real per-user default root (`%LOCALAPPDATA%\NAI\Lattice\v1`), independent
/// of `NAI_LATTICE_ROOT`. Using `RuntimeConfig::default()` here would make every
/// overridden root compare equal to itself and fall back to the production pipe.
fn per_user_default_root() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(
        PathBuf::from(local)
            .join("NAI")
            .join("Lattice")
            .join("v1"),
    )
}

/// Canonical pipe for the runtime root this command was pointed at.
///
/// Reads `<paths.root>/endpoint.json` and uses its `canonical_pipe`. The
/// per-user pipe name is only a fallback when the requested root really is the
/// per-user default, or when the operator explicitly set `NAI_FABRIC_PIPE`.
/// Any other root without its own manifest is reported as "no host" (`Ok(None)`),
/// so a scratch `NAI_LATTICE_ROOT` drill neither probes nor stops the production
/// host. The host identity is additionally checked with `runtime.describe`
/// before a destructive stop.
fn host_pipe(paths: &Paths) -> Result<Option<String>, String> {
    if let Ok(text) = std::fs::read_to_string(paths.root.join("endpoint.json")) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(pipe) = value
                .get("canonical_pipe")
                .and_then(|p| p.as_str())
                .filter(|p| !p.is_empty())
            {
                return Ok(Some(pipe.to_string()));
            }
        }
    }
    let is_default = per_user_default_root()
        .map(|root| same_path(&paths.root, &root))
        .unwrap_or(false);
    let explicit_pipe = std::env::var("NAI_FABRIC_PIPE")
        .ok()
        .filter(|value| !value.is_empty())
        .is_some();
    if is_default || explicit_pipe {
        return Ok(Some(
            nai_lattice_client::win_pipe::NamedPipeStream::pipe_name(),
        ));
    }
    Ok(None)
}

/// One NAF round trip against the running host. `caps` must list every method
/// the caller is about to use; a legacy host rejects unknown capabilities and
/// we translate that into an actionable error instead of a silent downgrade.
fn naf_call(
    paths: &Paths,
    method: &str,
    args: &serde_json::Value,
    caps: &[&str],
) -> Result<serde_json::Value, String> {
    let pipe = host_pipe(paths)?.ok_or_else(|| {
        format!(
            "no host endpoint for this runtime root {}: start the host for this root or set \
             NAI_FABRIC_PIPE (endpoint.json is missing or has no canonical_pipe)",
            paths.root.display()
        )
    })?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;
    let args = serde_json::to_vec(args).map_err(|e| e.to_string())?;
    let method = method.to_string();
    let caps: Vec<String> = caps.iter().map(|c| c.to_string()).collect();
    rt.block_on(async move {
        use nai_lattice_client::transport::NafTransport;
        let stream = nai_lattice_client::win_pipe::NamedPipeStream::connect(&pipe)
            .map_err(|e| format!("fabric pipe connect {pipe}: {e}"))?;
        let transport: NafTransport = Box::new(stream);
        let request = nai_lattice_protocol::pb::HandshakeRequest {
            client_app_id: "nai-lattice-cli-maintenance".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            client_pid: std::process::id() as i64,
            naf_min: 1,
            naf_max: 1,
            requested_capabilities: caps,
            package_identity: "nai-lattice-maintenance".into(),
            build_sha: String::new(),
            nonce: vec![],
        };
        let mut client = nai_lattice_client::NafClient::connect(transport, request)
            .await
            .map_err(|e| format!("fabric handshake: {e}"))?;
        let body = client
            .dispatch(&method, &args, None)
            .await
            .map_err(|e| format!("{method}: {e}"))?;
        serde_json::from_slice::<serde_json::Value>(&body)
            .map_err(|e| format!("{method} response: {e}"))
    })
}

/// Fail-closed host probe.
///
/// `Ok(true)` = a live host endpoint; `Ok(false)` = definitively no host;
/// `Err` = unknown state, which destructive commands must treat as a refusal.
fn host_running(paths: &Paths) -> Result<bool, String> {
    use nai_lattice_client::win_pipe::NamedPipeStream;
    let Some(pipe) = host_pipe(paths)? else {
        // This root has no endpoint and is not the per-user default: no host can
        // exist for it, so a destructive command must not probe another root.
        return Ok(false);
    };
    // The client transport is tokio-backed: even a synchronous connect needs a
    // reactor context (tokio named pipes register with the driver on creation).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;
    rt.block_on(async move {
        match NamedPipeStream::connect(&pipe) {
            Ok(_) => Ok(true),
            Err(e) => {
                #[cfg(windows)]
                {
                    match e.raw_os_error() {
                        // A busy server (all pipe instances in use) is alive.
                        Some(ERROR_PIPE_BUSY) => return Ok(true),
                        Some(ERROR_FILE_NOT_FOUND) | Some(ERROR_PATH_NOT_FOUND) => {
                            return Ok(false)
                        }
                        _ => {}
                    }
                }
                #[cfg(not(windows))]
                {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        return Ok(false);
                    }
                }
                Err(format!("fabric pipe probe {pipe} failed: {e}"))
            }
        }
    })
}

fn host_health(paths: &Paths) -> Result<serde_json::Value, CmdError> {
    naf_call(
        paths,
        "runtime.health",
        &serde_json::json!({}),
        &["runtime.health"],
    )
    .map_err(CmdError::HostBusy)
}

/// Stops the host cooperatively. Returns whether it was running.
///
/// The live host's `runtime.describe.root` must match this command's root
/// before anything is stopped; a scratch-root drill therefore cannot stop the
/// production host even if the pipe name resolves to the per-user default.
fn stop_host(paths: &Paths, timeout: Duration) -> Result<bool, CmdError> {
    if !host_running(paths).map_err(CmdError::HostBusy)? {
        return Ok(false);
    }
    let pipe = host_pipe(paths)
        .map_err(CmdError::HostBusy)?
        .unwrap_or_default();
    let describe = naf_call(
        paths,
        "runtime.describe",
        &serde_json::json!({}),
        &["runtime.describe"],
    )
    .map_err(CmdError::HostBusy)?;
    let host_root = describe
        .get("root")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if host_root.is_empty() || !same_path(Path::new(&host_root), &paths.root) {
        return Err(CmdError::HostBusy(format!(
            "refusing to stop the fabric host: endpoint {pipe} serves root `{}`, not this \
             runtime root {} — use the matching NAI_LATTICE_ROOT/NAI_FABRIC_PIPE for the host \
             you intend to stop",
            if host_root.is_empty() { "unknown" } else { &host_root },
            paths.root.display()
        )));
    }
    let resp = naf_call(
        paths,
        "runtime.shutdown_if_idle",
        &serde_json::json!({}),
        &["runtime.shutdown_if_idle"],
    )
    .map_err(|e| {
        CmdError::HostBusy(format!(
            "cooperative stop unavailable ({e}); this host predates \
             runtime.shutdown_if_idle — restart NAI OS/the fabric host to activate \
             the current Lattice runtime, or stop it from its product supervisor"
        ))
    })?;
    if !resp.get("accepted").and_then(|a| a.as_bool()).unwrap_or(false) {
        return Err(CmdError::HostBusy(format!(
            "host refused the stop: {}",
            resp.get("reason").and_then(|r| r.as_str()).unwrap_or("busy")
        )));
    }
    let deadline = SystemTime::now() + timeout;
    while SystemTime::now() < deadline {
        match host_running(paths) {
            Ok(false) => return Ok(true),
            // Still alive, or a transient probe failure: re-probe until the
            // deadline and decide from the final state.
            Ok(true) | Err(_) => {}
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if matches!(host_running(paths), Ok(false)) {
        return Ok(true);
    }
    Err(CmdError::HostBusy(format!(
        "host did not stop within {}s",
        timeout.as_secs()
    )))
}

fn host_exe_candidates(paths: &Paths) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(active) = active_runtime(paths) {
        if let Some(version) = active.get("version").and_then(|v| v.as_str()) {
            out.push(paths.base.join("runtime").join(version).join("nai-fabric-host.exe"));
        }
    }
    out.push(paths.root.join("nai-fabric-host.exe"));
    let runtime_dir = paths.base.join("runtime");
    if let Ok(rd) = std::fs::read_dir(&runtime_dir) {
        let mut dirs: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.path().join("nai-fabric-host.exe"))
            .filter(|p| p.is_file())
            .collect();
        dirs.sort_by_key(|p| file_mtime(p));
        dirs.reverse();
        out.extend(dirs);
    }
    out.retain(|p| p.is_file());
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

fn start_host(paths: &Paths, exe: &Path, timeout: Duration) -> Result<(), CmdError> {
    if host_running(paths).map_err(CmdError::HostBusy)? {
        return Ok(());
    }
    let mut cmd = std::process::Command::new(exe);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: the monitor outlives us.
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    cmd.spawn()
        .map_err(|e| format!("spawn {}: {e}", exe.display()))?;
    let deadline = SystemTime::now() + timeout;
    while SystemTime::now() < deadline {
        // A not-yet-published endpoint is normal during startup; any other probe
        // failure simply keeps waiting until the bounded deadline.
        if matches!(host_running(paths), Ok(true)) && host_health(paths).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(CmdError::HostBusy(format!(
        "host did not become healthy within {}s",
        timeout.as_secs()
    )))
}

fn restart_host(paths: &Paths) -> Result<(), CmdError> {
    let exe = host_exe_candidates(paths)
        .into_iter()
        .next()
        .ok_or_else(|| {
            CmdError::HostBusy(
                "no nai-fabric-host.exe found (pass --host-exe or restart NAI OS)".to_string(),
            )
        })?;
    start_host(paths, &exe, Duration::from_secs(30))
}

/// Refuses to mutate while a host is live unless the command may stop it.
/// A probe failure is propagated (never treated as "stopped").
fn require_stopped(
    paths: &Paths,
    allow_stop: bool,
    timeout: Duration,
) -> Result<bool, CmdError> {
    let running = host_running(paths)
        .map_err(|e| CmdError::HostBusy(format!("host probe failed: {e}")))?;
    if !running {
        return Ok(false);
    }
    if !allow_stop {
        return Err(CmdError::HostBusy(format!(
            "the fabric host is running; stop it first (`maintenance stop`) or pass --stop \
             (current endpoint: {})",
            endpoint(paths)
                .map(|e| format!("pid={:?} pipe={}", e.pid, e.pipe))
                .unwrap_or_else(|| "unknown".into())
        )));
    }
    stop_host(paths, timeout)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Maintenance session guard (lease + host stop/restart)
// ---------------------------------------------------------------------------

/// One RAII guard for every mutating command.
///
/// Acquires the cross-process lease, stops the host when allowed, and on drop
/// releases the lease and restarts the host if it was running and `--no-restart`
/// was not set. `finish()` performs the same cleanup eagerly so a successful
/// command can surface a restart failure as an exit code; `Drop` covers every
/// early error path.
struct MaintenanceSession<'a> {
    paths: &'a Paths,
    lock: MaintenanceLock,
    token: String,
    was_running: bool,
    restart: bool,
    finished: bool,
}

impl<'a> MaintenanceSession<'a> {
    fn begin(
        paths: &'a Paths,
        args: &Args,
        reason: &str,
        timeout: Duration,
    ) -> Result<Self, CmdError> {
        let lock = MaintenanceLock::new(&paths.root);
        let token = match lock.acquire(LEASE_OWNER, reason, LEASE_TTL_MS) {
            Ok(AcquireOutcome::Acquired { token, .. }) => token,
            Ok(AcquireOutcome::Refused(refusal)) => {
                return Err(CmdError::Unsafe(format!(
                    "refusing to start {reason}: the maintenance lease is held by another actor \
                     ({})",
                    refusal.to_json()
                )));
            }
            Err(e) => {
                return Err(CmdError::Io(format!(
                    "acquire maintenance lease {}: {e}",
                    lock.path().display()
                )));
            }
        };
        let was_running = match require_stopped(paths, args.stop, timeout) {
            Ok(was_running) => was_running,
            Err(e) => {
                release_lease(&lock, &token);
                return Err(e);
            }
        };
        Ok(Self {
            paths,
            lock,
            token,
            was_running,
            restart: was_running && !args.no_restart,
            finished: false,
        })
    }

    fn host_stopped(&self) -> bool {
        self.was_running
    }

    /// Renew before the long mutation so the lease covers it.
    fn extend(&self) -> Result<(), CmdError> {
        match self.lock.extend(&self.token, LEASE_TTL_MS) {
            Ok(ExtendOutcome::Extended(_)) => Ok(()),
            Ok(ExtendOutcome::Refused(refusal)) => Err(CmdError::Unsafe(format!(
                "maintenance lease lost before the operation: {}",
                refusal.to_json()
            ))),
            Err(e) => Err(CmdError::Io(format!("extend maintenance lease: {e}"))),
        }
    }

    fn finish(&mut self) -> Result<(), CmdError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        // ORDER MATTERS: release the lease FIRST. The host refuses to start
        // while a live maintenance lease exists, so restarting before the
        // release would always fail. If the release did not clear a live lease
        // (lost ownership / I/O failure), do not attempt a doomed restart.
        release_lease(&self.lock, &self.token);
        if self.restart {
            if self.lock.inspect().is_live() {
                return Err(CmdError::Unsafe(format!(
                    "host not restarted: the maintenance lease at {} is still live \
                     (another actor may have taken over)",
                    self.lock.path().display()
                )));
            }
            match restart_host(self.paths) {
                Ok(()) => println!("host restarted"),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl Drop for MaintenanceSession<'_> {
    fn drop(&mut self) {
        if let Err(e) = self.finish() {
            eprintln!("maintenance: warning: {e}");
        }
    }
}

/// Release the lease and surface failures as warnings. Callers that restart the
/// host must release first and then verify [`MaintenanceLock::inspect`] is not
/// live before starting it.
fn release_lease(lock: &MaintenanceLock, token: &str) {
    match lock.release(token) {
        Ok(ReleaseOutcome::Released(_)) => {}
        Ok(ReleaseOutcome::Refused(refusal)) => eprintln!(
            "maintenance: warning: maintenance lease release refused: {}",
            refusal.to_json()
        ),
        Err(e) => eprintln!("maintenance: warning: maintenance lease release failed: {e}"),
    }
}

// ---------------------------------------------------------------------------
// prune
// ---------------------------------------------------------------------------

struct PlanItem {
    path: PathBuf,
    bytes: u64,
    reason: &'static str,
    /// Shared policy class (`logs`, `backups`, `audit`) or CLI-only
    /// (`runtimes`). The apply path uses it to route audit rotation.
    class: &'static str,
}

struct PrunePlan {
    items: Vec<PlanItem>,
    /// Pinned runtime versions that were intentionally kept (never planned).
    pinned: Vec<PinRecord>,
}

fn prune_plan(
    paths: &Paths,
    cfg: &MaintenanceConfig,
    include_runtimes: bool,
) -> Result<PrunePlan, CmdError> {
    // Logs, backups and audit rotation are single-sourced in
    // `nai-lattice-storage` so the CLI, the host NAF surface and the dashboard
    // summary cannot drift. The shared plan already skips the live/newest log
    // and never includes the ledger/CAS/derived stores/lease.
    let mut plan: Vec<PlanItem> = safe_prune_plan(&storage_paths(paths), &storage_config(cfg))
        .into_iter()
        .map(|item| PlanItem {
            path: item.path,
            bytes: item.bytes,
            reason: item.reason,
            class: item.class,
        })
        .collect();
    // Superseded runtime versions (never the active one) stay CLI-only and
    // opt-in: shared runtime collection must be reference-counted across
    // installed products (PRODUCT_BUNDLING.md); without a product registry the
    // operator confirms.
    let mut pinned = Vec::new();
    if include_runtimes {
        // Never guess which runtime is active. An absent/unreadable active.json
        // means the protected version is unknown, so runtime pruning is refused.
        let active_version = active_runtime(paths)
            .and_then(|a| a.get("version").and_then(|v| v.as_str()).map(str::to_string))
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                CmdError::Unsafe(format!(
                    "refusing to prune runtime versions: {} is missing or has no `version`; \
                     the active runtime cannot be protected",
                    paths.base.join("active.json").display()
                ))
            })?;
        // Pins fail closed: a corrupt/unreadable pins file means no version can
        // be proven unpinned, so runtime pruning is refused entirely.
        let pins = PinStore::new(&paths.base).list().map_err(|error| {
            CmdError::Unsafe(format!(
                "refusing to prune runtime versions: cannot read {} ({error}); \
                 no runtime version can be proven unpinned",
                paths.base.join(PINS_FILE).display()
            ))
        })?;
        if let Ok(rd) = std::fs::read_dir(paths.base.join("runtime")) {
            let mut dirs: Vec<PathBuf> = rd
                .flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.path())
                .collect();
            // Newest first.
            dirs.sort_by_key(|p| file_mtime(p));
            dirs.reverse();
            let mut spare = 0usize;
            for dir in dirs {
                let name = dir.file_name().unwrap_or_default().to_string_lossy().to_string();
                if name == active_version {
                    continue;
                }
                // A pinned version is never planned, and does not consume a
                // retention slot: the operator explicitly asked to keep it.
                if let Some(pin) = pins.iter().find(|pin| pin.version == name) {
                    if !pinned.iter().any(|existing: &PinRecord| existing.id == pin.id) {
                        pinned.push(pin.clone());
                    }
                    continue;
                }
                if spare < cfg.runtime_keep {
                    spare += 1;
                    continue;
                }
                plan.push(PlanItem {
                    bytes: dir_size(&dir),
                    path: dir,
                    reason: "superseded runtime",
                    class: "runtimes",
                });
            }
        }
    }
    Ok(PrunePlan {
        items: plan,
        pinned,
    })
}

/// Re-check the plan after the host is stopped: drop vanished paths and any
/// runtime that became active while the host was going down. If runtime items
/// remain planned and the active pointer is no longer readable, refuse rather
/// than delete a runtime that might now be active.
fn refresh_plan_after_stop(paths: &Paths, plan: &mut PrunePlan) -> Result<(), CmdError> {
    let has_runtime_items = plan
        .items
        .iter()
        .any(|item| item.reason == "superseded runtime");
    let active_version = active_runtime(paths)
        .and_then(|a| a.get("version").and_then(|v| v.as_str()).map(str::to_string))
        .filter(|v| !v.is_empty());
    if has_runtime_items && active_version.is_none() {
        return Err(CmdError::Unsafe(format!(
            "refusing to prune runtime versions: {} became unreadable after the host stopped",
            paths.base.join("active.json").display()
        )));
    }
    // Re-read pins after the host stopped: a pin created in between must win.
    let pins = if has_runtime_items {
        Some(PinStore::new(&paths.base).list().map_err(|error| {
            CmdError::Unsafe(format!(
                "refusing to prune runtime versions: cannot read {} ({error}); \
                 no runtime version can be proven unpinned",
                paths.base.join(PINS_FILE).display()
            ))
        })?)
    } else {
        None
    };
    let mut kept = Vec::with_capacity(plan.items.len());
    for item in plan.items.drain(..) {
        if !item.path.exists() {
            continue;
        }
        if item.reason == "superseded runtime" {
            let version = item
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string();
            if active_version.as_deref() == Some(version.as_str()) {
                continue;
            }
            if let Some(pin) = pins
                .as_ref()
                .and_then(|pins| pins.iter().find(|pin| pin.version == version))
            {
                if !plan.pinned.iter().any(|existing| existing.id == pin.id) {
                    plan.pinned.push(pin.clone());
                }
                continue;
            }
        }
        kept.push(item);
    }
    plan.items = kept;
    Ok(())
}

fn print_plan(plan: &PrunePlan, json: bool) -> u64 {
    let total: u64 = plan.items.iter().map(|item| item.bytes).sum();
    if json {
        let items: Vec<serde_json::Value> = plan
            .items
            .iter()
            .map(|item| {
                serde_json::json!({
                    "path": item.path.display().to_string(),
                    "bytes": item.bytes,
                    "reason": item.reason,
                })
            })
            .collect();
        let pinned: Vec<serde_json::Value> = plan
            .pinned
            .iter()
            .map(|pin| {
                serde_json::json!({
                    "version": pin.version,
                    "id": pin.id,
                    "reason": pin.reason,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "items": items,
                "bytes": total,
                "pinned": pinned,
            }))
            .unwrap()
        );
    } else {
        for item in &plan.items {
            println!(
                "  {:<24} {:>10}  {}",
                item.reason,
                human_bytes(item.bytes),
                item.path.display()
            );
        }
        for pin in &plan.pinned {
            println!(
                "  {:<24} {:>10}  {} ({})",
                "pinned (kept)",
                "-",
                pin.version,
                pin.reason
            );
        }
        println!("  total {:>10}", human_bytes(total));
    }
    total
}

// ---------------------------------------------------------------------------
// purge
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Derived,
    All,
    Legacy,
}

fn parse_scope(s: &str) -> Result<Scope, String> {
    match s {
        "derived" => Ok(Scope::Derived),
        "all" => Ok(Scope::All),
        "legacy" => Ok(Scope::Legacy),
        other => Err(format!("invalid scope `{other}` (expected derived|all|legacy)")),
    }
}

fn scope_targets(paths: &Paths, scope: Scope) -> Vec<PathBuf> {
    match scope {
        Scope::Derived => vec![
            paths.root.join("hot"),
            paths.root.join("search"),
            paths.root.join("vector"),
            paths.root.join("read-model.json"),
            paths.root.join("multi-dim-index.json"),
        ],
        Scope::All => {
            let mut out = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&paths.root) {
                // The live maintenance lease is coordination state, not data:
                // purging it would let a second actor take over mid-operation.
                out.extend(rd.flatten().map(|e| e.path()).filter(|p| !is_lease_entry(p)));
            }
            out
        }
        Scope::Legacy => vec![paths.base.join("releases"), paths.base.join("legacy")],
    }
}

fn backup_dir(paths: &Paths, label: &str) -> PathBuf {
    paths.base.join(BACKUP_DIR).join(format!("{}-{label}", stamp()))
}

/// Refuse destinations that overlap the runtime root in either direction and
/// destinations that already contain data (never merge into an old backup).
fn ensure_backup_destination(paths: &Paths, dest: &Path) -> Result<(), CmdError> {
    if paths_overlap(&paths.root, dest) {
        return Err(CmdError::Unsafe(format!(
            "refusing backup destination {}: it overlaps the runtime root {}; choose a \
             directory outside the data root",
            dest.display(),
            paths.root.display()
        )));
    }
    match std::fs::metadata(dest) {
        Ok(md) if !md.is_dir() => Err(CmdError::Unsafe(format!(
            "refusing backup destination {}: it exists and is not a directory",
            dest.display()
        ))),
        Ok(_) => {
            let mut entries = Vec::new();
            if let Ok(rd) = std::fs::read_dir(dest) {
                for entry in rd.flatten().take(5) {
                    entries.push(entry.file_name().to_string_lossy().to_string());
                }
            }
            if entries.is_empty() {
                Ok(())
            } else {
                Err(CmdError::Unsafe(format!(
                    "refusing backup destination {}: it is not empty (contains: {}); choose a \
                     new empty directory",
                    dest.display(),
                    entries.join(", ")
                )))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CmdError::Io(format!("stat {}: {e}", dest.display()))),
    }
}

/// Copy the whole data root except the live maintenance lease.
fn copy_root_excluding_lease(src_root: &Path, dst: &Path) -> Result<(u64, u64), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    let mut files = 0u64;
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(src_root)
        .map_err(|e| format!("read {}: {e}", src_root.display()))?
        .flatten()
    {
        let path = entry.path();
        if is_lease_entry(&path) {
            continue;
        }
        let (f, b) = copy_tree(&path, &dst.join(entry.file_name()))?;
        files += f;
        bytes += b;
    }
    Ok((files, bytes))
}

fn create_backup(paths: &Paths, scope: Scope, label: &str) -> Result<PathBuf, CmdError> {
    create_backup_at(paths, scope, &backup_dir(paths, label))
}

fn create_backup_at(paths: &Paths, scope: Scope, dir: &Path) -> Result<PathBuf, CmdError> {
    ensure_backup_destination(paths, dir)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let data = dir.join("data");
    let (files, bytes) = match scope {
        Scope::All => copy_root_excluding_lease(&paths.root, &data)?,
        Scope::Derived | Scope::Legacy => {
            std::fs::create_dir_all(&data)
                .map_err(|e| format!("mkdir {}: {e}", data.display()))?;
            let mut files = 0u64;
            let mut bytes = 0u64;
            for target in scope_targets(paths, scope).into_iter().filter(|p| p.exists()) {
                let (f, b) =
                    copy_tree(&target, &data.join(target.file_name().unwrap_or_default()))?;
                files += f;
                bytes += b;
            }
            (files, bytes)
        }
    };
    let scope_name = match scope {
        Scope::Derived => "derived",
        Scope::All => "all",
        Scope::Legacy => "legacy",
    };
    let manifest = serde_json::json!({
        "schema": 1,
        "created_utc": now_iso(),
        "scope": scope_name,
        "source_root": paths.root.display().to_string(),
        "files": files,
        "bytes": bytes,
        "cli_version": env!("CARGO_PKG_VERSION"),
        "active": active_runtime(paths),
        "endpoint": endpoint(paths).map(|e| serde_json::json!({"pid": e.pid, "pipe": e.pipe, "instance_id": e.instance_id})),
    });
    std::fs::write(dir.join("manifest.json"), serde_json::to_string_pretty(&manifest).unwrap())
        .map_err(|e| format!("write manifest: {e}"))?;
    Ok(dir.to_path_buf())
}

fn count_files_bytes(path: &Path) -> (u64, u64) {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return (0, 0);
    };
    if is_reparse_point(&md) {
        return (0, 0);
    }
    if !md.is_dir() {
        return (1, md.len());
    }
    let Ok(rd) = std::fs::read_dir(path) else {
        return (0, 0);
    };
    rd.flatten()
        .map(|entry| count_files_bytes(&entry.path()))
        .fold((0u64, 0u64), |(files, bytes), (f, b)| (files + f, bytes + b))
}

#[derive(Debug)]
struct RestorePlan {
    from: PathBuf,
    data: PathBuf,
    files: u64,
    bytes: u64,
}

/// Validate a restore source completely before the host is stopped or anything
/// on disk is touched. Only full (`scope: all`) backups are restorable.
fn validate_restore(paths: &Paths, from: &Path) -> Result<RestorePlan, CmdError> {
    if paths_overlap(&paths.root, from) {
        return Err(CmdError::Unsafe(format!(
            "refusing to restore from {}: it overlaps the runtime root {}; the source must \
             live outside the data root",
            from.display(),
            paths.root.display()
        )));
    }
    let manifest_path = from.join("manifest.json");
    let manifest_text = std::fs::read_to_string(&manifest_path).map_err(|e| {
        CmdError::Io(format!(
            "read backup manifest {}: {e}",
            manifest_path.display()
        ))
    })?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest_text).map_err(|e| {
        CmdError::Io(format!(
            "parse backup manifest {}: {e}",
            manifest_path.display()
        ))
    })?;
    if manifest.get("schema").and_then(|s| s.as_u64()) != Some(1) {
        return Err(CmdError::Unsafe(
            "unsupported backup schema (expected schema 1)".into(),
        ));
    }
    let scope = manifest.get("scope").and_then(|s| s.as_str()).unwrap_or("");
    if scope != "all" {
        return Err(CmdError::Unsafe(format!(
            "refusing to restore from {}: backup scope is `{}`, not `all`; derived and legacy \
             backups are partial and cannot restore a full runtime root",
            from.display(),
            if scope.is_empty() { "<missing>" } else { scope }
        )));
    }
    let data = from.join("data");
    if !data.is_dir() {
        return Err(CmdError::Unsafe(format!(
            "refusing to restore from {}: the backup is incomplete (no data directory)",
            from.display()
        )));
    }
    let expected_files = manifest
        .get("files")
        .and_then(|f| f.as_u64())
        .ok_or_else(|| {
            CmdError::Unsafe(format!(
                "refusing to restore from {}: the manifest has no file count, so completeness \
                 cannot be verified",
                from.display()
            ))
        })?;
    let expected_bytes = manifest
        .get("bytes")
        .and_then(|b| b.as_u64())
        .ok_or_else(|| {
            CmdError::Unsafe(format!(
                "refusing to restore from {}: the manifest has no byte total, so completeness \
                 cannot be verified",
                from.display()
            ))
        })?;
    let (files, bytes) = count_files_bytes(&data);
    if files != expected_files || bytes != expected_bytes {
        return Err(CmdError::Unsafe(format!(
            "refusing to restore from {}: the backup is incomplete (data has {files} files / \
             {bytes} bytes, manifest records {expected_files} files / {expected_bytes} bytes)",
            from.display()
        )));
    }
    Ok(RestorePlan {
        from: from.to_path_buf(),
        data,
        files,
        bytes,
    })
}

fn copy_backup_data(data: &Path, root: &Path) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|e| format!("mkdir {}: {e}", root.display()))?;
    for entry in std::fs::read_dir(data)
        .map_err(|e| format!("read {}: {e}", data.display()))?
        .flatten()
    {
        // Never overwrite the live lease with the one captured by the backup.
        if is_lease_entry(&entry.path()) {
            continue;
        }
        copy_tree(&entry.path(), &root.join(entry.file_name()))?;
    }
    Ok(())
}

/// Clear the runtime root (keeping the live lease) and copy the validated
/// backup back. The caller has already taken a pre-restore backup.
fn restore_backup(paths: &Paths, plan: &RestorePlan) -> Result<(), CmdError> {
    if let Ok(rd) = std::fs::read_dir(&paths.root) {
        for entry in rd.flatten() {
            if is_lease_entry(&entry.path()) {
                continue;
            }
            remove_tree_safe(&entry.path())?;
        }
    }
    copy_backup_data(&plan.data, &paths.root)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Args {
    scope: Option<String>,
    host_exe: Option<PathBuf>,
    to: Option<PathBuf>,
    from: Option<PathBuf>,
    yes: bool,
    dry_run: bool,
    stop: bool,
    no_restart: bool,
    json: bool,
    init: bool,
    logs_days: Option<u64>,
    audit_days: Option<u64>,
    backup_days: Option<u64>,
    runtime_keep: Option<usize>,
    limit: Option<usize>,
    timeout: Option<u64>,
    /// `Some(true)` = --backup, `Some(false)` = --no-backup.
    backup: Option<bool>,
    prune_runtimes: bool,
    /// Scheduled-task invocation of `prune`: honors `auto_prune`.
    scheduled: bool,
    /// `prune --auto`: only run when the shared storage pressure warrants it.
    auto: bool,
    /// `pin`/`unpin --version`.
    version: Option<String>,
    /// `unpin --id`.
    id: Option<String>,
    /// `pin --reason`.
    reason: Option<String>,
    /// `pin --by`.
    by: Option<String>,
    /// `compress --sealed` (required acknowledgement of the sealed-only scope).
    sealed: bool,
    /// `compress --older-than <days>`.
    older_than: Option<u64>,
    /// `compress --execute` (without it the command only plans).
    execute: bool,
    /// `compress --allow-incompatible-readers`.
    allow_incompatible_readers: bool,
}

fn parse_args(raw: &[String]) -> Result<Args, String> {
    let mut a = Args::default();
    let mut i = 0;
    while i < raw.len() {
        let arg = raw[i].as_str();
        let mut next = |name: &str| -> Result<String, String> {
            i += 1;
            raw.get(i).cloned().ok_or_else(|| format!("{name} requires a value"))
        };
        match arg {
            "--scope" => a.scope = Some(next("--scope")?),
            "--host-exe" => a.host_exe = Some(PathBuf::from(next("--host-exe")?)),
            "--to" => a.to = Some(PathBuf::from(next("--to")?)),
            "--from" => a.from = Some(PathBuf::from(next("--from")?)),
            "--logs-days" => a.logs_days = Some(next("--logs-days")?.parse().map_err(|_| "invalid --logs-days")?),
            "--audit-days" => a.audit_days = Some(next("--audit-days")?.parse().map_err(|_| "invalid --audit-days")?),
            "--backup-days" => a.backup_days = Some(next("--backup-days")?.parse().map_err(|_| "invalid --backup-days")?),
            "--runtime-keep" => a.runtime_keep = Some(next("--runtime-keep")?.parse().map_err(|_| "invalid --runtime-keep")?),
            "--limit" => a.limit = Some(next("--limit")?.parse().map_err(|_| "invalid --limit")?),
            "--timeout" => a.timeout = Some(next("--timeout")?.parse().map_err(|_| "invalid --timeout")?),
            "--yes" => a.yes = true,
            "--dry-run" => a.dry_run = true,
            "--stop" => a.stop = true,
            "--no-restart" => a.no_restart = true,
            "--backup" => a.backup = Some(true),
            "--no-backup" => a.backup = Some(false),
            "--prune-runtimes" => a.prune_runtimes = true,
            "--scheduled" => a.scheduled = true,
            "--auto" => a.auto = true,
            "--version" => a.version = Some(next("--version")?),
            "--id" => a.id = Some(next("--id")?),
            "--reason" => a.reason = Some(next("--reason")?),
            "--by" => a.by = Some(next("--by")?),
            "--sealed" => a.sealed = true,
            "--older-than" => {
                a.older_than = Some(
                    next("--older-than")?
                        .parse()
                        .map_err(|_| "invalid --older-than".to_string())?,
                )
            }
            "--execute" => a.execute = true,
            "--allow-incompatible-readers" => a.allow_incompatible_readers = true,
            "--json" => a.json = true,
            "--init" => a.init = true,
            other => return Err(format!("unknown option `{other}`")),
        }
        i += 1;
    }
    Ok(a)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

pub fn run(raw: &[String]) -> u8 {
    let sub = raw.first().map(|s| s.as_str()).unwrap_or("status");
    let rest = if raw.is_empty() { raw } else { &raw[1..] };
    let args = match parse_args(rest) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("maintenance: {e}");
            return EXIT_USAGE;
        }
    };
    let paths = match paths() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("maintenance: {e}");
            return EXIT_IO;
        }
    };
    let result = match sub {
        "status" => cmd_status(&paths, &args),
        "stop" => cmd_stop(&paths, &args),
        "start" => cmd_start(&paths, &args),
        "prune" => cmd_prune(&paths, &args),
        "purge" => cmd_purge(&paths, &args),
        "backup" => cmd_backup(&paths, &args),
        "restore" => cmd_restore(&paths, &args),
        "audit" => cmd_audit(&paths, &args),
        "config" => cmd_config(&paths, &args),
        "pin" => cmd_pin(&paths, &args),
        "unpin" => cmd_unpin(&paths, &args),
        "pins" => cmd_pins(&paths, &args),
        "compress" => cmd_compress(&paths, &args),
        other => {
            eprintln!("maintenance: unknown command `{other}`");
            print_usage();
            return EXIT_USAGE;
        }
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("maintenance: {e}");
            e.exit_code()
        }
    }
}

fn print_usage() {
    eprintln!(
        "usage: nai-lattice-cli maintenance \
         <status|stop|start|prune|purge|backup|restore|audit|config|pin|unpin|pins|compress> \
         [options]\n\
         \n\
         prune:  [--scheduled] [--auto] [--dry-run] [--yes] [--stop] [--no-restart]\n\
                 [--logs-days N] [--audit-days N] [--backup-days N] [--runtime-keep N]\n\
                 [--prune-runtimes] [--json]\n\
                 --auto gates the run on shared storage pressure (normal/unknown = no-op)\n\
         purge:  --scope derived|all|legacy [--backup|--no-backup] [--dry-run] [--yes]\n\
                 [--stop] [--no-restart]\n\
         backup: [--to <dir>]\n\
         restore:--from <dir> [--yes] [--stop]\n\
         config: [--init] [--json]\n\
         pin:    --version <v> [--reason <r>] [--by <who>] [--json]\n\
         unpin:  --version <v> | --id <id> [--json]\n\
         pins:   [--json]\n\
         compress: --sealed --older-than <days> [--execute] \
         [--allow-incompatible-readers] [--json]\n\
         \n\
         exit codes: 0 ok, 2 usage, 3 host busy/unreachable, 4 unsafe request, 5 I/O"
    );
}

fn cmd_status(paths: &Paths, args: &Args) -> Result<u8, CmdError> {
    let running = match host_running(paths) {
        Ok(running) => running,
        Err(e) => {
            eprintln!("maintenance: warning: {e}");
            false
        }
    };
    let health = if running { host_health(paths).ok() } else { None };
    let ep = endpoint(paths);
    let active = active_runtime(paths);
    let cfg = load_config(paths);
    // Inventory sizes come from the shared storage policy (same measurement as
    // the host `runtime.storage.status` / dashboard summary), so the CLI cannot
    // drift from the other consumers. The historical `sizes_bytes` names/order
    // are preserved for scripts.
    let storage = storage_status(&storage_paths(paths), &storage_config(&cfg));
    let class = |name: &str| storage["classes"][name].as_u64().unwrap_or(0);
    let sizes = [
        ("ledger", class("ledger")),
        ("cas", class("cas")),
        ("hot", class("hot")),
        ("search", class("search")),
        ("vector", class("vector")),
        ("total", directory_size(&paths.root)),
    ];
    let audit = read_audit_tail(paths, 3);
    // CAS dedup/integrity report (S03 partial): whole-object BLAKE3 addressing
    // means duplicate content is already stored once. Read-only and cheap.
    let cas = match nai_lattice_cas::cas_report(&paths.root.join("cas")) {
        Ok(report) => serde_json::to_value(report)
            .unwrap_or_else(|error| serde_json::json!({"error": error.to_string()})),
        Err(error) => serde_json::json!({"error": error.to_string()}),
    };
    let pins = match PinStore::new(&paths.base).list() {
        Ok(pins) => serde_json::json!({"count": pins.len(), "pins": pins}),
        Err(error) => serde_json::json!({"count": serde_json::Value::Null, "pins": [], "error": error.to_string()}),
    };
    let report = serde_json::json!({
        "root": paths.root.display().to_string(),
        "base": paths.base.display().to_string(),
        "host": {
            "running": running,
            "health": health,
            "endpoint": ep.as_ref().map(|e| serde_json::json!({
                "pid": e.pid, "pipe": e.pipe, "instance_id": e.instance_id,
            })),
        },
        "active_runtime": active,
        "sizes_bytes": sizes.iter().map(|(n, b)| serde_json::json!({"name": n, "bytes": b})).collect::<Vec<_>>(),
        "config": cfg,
        "pins": pins,
        "cas": cas,
        "audit_tail": audit,
    });
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        println!("root             {}", paths.root.display());
        println!("base             {}", paths.base.display());
        match &ep {
            Some(e) => println!("host             {} (pid {:?}, instance {})", if running { "running" } else { "unreachable" }, e.pid, e.instance_id.as_deref().unwrap_or("?")),
            None => println!("host             {}", if running { "running (no endpoint manifest)" } else { "stopped" }),
        }
        if let Some(active) = &active {
            println!("active runtime   {} (git {}, source {})",
                active.get("version").and_then(|v| v.as_str()).unwrap_or("?"),
                active.get("git").and_then(|v| v.as_str()).unwrap_or("?"),
                active.get("source").and_then(|v| v.as_str()).unwrap_or("?"));
        }
        for (name, bytes) in &sizes {
            println!("{:<16} {:>12}", name, human_bytes(*bytes));
        }
        println!("retention        logs {}d, audit {}d, backups {}d, runtimes keep {} (opt-in)",
            cfg.logs_days, cfg.audit_days, cfg.backup_days, cfg.runtime_keep);
        if let Some(h) = &health {
            println!(
                "health           {}",
                h.get("state")
                    .or_else(|| h.get("status"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("?")
            );
        }
        if let Some(last) = audit.last() {
            println!("last audit       {} {} {}", last.get("ts_utc").and_then(|s| s.as_str()).unwrap_or("?"), last.get("action").and_then(|s| s.as_str()).unwrap_or("?"), last.get("result").and_then(|s| s.as_str()).unwrap_or("?"));
        }
    }
    Ok(EXIT_OK)
}

fn cmd_stop(paths: &Paths, args: &Args) -> Result<u8, CmdError> {
    let timeout = Duration::from_secs(args.timeout.unwrap_or(30));
    let was = stop_host(paths, timeout)?;
    println!("{}", if was { "host stopped" } else { "host already stopped" });
    Ok(EXIT_OK)
}

fn cmd_start(paths: &Paths, args: &Args) -> Result<u8, CmdError> {
    let timeout = Duration::from_secs(args.timeout.unwrap_or(30));
    let exe = match &args.host_exe {
        Some(e) => e.clone(),
        None => host_exe_candidates(paths).into_iter().next().ok_or_else(|| {
            CmdError::HostBusy(
                "no nai-fabric-host.exe found; pass --host-exe or start NAI OS".to_string(),
            )
        })?,
    };
    if host_running(paths).map_err(CmdError::HostBusy)? {
        println!("host already running");
        return Ok(EXIT_OK);
    }
    start_host(paths, &exe, timeout)?;
    println!("host started ({})", exe.display());
    Ok(EXIT_OK)
}

fn cmd_prune(paths: &Paths, args: &Args) -> Result<u8, CmdError> {
    cmd_prune_with_status(paths, args, None)
}

/// `status_override` is the measured shared `storage_status` payload. Production
/// always measures it (`None`); tests inject the pressure case they exercise. It
/// is only read when `--auto` is set.
fn cmd_prune_with_status(
    paths: &Paths,
    args: &Args,
    status_override: Option<serde_json::Value>,
) -> Result<u8, CmdError> {
    let mut cfg = load_config(paths);
    if let Some(v) = args.logs_days { cfg.logs_days = v; }
    if let Some(v) = args.audit_days { cfg.audit_days = v; }
    if let Some(v) = args.backup_days { cfg.backup_days = v; }
    if let Some(v) = args.runtime_keep { cfg.runtime_keep = v; }

    // Scheduled task opt-in: a disabled policy is a successful no-op, decided
    // before anything (plan, lease, host) is touched.
    if args.scheduled && !cfg.auto_prune {
        println!(
            "scheduled prune skipped: auto_prune is false in {}",
            config_path(paths).display()
        );
        return Ok(EXIT_OK);
    }

    // Automatic reclaim: pressure-gated BEFORE any plan, lease or host action, so
    // normal/unknown is a pure no-op (no audit record, no lease, no host stop).
    // The decision uses the same shared storage policy as `maintenance status`,
    // `runtime.storage.status` and the dashboard summary.
    let mut auto_pressure: Option<String> = None;
    if args.auto {
        let status = match status_override {
            Some(status) => status,
            None => storage_status(&storage_paths(paths), &storage_config(&cfg)),
        };
        match reclaim_decision(&status) {
            ReclaimDecision::Skip(reason) => {
                println!("prune --auto skipped: {reason}");
                return Ok(EXIT_OK);
            }
            ReclaimDecision::Prune => {
                auto_pressure = status
                    .get("pressure")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                println!(
                    "prune --auto: pressure {}; running the safe prune",
                    auto_pressure.as_deref().unwrap_or("?")
                );
            }
        }
    }

    let mut plan = prune_plan(paths, &cfg, args.prune_runtimes || cfg.prune_runtimes)?;
    let (_, preview_dropped) = rotate_audit(paths, cfg.audit_days, true)?;
    if args.dry_run {
        println!("prune plan (dry run; nothing changed):");
        print_plan(&plan, args.json);
        if preview_dropped > 0 {
            println!("  audit: {preview_dropped} record(s) older than {}d would be removed", cfg.audit_days);
        }
        return Ok(EXIT_OK);
    }
    // `--auto` never stops the host (or writes an audit record) to reclaim
    // nothing.
    if args.auto && plan.items.is_empty() && preview_dropped == 0 {
        for pin in &plan.pinned {
            println!("pinned (kept) {} ({})", pin.version, pin.reason);
        }
        println!("prune --auto: nothing safe to reclaim");
        return Ok(EXIT_OK);
    }
    if !args.yes {
        println!("prune requires --yes (add --dry-run to preview):");
        print_plan(&plan, args.json);
        return Ok(EXIT_USAGE);
    }
    let mut guard = MaintenanceSession::begin(
        paths,
        args,
        "prune",
        Duration::from_secs(args.timeout.unwrap_or(30)),
    )?;
    guard.extend()?;
    // The host is stopped now: never delete a runtime that became active while
    // it was going down, and never chase a path that vanished.
    refresh_plan_after_stop(paths, &mut plan)?;
    for pin in &plan.pinned {
        println!("pinned (kept) {} ({})", pin.version, pin.reason);
    }
    let items: Vec<PruneItem> = plan
        .items
        .iter()
        .map(|item| PruneItem {
            path: item.path.clone(),
            bytes: item.bytes,
            reason: item.reason,
            class: item.class,
        })
        .collect();
    // Single shared apply path: deletion + audit rotation + the audit record.
    // `host_stopped` and the full CLI policy object are preserved in the detail.
    let mut extra = serde_json::json!({
        "host_stopped": guard.host_stopped(),
        "config": cfg,
    });
    if let Some(pressure) = &auto_pressure {
        // Evidence that this record was produced by the pressure gate, not an
        // operator-initiated prune (`--auto` only).
        if let Some(detail) = extra.as_object_mut() {
            detail.insert("auto".to_string(), serde_json::json!(true));
            detail.insert("pressure".to_string(), serde_json::json!(pressure));
        }
    }
    let report = apply_prune_items(
        &storage_paths(paths),
        &storage_config(&cfg),
        &items,
        false,
        Some(&extra),
    )
    .map_err(CmdError::Io)?;
    let removed_path_bytes: u64 = items
        .iter()
        .filter(|item| item.class != "audit")
        .map(|item| item.bytes)
        .sum();
    println!(
        "pruned {} item(s), {}",
        report.items,
        human_bytes(removed_path_bytes)
    );
    guard.finish()?;
    Ok(EXIT_OK)
}

fn cmd_purge(paths: &Paths, args: &Args) -> Result<u8, CmdError> {
    let scope_s = args
        .scope
        .clone()
        .ok_or_else(|| CmdError::Usage("--scope derived|all|legacy is required".into()))?;
    let scope = parse_scope(&scope_s).map_err(CmdError::Usage)?;
    let targets = scope_targets(paths, scope);
    let total: u64 = targets.iter().map(|p| dir_size(p)).sum();
    let items = targets.iter().filter(|p| p.exists()).count();

    if args.dry_run {
        println!("purge --scope {scope_s:?} (dry run; nothing changed):");
        for t in targets.iter().filter(|p| p.exists()) {
            println!("  {:>10}  {}", human_bytes(dir_size(t)), t.display());
        }
        println!("  total {:>10}  ({items} item(s))", human_bytes(total));
        return Ok(EXIT_OK);
    }
    if !args.yes {
        return Err(CmdError::Unsafe(
            "refusing to purge without --yes (use --dry-run to preview)".into(),
        ));
    }
    // `all` is irreversible by design: back up unless explicitly declined.
    let backup = args.backup.unwrap_or(scope == Scope::All);
    let mut guard = MaintenanceSession::begin(
        paths,
        args,
        "purge",
        Duration::from_secs(args.timeout.unwrap_or(30)),
    )?;
    guard.extend()?;
    let mut backup_dir_created: Option<PathBuf> = None;
    if backup {
        let dir = create_backup(paths, scope, "pre-purge")?;
        println!("backup         {}", dir.display());
        backup_dir_created = Some(dir);
    }
    let mut removed = 0u64;
    let mut bytes = 0u64;
    for t in &targets {
        if t.exists() {
            bytes += remove_tree_safe(t)?;
            removed += 1;
        }
    }
    append_audit(
        paths,
        audit_record(
            "purge",
            serde_json::json!({
                "scope": scope_s,
                "items": items,
                "bytes": bytes,
                "backup": backup_dir_created.as_ref().map(|p| p.display().to_string()),
                "host_stopped": guard.host_stopped(),
            }),
            "ok",
        ),
    );
    println!("purged {scope_s}: {removed} item(s), {}", human_bytes(bytes));
    guard.finish()?;
    Ok(EXIT_OK)
}

fn cmd_backup(paths: &Paths, args: &Args) -> Result<u8, CmdError> {
    // Validate the destination before stopping anything.
    let dest = match &args.to {
        Some(to) => to.clone(),
        None => backup_dir(paths, "manual"),
    };
    ensure_backup_destination(paths, &dest)?;
    let mut guard = MaintenanceSession::begin(
        paths,
        args,
        "backup",
        Duration::from_secs(args.timeout.unwrap_or(30)),
    )?;
    guard.extend()?;
    let dir = if args.to.is_some() {
        // Re-check after the host is stopped (the destination may be a live dir).
        ensure_backup_destination(paths, &dest)?;
        std::fs::create_dir_all(&dest).map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
        let (files, bytes) = copy_root_excluding_lease(&paths.root, &dest.join("data"))?;
        let manifest = serde_json::json!({
            "schema": 1,
            "created_utc": now_iso(),
            "scope": "all",
            "source_root": paths.root.display().to_string(),
            "files": files,
            "bytes": bytes,
            "cli_version": env!("CARGO_PKG_VERSION"),
            "active": active_runtime(paths),
        });
        std::fs::write(dest.join("manifest.json"), serde_json::to_string_pretty(&manifest).unwrap())
            .map_err(|e| format!("write manifest: {e}"))?;
        println!("backup {} ({} files, {})", dest.display(), files, human_bytes(bytes));
        dest
    } else {
        let dir = create_backup_at(paths, Scope::All, &dest)?;
        println!("backup {}", dir.display());
        dir
    };
    append_audit(paths, audit_record("backup", serde_json::json!({"path": dir.display().to_string()}), "ok"));
    guard.finish()?;
    Ok(EXIT_OK)
}

fn cmd_restore(paths: &Paths, args: &Args) -> Result<u8, CmdError> {
    let from = args
        .from
        .clone()
        .ok_or_else(|| CmdError::Usage("--from <backup-dir> is required".into()))?;
    if !args.yes {
        return Err(CmdError::Unsafe(
            "restore replaces the runtime root; pass --yes".into(),
        ));
    }
    // Full validation happens BEFORE the host is stopped: source/manifest/scope/
    // data directory/completeness and root overlap.
    let plan = validate_restore(paths, &from)?;
    let mut guard = MaintenanceSession::begin(
        paths,
        args,
        "restore",
        Duration::from_secs(args.timeout.unwrap_or(30)),
    )?;
    guard.extend()?;
    // Keep a rollback point of the current root before clearing it.
    let pre_restore = create_backup(paths, Scope::All, "pre-restore")?;
    println!("pre-restore backup {}", pre_restore.display());
    restore_backup(paths, &plan)?;
    append_audit(
        paths,
        audit_record(
            "restore",
            serde_json::json!({
                "from": plan.from.display().to_string(),
                "files": plan.files,
                "bytes": plan.bytes,
                "pre_restore_backup": pre_restore.display().to_string(),
                "host_stopped": guard.host_stopped(),
            }),
            "ok",
        ),
    );
    println!(
        "restored {} ({} files, {})",
        plan.from.display(),
        plan.files,
        human_bytes(plan.bytes)
    );
    guard.finish()?;
    Ok(EXIT_OK)
}

fn cmd_audit(paths: &Paths, args: &Args) -> Result<u8, CmdError> {
    let limit = args.limit.unwrap_or(20);
    for rec in read_audit_tail(paths, limit) {
        println!("{}", serde_json::to_string(&rec).unwrap_or_default());
    }
    Ok(EXIT_OK)
}

fn cmd_config(paths: &Paths, args: &Args) -> Result<u8, CmdError> {
    let mut cfg = load_config(paths);
    if args.init {
        save_config(paths, &cfg)?;
        println!("wrote {}", config_path(paths).display());
    }
    // Apply explicit overrides in-memory for display only; persistence stays
    // with the operator (`--init` writes the file).
    if let Some(v) = args.logs_days { cfg.logs_days = v; }
    if let Some(v) = args.audit_days { cfg.audit_days = v; }
    if let Some(v) = args.backup_days { cfg.backup_days = v; }
    if let Some(v) = args.runtime_keep { cfg.runtime_keep = v; }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&cfg).unwrap());
    } else {
        println!("{}", serde_json::to_string_pretty(&cfg).unwrap());
    }
    Ok(EXIT_OK)
}

// ---------------------------------------------------------------------------
// Runtime pins (R04 / NAIOS-007)
// ---------------------------------------------------------------------------

fn pin_store(paths: &Paths) -> PinStore {
    PinStore::new(&paths.base)
}

fn map_pin_error(error: PinError) -> CmdError {
    match error {
        PinError::Io(message) => CmdError::Io(message),
        PinError::Corrupt(message) => CmdError::Unsafe(message),
        PinError::Invalid(message) => CmdError::Usage(message),
    }
}

/// `maintenance pin --version <v> [--reason <r>] [--by <who>] [--json]`.
///
/// Pins are lifecycle metadata, not data: this never stops the host, never
/// takes the maintenance lease and never rotates/prunes anything.
fn cmd_pin(paths: &Paths, args: &Args) -> Result<u8, CmdError> {
    let version = args
        .version
        .clone()
        .ok_or_else(|| CmdError::Usage("pin requires --version <version>".into()))?;
    let reason = args
        .reason
        .clone()
        .unwrap_or_else(|| "operator pin".to_string());
    let by = args
        .by
        .clone()
        .unwrap_or_else(|| "nai-lattice-cli".to_string());
    let record = pin_store(paths)
        .pin(&version, &reason, &by)
        .map_err(map_pin_error)?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "pinned": record,
                "pinsFile": pin_store(paths).path().display().to_string(),
            }))
            .unwrap()
        );
    } else {
        println!(
            "pinned {} ({}) {}",
            record.version,
            record.id,
            record.reason
        );
    }
    Ok(EXIT_OK)
}

/// `maintenance unpin --version <v> | --id <id> [--json]`.
fn cmd_unpin(paths: &Paths, args: &Args) -> Result<u8, CmdError> {
    let removed = match (&args.version, &args.id) {
        (Some(version), None) => pin_store(paths)
            .unpin_version(version)
            .map_err(map_pin_error)?,
        (None, Some(id)) => pin_store(paths).unpin_id(id).map_err(map_pin_error)?,
        (Some(_), Some(_)) => {
            return Err(CmdError::Usage(
                "unpin takes exactly one of --version or --id".into(),
            ))
        }
        (None, None) => {
            return Err(CmdError::Usage(
                "unpin requires --version <version> or --id <id>".into(),
            ))
        }
    };
    if !removed {
        return Err(CmdError::Unsafe(match (&args.version, &args.id) {
            (Some(version), _) => format!("no pin exists for version {version}"),
            (_, Some(id)) => format!("no pin exists with id {id}"),
            _ => "no pin matched".into(),
        }));
    }
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"unpinned": true}))
                .unwrap()
        );
    } else {
        println!("unpinned");
    }
    Ok(EXIT_OK)
}

/// `maintenance pins [--json]`. A corrupt pins file is reported (exit 5) rather
/// than hidden; pruning fails closed on the same condition.
fn cmd_pins(paths: &Paths, args: &Args) -> Result<u8, CmdError> {
    match pin_store(paths).list() {
        Ok(pins) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "schema": 1,
                        "pinsFile": pin_store(paths).path().display().to_string(),
                        "count": pins.len(),
                        "pins": pins,
                    }))
                    .unwrap()
                );
            } else if pins.is_empty() {
                println!("no pins ({})", pin_store(paths).path().display());
            } else {
                for pin in &pins {
                    println!(
                        "{:<16} {:<40} {}",
                        pin.version,
                        pin.id,
                        pin.reason
                    );
                }
            }
            Ok(EXIT_OK)
        }
        Err(error) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "schema": 1,
                        "pinsFile": pin_store(paths).path().display().to_string(),
                        "count": serde_json::Value::Null,
                        "pins": [],
                        "error": error.to_string(),
                    }))
                    .unwrap()
                );
            }
            eprintln!("maintenance: pins: {error}");
            Ok(EXIT_IO)
        }
    }
}

// ---------------------------------------------------------------------------
// Sealed-segment compression (S03 / NAIOS-011/012)
// ---------------------------------------------------------------------------

struct CompressCandidate {
    path: PathBuf,
    bytes: u64,
    mtime_ms: u64,
}

struct CompressSkip {
    path: PathBuf,
    reason: String,
}

/// Plan compressed sealed segments older than `cutoff_ms`.
///
/// Only `<root>/ledger/sealed` is walked: the active segment and the
/// maintenance lease are never candidates. Reparse points are not traversed.
fn compression_candidates(
    paths: &Paths,
    cutoff_ms: u64,
) -> Result<(Vec<CompressCandidate>, Vec<CompressSkip>), CmdError> {
    let sealed = paths.root.join("ledger").join("sealed");
    let mut candidates = Vec::new();
    let mut skipped = Vec::new();
    if !sealed.exists() {
        return Ok((candidates, skipped));
    }
    collect_compress_candidates(&sealed, 0, cutoff_ms, &mut candidates, &mut skipped)?;
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    skipped.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((candidates, skipped))
}

fn collect_compress_candidates(
    dir: &Path,
    depth: usize,
    cutoff_ms: u64,
    candidates: &mut Vec<CompressCandidate>,
    skipped: &mut Vec<CompressSkip>,
) -> Result<(), CmdError> {
    const MAX_DEPTH: usize = 8;
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir)
        .map_err(|error| CmdError::Io(format!("read {}: {error}", dir.display())))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if is_reparse_point(&metadata) {
            continue;
        }
        if metadata.is_dir() {
            collect_compress_candidates(&path, depth + 1, cutoff_ms, candidates, skipped)?;
            continue;
        }
        if path.extension().map(|ext| ext != "segment").unwrap_or(true) {
            continue;
        }
        if path.file_name().map(|name| name == "active.segment").unwrap_or(false) {
            skipped.push(CompressSkip {
                path,
                reason: "active segment".into(),
            });
            continue;
        }
        match compression::is_compressed_file(&path) {
            Ok(true) => {
                skipped.push(CompressSkip {
                    path,
                    reason: "already compressed".into(),
                });
                continue;
            }
            Ok(false) => {}
            Err(error) => {
                return Err(CmdError::Io(format!(
                    "probe {}: {error}",
                    path.display()
                )))
            }
        }
        let Some(modified) = file_mtime(&path) else {
            skipped.push(CompressSkip {
                path,
                reason: "unreadable mtime (fail closed)".into(),
            });
            continue;
        };
        let mtime_ms = modified
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        if mtime_ms >= cutoff_ms {
            skipped.push(CompressSkip {
                path,
                reason: "not older than --older-than".into(),
            });
            continue;
        }
        let bytes = metadata.len();
        candidates.push(CompressCandidate {
            path,
            bytes,
            mtime_ms,
        });
    }
    Ok(())
}

/// Rollback runtime versions that cannot read a NAIZ envelope.
fn rollback_reader_versions(paths: &Paths) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(paths.base.join("runtime")) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("0.1.1") || name.starts_with("0.1.2") {
            out.push(name);
        }
    }
    out.sort();
    out
}

/// Reuse an existing valid dictionary, train one when absent, or fall back to
/// no dictionary. An existing but invalid dictionary is never replaced: old
/// envelopes reference it by BLAKE3.
fn ensure_compression_dictionary(
    paths: &Paths,
    candidates: &[CompressCandidate],
) -> (Option<Vec<u8>>, String) {
    let path = paths.base.join(COMPRESSION_DICT_FILE);
    if path.exists() {
        return match compression::load_dictionary(&paths.base) {
            Some(dictionary) => (
                Some(dictionary),
                format!("reused existing {}", path.display()),
            ),
            None => (
                None,
                format!(
                    "{} exists but is not a valid zstd dictionary; not replacing it \
                     (existing envelopes may reference it), compressing without a dictionary",
                    path.display()
                ),
            ),
        };
    }
    let mut samples: Vec<Vec<u8>> = Vec::new();
    for candidate in candidates.iter().take(32) {
        let Ok(file) = std::fs::File::open(&candidate.path) else {
            continue;
        };
        let mut sample = Vec::new();
        if file.take(64 * 1024).read_to_end(&mut sample).is_ok() && !sample.is_empty() {
            samples.push(sample);
        }
    }
    match compression::train_dictionary(&samples, 64 * 1024) {
        Ok(dictionary) => match compression::write_dictionary_if_absent(&paths.base, &dictionary) {
            Ok(true) => (
                Some(dictionary),
                format!("trained {}", path.display()),
            ),
            Ok(false) => match compression::load_dictionary(&paths.base) {
                Some(existing) => (
                    Some(existing),
                    format!("reused concurrently written {}", path.display()),
                ),
                None => (
                    None,
                    "a dictionary appeared concurrently but is invalid; compressing without one"
                        .into(),
                ),
            },
            Err(error) => (
                None,
                format!("dictionary training succeeded but could not be persisted ({error}); \
                         compressing without a dictionary"),
            ),
        },
        Err(error) => (
            None,
            format!("dictionary training unavailable ({error}); compressing without a dictionary"),
        ),
    }
}

/// `maintenance compress --sealed --older-than <days> [--execute]
/// [--allow-incompatible-readers] [--json]`.
///
/// No host stop and no maintenance lease: sealed segments are replaced
/// atomically, the active segment and lease are never touched, and the default
/// (no `--execute`) is a pure plan.
fn cmd_compress(paths: &Paths, args: &Args) -> Result<u8, CmdError> {
    if !args.sealed {
        return Err(CmdError::Usage(
            "compress requires --sealed (only sealed segments are eligible)".into(),
        ));
    }
    let days = args.older_than.ok_or_else(|| {
        CmdError::Usage("compress requires --older-than <days>".into())
    })?;
    if !args.allow_incompatible_readers {
        let rollbacks = rollback_reader_versions(paths);
        let installed = if rollbacks.is_empty() {
            "a rollback to 0.1.1/0.1.2 would be unable to read them".to_string()
        } else {
            format!(
                "installed rollback runtimes that cannot read them: {}",
                rollbacks.join(", ")
            )
        };
        return Err(CmdError::Unsafe(format!(
            "refusing to compress sealed segments without --allow-incompatible-readers: \
             NAIZ envelopes require a reader that understands the envelope, and {installed}. \
             Re-run with --allow-incompatible-readers to acknowledge this."
        )));
    }
    let cutoff = now_ms().saturating_sub(days.saturating_mul(86_400_000));
    let (candidates, skipped) = compression_candidates(paths, cutoff)?;
    let total: u64 = candidates.iter().map(|candidate| candidate.bytes).sum();

    if args.json {
        // A dry run is one JSON document; an execution emits exactly one final
        // JSON result (the plan is folded into the summary counters).
        if !args.execute {
            let planned: Vec<serde_json::Value> = candidates
                .iter()
                .map(|candidate| {
                    serde_json::json!({
                        "path": candidate.path.display().to_string(),
                        "bytes": candidate.bytes,
                        "mtimeUnixMs": candidate.mtime_ms,
                    })
                })
                .collect();
            let skipped_json: Vec<serde_json::Value> = skipped
                .iter()
                .map(|skip| {
                    serde_json::json!({
                        "path": skip.path.display().to_string(),
                        "reason": skip.reason,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "sealed": true,
                    "execute": false,
                    "olderThanDays": days,
                    "candidates": planned,
                    "bytes": total,
                    "skipped": skipped_json,
                }))
                .unwrap()
            );
        }
    } else {
        println!(
            "sealed compression plan ({}): {} segment(s), {}",
            if args.execute { "execute" } else { "dry run; nothing changed" },
            candidates.len(),
            human_bytes(total)
        );
        for skip in &skipped {
            println!("  skip  {:<28} {}", skip.reason, skip.path.display());
        }
        for candidate in &candidates {
            println!(
                "  {:<28} {:>10}  {}",
                "compress",
                human_bytes(candidate.bytes),
                candidate.path.display()
            );
        }
    }
    if !args.execute {
        return Ok(EXIT_OK);
    }
    if candidates.is_empty() {
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "sealed": true,
                    "execute": true,
                    "candidates": 0,
                    "compressed": 0,
                    "failed": 0,
                    "sourceBytes": 0,
                    "envelopeBytes": 0,
                }))
                .unwrap()
            );
        } else {
            println!("compress: nothing eligible to compress");
        }
        return Ok(EXIT_OK);
    }

    let (dictionary, dictionary_state) = ensure_compression_dictionary(paths, &candidates);
    if !args.json {
        println!("dictionary: {dictionary_state}");
    }
    let mut compressed = 0u64;
    let mut failed = 0u64;
    let mut source_bytes = 0u64;
    let mut envelope_bytes = 0u64;
    for candidate in &candidates {
        let options = CompressionOptions {
            codec: SealedCodec::Zstd,
            block_bytes: DEFAULT_BLOCK_BYTES,
            level: 3,
            dictionary: dictionary.clone(),
        };
        match compression::compress_sealed_in_place(&candidate.path, &options) {
            Ok(report) => {
                compressed += 1;
                source_bytes = source_bytes.saturating_add(report.source_bytes);
                envelope_bytes = envelope_bytes.saturating_add(report.envelope_bytes);
                if !args.json {
                    println!(
                        "compressed {} ({} -> {}, {} frames, {} blocks)",
                        candidate.path.display(),
                        human_bytes(report.source_bytes),
                        human_bytes(report.envelope_bytes),
                        report.frames,
                        report.blocks
                    );
                }
            }
            Err(error) => {
                failed += 1;
                eprintln!(
                    "maintenance: compress {}: {error}",
                    candidate.path.display()
                );
            }
        }
    }
    append_audit(
        paths,
        audit_record(
            "compress",
            serde_json::json!({
                "sealed": true,
                "compressed": compressed,
                "failed": failed,
                "sourceBytes": source_bytes,
                "envelopeBytes": envelope_bytes,
                "dictionary": dictionary_state,
                "olderThanDays": days,
            }),
            if failed == 0 { "ok" } else { "error" },
        ),
    );
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "sealed": true,
                "execute": true,
                "candidates": candidates.len(),
                "compressed": compressed,
                "failed": failed,
                "sourceBytes": source_bytes,
                "envelopeBytes": envelope_bytes,
                "dictionary": dictionary_state,
            }))
            .unwrap()
        );
    } else {
        println!(
            "compressed {compressed} segment(s), {} -> {} ({} failed)",
            human_bytes(source_bytes),
            human_bytes(envelope_bytes),
            failed
        );
    }
    Ok(if failed == 0 { EXIT_OK } else { EXIT_IO })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_formats_epoch() {
        assert_eq!(iso_from_ms(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_from_ms(1_000), "1970-01-01T00:00:01Z");
    }

    #[test]
    fn iso_formats_known_timestamp() {
        // 2026-11-24T18:42:05Z
        let ms = 1_795_545_725_000u64;
        assert_eq!(iso_from_ms(ms), "2026-11-24T18:42:05Z");
    }

    #[test]
    fn scope_parsing_is_strict() {
        assert!(matches!(parse_scope("derived"), Ok(Scope::Derived)));
        assert!(matches!(parse_scope("all"), Ok(Scope::All)));
        assert!(matches!(parse_scope("legacy"), Ok(Scope::Legacy)));
        assert!(parse_scope("everything").is_err());
    }

    #[test]
    fn config_defaults_are_sane() {
        let c = MaintenanceConfig::default();
        assert_eq!(c.logs_days, 30);
        assert_eq!(c.audit_days, 90);
        assert_eq!(c.backup_days, 14);
        assert!(c.runtime_keep >= 1);
    }

    #[test]
    fn audit_rotation_keeps_recent_and_malformed() {
        let base = std::env::temp_dir().join(format!("nai-lattice-maint-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let paths = Paths { base: base.clone(), root: base.join("v1") };
        std::fs::create_dir_all(&paths.root).unwrap();
        let old = serde_json::json!({"ts_ms": 0, "action": "old"});
        let new = serde_json::json!({"ts_ms": now_ms(), "action": "new"});
        let mut text = String::new();
        text.push_str(&serde_json::to_string(&old).unwrap());
        text.push('\n');
        text.push_str("{not json}\n");
        text.push_str(&serde_json::to_string(&new).unwrap());
        text.push('\n');
        std::fs::write(audit_path(&paths), text).unwrap();
        let (kept, dropped) = rotate_audit(&paths, 1, false).unwrap();
        assert_eq!(dropped, 1);
        assert_eq!(kept, 2);
        let after = std::fs::read_to_string(audit_path(&paths)).unwrap();
        assert!(after.contains("\"new\""));
        assert!(after.contains("{not json}"));
        assert!(!after.contains("old"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_tree_refuses_to_follow_links() {
        // A plain file deletes; a missing path is a no-op.
        let dir = std::env::temp_dir().join(format!("nai-lattice-maint-del-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.log");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(remove_tree_safe(&f).unwrap(), 3);
        assert!(!f.exists());
        assert_eq!(remove_tree_safe(&f).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_plan_never_targets_active_runtime() {
        let base = std::env::temp_dir().join(format!("nai-lattice-maint-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let paths = Paths { base: base.clone(), root: base.join("v1") };
        std::fs::create_dir_all(paths.base.join("runtime").join("0.1.1")).unwrap();
        std::fs::create_dir_all(paths.base.join("runtime").join("0.1.2")).unwrap();
        std::fs::create_dir_all(paths.base.join("runtime").join("0.1.3")).unwrap();
        std::fs::write(
            paths.base.join("active.json"),
            r#"{"version":"0.1.3"}"#,
        )
        .unwrap();
        let cfg = MaintenanceConfig { runtime_keep: 0, prune_runtimes: true, ..Default::default() };
        let plan = prune_plan(&paths, &cfg, true).unwrap();
        let joined: Vec<String> = plan.items.iter().map(|i| i.path.display().to_string()).collect();
        assert!(joined.iter().any(|p| p.ends_with("0.1.1") || p.ends_with("0.1.2")));
        assert!(!joined.iter().any(|p| p.ends_with("0.1.3")), "active runtime must never be planned for removal");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn backup_manifest_roundtrip() {
        let base = std::env::temp_dir().join(format!("nai-lattice-maint-bk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let paths = Paths { base: base.clone(), root: base.join("v1") };
        std::fs::create_dir_all(paths.root.join("ledger").join("active")).unwrap();
        std::fs::write(paths.root.join("ledger").join("active").join("active.segment"), b"seg").unwrap();
        std::fs::write(paths.root.join("read-model.json"), b"{}").unwrap();
        let dir = create_backup(&paths, Scope::All, "test").unwrap();
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["scope"], "all");
        assert!(manifest["files"].as_u64().unwrap() >= 2);
        let _ = std::fs::remove_dir_all(&base);
    }

    fn temp_paths(tag: &str) -> Paths {
        let base = std::env::temp_dir().join(format!(
            "nai-lattice-maint-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let paths = Paths { base: base.clone(), root: base.join("v1") };
        std::fs::create_dir_all(&paths.root).unwrap();
        paths
    }

    #[test]
    fn typed_errors_map_to_documented_exit_codes() {
        assert_eq!(CmdError::Usage("x".into()).exit_code(), EXIT_USAGE);
        assert_eq!(CmdError::Unsafe("x".into()).exit_code(), EXIT_UNSAFE);
        assert_eq!(CmdError::HostBusy("x".into()).exit_code(), EXIT_HOST_BUSY);
        assert_eq!(CmdError::Io("x".into()).exit_code(), EXIT_IO);
        // Plain strings default to I/O.
        assert!(matches!(CmdError::from(String::from("disk")), CmdError::Io(_)));
        assert!(matches!(CmdError::from("disk"), CmdError::Io(_)));
    }

    #[test]
    fn restore_refuses_derived_backup_scope() {
        let paths = temp_paths("restore-derived");
        let from = paths.base.join("backup-derived");
        std::fs::create_dir_all(from.join("data")).unwrap();
        std::fs::write(from.join("data").join("hot.json"), b"{}").unwrap();
        let manifest = serde_json::json!({
            "schema": 1, "scope": "derived", "files": 1, "bytes": 2,
        });
        std::fs::write(
            from.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        match validate_restore(&paths, &from) {
            Err(CmdError::Unsafe(message)) => {
                assert!(message.contains("derived"), "{message}");
            }
            Err(other) => panic!("expected Unsafe, got {other:?}"),
            Ok(_) => panic!("a derived backup must never be restored as a full root"),
        }
        let _ = std::fs::remove_dir_all(&paths.base);
    }

    #[test]
    fn restore_refuses_incomplete_full_backup() {
        let paths = temp_paths("restore-incomplete");
        let from = paths.base.join("backup-incomplete");
        std::fs::create_dir_all(from.join("data")).unwrap();
        std::fs::write(from.join("data").join("only.bin"), b"x").unwrap();
        // Manifest claims 2 files / 9 bytes; the data has 1 file / 1 byte.
        let manifest = serde_json::json!({
            "schema": 1, "scope": "all", "files": 2, "bytes": 9,
        });
        std::fs::write(
            from.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        match validate_restore(&paths, &from) {
            Err(CmdError::Unsafe(message)) => {
                assert!(message.contains("incomplete"), "{message}");
            }
            Err(other) => panic!("expected Unsafe, got {other:?}"),
            Ok(_) => panic!("an incomplete backup must be refused"),
        }
        let _ = std::fs::remove_dir_all(&paths.base);
    }

    #[test]
    fn backup_destination_must_not_overlap_runtime_root_or_be_occupied() {
        let paths = temp_paths("backup-dest");
        let inside = paths.root.join("nested").join("backup");
        assert!(matches!(
            ensure_backup_destination(&paths, &inside),
            Err(CmdError::Unsafe(_))
        ));
        assert!(matches!(
            ensure_backup_destination(&paths, &paths.base),
            Err(CmdError::Unsafe(_))
        ));
        // Lexical `..` must be resolved before the containment check.
        let lexical = paths
            .base
            .join("elsewhere")
            .join("..")
            .join("v1")
            .join("backup");
        assert!(matches!(
            ensure_backup_destination(&paths, &lexical),
            Err(CmdError::Unsafe(_))
        ));
        let occupied = paths.base.join("occupied");
        std::fs::create_dir_all(&occupied).unwrap();
        std::fs::write(occupied.join("manifest.json"), b"{}").unwrap();
        match ensure_backup_destination(&paths, &occupied) {
            Err(CmdError::Unsafe(message)) => {
                assert!(message.contains("not empty"), "{message}");
                assert!(message.contains("manifest.json"), "{message}");
            }
            Err(other) => panic!("expected Unsafe, got {other:?}"),
            Ok(_) => panic!("an occupied destination must be refused"),
        }
        let empty = paths.base.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(ensure_backup_destination(&paths, &empty).is_ok());
        assert!(ensure_backup_destination(&paths, &paths.base.join("new")).is_ok());
        let _ = std::fs::remove_dir_all(&paths.base);
    }

    #[test]
    fn runtime_prune_requires_readable_active_version() {
        let paths = temp_paths("prune-active");
        std::fs::create_dir_all(paths.base.join("runtime").join("0.1.1")).unwrap();
        std::fs::create_dir_all(paths.base.join("runtime").join("0.1.2")).unwrap();
        let cfg = MaintenanceConfig {
            runtime_keep: 0,
            prune_runtimes: true,
            ..Default::default()
        };
        match prune_plan(&paths, &cfg, true) {
            Err(CmdError::Unsafe(message)) => {
                assert!(message.contains("active.json"), "{message}");
            }
            Err(other) => panic!("expected Unsafe, got {other:?}"),
            Ok(_) => panic!("runtime prune must be refused without active.json"),
        }
        std::fs::write(paths.base.join("active.json"), b"{}").unwrap();
        assert!(matches!(
            prune_plan(&paths, &cfg, true),
            Err(CmdError::Unsafe(_))
        ));
        std::fs::write(paths.base.join("active.json"), br#"{"version":"0.1.2"}"#).unwrap();
        let plan = prune_plan(&paths, &cfg, true).unwrap();
        assert!(plan.items.iter().any(|item| item.path.ends_with("0.1.1")));
        assert!(
            plan.items.iter().all(|item| !item.path.ends_with("0.1.2")),
            "the active runtime must never be planned for removal"
        );
        let _ = std::fs::remove_dir_all(&paths.base);
    }

    #[test]
    fn scheduled_prune_respects_auto_prune_false() {
        let paths = temp_paths("scheduled");
        std::fs::write(
            config_path(&paths),
            serde_json::to_vec(&MaintenanceConfig {
                auto_prune: false,
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        let stale_log = paths.root.join("old.log");
        std::fs::write(&stale_log, b"stale").unwrap();
        let args = Args {
            scheduled: true,
            yes: true,
            stop: true,
            ..Default::default()
        };
        assert_eq!(cmd_prune(&paths, &args).unwrap(), EXIT_OK);
        assert!(stale_log.exists(), "auto_prune=false must change nothing");
        assert!(!paths.root.join(MAINTENANCE_LOCK_FILE).exists());
        assert!(!paths.base.join(BACKUP_DIR).exists());
        let _ = std::fs::remove_dir_all(&paths.base);
    }

    /// Age a file or directory (test-only). `File::set_modified` needs an open
    /// handle; on Windows a directory handle requires FILE_FLAG_BACKUP_SEMANTICS,
    /// which std exposes through `OpenOptionsExt::custom_flags`.
    fn age_path(path: &Path, days: u64) {
        let when = SystemTime::now()
            .checked_sub(Duration::from_secs(days * 86_400))
            .unwrap_or(UNIX_EPOCH);
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_WRITE_ATTRIBUTES is what SetFileTime needs; GENERIC_READ
            // (plain File::open) is refused. FILE_FLAG_BACKUP_SEMANTICS lets the
            // same call open a directory instead of failing.
            std::fs::OpenOptions::new()
                .access_mode(0x0100) // FILE_WRITE_ATTRIBUTES
                .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS
                .open(path)
                .expect("open path for aging")
        };
        #[cfg(not(windows))]
        let file = std::fs::File::open(path).expect("open path for aging");
        file.set_modified(when).expect("set mtime");
    }

    #[test]
    fn auto_prune_at_normal_pressure_is_a_no_op() {
        let paths = temp_paths("auto-normal");
        // The newest log is the live log and is never planned; `old.log` is the
        // rotated one and would be eligible at elevated pressure.
        std::fs::write(paths.root.join("current.log"), b"current").unwrap();
        let stale_log = paths.root.join("old.log");
        std::fs::write(&stale_log, b"stale").unwrap();
        age_path(&stale_log, 60);
        let expired_backup = paths.base.join(BACKUP_DIR).join("old");
        std::fs::create_dir_all(&expired_backup).unwrap();
        std::fs::write(expired_backup.join("f"), b"old").unwrap();
        age_path(&expired_backup, 30);

        let args = Args {
            auto: true,
            yes: true,
            scheduled: true,
            stop: true,
            ..Default::default()
        };
        let code = cmd_prune_with_status(
            &paths,
            &args,
            Some(serde_json::json!({"pressure": "normal"})),
        )
        .unwrap();
        assert_eq!(code, EXIT_OK);
        assert!(stale_log.exists(), "normal pressure must not prune");
        assert!(expired_backup.exists(), "normal pressure must not prune");
        assert!(
            !paths.base.join(AUDIT_FILE).exists(),
            "a skipped auto prune must not write an audit record"
        );
        assert!(
            !paths.root.join(MAINTENANCE_LOCK_FILE).exists(),
            "a skipped auto prune must not take the lease"
        );
        let _ = std::fs::remove_dir_all(&paths.base);
    }

    #[test]
    fn auto_prune_at_critical_pressure_prunes_only_safe_classes() {
        let paths = temp_paths("auto-critical");
        std::fs::write(paths.root.join("current.log"), b"current").unwrap();
        let stale_log = paths.root.join("old.log");
        std::fs::write(&stale_log, b"stale").unwrap();
        age_path(&stale_log, 60);
        let expired_backup = paths.base.join(BACKUP_DIR).join("expired");
        std::fs::create_dir_all(&expired_backup).unwrap();
        std::fs::write(expired_backup.join("f"), b"old").unwrap();
        age_path(&expired_backup, 30);
        // A current backup is not stale: the safe policy keeps it.
        let fresh_backup = paths.base.join(BACKUP_DIR).join("fresh");
        std::fs::create_dir_all(&fresh_backup).unwrap();
        std::fs::write(fresh_backup.join("f"), b"new").unwrap();
        // Canonical data the shared policy must never touch.
        let ledger = paths.root.join("ledger").join("active").join("active.segment");
        std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();
        std::fs::write(&ledger, b"ledger").unwrap();
        std::fs::create_dir_all(paths.root.join("cas")).unwrap();
        std::fs::write(paths.root.join("cas").join("obj.bin"), b"cas").unwrap();
        std::fs::write(paths.root.join("read-model.json"), b"{}").unwrap();

        let args = Args {
            auto: true,
            yes: true,
            scheduled: true,
            stop: true,
            ..Default::default()
        };
        let code = cmd_prune_with_status(
            &paths,
            &args,
            Some(serde_json::json!({"pressure": "critical"})),
        )
        .unwrap();
        assert_eq!(code, EXIT_OK);
        assert!(!stale_log.exists(), "the stale log is safe to remove");
        assert!(paths.root.join("current.log").exists(), "the live log is kept");
        assert!(!expired_backup.exists(), "the expired backup is safe to remove");
        assert!(fresh_backup.join("f").exists(), "a current backup is kept");
        assert!(ledger.exists(), "the canonical ledger is never pruned");
        assert!(paths.root.join("cas").join("obj.bin").exists());
        assert!(paths.root.join("read-model.json").exists());

        let audit = std::fs::read_to_string(paths.base.join(AUDIT_FILE)).unwrap();
        let last: serde_json::Value = serde_json::from_str(audit.lines().last().unwrap()).unwrap();
        assert_eq!(last["action"], "prune");
        assert_eq!(last["result"], "ok");
        assert_eq!(
            last["detail"]["items"], 2,
            "one stale log and one expired backup"
        );
        assert_eq!(last["detail"]["auto"], true);
        assert_eq!(last["detail"]["pressure"], "critical");
        let _ = std::fs::remove_dir_all(&paths.base);
    }

    /// Build a valid plain sealed segment (enough frames to compress).
    fn append_plain_segment(path: &Path, count: u64) {
        use prost::Message;
        let mut writer =
            nai_lattice_ledger::segment::SegmentWriter::create_empty(path).expect("segment writer");
        let mut previous: Option<String> = None;
        for sequence in 1..=count {
            let event = nai_lattice_protocol::pb::Event {
                event_id: format!("evt-{sequence}"),
                runtime_sequence: sequence,
                timestamp_utc_ms: sequence as i64,
                producer_app: "cli-test".into(),
                namespace: "test".into(),
                event_type: "test.event".into(),
                previous_hash: previous.clone().unwrap_or_default(),
                ..Default::default()
            };
            let event_body = event.encode_to_vec();
            let mut frame = nai_lattice_protocol::pb::Frame {
                magic: nai_lattice_protocol::FRAME_MAGIC,
                format_version: 1,
                frame_length: event_body.len() as u64,
                runtime_sequence: sequence,
                event_id: event.event_id.clone(),
                protobuf_event: event_body,
                crc32c: 0,
                frame_blake3: Vec::new(),
            };
            let mut bare = Vec::new();
            frame.encode(&mut bare).unwrap();
            frame.crc32c = crc32c::crc32c(&bare);
            let hash = blake3::hash(&bare);
            frame.frame_blake3 = hash.as_bytes().to_vec();
            previous = Some(hash.to_hex().to_string());
            writer.append(&frame).unwrap();
        }
        writer.seal().unwrap();
    }

    #[test]
    fn pins_do_not_stop_the_host_or_take_the_maintenance_lease() {
        let paths = temp_paths("pins");
        let pin_args = Args {
            version: Some("0.1.1".into()),
            reason: Some("rollback".into()),
            by: Some("test".into()),
            ..Default::default()
        };
        assert_eq!(cmd_pin(&paths, &pin_args).unwrap(), EXIT_OK);
        assert_eq!(cmd_pin(&paths, &pin_args).unwrap(), EXIT_OK, "idempotent");
        let pins = PinStore::new(&paths.base).list().unwrap();
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].version, "0.1.1");
        assert_eq!(pins[0].reason, "rollback");
        assert_eq!(pins[0].created_by, "test");
        assert!(
            !paths.root.join(MAINTENANCE_LOCK_FILE).exists(),
            "pins must not take the maintenance lease"
        );
        assert!(
            !paths.base.join(AUDIT_FILE).exists(),
            "pins are metadata, not a prune/backup audit action"
        );
        assert_eq!(cmd_pins(&paths, &Args::default()).unwrap(), EXIT_OK);

        let unpin = Args {
            version: Some("0.1.1".into()),
            ..Default::default()
        };
        assert_eq!(cmd_unpin(&paths, &unpin).unwrap(), EXIT_OK);
        assert!(PinStore::new(&paths.base).list().unwrap().is_empty());
        assert!(
            matches!(cmd_unpin(&paths, &unpin), Err(CmdError::Unsafe(_))),
            "unpinning a missing version is a typed unsafe result"
        );

        // Exactly one selector is required.
        assert!(matches!(
            cmd_unpin(&paths, &Args::default()),
            Err(CmdError::Usage(_))
        ));
        let both = Args {
            version: Some("0.1.1".into()),
            id: Some("x".into()),
            ..Default::default()
        };
        assert!(matches!(
            cmd_unpin(&paths, &both),
            Err(CmdError::Usage(_))
        ));
    }

    #[test]
    fn corrupt_pins_file_is_reported_and_fails_closed_for_runtime_prune() {
        let paths = temp_paths("pins-corrupt");
        std::fs::write(paths.base.join(PINS_FILE), b"{broken").unwrap();
        assert_eq!(
            cmd_pins(&paths, &Args::default()).unwrap(),
            EXIT_IO,
            "listing must report the error instead of crashing"
        );
        assert!(
            matches!(
                cmd_pin(
                    &paths,
                    &Args {
                        version: Some("0.1.1".into()),
                        ..Default::default()
                    }
                ),
                Err(CmdError::Unsafe(_))
            ),
            "a corrupt pins file must never be overwritten"
        );

        let old = paths.base.join("runtime").join("0.1.1");
        let active = paths.base.join("runtime").join("0.1.2");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&active).unwrap();
        std::fs::write(old.join("host.exe"), b"old").unwrap();
        std::fs::write(active.join("host.exe"), b"active").unwrap();
        std::fs::write(paths.base.join("active.json"), br#"{"version":"0.1.2"}"#).unwrap();
        let cfg = MaintenanceConfig {
            runtime_keep: 0,
            prune_runtimes: true,
            ..Default::default()
        };
        match prune_plan(&paths, &cfg, true) {
            Err(CmdError::Unsafe(message)) => {
                assert!(message.contains("pins"), "{message}");
                assert!(message.contains("unpinned"), "{message}");
            }
            Err(other) => panic!("expected Unsafe, got {other:?}"),
            Ok(_) => panic!("runtime pruning must fail closed on a corrupt pins file"),
        }
    }

    #[test]
    fn prune_never_plans_a_pinned_runtime_and_says_why() {
        let paths = temp_paths("prune-pinned");
        let old = paths.base.join("runtime").join("0.1.1");
        let active = paths.base.join("runtime").join("0.1.2");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&active).unwrap();
        std::fs::write(old.join("host.exe"), b"old").unwrap();
        std::fs::write(active.join("host.exe"), b"active").unwrap();
        std::fs::write(paths.base.join("active.json"), br#"{"version":"0.1.2"}"#).unwrap();
        PinStore::new(&paths.base)
            .pin("0.1.1", "keep rollback", "test")
            .unwrap();
        let cfg = MaintenanceConfig {
            runtime_keep: 0,
            prune_runtimes: true,
            ..Default::default()
        };
        let plan = prune_plan(&paths, &cfg, true).unwrap();
        assert!(
            plan.items.iter().all(|item| !item.path.ends_with("0.1.1")),
            "a pinned runtime must never be planned"
        );
        assert_eq!(plan.pinned.len(), 1);
        assert_eq!(plan.pinned[0].version, "0.1.1");
        assert_eq!(plan.pinned[0].reason, "keep rollback");

        // Unpin -> the same superseded version becomes eligible again.
        PinStore::new(&paths.base)
            .unpin_version("0.1.1")
            .unwrap();
        let plan = prune_plan(&paths, &cfg, true).unwrap();
        assert!(plan.items.iter().any(|item| item.path.ends_with("0.1.1")));
        assert!(plan.pinned.is_empty());
    }

    #[test]
    fn compress_refuses_without_ack_and_only_touches_sealed_segments() {
        let paths = temp_paths("compress");
        let sealed = paths
            .root
            .join("ledger")
            .join("sealed")
            .join("2026")
            .join("11")
            .join("25")
            .join("old.segment");
        std::fs::create_dir_all(sealed.parent().unwrap()).unwrap();
        append_plain_segment(&sealed, 32);
        age_path(&sealed, 2);
        let active = paths.root.join("ledger").join("active").join("active.segment");
        std::fs::create_dir_all(active.parent().unwrap()).unwrap();
        append_plain_segment(&active, 3);
        let active_before = std::fs::read(&active).unwrap();

        let mut args = Args {
            sealed: true,
            older_than: Some(0),
            ..Default::default()
        };
        match cmd_compress(&paths, &args) {
            Err(CmdError::Unsafe(message)) => {
                assert!(message.contains("--allow-incompatible-readers"), "{message}");
                assert!(message.contains("0.1.1"), "{message}");
            }
            Err(other) => panic!("expected Unsafe, got {other:?}"),
            Ok(_) => panic!("compression without the acknowledgement must be refused"),
        }
        let head = std::fs::read(&sealed).unwrap();
        assert_ne!(
            &head[..4],
            &nai_lattice_ledger::compression::ENVELOPE_MAGIC[..],
            "a refusal must not modify the segment"
        );

        args.allow_incompatible_readers = true;
        assert_eq!(cmd_compress(&paths, &args).unwrap(), EXIT_OK);
        let head = std::fs::read(&sealed).unwrap();
        assert_ne!(
            &head[..4],
            &nai_lattice_ledger::compression::ENVELOPE_MAGIC[..],
            "a dry run must not compress"
        );
        assert!(
            !paths.root.join(MAINTENANCE_LOCK_FILE).exists(),
            "compress must not take the maintenance lease"
        );

        args.execute = true;
        assert_eq!(cmd_compress(&paths, &args).unwrap(), EXIT_OK);
        let compressed = std::fs::read(&sealed).unwrap();
        assert_eq!(
            &compressed[..4],
            &nai_lattice_ledger::compression::ENVELOPE_MAGIC[..],
            "the sealed segment must be a NAIZ envelope"
        );
        assert_eq!(
            std::fs::read(&active).unwrap(),
            active_before,
            "the active segment must never be touched"
        );
        let (frames, _tail, hwm, _truncate) =
            nai_lattice_ledger::segment::recover(&active).unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(hwm, 3);
        let (sealed_frames, _tail, sealed_hwm, _trunc) =
            nai_lattice_ledger::segment::recover(&sealed).unwrap();
        assert_eq!(sealed_frames.len(), 32);
        assert_eq!(sealed_hwm, 32);

        // A second run skips the already-compressed file.
        assert_eq!(cmd_compress(&paths, &args).unwrap(), EXIT_OK);
        let audit = std::fs::read_to_string(paths.base.join(AUDIT_FILE)).unwrap();
        let last: serde_json::Value = serde_json::from_str(audit.lines().last().unwrap()).unwrap();
        assert_eq!(last["action"], "compress");
        assert_eq!(last["result"], "ok");
        assert_eq!(last["detail"]["compressed"], 1);
        assert_eq!(last["detail"]["failed"], 0);
    }

    #[test]
    fn compress_requires_sealed_and_older_than() {
        let paths = temp_paths("compress-args");
        assert!(matches!(
            cmd_compress(&paths, &Args::default()),
            Err(CmdError::Usage(_))
        ));
        assert!(matches!(
            cmd_compress(
                &paths,
                &Args {
                    sealed: true,
                    allow_incompatible_readers: true,
                    ..Default::default()
                }
            ),
            Err(CmdError::Usage(_))
        ));
    }

    #[test]
    fn purge_and_restore_preserve_the_live_maintenance_lease() {
        let paths = temp_paths("lease");
        // Purge planning never targets the lease, even when one already exists.
        std::fs::write(paths.root.join(MAINTENANCE_LOCK_FILE), b"live-lease").unwrap();
        std::fs::write(paths.root.join("data.bin"), b"old").unwrap();
        let targets = scope_targets(&paths, Scope::All);
        assert!(targets.iter().all(|p| !is_lease_entry(p)));
        assert!(targets
            .iter()
            .any(|p| p.file_name().map(|n| n == "data.bin").unwrap_or(false)));

        // Restore keeps the live lease and never copies the backup's snapshot
        // over it.
        let from = paths.base.join("backup-full");
        std::fs::create_dir_all(from.join("data")).unwrap();
        std::fs::write(
            from.join("data").join(MAINTENANCE_LOCK_FILE),
            b"backup-lease",
        )
        .unwrap();
        std::fs::write(from.join("data").join("restored.bin"), b"new").unwrap();
        let manifest = serde_json::json!({
            "schema": 1,
            "scope": "all",
            "files": 2,
            "bytes": (b"backup-lease".len() + b"new".len()) as u64,
        });
        std::fs::write(
            from.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let plan = validate_restore(&paths, &from).unwrap();
        restore_backup(&paths, &plan).unwrap();
        assert_eq!(
            std::fs::read(paths.root.join(MAINTENANCE_LOCK_FILE)).unwrap(),
            b"live-lease"
        );
        assert_eq!(
            std::fs::read(paths.root.join("restored.bin")).unwrap(),
            b"new"
        );
        assert!(!paths.root.join("data.bin").exists());
        let _ = std::fs::remove_dir_all(&paths.base);
    }
}
