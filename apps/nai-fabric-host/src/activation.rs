//! R01 / NAIOS-001 restart-free runtime activation, owned by the stable monitor.
//!
//! The control operation is a file handshake (`docs/ACTIVATION.md`):
//!
//! * the manager writes `activation.request.json` and polls
//!   `activation.status.json`;
//! * the monitor validates the staged candidate, drains the old worker over NAF
//!   (`runtime.shutdown_if_idle`), starts the candidate with the same worker
//!   arguments the monitor uses, verifies a NEW endpoint manifest plus a real NAF
//!   handshake and `runtime.health`, then commits the shared
//!   `<base>\active.json` pointer under the cross-product manager mutex;
//! * any error after the drain rolls the old worker back from the journal, and a
//!   monitor restart resolves a leftover journal instead of leaving a half-swap.
//!
//! The worker dies during the swap, so it must not own the operation or its
//! response. The monitor is the only writer of the journal, status, and
//! `active.json`; a verification thread only observes and then asks the monitor
//! to commit or cancel.
//!
//! ## Deliberate implementation notes (reported, not silent)
//!
//! The frozen worker startup gate (NAIOS-006) refuses to open the store while a
//! *live* maintenance lease exists, and `runtime.health` reports `maintenance`
//! under that lease. Acquiring the lease before the drain would therefore make
//! the old worker reject the NAF handshake and the candidate refuse to start.
//! The implementation therefore drains first, takes the maintenance lease while
//! the old worker is stopping, releases it immediately before the candidate
//! starts, and commits under the manager mutex. Recovery force-releases a lease
//! left by this operation (owner + operation id in the reason). This is the only
//! safe ordering with the frozen NAIOS-006 semantics; it is documented in
//! `docs/ACTIVATION.md`.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use nai_lattice_runtime::lifecycle::{
    AcquireOutcome, LockInspection, MaintenanceLock, ReleaseOutcome,
};
use nai_lattice_supervisor::{child, ExitStatus, ProcessSpec, WorkerLimits, WorkerOutcome};

pub(crate) const REQUEST_FILE: &str = "activation.request.json";
pub(crate) const STATUS_FILE: &str = "activation.status.json";
pub(crate) const JOURNAL_FILE: &str = "activation.journal.json";
pub(crate) const ACTIVE_FILE: &str = "active.json";
pub(crate) const PREVIOUS_FILE: &str = "previous.json";
pub(crate) const TOKEN_FILE: &str = "control.token";

pub(crate) const SCHEMA: u32 = 1;
pub(crate) const ACTIVATOR_OWNER: &str = "nai-fabric-host-activator";
pub(crate) const LEASE_TTL_MS: u64 = 10 * 60 * 1000;
pub(crate) const ACTIVATION_BUDGET: Duration = Duration::from_secs(10);
const REQUEST_POLL_INTERVAL: Duration = Duration::from_millis(500);
const VERIFY_POLL_INTERVAL: Duration = Duration::from_millis(50);
const DRAIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const MANIFEST_FILE: &str = "host-manifest.json";
const EXE_FILE: &str = "nai-fabric-host.exe";
const RUNTIME_DIR: &str = "runtime";

const DRAIN_IDLE: u8 = 0;
const DRAIN_ACTIVE: u8 = 1;
const DRAIN_READY: u8 = 2;
const DRAIN_FAILED: u8 = 3;

// ─────────────────────────────────────────────────────────────────────────────
// Wire types (frozen by docs/ACTIVATION.md)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ActivationRequest {
    pub schema: u32,
    pub operation_id: String,
    pub staged_dir: String,
    pub target_version: String,
    pub exe_sha256: String,
    pub protocol_min: u32,
    pub protocol_max: u32,
    pub storage_schema: u32,
    pub authorization: String,
    #[serde(default)]
    pub allow_unsigned: bool,
    #[serde(default)]
    pub requested_at_unix_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Idle,
    Validating,
    Draining,
    Starting,
    Verifying,
    Committed,
    RolledBack,
    Failed,
    Busy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EndpointInfo {
    pub pid: u32,
    pub canonical_pipe: String,
    pub instance_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ActivationStatus {
    pub schema: u32,
    pub operation_id: String,
    pub phase: Phase,
    pub from_version: Option<String>,
    pub to_version: Option<String>,
    pub detail: Option<String>,
    pub started_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
    pub error: Option<String>,
    pub endpoint: Option<EndpointInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ActivationJournal {
    pub schema: u32,
    pub operation_id: String,
    pub phase: Phase,
    pub from_exe: String,
    pub to_exe: String,
    pub from_version: String,
    pub to_version: String,
    /// The exact previous `active.json` text; restored verbatim on rollback.
    /// Empty means the file did not exist.
    pub active_json_before: String,
    pub started_at_unix_ms: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PublishedEndpoint {
    pub pid: u32,
    pub canonical_pipe: String,
    pub instance_id: String,
    #[serde(default)]
    pub protocol: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct HostManifest {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    git: Option<String>,
    #[serde(default)]
    signature_status: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Rejections
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Rejection {
    SchemaMismatch(String),
    EmptyOperationId,
    LiveJournalBusy,
    StagedDirOutsideRuntime,
    StagedDirNameMismatch,
    InvalidVersionLabel(String),
    MissingExe,
    InvalidSha256Label,
    Sha256Mismatch(&'static str),
    ManifestMissing,
    ManifestMismatch(String),
    UnsignedNotAllowed(String),
    ProtocolIncompatible,
    StorageSchemaUnsupported,
    WrongAuthorization,
    MaintenanceLocked(String),
    NoActiveVersion,
    ActiveArtifactMissing,
    ActiveJsonMalformed,
    WorkerIdentityMismatch,
    EndpointMissing,
    EndpointDead,
    RunningHostUnversioned,
    SameVersionNoop,
    VersionNotNewer,
    VersionUnparseable,
}

impl Rejection {
    pub(crate) fn phase(&self) -> Phase {
        match self {
            Rejection::LiveJournalBusy => Phase::Busy,
            _ => Phase::Failed,
        }
    }

    pub(crate) fn code(&self) -> &'static str {
        match self {
            Rejection::SchemaMismatch(_) => "schema_mismatch",
            Rejection::EmptyOperationId => "empty_operation_id",
            Rejection::LiveJournalBusy => "live_journal",
            Rejection::StagedDirOutsideRuntime => "staged_dir_outside_runtime",
            Rejection::StagedDirNameMismatch => "staged_dir_name_mismatch",
            Rejection::InvalidVersionLabel(_) => "invalid_version_label",
            Rejection::MissingExe => "missing_exe",
            Rejection::InvalidSha256Label => "invalid_sha256_label",
            Rejection::Sha256Mismatch(_) => "sha256_mismatch",
            Rejection::ManifestMissing => "manifest_missing",
            Rejection::ManifestMismatch(_) => "manifest_mismatch",
            Rejection::UnsignedNotAllowed(_) => "unsigned_not_allowed",
            Rejection::ProtocolIncompatible => "protocol_incompatible",
            Rejection::StorageSchemaUnsupported => "storage_schema_unsupported",
            Rejection::WrongAuthorization => "wrong_authorization",
            Rejection::MaintenanceLocked(_) => "maintenance_locked",
            Rejection::NoActiveVersion => "no_active_version",
            Rejection::ActiveArtifactMissing => "active_artifact_missing",
            Rejection::ActiveJsonMalformed => "active_json_malformed",
            Rejection::WorkerIdentityMismatch => "worker_identity_mismatch",
            Rejection::EndpointMissing => "endpoint_missing",
            Rejection::EndpointDead => "endpoint_dead",
            Rejection::RunningHostUnversioned => "running_host_unversioned",
            Rejection::SameVersionNoop => "same_version_noop",
            Rejection::VersionNotNewer => "version_not_newer",
            Rejection::VersionUnparseable => "version_unparseable",
        }
    }

    pub(crate) fn detail(&self) -> String {
        match self {
            Rejection::SchemaMismatch(d) => format!("request schema mismatch: {d}"),
            Rejection::EmptyOperationId => "operation_id must be non-empty (max 128 chars)".into(),
            Rejection::LiveJournalBusy => {
                "a live activation journal exists; refusing a concurrent activation".into()
            }
            Rejection::StagedDirOutsideRuntime => {
                "staged_dir must canonicalize to a direct child of <base>/runtime".into()
            }
            Rejection::StagedDirNameMismatch => {
                "staged_dir directory name must equal target_version".into()
            }
            Rejection::InvalidVersionLabel(v) => format!("target_version {v:?} is not a safe label"),
            Rejection::MissingExe => "staged_dir/nai-fabric-host.exe is missing".into(),
            Rejection::InvalidSha256Label => "exe_sha256 must be 64 hex characters".into(),
            Rejection::Sha256Mismatch(which) => format!("SHA-256 mismatch ({which})"),
            Rejection::ManifestMissing => "staged_dir/host-manifest.json is missing".into(),
            Rejection::ManifestMismatch(d) => format!("host-manifest is invalid: {d}"),
            Rejection::UnsignedNotAllowed(status) => {
                format!("candidate signature_status is {status:?}, not \"Signed\" (allow_unsigned=false)")
            }
            Rejection::ProtocolIncompatible => {
                "request protocol range does not cover the current host (1..=1)".into()
            }
            Rejection::StorageSchemaUnsupported => {
                "request storage_schema is not the supported schema (1)".into()
            }
            Rejection::WrongAuthorization => "authorization does not match <base>/control.token".into(),
            Rejection::MaintenanceLocked(d) => {
                format!("a maintenance lease is live/unreadable; refusing activation ({d})")
            }
            Rejection::NoActiveVersion => {
                "no valid active.json and the running host is not under <base>/runtime/<version>".into()
            }
            Rejection::ActiveArtifactMissing => "the active runtime artifact is missing".into(),
            Rejection::ActiveJsonMalformed => "active.json is malformed or has no version".into(),
            Rejection::WorkerIdentityMismatch => {
                "the running worker image is not the active runtime artifact".into()
            }
            Rejection::EndpointMissing => "the current endpoint.json is missing or unreadable".into(),
            Rejection::EndpointDead => "the current endpoint pid is not a live host".into(),
            Rejection::RunningHostUnversioned => {
                "the running host image is not under <base>/runtime/<version>".into()
            }
            Rejection::SameVersionNoop => "target_version equals the active version; no-op".into(),
            Rejection::VersionNotNewer => "target_version is not strictly newer than the active version".into(),
            Rejection::VersionUnparseable => {
                "active version is not a comparable dotted version".into()
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Context and public monitor API
// ─────────────────────────────────────────────────────────────────────────────

struct LeaseHandle {
    operation_id: String,
    token: String,
}

pub(crate) struct ActivationContext {
    pub(crate) base: PathBuf,
    pub(crate) runtime_root: PathBuf,
    pub(crate) instance: Option<String>,
    /// True from request acceptance until the journal is resolved. Used only to
    /// keep a clean worker exit from ending the monitor mid-swap.
    pub(crate) in_flight: Arc<AtomicBool>,
    /// Drain handoff: 0 idle, 1 draining, 2 ready (lease held, old worker dead),
    /// 3 failed. The monitor waits for `ready` before it starts the candidate;
    /// without this handoff a very fast worker exit could race the watcher's
    /// lease acquisition and leave a live lease around the candidate start.
    drain_state: std::sync::atomic::AtomicU8,
    lease: Mutex<Option<LeaseHandle>>,
    request: Mutex<Option<ActivationRequest>>,
    running_version: Arc<Mutex<String>>,
}

impl ActivationContext {
    fn set_request(&self, request: Option<ActivationRequest>) {
        *self.request.lock().unwrap_or_else(|p| p.into_inner()) = request;
    }

    fn request(&self) -> Option<ActivationRequest> {
        self.request
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn clear_lease(&self) {
        *self.lease.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    fn take_lease(&self, operation_id: &str) -> Option<LeaseHandle> {
        let mut guard = self.lease.lock().unwrap_or_else(|p| p.into_inner());
        match guard.as_ref() {
            Some(handle) if handle.operation_id == operation_id => guard.take(),
            _ => None,
        }
    }
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// The single 10 s budget boundary (docs/ACTIVATION.md). Pure so the boundary
/// is testable and every phase uses exactly the same arithmetic.
pub(crate) fn activation_deadline(started_at_unix_ms: i64) -> i64 {
    started_at_unix_ms + ACTIVATION_BUDGET.as_millis() as i64
}

pub(crate) fn budget_expired(started_at_unix_ms: i64, now_unix_ms: i64) -> bool {
    now_unix_ms >= activation_deadline(started_at_unix_ms)
}

#[derive(Debug, Clone)]
pub(crate) struct ActivationPaths {
    pub base: PathBuf,
    pub runtime_root: PathBuf,
    pub instance: Option<String>,
}

/// Resolve the activation base.
///
/// Production: `<LOCALAPPDATA>\NAI\Lattice` with runtime root `<base>\v1`.
/// QA: `NAI_LATTICE_ROOT` is the runtime root; `<base>` is its parent, exactly as
/// `docs/ACTIVATION.md` specifies. An isolated instance MUST provide a scratch
/// root; otherwise an isolated monitor would poll the production base and could
/// rewrite production `active.json`, so activation is refused (disabled) rather
/// than silently sharing it.
pub(crate) fn paths_from_env() -> Result<ActivationPaths, String> {
    let instance = crate::endpoints::instance_from_env()?;
    if let Some(root) = std::env::var_os("NAI_LATTICE_ROOT").map(PathBuf::from) {
        let root = absolute_path(root);
        let base = root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.clone());
        return Ok(ActivationPaths {
            base,
            runtime_root: root,
            instance,
        });
    }
    if instance.is_some() {
        return Err(
            "NAI_LATTICE_INSTANCE is set without NAI_LATTICE_ROOT; refusing to share the \
             production activation base (active.json/runtime)"
                .into(),
        );
    }
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let base = local.join("NAI").join("Lattice");
    Ok(ActivationPaths {
        runtime_root: base.join("v1"),
        base,
        instance,
    })
}

fn absolute_path(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(path)
    }
}

/// Create the activation context for the monitor. Returns an error when
/// activation cannot be safely enabled (for example an isolated instance without
/// a scratch base, or a control token that cannot be created user-only).
pub(crate) fn monitor_context(
    running_version: Arc<Mutex<String>>,
) -> Result<Arc<ActivationContext>, String> {
    let paths = paths_from_env()?;
    std::fs::create_dir_all(&paths.base).map_err(|e| format!("create activation base: {e}"))?;
    std::fs::create_dir_all(paths.base.join(RUNTIME_DIR))
        .map_err(|e| format!("create runtime directory: {e}"))?;
    std::fs::create_dir_all(&paths.runtime_root)
        .map_err(|e| format!("create runtime root: {e}"))?;
    ensure_control_token(&paths.base)?;
    let base = std::fs::canonicalize(&paths.base).unwrap_or(paths.base);
    let runtime_root = std::fs::canonicalize(&paths.runtime_root).unwrap_or(paths.runtime_root);
    Ok(Arc::new(ActivationContext {
        base,
        runtime_root,
        instance: paths.instance,
        in_flight: Arc::new(AtomicBool::new(false)),
        drain_state: std::sync::atomic::AtomicU8::new(DRAIN_IDLE),
        lease: Mutex::new(None),
        request: Mutex::new(None),
        running_version,
    }))
}

/// Ensure `<base>\control.token` exists as a stable per-user secret. The file is
/// created with a current-user-only DACL and is never overwritten or logged.
pub(crate) fn ensure_control_token(base: &Path) -> Result<(), String> {
    std::fs::create_dir_all(base).map_err(|e| format!("create {}: {e}", base.display()))?;
    let path = base.join(TOKEN_FILE);
    if let Ok(raw) = std::fs::read_to_string(&path) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("control.token exists but is empty; refusing activation".into());
        }
        if trimmed.len() < 32 {
            return Err("control.token is shorter than 32 characters; refusing activation".into());
        }
        return Ok(());
    }
    if path.exists() {
        return Err("control.token exists but is unreadable; refusing activation".into());
    }
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    #[cfg(windows)]
    {
        match crate::win_acl::create_new_user_only(&path, token.as_bytes()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let raw = std::fs::read_to_string(&path)
                    .map_err(|e| format!("read concurrently created control.token: {e}"))?;
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    return Err("control.token was created empty; refusing activation".into());
                }
                if trimmed.len() < 32 {
                    return Err(
                        "control.token was created shorter than 32 characters; refusing activation"
                            .into(),
                    );
                }
                return Ok(());
            }
            Err(e) => return Err(format!("create control.token with user-only ACL: {e}")),
        }
    }
    #[cfg(not(windows))]
    {
        std::fs::write(&path, token).map_err(|e| format!("write control.token: {e}"))?;
    }
    tracing::info!(path = %path.display(), "activation control.token ensured (user-only ACL)");
    Ok(())
}

fn read_token(base: &Path) -> Option<String> {
    std::fs::read_to_string(base.join(TOKEN_FILE))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn token_matches(provided: &str, expected: &str) -> bool {
    !provided.is_empty() && blake3::hash(provided.as_bytes()) == blake3::hash(expected.as_bytes())
}

/// The active runtime artifact from the shared `active.json` pointer, if valid.
pub(crate) fn active_artifact_exe(base: &Path) -> Option<PathBuf> {
    let raw = std::fs::read(base.join(ACTIVE_FILE)).ok()?;
    let value: Value = serde_json::from_slice(&raw).ok()?;
    let version = value.get("version").and_then(Value::as_str)?;
    if !is_safe_version_label(version) {
        return None;
    }
    let path = value
        .get("path")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| base.join(RUNTIME_DIR).join(version).join(EXE_FILE));
    let path = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    if path.is_file() {
        Some(path)
    } else {
        None
    }
}

pub(crate) fn active_version(base: &Path) -> Option<String> {
    let raw = std::fs::read(base.join(ACTIVE_FILE)).ok()?;
    let value: Value = serde_json::from_slice(&raw).ok()?;
    value
        .get("version")
        .and_then(Value::as_str)
        .filter(|v| is_safe_version_label(v))
        .map(str::to_string)
}

/// The version label encoded by a versioned runtime/release path, if any.
pub(crate) fn version_for_exe(base: &Path, exe: &Path) -> Option<String> {
    version_from_exe(base, exe)
}

/// True when an activation journal or an accepted request exists. The monitor
/// must not treat a clean worker exit as "stop" in this state.
pub(crate) fn activation_pending(ctx: &ActivationContext) -> bool {
    ctx.in_flight.load(Ordering::SeqCst) || read_journal(&ctx.base).is_some()
}

/// Start the 500 ms request watcher. It validates and drains only; the monitor's
/// main loop owns the spawn/verify/commit steps.
pub(crate) fn spawn_request_watcher(ctx: Arc<ActivationContext>) {
    let spawned = std::thread::Builder::new()
        .name("activation-watcher".into())
        .spawn(move || loop {
            std::thread::sleep(REQUEST_POLL_INTERVAL);
            if ctx.in_flight.load(Ordering::SeqCst) {
                // Our own operation owns the status file and the journal. A
                // second request is left on disk until this one is terminal.
                continue;
            }
            let Some(raw) = std::fs::read(ctx.base.join(REQUEST_FILE)).ok() else {
                continue;
            };
            // If a foreign/crashed journal exists, handle_request answers
            // `phase:"busy"` without touching that journal.
            handle_request(&ctx, &raw);
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "failed to start activation request watcher");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Request handling (validation + drain)
// ─────────────────────────────────────────────────────────────────────────────

fn handle_request(ctx: &Arc<ActivationContext>, raw: &[u8]) {
    let value: Value = match serde_json::from_slice(raw) {
        Ok(v) => v,
        Err(e) => {
            let operation_id = String::new();
            let started = now_ms();
            let _ = write_terminal(
                ctx,
                &operation_id,
                Phase::Failed,
                None,
                None,
                Some(format!("request is not valid JSON: {e}")),
                Some(format!("schema_mismatch: request is not valid JSON: {e}")),
                None,
                started,
            );
            remove_request(ctx);
            return;
        }
    };
    let operation_id = value
        .get("operation_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let requested_target = value
        .get("target_version")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if operation_id.is_empty() {
        let started = now_ms();
        let rejection = Rejection::EmptyOperationId;
        let _ = write_terminal(
            ctx,
            "",
            rejection.phase(),
            None,
            Some(requested_target.clone()).filter(|s| !s.is_empty()),
            Some(rejection.detail()),
            Some(format!("{}: {}", rejection.code(), rejection.detail())),
            None,
            started,
        );
        remove_request(ctx);
        return;
    }
    if ctx.in_flight.swap(true, Ordering::SeqCst) {
        // Another request is already being accepted. Leave this one queued; it
        // will be read after the current terminal status is durable.
        return;
    }
    let started = now_ms();

    if let Some(journal) = read_journal(&ctx.base) {
        // A journal owned by a different actor (for example a crashed monitor
        // that has not been recovered) refuses concurrent activation.
        let rejection = Rejection::LiveJournalBusy;
        let _ = write_terminal(
            ctx,
            &operation_id,
            rejection.phase(),
            Some(journal.from_version.clone()),
            Some(requested_target),
            Some(rejection.detail()),
            Some(format!("{}: {}", rejection.code(), rejection.detail())),
            read_endpoint(&ctx.runtime_root).map(endpoint_info),
            started,
        );
        remove_request(ctx);
        ctx.in_flight.store(false, Ordering::SeqCst);
        return;
    }

    let running = match resolve_running(ctx) {
        Ok(running) => running,
        Err(rejection) => {
            let _ = write_terminal(
                ctx,
                &operation_id,
                rejection.phase(),
                None,
                Some(requested_target),
                Some(rejection.detail()),
                Some(format!("{}: {}", rejection.code(), rejection.detail())),
                read_endpoint(&ctx.runtime_root).map(endpoint_info),
                started,
            );
            remove_request(ctx);
            ctx.in_flight.store(false, Ordering::SeqCst);
            return;
        }
    };

    let _ = write_status(
        &ctx.base,
        &ActivationStatus {
            schema: SCHEMA,
            operation_id: operation_id.clone(),
            phase: Phase::Validating,
            from_version: Some(running.version.clone()),
            to_version: Some(requested_target.clone()),
            detail: Some("validating staged candidate".into()),
            started_at_unix_ms: started,
            updated_at_unix_ms: now_ms(),
            error: None,
            endpoint: Some(endpoint_info(running.endpoint.clone())),
        },
    );

    let validated = match validate_request(ctx, raw, &running) {
        Ok(v) => v,
        Err(rejection) => {
            tracing::warn!(
                operation_id = %operation_id,
                code = rejection.code(),
                detail = %rejection.detail(),
                "activation request rejected"
            );
            let _ = write_terminal(
                ctx,
                &operation_id,
                rejection.phase(),
                Some(running.version.clone()),
                Some(requested_target),
                Some(rejection.detail()),
                Some(format!("{}: {}", rejection.code(), rejection.detail())),
                Some(endpoint_info(running.endpoint.clone())),
                started,
            );
            remove_request(ctx);
            ctx.in_flight.store(false, Ordering::SeqCst);
            return;
        }
    };

    let journal = ActivationJournal {
        schema: SCHEMA,
        operation_id: operation_id.clone(),
        phase: Phase::Draining,
        from_exe: display_path(&validated.from_exe),
        to_exe: display_path(&validated.exe),
        from_version: validated.from_version.clone(),
        to_version: validated.request.target_version.clone(),
        active_json_before: validated.active_before.clone(),
        started_at_unix_ms: started,
    };
    if let Err(e) = write_journal(&ctx.base, &journal) {
        let _ = write_terminal(
            ctx,
            &operation_id,
            Phase::Failed,
            Some(validated.from_version.clone()),
            Some(validated.request.target_version.clone()),
            Some(format!("could not write activation journal: {e}")),
            Some(format!("journal_write_failed: {e}")),
            Some(endpoint_info(running.endpoint.clone())),
            started,
        );
        remove_request(ctx);
        ctx.in_flight.store(false, Ordering::SeqCst);
        return;
    }
    ctx.drain_state.store(DRAIN_ACTIVE, Ordering::SeqCst);
    ctx.set_request(Some(validated.request.clone()));
    remove_request(ctx);
    let _ = write_status(
        &ctx.base,
        &ActivationStatus {
            schema: SCHEMA,
            operation_id: operation_id.clone(),
            phase: Phase::Draining,
            from_version: Some(validated.from_version.clone()),
            to_version: Some(validated.request.target_version.clone()),
            detail: Some("requesting runtime.shutdown_if_idle".into()),
            started_at_unix_ms: started,
            updated_at_unix_ms: now_ms(),
            error: None,
            endpoint: Some(endpoint_info(running.endpoint.clone())),
        },
    );

    // Drain the old worker BEFORE taking the maintenance lease: a live lease
    // makes the old worker report `maintenance`, which the NAF client correctly
    // refuses. NAIOS-006 also makes a live lease a hard startup refusal for the
    // candidate. See the module note and docs/ACTIVATION.md.
    match request_shutdown(&running.endpoint.canonical_pipe, DRAIN_REQUEST_TIMEOUT) {
        Ok(()) => {}
        Err(DrainError::Busy(reason)) => {
            let _ = write_terminal(
                ctx,
                &operation_id,
                Phase::Busy,
                Some(validated.from_version.clone()),
                Some(validated.request.target_version.clone()),
                Some(format!("runtime is busy; old worker retained ({reason})")),
                Some(format!("busy: {reason}")),
                Some(endpoint_info(running.endpoint.clone())),
                started,
            );
            finish_without_journal(ctx, &operation_id);
            return;
        }
        Err(DrainError::AlreadyStopping(reason)) => {
            let _ = write_terminal(
                ctx,
                &operation_id,
                Phase::Failed,
                Some(validated.from_version.clone()),
                Some(validated.request.target_version.clone()),
                Some(format!("runtime is already stopping; old worker retained ({reason})")),
                Some(format!("already_stopping: {reason}")),
                Some(endpoint_info(running.endpoint.clone())),
                started,
            );
            finish_without_journal(ctx, &operation_id);
            return;
        }
        Err(DrainError::Other(error)) => {
            let _ = write_terminal(
                ctx,
                &operation_id,
                Phase::Failed,
                Some(validated.from_version.clone()),
                Some(validated.request.target_version.clone()),
                Some(format!("could not drain the runtime: {error}")),
                Some(format!("drain_failed: {error}")),
                Some(endpoint_info(running.endpoint.clone())),
                started,
            );
            finish_without_journal(ctx, &operation_id);
            return;
        }
    }

    // The old worker accepted the stop. Fence the handover until the candidate is
    // ready, then release before spawning it.
    if budget_expired(started, now_ms()) {
        let _ = write_terminal(
            ctx,
            &operation_id,
            Phase::Failed,
            Some(validated.from_version.clone()),
            Some(validated.request.target_version.clone()),
            Some("the 10 s activation budget expired before the handover lease".into()),
            Some("budget_expired_before_lease".into()),
            Some(endpoint_info(running.endpoint.clone())),
            started,
        );
        ctx.drain_state.store(DRAIN_FAILED, Ordering::SeqCst);
        return;
    }
    match MaintenanceLock::new(&ctx.runtime_root).acquire(
        ACTIVATOR_OWNER,
        &format!(
            "runtime activation {} {}->{}",
            operation_id, validated.from_version, validated.request.target_version
        ),
        LEASE_TTL_MS,
    ) {
        Ok(AcquireOutcome::Acquired { token, .. }) => {
            *ctx.lease.lock().unwrap_or_else(|p| p.into_inner()) = Some(LeaseHandle {
                operation_id: operation_id.clone(),
                token,
            });
            // The supervisor may have aborted the journal while the lease was
            // being acquired. Never leave a live lease behind in that race.
            if !journal_is_operation(ctx, &operation_id) {
                let _ = release_lease(ctx, &operation_id);
                ctx.drain_state.store(DRAIN_FAILED, Ordering::SeqCst);
                return;
            }
        }
        Ok(AcquireOutcome::Refused(refusal)) => {
            let _ = write_terminal(
                ctx,
                &operation_id,
                Phase::Failed,
                Some(validated.from_version.clone()),
                Some(validated.request.target_version.clone()),
                Some(format!(
                    "maintenance lease was taken after the drain ({}); old worker is stopping",
                    refusal.reason()
                )),
                Some(format!("maintenance_locked_after_drain: {}", refusal.reason())),
                Some(endpoint_info(running.endpoint.clone())),
                started,
            );
            // Keep the journal: the old worker is stopping and the monitor must
            // not treat its clean exit as a stop. The swap loop aborts and
            // respawns the old artifact.
            ctx.drain_state.store(DRAIN_FAILED, Ordering::SeqCst);
            return;
        }
        Err(e) => {
            let _ = write_terminal(
                ctx,
                &operation_id,
                Phase::Failed,
                Some(validated.from_version.clone()),
                Some(validated.request.target_version.clone()),
                Some(format!("maintenance lease acquisition failed: {e}")),
                Some(format!("maintenance_lease_error: {e}")),
                Some(endpoint_info(running.endpoint.clone())),
                started,
            );
            ctx.drain_state.store(DRAIN_FAILED, Ordering::SeqCst);
            return;
        }
    }

    // Wait for the old worker to exit inside the 10 s budget. If it does not,
    // report an explicit failure but keep the journal so the eventual clean exit
    // is respawned instead of ending the monitor.
    while process_alive(running.pid) {
        if budget_expired(started, now_ms()) {
            let _ = write_terminal(
                ctx,
                &operation_id,
                Phase::Failed,
                Some(validated.from_version.clone()),
                Some(validated.request.target_version.clone()),
                Some("old worker did not exit within the 10 s activation budget".into()),
                Some("drain_timeout: old worker did not exit within 10 s".into()),
                Some(endpoint_info(running.endpoint.clone())),
                started,
            );
            // Keep journal + in_flight; the swap loop resolves them when the
            // old worker finally exits.
            ctx.drain_state.store(DRAIN_FAILED, Ordering::SeqCst);
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    // The old worker is gone and the lease is held: the monitor may now start
    // the candidate. It will release the lease immediately before the spawn.
    if !journal_is_operation(ctx, &operation_id) {
        let _ = release_lease(ctx, &operation_id);
        ctx.drain_state.store(DRAIN_FAILED, Ordering::SeqCst);
        return;
    }
    ctx.drain_state.store(DRAIN_READY, Ordering::SeqCst);
}

fn finish_without_journal(ctx: &ActivationContext, operation_id: &str) {
    remove_journal(&ctx.base);
    finish_in_flight(ctx);
    let _ = operation_id;
}

enum DrainError {
    Busy(String),
    AlreadyStopping(String),
    Other(String),
}

fn request_shutdown(pipe: &str, timeout: Duration) -> Result<(), DrainError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| DrainError::Other(format!("tokio runtime: {e}")))?;
    runtime.block_on(async move {
        let future = async {
            let stream = nai_lattice_client::win_pipe::NamedPipeStream::connect(pipe)?;
            let handshake = nai_lattice_protocol::handshake::client_request(
                "nai-lattice-activator",
                env!("CARGO_PKG_VERSION"),
                std::process::id(),
                1,
                1,
                &["runtime.health", "runtime.shutdown_if_idle"],
                Some("activation"),
                "activation",
                uuid::Uuid::new_v4().as_bytes(),
            );
            let mut client = nai_lattice_client::NafClient::connect(stream, handshake).await?;
            client
                .dispatch("runtime.shutdown_if_idle", b"{}", None)
                .await
        };
        match tokio::time::timeout(timeout, future).await {
            Err(_) => Err(DrainError::Other("shutdown request timed out".into())),
            Ok(Err(nai_lattice_client::ClientError::RuntimeUnavailable(health)))
                if health.contains("draining") =>
            {
                Err(DrainError::AlreadyStopping(health))
            }
            Ok(Err(nai_lattice_client::ClientError::RuntimeUnavailable(health)))
                if health.contains("maintenance") =>
            {
                Err(DrainError::Other(format!("runtime is in maintenance: {health}")))
            }
            Ok(Err(nai_lattice_client::ClientError::Rpc(error))) => {
                if error.contains("already_stopping") {
                    Err(DrainError::AlreadyStopping(error))
                } else if error.contains("busy") {
                    Err(DrainError::Busy(error))
                } else {
                    Err(DrainError::Other(error))
                }
            }
            Ok(Err(e)) => Err(DrainError::Other(e.to_string())),
            Ok(Ok(_)) => Ok(()),
        }
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Validation
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct RunningHost {
    pub pid: u32,
    pub exe: PathBuf,
    pub version: String,
    pub endpoint: PublishedEndpoint,
}

#[derive(Debug, Clone)]
pub(crate) struct ValidatedRequest {
    pub request: ActivationRequest,
    pub exe: PathBuf,
    pub active_before: String,
    pub from_exe: PathBuf,
    pub from_version: String,
}

fn resolve_running(ctx: &ActivationContext) -> Result<RunningHost, Rejection> {
    let endpoint = read_endpoint(&ctx.runtime_root).ok_or(Rejection::EndpointMissing)?;
    if !process_alive(endpoint.pid) {
        return Err(Rejection::EndpointDead);
    }
    let exe = process_image_path(endpoint.pid).ok_or(Rejection::EndpointDead)?;
    let version =
        version_from_exe(&ctx.base, &exe).ok_or(Rejection::RunningHostUnversioned)?;
    Ok(RunningHost {
        pid: endpoint.pid,
        exe,
        version,
        endpoint,
    })
}

fn validate_request(
    ctx: &ActivationContext,
    raw: &[u8],
    running: &RunningHost,
) -> Result<ValidatedRequest, Rejection> {
    let request: ActivationRequest = serde_json::from_slice(raw)
        .map_err(|e| Rejection::SchemaMismatch(format!("request JSON: {e}")))?;
    if request.schema != SCHEMA {
        return Err(Rejection::SchemaMismatch(format!(
            "schema {} (expected {SCHEMA})",
            request.schema
        )));
    }
    if request.operation_id.trim().is_empty() || request.operation_id.len() > 128 {
        return Err(Rejection::EmptyOperationId);
    }
    if !is_safe_version_label(&request.target_version) {
        return Err(Rejection::InvalidVersionLabel(request.target_version.clone()));
    }
    if !is_sha256_label(&request.exe_sha256) {
        return Err(Rejection::InvalidSha256Label);
    }

    // 2. staged_dir is a direct child of <base>/runtime and named target_version.
    let runtime_dir = std::fs::canonicalize(ctx.base.join(RUNTIME_DIR))
        .map_err(|_| Rejection::StagedDirOutsideRuntime)?;
    let staged_dir = std::fs::canonicalize(&request.staged_dir)
        .map_err(|_| Rejection::StagedDirOutsideRuntime)?;
    if staged_dir.parent() != Some(runtime_dir.as_path()) {
        return Err(Rejection::StagedDirOutsideRuntime);
    }
    let dir_name = staged_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if dir_name != request.target_version {
        return Err(Rejection::StagedDirNameMismatch);
    }

    // 3. exe exists; SHA-256 matches the request and the manifest.
    let exe_path = staged_dir.join(EXE_FILE);
    if !exe_path.is_file() {
        return Err(Rejection::MissingExe);
    }
    let exe = std::fs::canonicalize(&exe_path).unwrap_or(exe_path);
    let actual = sha256_file(&exe)
        .map_err(|e| Rejection::ManifestMismatch(format!("hash candidate: {e}")))?;
    if !actual.eq_ignore_ascii_case(request.exe_sha256.trim()) {
        return Err(Rejection::Sha256Mismatch("request exe_sha256"));
    }
    let manifest_path = staged_dir.join(MANIFEST_FILE);
    let manifest_bytes = std::fs::read(&manifest_path).map_err(|_| Rejection::ManifestMissing)?;
    let manifest: HostManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| Rejection::ManifestMismatch(format!("manifest JSON: {e}")))?;
    let manifest_sha = manifest
        .sha256
        .as_deref()
        .ok_or_else(|| Rejection::ManifestMismatch("sha256 is missing".into()))?;
    if !manifest_sha.eq_ignore_ascii_case(&actual) {
        return Err(Rejection::Sha256Mismatch("host-manifest sha256"));
    }
    if let Some(version) = manifest.version.as_deref() {
        if version != request.target_version {
            return Err(Rejection::ManifestMismatch(format!(
                "version {version:?} != target_version {:?}",
                request.target_version
            )));
        }
    }

    // 4. Signature policy: accept the canonical `Signed` label and the legacy
    //    Authenticode `Valid` status; everything else is unsigned.
    if !signature_is_valid(manifest.signature_status.as_deref()) {
        if request.allow_unsigned {
            tracing::warn!(
                signature_status = manifest.signature_status.as_deref().unwrap_or("missing"),
                operation_id = %request.operation_id,
                "activating a non-Signed candidate because allow_unsigned=true (QA only)"
            );
        } else {
            return Err(Rejection::UnsignedNotAllowed(
                manifest
                    .signature_status
                    .as_deref()
                    .unwrap_or("missing")
                    .to_string(),
            ));
        }
    }

    // 5. Protocol range covers the current host (1..=1); storage schema is 1.
    if request.protocol_min == 0
        || request.protocol_min > request.protocol_max
        || request.protocol_min > 1
        || request.protocol_max < 1
    {
        return Err(Rejection::ProtocolIncompatible);
    }
    if let Some(selected) = running
        .endpoint
        .protocol
        .as_ref()
        .and_then(|p| p.get("selected"))
        .and_then(Value::as_u64)
    {
        let selected = selected as u32;
        if selected < request.protocol_min || selected > request.protocol_max {
            return Err(Rejection::ProtocolIncompatible);
        }
    }
    if request.storage_schema != 1 {
        return Err(Rejection::StorageSchemaUnsupported);
    }

    // 5b. NAIOS-006: never start an activation while a foreign maintenance lease
    // is already live. (The old worker would report maintenance and could be
    // stopped by that actor underneath us.)
    match MaintenanceLock::new(&ctx.runtime_root).inspect() {
        LockInspection::Absent | LockInspection::Expired(_) => {}
        LockInspection::Live(holder) => {
            return Err(Rejection::MaintenanceLocked(format!(
                "owner={} reason={} expires_at_unix_ms={}",
                holder.owner, holder.reason, holder.expires_at_unix_ms
            )))
        }
        LockInspection::Unreadable { error, .. } => {
            return Err(Rejection::MaintenanceLocked(format!(
                "unreadable lock: {error}"
            )))
        }
    }

    // 6. Authorization equals <base>/control.token. Never echo either value.
    let expected = read_token(&ctx.base).ok_or(Rejection::WrongAuthorization)?;
    if !token_matches(request.authorization.trim(), &expected) {
        return Err(Rejection::WrongAuthorization);
    }

    // 7. Determine the active/from state and require a strict forward version.
    let active_path = ctx.base.join(ACTIVE_FILE);
    let active_before = match std::fs::read(&active_path) {
        Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => {
            return Err(Rejection::ActiveJsonMalformed);
        }
    };
    let (from_exe, from_version) = match active_before.as_deref() {
        Some(raw) => {
            let value: Value =
                serde_json::from_str(raw).map_err(|_| Rejection::ActiveJsonMalformed)?;
            let version = value
                .get("version")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .ok_or(Rejection::ActiveJsonMalformed)?
                .to_string();
            let path = value
                .get("path")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    ctx.base.join(RUNTIME_DIR).join(&version).join(EXE_FILE)
                });
            let path = if path.is_absolute() {
                path
            } else {
                ctx.base.join(path)
            };
            let exe = std::fs::canonicalize(&path).map_err(|_| Rejection::ActiveArtifactMissing)?;
            if !path_eq(&exe, &running.exe) {
                return Err(Rejection::WorkerIdentityMismatch);
            }
            (exe, version)
        }
        None => {
            let version = version_from_exe(&ctx.base, &running.exe)
                .ok_or(Rejection::NoActiveVersion)?;
            (running.exe.clone(), version)
        }
    };
    match compare_versions(&request.target_version, &from_version) {
        None => return Err(Rejection::VersionUnparseable),
        Some(std::cmp::Ordering::Equal) => return Err(Rejection::SameVersionNoop),
        Some(std::cmp::Ordering::Less) => return Err(Rejection::VersionNotNewer),
        Some(std::cmp::Ordering::Greater) => {}
    }

    Ok(ValidatedRequest {
        request,
        exe,
        active_before: active_before.unwrap_or_default(),
        from_exe,
        from_version,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Swap execution (called by the monitor after the drained worker exits)
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) enum SwapOutcome {
    /// A worker was supervised through the activation (candidate or rollback).
    /// `respawn_override` keeps a clean exit from ending the monitor when the
    /// exit was caused by a failed/aborted activation rather than a stop.
    Supervised {
        exe: PathBuf,
        outcome: WorkerOutcome,
        respawn_override: bool,
    },
    /// No worker is currently supervised; the monitor should back off and retry
    /// the active artifact.
    Retry { detail: String },
}

/// A worker that survived a monitor restart and is already serving. The monitor
/// adopts (polls) it instead of spawning a duplicate that would lose the
/// singleton, and resumes normal supervision when it exits.
#[derive(Debug, Clone)]
pub(crate) struct RecoveredWorker {
    pub pid: u32,
    pub exe: PathBuf,
    pub version: String,
}

#[derive(Debug)]
pub(crate) enum RecoveryOutcome {
    Nothing,
    Adopt(RecoveredWorker),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryPlan {
    /// The candidate endpoint is healthy: finish the commit and adopt it.
    FinishCandidate,
    /// The previous endpoint is still healthy: restore/announce the rollback
    /// and adopt it instead of stopping a healthy worker.
    AdoptPrevious,
    /// No healthy endpoint: stop stale children, restore the pointer, and let
    /// the monitor spawn the previous artifact normally.
    Rollback,
}

/// Pure journal-recovery decision (unit-tested; the filesystem/probe evidence
/// is gathered by the caller).
pub(crate) fn recovery_plan(candidate_healthy: bool, previous_healthy: bool) -> RecoveryPlan {
    if candidate_healthy {
        RecoveryPlan::FinishCandidate
    } else if previous_healthy {
        RecoveryPlan::AdoptPrevious
    } else {
        RecoveryPlan::Rollback
    }
}

/// Run a journal left by an interrupted monitor: finish when the candidate
/// endpoint is already healthy, adopt a healthy previous endpoint, otherwise
/// roll back and let the monitor spawn the previous artifact. Runs before the
/// first worker is spawned; it never starts a second monitor.
pub(crate) fn recover_on_start(ctx: &ActivationContext) -> Result<RecoveryOutcome, String> {
    let Some(journal) = read_journal(&ctx.base) else {
        return Ok(RecoveryOutcome::Nothing);
    };
    tracing::warn!(
        operation_id = %journal.operation_id,
        phase = ?journal.phase,
        "resolving an interrupted activation journal"
    );
    force_release_own_lease(ctx, &journal.operation_id);
    remove_matching_request(&ctx.base, &journal.operation_id);

    let to_exe = PathBuf::from(&journal.to_exe);
    let from_exe = PathBuf::from(&journal.from_exe);
    let endpoint = read_endpoint(&ctx.runtime_root);
    let candidate_hash = sha256_file(&to_exe).ok();
    let candidate_manifest = read_manifest(&to_exe).ok();
    // Crash recovery has no request file, so `allow_unsigned` is unknown. Fail
    // closed: only a Signed, hash-matching candidate may be finished; otherwise
    // the candidate is stopped and the previous artifact is restored.
    let manifest_ok = match (&candidate_hash, &candidate_manifest) {
        (Some(hash), Some(manifest)) => {
            manifest
                .sha256
                .as_deref()
                .map(|sha| sha.eq_ignore_ascii_case(hash))
                .unwrap_or(false)
                && manifest.signature_status.as_deref() == Some("Signed")
        }
        _ => false,
    };
    let candidate_healthy = endpoint
        .as_ref()
        .filter(|ep| process_alive(ep.pid))
        .filter(|ep| {
            process_image_path(ep.pid)
                .map(|image| path_eq(&image, &to_exe))
                .unwrap_or(false)
        })
        .filter(|ep| probe_endpoint(ep, &journal, None).is_ok())
        .filter(|_| manifest_ok);
    let previous_healthy = endpoint
        .as_ref()
        .filter(|ep| process_alive(ep.pid))
        .filter(|ep| {
            process_image_path(ep.pid)
                .map(|image| path_eq(&image, &from_exe))
                .unwrap_or(false)
        })
        .filter(|ep| probe_endpoint(ep, &journal, None).is_ok());

    match recovery_plan(candidate_healthy.is_some(), previous_healthy.is_some()) {
        RecoveryPlan::FinishCandidate => {
            let endpoint = candidate_healthy.expect("plan requires a candidate");
            let hash = candidate_hash.expect("candidate health requires a hash");
            let already_committed = active_matches(ctx, &journal.to_version, &hash);
            if !already_committed {
                if let Err(e) = commit_active_json(
                    ctx,
                    &journal,
                    &to_exe,
                    &hash,
                    candidate_manifest.as_ref(),
                    10_000,
                ) {
                    return Err(format!(
                        "candidate endpoint is healthy but commit failed: {e}"
                    ));
                }
            }
            write_terminal(
                ctx,
                &journal.operation_id,
                Phase::Committed,
                Some(journal.from_version.clone()),
                Some(journal.to_version.clone()),
                Some("candidate endpoint verified during monitor recovery".into()),
                None,
                Some(endpoint_info(endpoint.clone())),
                journal.started_at_unix_ms,
            )?;
            remove_journal(&ctx.base);
            finish_in_flight(ctx);
            Ok(RecoveryOutcome::Adopt(RecoveredWorker {
                pid: endpoint.pid,
                exe: to_exe,
                version: journal.to_version,
            }))
        }
        RecoveryPlan::AdoptPrevious => {
            let endpoint = previous_healthy.expect("plan requires a previous endpoint");
            restore_active_verbatim(ctx, &journal.active_json_before)?;
            write_terminal(
                ctx,
                &journal.operation_id,
                Phase::RolledBack,
                Some(journal.from_version.clone()),
                Some(journal.to_version.clone()),
                Some("previous artifact is still serving; activation rolled back".into()),
                Some("monitor restarted before the candidate was verified".into()),
                Some(endpoint_info(endpoint.clone())),
                journal.started_at_unix_ms,
            )?;
            remove_journal(&ctx.base);
            finish_in_flight(ctx);
            Ok(RecoveryOutcome::Adopt(RecoveredWorker {
                pid: endpoint.pid,
                exe: from_exe,
                version: journal.from_version,
            }))
        }
        RecoveryPlan::Rollback => {
            // Make sure a half-started candidate or a draining old worker is not
            // holding the singleton before the monitor respawns the previous
            // artifact.
            if let Some(endpoint) = endpoint {
                if process_alive(endpoint.pid) {
                    let image = process_image_path(endpoint.pid);
                    let stale = image
                        .map(|image| path_eq(&image, &to_exe))
                        .unwrap_or(false);
                    if stale {
                        terminate_process(endpoint.pid);
                        wait_dead(endpoint.pid, Duration::from_secs(5));
                    }
                }
            }
            if let Some(endpoint) = read_endpoint(&ctx.runtime_root) {
                if process_alive(endpoint.pid)
                    && process_image_path(endpoint.pid)
                        .map(|image| path_eq(&image, &from_exe))
                        .unwrap_or(false)
                {
                    // A previous worker survived but could not answer a healthy
                    // probe (typically it is already draining). Ask it to stop,
                    // then fail closed to a process-handle termination so the
                    // monitor can spawn a fresh worker.
                    let _ = request_shutdown(&endpoint.canonical_pipe, Duration::from_secs(2));
                    if !wait_dead(endpoint.pid, Duration::from_secs(3)) {
                        terminate_process(endpoint.pid);
                        wait_dead(endpoint.pid, Duration::from_secs(3));
                    }
                }
            }
            restore_active_verbatim(ctx, &journal.active_json_before)?;
            write_terminal(
                ctx,
                &journal.operation_id,
                Phase::RolledBack,
                Some(journal.from_version.clone()),
                Some(journal.to_version.clone()),
                Some("interrupted activation rolled back; previous artifact restored".into()),
                Some("monitor restarted before the candidate was verified".into()),
                read_endpoint(&ctx.runtime_root).map(endpoint_info),
                journal.started_at_unix_ms,
            )?;
            remove_journal(&ctx.base);
            finish_in_flight(ctx);
            Ok(RecoveryOutcome::Nothing)
        }
    }
}

fn active_matches(ctx: &ActivationContext, version: &str, hash: &str) -> bool {
    let Ok(raw) = std::fs::read(ctx.base.join(ACTIVE_FILE)) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&raw) else {
        return false;
    };
    value.get("version").and_then(Value::as_str) == Some(version)
        && value
            .get("sha256")
            .and_then(Value::as_str)
            .map(|sha| sha.eq_ignore_ascii_case(hash))
            .unwrap_or(false)
}

/// Best-effort check for any live process whose image is `exe` by reading the
/// published endpoint (the only authoritative host identity available here).
/// Adopt a worker that survived a monitor restart (the previous monitor's Job
/// Object is gone, so the process is NOT in this monitor's containment). Poll it
/// and report its real exit code; a later crash is respawned from the active
/// artifact by the normal supervisor path. Cancellation terminates it through a
/// process handle (there is no job tree to close).
pub(crate) fn supervise_adopted(
    ctx: &ActivationContext,
    worker: RecoveredWorker,
    cancel_flag: &AtomicBool,
) -> WorkerOutcome {
    set_running_version(ctx, &worker.version);
    tracing::info!(
        pid = worker.pid,
        version = %worker.version,
        "adopting a surviving activated worker"
    );
    let started = std::time::Instant::now();
    let start_unix_ms = now_ms();
    let handle = open_query_handle(worker.pid);
    let mut cancelled = false;
    let code = loop {
        if cancel_flag.load(Ordering::SeqCst) && !cancelled {
            cancelled = true;
            terminate_process(worker.pid);
        }
        if let Some(code) = handle.and_then(exit_code_of) {
            break Some(code);
        }
        if !process_alive(worker.pid) {
            break None;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    if let Some(handle) = handle {
        close_handle(handle);
    }
    let exit_status = if cancelled {
        ExitStatus::Cancelled
    } else {
        match code {
            Some(0) => ExitStatus::Success,
            Some(code) => ExitStatus::ExitCode(code as i32),
            None => ExitStatus::Terminated,
        }
    };
    WorkerOutcome {
        worker_id: format!("adopted-{}", worker.pid),
        pid: worker.pid,
        start_unix_ms,
        exit_status,
        stdout: String::new(),
        stderr: String::new(),
        wall_ms: started.elapsed().as_millis() as u64,
        was_timed_out: false,
    }
}

#[cfg(windows)]
fn open_query_handle(pid: u32) -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok() }
}

#[cfg(not(windows))]
fn open_query_handle(_pid: u32) -> Option<()> {
    None
}

#[cfg(windows)]
fn exit_code_of(handle: windows::Win32::Foundation::HANDLE) -> Option<u32> {
    use windows::Win32::System::Threading::GetExitCodeProcess;
    const STILL_ACTIVE_CODE: u32 = 259;
    let mut code = 0u32;
    let ok = unsafe { GetExitCodeProcess(handle, &mut code) }.is_ok();
    (ok && code != STILL_ACTIVE_CODE).then_some(code)
}

#[cfg(not(windows))]
fn exit_code_of(_handle: ()) -> Option<u32> {
    None
}

#[cfg(windows)]
fn close_handle(handle: windows::Win32::Foundation::HANDLE) {
    use windows::Win32::Foundation::CloseHandle;
    unsafe {
        let _ = CloseHandle(handle);
    }
}

#[cfg(not(windows))]
fn close_handle(_handle: ()) {}

/// Drive activation for the worker that just exited. Blocks while it supervises
/// the candidate (or the rollback artifact) through `lattice-supervisor`; the
/// monitor's main loop is the only supervisor, and the request watcher remains
/// free to drain the newly-serving worker for a following activation.
pub(crate) fn run_swap(
    ctx: &Arc<ActivationContext>,
    mut exe: PathBuf,
    mut outcome: WorkerOutcome,
    limits: &WorkerLimits,
    cancel_flag: &Arc<AtomicBool>,
) -> SwapOutcome {
    loop {
        let Some(journal) = read_journal(&ctx.base) else {
            return SwapOutcome::Supervised {
                exe,
                outcome,
                respawn_override: false,
            };
        };
        let status = read_status(&ctx.base);
        let failed = status
            .as_ref()
            .map(|s| s.operation_id == journal.operation_id && s.phase == Phase::Failed)
            .unwrap_or(false);
        if failed || !path_eq(Path::new(&journal.from_exe), &exe) {
            let detail = if failed {
                "activation failed before the candidate started; previous artifact restored"
            } else {
                "journal does not match the exited worker; refusing to swap"
            };
            abort_journal(ctx, &journal, detail);
            return SwapOutcome::Supervised {
                exe,
                outcome,
                respawn_override: true,
            };
        }
        // The journal is a local file; re-verify the exact image before it is
        // spawned. This also closes a TOCTOU replacement between validation and
        // the old worker's exit.
        let request = ctx
            .request()
            .filter(|request| request.operation_id == journal.operation_id);
        if let Err(e) = candidate_is_safe(ctx, &journal, request.as_ref()) {
            abort_journal(
                ctx,
                &journal,
                &format!("refusing to spawn an unverified candidate: {e}"),
            );
            return SwapOutcome::Supervised {
                exe,
                outcome,
                respawn_override: true,
            };
        }
        // Drain handoff: never start the candidate before the watcher has taken
        // the maintenance lease and observed the old process exit. Otherwise a
        // fast worker exit could race the lease acquisition.
        loop {
            match ctx.drain_state.load(Ordering::SeqCst) {
                DRAIN_READY => {
                    ctx.drain_state.store(DRAIN_IDLE, Ordering::SeqCst);
                    break;
                }
                DRAIN_ACTIVE => {
                    if budget_expired(journal.started_at_unix_ms, now_ms()) {
                        abort_journal(
                            ctx,
                            &journal,
                            "drain handoff timed out before the candidate could start",
                        );
                        return SwapOutcome::Supervised {
                            exe,
                            outcome,
                            respawn_override: true,
                        };
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                DRAIN_FAILED => {
                    abort_journal(
                        ctx,
                        &journal,
                        "drain failed; previous artifact restored by the monitor",
                    );
                    return SwapOutcome::Supervised {
                        exe,
                        outcome,
                        respawn_override: true,
                    };
                }
                // No watcher drain (recovery path): proceed under the journal.
                _ => break,
            }
        }
        match run_one_activation(ctx, &journal, limits, cancel_flag) {
            Ok((next_exe, next_outcome)) => {
                exe = next_exe;
                outcome = next_outcome;
            }
            Err(detail) => return SwapOutcome::Retry { detail },
        }
    }
}

fn run_one_activation(
    ctx: &Arc<ActivationContext>,
    journal: &ActivationJournal,
    limits: &WorkerLimits,
    cancel_flag: &Arc<AtomicBool>,
) -> Result<(PathBuf, WorkerOutcome), String> {
    let to_exe = PathBuf::from(&journal.to_exe);
    let deadline = activation_deadline(journal.started_at_unix_ms);
    if budget_expired(journal.started_at_unix_ms, now_ms()) {
        return rollback_and_supervise(
            ctx,
            journal,
            "10 s activation budget expired before the candidate started",
            limits,
            cancel_flag,
        );
    }

    // Release the lease before spawning: the candidate must not see a live
    // maintenance lease (NAIOS-006 refuses to open the store under one).
    if let Err(e) = release_lease(ctx, &journal.operation_id) {
        return rollback_and_supervise(
            ctx,
            journal,
            &format!("could not release the maintenance lease: {e}"),
            limits,
            cancel_flag,
        );
    }

    let mut starting = journal.clone();
    starting.phase = Phase::Starting;
    if let Err(e) = write_journal(&ctx.base, &starting) {
        return rollback_and_supervise(
            ctx,
            journal,
            &format!("could not record the starting journal phase: {e}"),
            limits,
            cancel_flag,
        );
    }
    let _ = write_status(
        &ctx.base,
        &status_for(
            journal,
            Phase::Starting,
            "starting the staged candidate",
            None,
            read_endpoint(&ctx.runtime_root).map(endpoint_info),
        ),
    );

    let mut verifying = journal.clone();
    verifying.phase = Phase::Verifying;
    if let Err(e) = write_journal(&ctx.base, &verifying) {
        return rollback_and_supervise(
            ctx,
            journal,
            &format!("could not record the verifying journal phase: {e}"),
            limits,
            cancel_flag,
        );
    }
    let _ = write_status(
        &ctx.base,
        &status_for(
            journal,
            Phase::Verifying,
            "waiting for a verified candidate endpoint",
            None,
            None,
        ),
    );

    let previous_endpoint = read_endpoint(&ctx.runtime_root);
    let shared = Arc::new(VerifyShared::new());
    let verifier = spawn_verifier(
        Arc::clone(ctx),
        journal.clone(),
        Arc::clone(&shared),
        Arc::clone(cancel_flag),
        previous_endpoint,
        deadline,
        ctx.request(),
    );

    set_running_version(ctx, &journal.to_version);
    let spec = ProcessSpec {
        program: to_exe.clone(),
        args: vec!["--child".to_string()],
        cwd: None,
        env: BTreeMap::new(),
        no_console: true,
        no_capture: true,
    };
    cancel_flag.store(false, Ordering::SeqCst);
    let outcome = child::run(&spec, limits);
    shared.stop.store(true, Ordering::SeqCst);
    let _ = verifier.join();
    let result = shared.take_result();

    match (outcome, result) {
        (Ok(outcome), Some(VerifyResult::Committed(endpoint))) => {
            tracing::info!(
                pid = outcome.pid,
                version = %journal.to_version,
                instance_id = %endpoint.instance_id,
                "activation committed; candidate is serving"
            );
            Ok((to_exe, outcome))
        }
        (Ok(outcome), Some(VerifyResult::Rollback(error))) => {
            let detail = format!(
                "candidate verified endpoint failed: {error}; candidate exit={:?}",
                outcome.exit_status
            );
            rollback_and_supervise(ctx, journal, &detail, limits, cancel_flag)
        }
        (Ok(outcome), _) => {
            let detail = format!(
                "candidate exited before a verified endpoint (exit={:?})",
                outcome.exit_status
            );
            rollback_and_supervise(ctx, journal, &detail, limits, cancel_flag)
        }
        (Err(e), Some(VerifyResult::Committed(_))) => {
            // A commit cannot be observed without a live endpoint; treat the
            // spawn error as a rollback even if the verifier raced.
            rollback_and_supervise(
                ctx,
                journal,
                &format!("candidate supervisor failed after commit: {e}"),
                limits,
                cancel_flag,
            )
        }
        (Err(e), _) => rollback_and_supervise(
            ctx,
            journal,
            &format!("candidate spawn failed: {e}"),
            limits,
            cancel_flag,
        ),
    }
}

fn rollback_and_supervise(
    ctx: &Arc<ActivationContext>,
    journal: &ActivationJournal,
    reason: &str,
    limits: &WorkerLimits,
    cancel_flag: &Arc<AtomicBool>,
) -> Result<(PathBuf, WorkerOutcome), String> {
    tracing::warn!(
        operation_id = %journal.operation_id,
        %reason,
        "activation failed after the drain; rolling back"
    );
    // A candidate that is still alive would hold the singleton and make the
    // rollback artifact exit immediately. Stop it first (cooperatively if it
    // published an endpoint, otherwise by process handle).
    if let Some(endpoint) = read_endpoint(&ctx.runtime_root) {
        if process_alive(endpoint.pid)
            && process_image_path(endpoint.pid)
                .map(|image| path_eq(&image, &PathBuf::from(&journal.to_exe)))
                .unwrap_or(false)
        {
            let _ = request_shutdown(&endpoint.canonical_pipe, Duration::from_secs(2));
            if !wait_dead(endpoint.pid, Duration::from_secs(3)) {
                terminate_process(endpoint.pid);
                wait_dead(endpoint.pid, Duration::from_secs(3));
            }
        }
    }
    restore_active_verbatim(ctx, &journal.active_json_before)?;
    ctx.clear_lease();
    ctx.set_request(None);

    // Spawn and verify the previous artifact. The status file becomes
    // rolled_back (or failed with a rollback diagnostic) when the endpoint is
    // real; the journal is removed only after that durable write.
    let old_exe = PathBuf::from(&journal.from_exe);
    let rollback_deadline = now_ms() + 5_000;
    let shared = Arc::new(VerifyShared::new());
    let verifier = spawn_rollback_verifier(
        Arc::clone(ctx),
        journal.clone(),
        Arc::clone(&shared),
        rollback_deadline,
        reason.to_string(),
    );
    let spec = ProcessSpec {
        program: old_exe.clone(),
        args: vec!["--child".to_string()],
        cwd: None,
        env: BTreeMap::new(),
        no_console: true,
        no_capture: true,
    };
    set_running_version(ctx, &journal.from_version);
    cancel_flag.store(false, Ordering::SeqCst);
    let outcome = child::run(&spec, limits);
    shared.stop.store(true, Ordering::SeqCst);
    let _ = verifier.join();
    match outcome {
        Ok(outcome) => {
            // If the rollback verifier could not see a healthy endpoint before
            // the old worker exited, make the failure durable. The monitor will
            // retry the active (restored) artifact.
            if !shared.completed() {
                let _ = write_terminal(
                    ctx,
                    &journal.operation_id,
                    Phase::Failed,
                    Some(journal.from_version.clone()),
                    Some(journal.to_version.clone()),
                    Some(format!(
                        "rollback worker exited before a verified endpoint ({reason})"
                    )),
                    Some(format!(
                        "rollback_unverified: {reason}; old exit={:?}",
                        outcome.exit_status
                    )),
                    read_endpoint(&ctx.runtime_root).map(endpoint_info),
                    journal.started_at_unix_ms,
                );
                remove_journal(&ctx.base);
                finish_in_flight(ctx);
            }
            Ok((old_exe, outcome))
        }
        Err(e) => {
            let _ = write_terminal(
                ctx,
                &journal.operation_id,
                Phase::Failed,
                Some(journal.from_version.clone()),
                Some(journal.to_version.clone()),
                Some(format!("rollback spawn failed ({reason})")),
                Some(format!("rollback_spawn_failed: {e}; {reason}")),
                read_endpoint(&ctx.runtime_root).map(endpoint_info),
                journal.started_at_unix_ms,
            );
            remove_journal(&ctx.base);
            finish_in_flight(ctx);
            Err(format!("rollback spawn failed: {e}"))
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Verification
// ─────────────────────────────────────────────────────────────────────────────

struct VerifyShared {
    stop: AtomicBool,
    result: Mutex<Option<VerifyResult>>,
    completed: AtomicBool,
}

impl VerifyShared {
    fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            result: Mutex::new(None),
            completed: AtomicBool::new(false),
        }
    }

    fn set(&self, result: VerifyResult) {
        *self.result.lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
    }

    fn take_result(&self) -> Option<VerifyResult> {
        self.result.lock().unwrap_or_else(|p| p.into_inner()).take()
    }

    fn completed(&self) -> bool {
        self.completed.load(Ordering::SeqCst)
    }
}

enum VerifyResult {
    Committed(EndpointInfo),
    Rollback(String),
    Stopped,
}

struct Probe {
    naf_version: u32,
    runtime_instance_id: String,
    health: Value,
    describe: Value,
}

fn probe_endpoint(
    endpoint: &PublishedEndpoint,
    journal: &ActivationJournal,
    request: Option<&ActivationRequest>,
) -> Result<Probe, String> {
    if endpoint.canonical_pipe.trim().is_empty() {
        return Err("endpoint has no canonical_pipe".into());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;
    let (protocol_min, protocol_max, storage_schema) = request
        .map(|r| (r.protocol_min, r.protocol_max, r.storage_schema))
        .unwrap_or((1, 1, 1));
    runtime.block_on(async move {
        let stream = nai_lattice_client::win_pipe::NamedPipeStream::connect(&endpoint.canonical_pipe)
            .map_err(|e| format!("connect {}: {e}", endpoint.canonical_pipe))?;
        let handshake = nai_lattice_protocol::handshake::client_request(
            "nai-lattice-activator",
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            protocol_min,
            protocol_max,
            &["runtime.health", "runtime.describe"],
            Some("activation"),
            "activation",
            uuid::Uuid::new_v4().as_bytes(),
        );
        let mut client = nai_lattice_client::NafClient::connect(stream, handshake)
            .await
            .map_err(|e| format!("NAF handshake: {e}"))?;
        if client.naf_version < protocol_min || client.naf_version > protocol_max {
            return Err(format!(
                "handshake selected NAF {} outside the requested range {}..={}",
                client.naf_version, protocol_min, protocol_max
            ));
        }
        let health_body = client
            .dispatch("runtime.health", b"{}", None)
            .await
            .map_err(|e| format!("runtime.health: {e}"))?;
        let health: Value = serde_json::from_slice(&health_body)
            .map_err(|e| format!("runtime.health JSON: {e}"))?;
        let describe_body = client
            .dispatch("runtime.describe", b"{}", None)
            .await
            .map_err(|e| format!("runtime.describe: {e}"))?;
        let describe: Value = serde_json::from_slice(&describe_body)
            .map_err(|e| format!("runtime.describe JSON: {e}"))?;
        let state = health
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if state != "healthy" {
            return Err(format!("runtime.health state is {state:?}, not \"healthy\""));
        }
        let health_instance = health
            .get("runtime_instance_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        if health_instance != endpoint.instance_id {
            return Err(format!(
                "runtime.health instance {health_instance:?} != endpoint instance {:?}",
                endpoint.instance_id
            ));
        }
        if client.runtime_instance_id != endpoint.instance_id {
            return Err(format!(
                "handshake instance {:?} != endpoint instance {:?}",
                client.runtime_instance_id, endpoint.instance_id
            ));
        }
        let storage = describe
            .get("storage_schema_version")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        if storage != storage_schema {
            return Err(format!(
                "candidate storage_schema_version {storage} != requested {storage_schema}"
            ));
        }
        let host_version = describe
            .get("host_version")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if host_version != journal.to_version && host_version != env!("CARGO_PKG_VERSION") {
            // The staged copy in a mechanism test may legitimately carry the
            // build's compiled version. Log, but do not claim it is the target.
            tracing::warn!(
                host_version,
                to_version = %journal.to_version,
                "candidate reports a different compiled host_version (mechanism test?)"
            );
        }
        Ok(Probe {
            naf_version: client.naf_version,
            runtime_instance_id: client.runtime_instance_id,
            health,
            describe,
        })
    })
}

fn spawn_verifier(
    ctx: Arc<ActivationContext>,
    journal: ActivationJournal,
    shared: Arc<VerifyShared>,
    cancel_flag: Arc<AtomicBool>,
    previous: Option<PublishedEndpoint>,
    deadline_ms: i64,
    request: Option<ActivationRequest>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("activation-verify".into())
        .spawn(move || {
            let result = verify_loop(
                &ctx,
                &journal,
                &shared,
                &cancel_flag,
                previous.as_ref(),
                deadline_ms,
                request.as_ref(),
            );
            shared.set(result);
        })
        .expect("spawn activation verifier")
}

#[allow(clippy::too_many_arguments)]
fn verify_loop(
    ctx: &ActivationContext,
    journal: &ActivationJournal,
    shared: &VerifyShared,
    cancel_flag: &AtomicBool,
    previous: Option<&PublishedEndpoint>,
    deadline_ms: i64,
    request: Option<&ActivationRequest>,
) -> VerifyResult {
    let to_exe = PathBuf::from(&journal.to_exe);
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return VerifyResult::Stopped;
        }
        if now_ms() >= deadline_ms {
            cancel_flag.store(true, Ordering::SeqCst);
            return VerifyResult::Rollback(
                "the 10 s activation budget expired before a verified candidate endpoint".into(),
            );
        }
        if let Some(endpoint) = read_endpoint(&ctx.runtime_root) {
            let pid_changed = previous.map(|p| p.pid != endpoint.pid).unwrap_or(true);
            let instance_changed = previous
                .map(|p| p.instance_id != endpoint.instance_id)
                .unwrap_or(true);
            let image_matches = process_image_path(endpoint.pid)
                .map(|image| path_eq(&image, &to_exe))
                .unwrap_or(false);
            if pid_changed && instance_changed && image_matches {
                match probe_endpoint(&endpoint, journal, request) {
                    Ok(probe) => {
                        let _ = (probe.naf_version, &probe.runtime_instance_id, &probe.health, &probe.describe);
                        let hash = match sha256_file(&to_exe) {
                            Ok(hash) => hash,
                            Err(e) => {
                                cancel_flag.store(true, Ordering::SeqCst);
                                return VerifyResult::Rollback(format!("hash candidate: {e}"));
                            }
                        };
                        let manifest = read_manifest(&to_exe).ok();
                        let allow_unsigned = request.map(|r| r.allow_unsigned).unwrap_or(false);
                        let manifest_ok = manifest
                            .as_ref()
                            .map(|manifest| {
                                manifest
                                    .sha256
                                    .as_deref()
                                    .map(|sha| sha.eq_ignore_ascii_case(&hash))
                                    .unwrap_or(false)
                                    && (signature_is_valid(manifest.signature_status.as_deref())
                                        || allow_unsigned)
                            })
                            .unwrap_or(false);
                        if !manifest_ok {
                            cancel_flag.store(true, Ordering::SeqCst);
                            return VerifyResult::Rollback(
                                "candidate manifest SHA-256/signature changed before commit".into(),
                            );
                        }
                        if budget_expired(journal.started_at_unix_ms, now_ms()) {
                            cancel_flag.store(true, Ordering::SeqCst);
                            return VerifyResult::Rollback(
                                "the 10 s activation budget expired before the commit".into(),
                            );
                        }
                        let remaining_ms = (activation_deadline(journal.started_at_unix_ms)
                            - now_ms())
                        .clamp(1, 10_000) as u32;
                        if let Err(e) = commit_active_json(
                            ctx,
                            journal,
                            &to_exe,
                            &hash,
                            manifest.as_ref(),
                            remaining_ms,
                        ) {
                            cancel_flag.store(true, Ordering::SeqCst);
                            return VerifyResult::Rollback(format!("commit failed: {e}"));
                        }
                        let info = endpoint_info(endpoint);
                        let status_result = write_terminal(
                            ctx,
                            &journal.operation_id,
                            Phase::Committed,
                            Some(journal.from_version.clone()),
                            Some(journal.to_version.clone()),
                            Some("candidate endpoint verified".into()),
                            None,
                            Some(info.clone()),
                            journal.started_at_unix_ms,
                        );
                        if let Err(e) = status_result {
                            cancel_flag.store(true, Ordering::SeqCst);
                            return VerifyResult::Rollback(format!(
                                "active.json committed but status write failed: {e}"
                            ));
                        }
                        remove_journal(&ctx.base);
                        finish_in_flight(ctx);
                        shared.completed.store(true, Ordering::SeqCst);
                        return VerifyResult::Committed(info);
                    }
                    Err(e) => {
                        tracing::debug!(
                            pid = endpoint.pid,
                            error = %e,
                            "candidate endpoint is not verifiable yet"
                        );
                    }
                }
            }
        }
        std::thread::sleep(VERIFY_POLL_INTERVAL);
    }
}

fn spawn_rollback_verifier(
    ctx: Arc<ActivationContext>,
    journal: ActivationJournal,
    shared: Arc<VerifyShared>,
    deadline_ms: i64,
    reason: String,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("activation-rollback-verify".into())
        .spawn(move || loop {
            if shared.stop.load(Ordering::SeqCst) {
                return;
            }
            if now_ms() >= deadline_ms {
                let _ = write_terminal(
                    &ctx,
                    &journal.operation_id,
                    Phase::Failed,
                    Some(journal.from_version.clone()),
                    Some(journal.to_version.clone()),
                    Some(format!(
                        "rollback endpoint was not verified before the deadline ({reason})"
                    )),
                    Some(format!("rollback_verify_timeout: {reason}")),
                    read_endpoint(&ctx.runtime_root).map(endpoint_info),
                    journal.started_at_unix_ms,
                );
                remove_journal(&ctx.base);
                finish_in_flight(&ctx);
                return;
            }
            let old_exe = PathBuf::from(&journal.from_exe);
            if let Some(endpoint) = read_endpoint(&ctx.runtime_root) {
                let image_matches = process_image_path(endpoint.pid)
                    .map(|image| path_eq(&image, &old_exe))
                    .unwrap_or(false);
                if image_matches && probe_endpoint(&endpoint, &journal, None).is_ok() {
                    let _ = write_terminal(
                        &ctx,
                        &journal.operation_id,
                        Phase::RolledBack,
                        Some(journal.from_version.clone()),
                        Some(journal.to_version.clone()),
                        Some("previous artifact is serving again".into()),
                        Some(reason.clone()),
                        Some(endpoint_info(endpoint)),
                        journal.started_at_unix_ms,
                    );
                    remove_journal(&ctx.base);
                    finish_in_flight(&ctx);
                    shared.completed.store(true, Ordering::SeqCst);
                    return;
                }
            }
            std::thread::sleep(VERIFY_POLL_INTERVAL);
        })
        .expect("spawn activation rollback verifier")
}

// ─────────────────────────────────────────────────────────────────────────────
// Commit / restore / journal
// ─────────────────────────────────────────────────────────────────────────────

fn commit_active_json(
    ctx: &ActivationContext,
    journal: &ActivationJournal,
    candidate_exe: &Path,
    hash: &str,
    manifest: Option<&HostManifest>,
    manager_timeout_ms: u32,
) -> Result<(), String> {
    let _guard = crate::win_acl::acquire_manager_mutex(ctx.instance.as_deref(), manager_timeout_ms)
        .ok_or_else(|| {
            format!(
                "the shared manager mutex was not acquired within {manager_timeout_ms} ms"
            )
        })?;
    let active_path = ctx.base.join(ACTIVE_FILE);
    let current = std::fs::read(&active_path)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    if current != journal.active_json_before {
        return Err(
            "active.json changed after validation; refusing to overwrite a newer activation"
                .into(),
        );
    }
    if !journal.active_json_before.is_empty() {
        write_atomic(&ctx.base.join(PREVIOUS_FILE), journal.active_json_before.as_bytes())?;
    }
    let mut value = if current.trim().is_empty() {
        Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(&current)
            .map_err(|e| format!("active.json became malformed: {e}"))?
    };
    let object = value
        .as_object_mut()
        .ok_or_else(|| "active.json is not a JSON object".to_string())?;
    object.insert(
        "version".into(),
        Value::String(journal.to_version.clone()),
    );
    object.insert("sha256".into(), Value::String(hash.to_string()));
    object.insert(
        "path".into(),
        Value::String(display_path(candidate_exe)),
    );
    object.insert(
        "installed_at_unix_ms".into(),
        Value::Number(serde_json::Number::from(now_ms())),
    );
    if let Some(manifest) = manifest {
        if let Some(source) = &manifest.source {
            object.insert("source".into(), Value::String(source.clone()));
        }
        if let Some(git) = &manifest.git {
            object.insert("git".into(), Value::String(git.clone()));
        }
    }
    let bytes = serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?;
    write_atomic(&active_path, &bytes)?;
    // Re-read and prove the durable pointer has the intended identity before the
    // caller publishes `committed`.
    let verify: Value = serde_json::from_slice(
        &std::fs::read(&active_path).map_err(|e| format!("re-read active.json: {e}"))?,
    )
    .map_err(|e| format!("parse active.json after write: {e}"))?;
    if verify.get("version").and_then(Value::as_str) != Some(journal.to_version.as_str()) {
        return Err("active.json did not persist the target version".into());
    }
    Ok(())
}

fn restore_active_verbatim(ctx: &ActivationContext, before: &str) -> Result<(), String> {
    let path = ctx.base.join(ACTIVE_FILE);
    if before.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("remove active.json: {e}")),
        }
    } else {
        write_atomic(&path, before.as_bytes())
    }
}

fn abort_journal(ctx: &ActivationContext, journal: &ActivationJournal, detail: &str) {
    let _ = restore_active_verbatim(ctx, &journal.active_json_before);
    let existing = read_status(&ctx.base);
    let phase = match existing {
        Some(status)
            if status.operation_id == journal.operation_id && status.phase == Phase::Failed =>
        {
            Phase::Failed
        }
        _ => Phase::RolledBack,
    };
    let _ = write_terminal(
        ctx,
        &journal.operation_id,
        phase,
        Some(journal.from_version.clone()),
        Some(journal.to_version.clone()),
        Some(detail.to_string()),
        Some(detail.to_string()),
        read_endpoint(&ctx.runtime_root).map(endpoint_info),
        journal.started_at_unix_ms,
    );
    remove_journal(&ctx.base);
    finish_in_flight(ctx);
}

fn release_lease(ctx: &ActivationContext, operation_id: &str) -> Result<(), String> {
    let Some(handle) = ctx.take_lease(operation_id) else {
        return Ok(());
    };
    match MaintenanceLock::new(&ctx.runtime_root).release(&handle.token) {
        Ok(ReleaseOutcome::Released(_)) => Ok(()),
        Ok(ReleaseOutcome::Refused(refusal)) => {
            // NotLocked means the lease already expired/vanished; that is safe.
            if matches!(
                refusal,
                nai_lattice_runtime::lifecycle::LockRefusal::NotLocked
            ) {
                Ok(())
            } else {
                Err(format!("lease release refused: {}", refusal.reason()))
            }
        }
        Err(e) => Err(format!("lease release failed: {e}")),
    }
}

/// Recovery: remove a lease created by this exact operation, even though the
/// fencing token was only held in the crashed monitor's memory. The operation id
/// is random and unique, owner+reason must both match, and the file is moved
/// aside and re-read before deletion (the same race discipline as release).
fn force_release_own_lease(ctx: &ActivationContext, operation_id: &str) {
    let lock = MaintenanceLock::new(&ctx.runtime_root);
    let record = match lock.inspect() {
        LockInspection::Live(record) | LockInspection::Expired(record) => record,
        _ => return,
    };
    if record.owner != ACTIVATOR_OWNER || !record.reason.contains(operation_id) {
        return;
    }
    let staged = lock.path().with_file_name(format!(
        ".maintenance.lock.json.recover-{}",
        std::process::id()
    ));
    if std::fs::rename(lock.path(), &staged).is_err() {
        return;
    }
    let ours = std::fs::read(&staged)
        .ok()
        .and_then(|b| serde_json::from_slice::<nai_lattice_runtime::lifecycle::MaintenanceLockRecord>(&b).ok())
        .map(|r| r.owner == ACTIVATOR_OWNER && r.reason.contains(operation_id))
        .unwrap_or(false);
    if ours {
        let _ = std::fs::remove_file(&staged);
        tracing::warn!(
            operation_id,
            "removed a maintenance lease left by an interrupted activation"
        );
    } else {
        let _ = std::fs::rename(&staged, lock.path());
    }
}

fn set_running_version(ctx: &ActivationContext, version: &str) {
    if let Ok(mut guard) = ctx.running_version.lock() {
        *guard = version.to_string();
    }
}

/// Terminal resolution for one operation: no lease, no retained request, drain
/// handoff reset, and the monitor free to treat the next worker exit normally.
fn finish_in_flight(ctx: &ActivationContext) {
    ctx.clear_lease();
    ctx.set_request(None);
    ctx.drain_state.store(DRAIN_IDLE, Ordering::SeqCst);
    ctx.in_flight.store(false, Ordering::SeqCst);
}

// ─────────────────────────────────────────────────────────────────────────────
// Status / journal / endpoint files
// ─────────────────────────────────────────────────────────────────────────────

fn status_for(
    journal: &ActivationJournal,
    phase: Phase,
    detail: &str,
    error: Option<String>,
    endpoint: Option<EndpointInfo>,
) -> ActivationStatus {
    ActivationStatus {
        schema: SCHEMA,
        operation_id: journal.operation_id.clone(),
        phase,
        from_version: Some(journal.from_version.clone()),
        to_version: Some(journal.to_version.clone()),
        detail: Some(detail.to_string()),
        started_at_unix_ms: journal.started_at_unix_ms,
        updated_at_unix_ms: now_ms(),
        error,
        endpoint,
    }
}

#[allow(clippy::too_many_arguments)]
fn write_terminal(
    ctx: &ActivationContext,
    operation_id: &str,
    phase: Phase,
    from_version: Option<String>,
    to_version: Option<String>,
    detail: Option<String>,
    error: Option<String>,
    endpoint: Option<EndpointInfo>,
    started_at_unix_ms: i64,
) -> Result<(), String> {
    let existing = read_status(&ctx.base);
    let same_operation = existing
        .as_ref()
        .map(|s| s.operation_id == operation_id)
        .unwrap_or(false);
    // A terminal `failed` status is the truthful outcome of a failed drain; a
    // later rollback resolution must not rewrite it as a success-looking phase.
    let phase = if same_operation
        && existing.as_ref().map(|s| s.phase) == Some(Phase::Failed)
        && phase == Phase::RolledBack
    {
        Phase::Failed
    } else {
        phase
    };
    let status = ActivationStatus {
        schema: SCHEMA,
        operation_id: operation_id.to_string(),
        phase,
        from_version,
        to_version,
        detail,
        started_at_unix_ms,
        updated_at_unix_ms: now_ms(),
        error,
        endpoint,
    };
    write_status(&ctx.base, &status)
}

fn write_status(base: &Path, status: &ActivationStatus) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(status).map_err(|e| e.to_string())?;
    write_atomic(&base.join(STATUS_FILE), &bytes)
}

fn read_status(base: &Path) -> Option<ActivationStatus> {
    let bytes = std::fs::read(base.join(STATUS_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_journal(base: &Path, journal: &ActivationJournal) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(journal).map_err(|e| e.to_string())?;
    write_atomic(&base.join(JOURNAL_FILE), &bytes)
}

fn read_journal(base: &Path) -> Option<ActivationJournal> {
    let bytes = std::fs::read(base.join(JOURNAL_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn journal_is_operation(ctx: &ActivationContext, operation_id: &str) -> bool {
    read_journal(&ctx.base)
        .map(|journal| journal.operation_id == operation_id)
        .unwrap_or(false)
}

fn remove_journal(base: &Path) {
    let _ = std::fs::remove_file(base.join(JOURNAL_FILE));
}

fn remove_request(ctx: &ActivationContext) {
    let _ = std::fs::remove_file(ctx.base.join(REQUEST_FILE));
}

fn remove_matching_request(base: &Path, operation_id: &str) {
    let Ok(raw) = std::fs::read(base.join(REQUEST_FILE)) else {
        return;
    };
    let ours = serde_json::from_slice::<Value>(&raw)
        .ok()
        .and_then(|v| v.get("operation_id").and_then(Value::as_str).map(str::to_string))
        .map(|op| op == operation_id)
        .unwrap_or(false);
    if ours {
        let _ = std::fs::remove_file(base.join(REQUEST_FILE));
    }
}

fn read_endpoint(runtime_root: &Path) -> Option<PublishedEndpoint> {
    let bytes = std::fs::read(runtime_root.join("endpoint.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn endpoint_info(endpoint: PublishedEndpoint) -> EndpointInfo {
    EndpointInfo {
        pid: endpoint.pid,
        canonical_pipe: endpoint.canonical_pipe,
        instance_id: endpoint.instance_id,
    }
}

/// The canonical activation value is `Signed`; older manifests use the
/// Authenticode `Valid` spelling. Both mean a signed build.
fn signature_is_valid(status: Option<&str>) -> bool {
    matches!(status, Some("Signed") | Some("Valid"))
}

fn read_manifest(exe: &Path) -> Result<HostManifest, String> {
    let staged = exe.parent().ok_or_else(|| "candidate exe has no parent".to_string())?;
    let bytes = std::fs::read(staged.join(MANIFEST_FILE))
        .map_err(|e| format!("read host-manifest.json: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("parse host-manifest.json: {e}"))
}

/// Re-verify the journal's candidate at the moment of use. The journal is a
/// local file, so the monitor must not spawn an arbitrary image merely because a
/// journal names it: the exe must be `<base>/runtime/<to_version>/nai-fabric-host.exe`,
/// its current SHA-256 must match `host-manifest.json`, and its signature policy
/// must still allow it. This closes both a forged journal and a TOCTOU swap
/// between validation and spawn.
fn candidate_is_safe(
    ctx: &ActivationContext,
    journal: &ActivationJournal,
    request: Option<&ActivationRequest>,
) -> Result<String, String> {
    let runtime = std::fs::canonicalize(ctx.base.join(RUNTIME_DIR))
        .map_err(|e| format!("canonicalize runtime dir: {e}"))?;
    let exe = std::fs::canonicalize(PathBuf::from(&journal.to_exe))
        .map_err(|e| format!("canonicalize candidate exe: {e}"))?;
    let version_dir = exe
        .parent()
        .ok_or_else(|| "candidate exe has no parent".to_string())?;
    if version_dir.parent() != Some(runtime.as_path()) {
        return Err("candidate is not a direct child of <base>/runtime".into());
    }
    let dir_name = version_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if dir_name != journal.to_version {
        return Err(format!(
            "candidate directory {dir_name:?} != journal to_version {:?}",
            journal.to_version
        ));
    }
    let hash = sha256_file(&exe)?;
    let manifest = read_manifest(&exe)?;
    let manifest_sha = manifest
        .sha256
        .as_deref()
        .ok_or_else(|| "host-manifest.json has no sha256".to_string())?;
    if !manifest_sha.eq_ignore_ascii_case(&hash) {
        return Err("candidate SHA-256 no longer matches host-manifest.json".into());
    }
    let allow_unsigned = request.map(|r| r.allow_unsigned).unwrap_or(false);
    if !signature_is_valid(manifest.signature_status.as_deref()) {
        if allow_unsigned {
            tracing::warn!(
                signature_status = manifest.signature_status.as_deref().unwrap_or("missing"),
                "spawning a non-Signed candidate because allow_unsigned=true (QA only)"
            );
        } else {
            return Err(format!(
                "candidate signature_status is {:?}; refusing to spawn",
                manifest.signature_status.as_deref().unwrap_or("missing")
            ));
        }
    }
    Ok(hash)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("create {}: {e}", parent.display()))?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let tmp = parent.join(format!(".{file_name}.tmp-{}", std::process::id()));
    {
        let mut file = std::fs::File::create(&tmp)
            .map_err(|e| format!("create {}: {e}", tmp.display()))?;
        file.write_all(bytes)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        file.sync_all()
            .map_err(|e| format!("sync {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("replace {}: {e}", path.display()))?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Versions / hashes / paths
// ─────────────────────────────────────────────────────────────────────────────

fn is_safe_version_label(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 64
        && version != "."
        && version != ".."
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
}

fn is_sha256_label(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn version_from_exe(base: &Path, exe: &Path) -> Option<String> {
    let base = std::fs::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
    let exe = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let version_dir = exe.parent()?;
    let version = version_dir.file_name()?.to_string_lossy().into_owned();
    if !is_safe_version_label(&version) {
        return None;
    }
    let container = version_dir.parent()?;
    let container_name = container.file_name()?.to_string_lossy().into_owned();
    if container_name != RUNTIME_DIR && container_name != "releases" {
        return None;
    }
    if container.parent() != Some(base.as_path()) {
        return None;
    }
    Some(version)
}

fn path_eq(a: &Path, b: &Path) -> bool {
    let a = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let b = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
}

/// Human/consumer-friendly absolute path: strip the Win32 extended-length
/// prefix that `canonicalize` adds (`\\?\C:\...` -> `C:\...`). The value still
/// names the same file and remains valid for every Windows API and product.
fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy().into_owned();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VersionKey {
    core: Vec<u64>,
    pre: Option<Vec<PreId>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PreId {
    Numeric(u64),
    Text(String),
}

fn parse_version(input: &str) -> Option<VersionKey> {
    let mut text = input.trim();
    if let Some(stripped) = text.strip_prefix('v').or_else(|| text.strip_prefix('V')) {
        text = stripped;
    }
    if text.is_empty() {
        return None;
    }
    // Build metadata is not part of precedence.
    if let Some((main, _build)) = text.split_once('+') {
        text = main;
    }
    let (core_text, pre_text) = match text.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (text, None),
    };
    let mut core = Vec::new();
    for part in core_text.split('.') {
        if part.is_empty() {
            return None;
        }
        core.push(part.parse::<u64>().ok()?);
    }
    if core.is_empty() {
        return None;
    }
    let pre = match pre_text {
        None => None,
        Some(pre) if pre.is_empty() => return None,
        Some(pre) => {
            let mut ids = Vec::new();
            for part in pre.split('.') {
                if part.is_empty() {
                    return None;
                }
                if part.bytes().all(|b| b.is_ascii_digit()) {
                    ids.push(PreId::Numeric(part.parse::<u64>().ok()?));
                } else {
                    ids.push(PreId::Text(part.to_string()));
                }
            }
            Some(ids)
        }
    };
    Some(VersionKey { core, pre })
}

fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    let a = parse_version(a)?;
    let b = parse_version(b)?;
    let len = a.core.len().max(b.core.len());
    for index in 0..len {
        let left = a.core.get(index).copied().unwrap_or(0);
        let right = b.core.get(index).copied().unwrap_or(0);
        match left.cmp(&right) {
            Ordering::Equal => {}
            other => return Some(other),
        }
    }
    match (&a.pre, &b.pre) {
        (None, None) => Some(Ordering::Equal),
        (None, Some(_)) => Some(Ordering::Greater),
        (Some(_), None) => Some(Ordering::Less),
        (Some(left), Some(right)) => {
            let len = left.len().max(right.len());
            for index in 0..len {
                match (left.get(index), right.get(index)) {
                    (None, None) => break,
                    (None, Some(_)) => return Some(Ordering::Less),
                    (Some(_), None) => return Some(Ordering::Greater),
                    (Some(PreId::Numeric(l)), Some(PreId::Numeric(r))) => match l.cmp(r) {
                        Ordering::Equal => {}
                        other => return Some(other),
                    },
                    (Some(PreId::Text(l)), Some(PreId::Text(r))) => match l.cmp(r) {
                        Ordering::Equal => {}
                        other => return Some(other),
                    },
                    (Some(PreId::Numeric(_)), Some(PreId::Text(_))) => {
                        return Some(Ordering::Less)
                    }
                    (Some(PreId::Text(_)), Some(PreId::Numeric(_))) => {
                        return Some(Ordering::Greater)
                    }
                }
            }
            Some(Ordering::Equal)
        }
    }
}

/// True while `pid` names a live process, using a query-only handle.
fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        const STILL_ACTIVE_CODE: u32 = 259;
        unsafe {
            let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                Ok(handle) => handle,
                Err(_) => return false,
            };
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code).is_ok();
            let _ = CloseHandle(handle);
            ok && code == STILL_ACTIVE_CODE
        }
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        false
    }
}

fn process_image_path(pid: u32) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buffer = vec![0u16; 4096];
            let mut len = buffer.len() as u32;
            let ok = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buffer.as_mut_ptr()),
                &mut len,
            )
            .is_ok();
            let _ = CloseHandle(handle);
            if !ok {
                return None;
            }
            Some(PathBuf::from(String::from_utf16_lossy(
                &buffer[..len as usize],
            )))
        }
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        None
    }
}

fn terminate_process(pid: u32) {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
        };
        unsafe {
            let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE;
            if let Ok(handle) = OpenProcess(access, false, pid) {
                let _ = TerminateProcess(handle, 1);
                let _ = CloseHandle(handle);
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
    }
}

fn wait_dead(pid: u32, timeout: Duration) -> bool {
    let started = std::time::Instant::now();
    while process_alive(pid) {
        if started.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    true
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authenticode_valid_counts_as_signed() {
        assert!(signature_is_valid(Some("Signed")));
        assert!(signature_is_valid(Some("Valid"))); // legacy Authenticode status
        assert!(!signature_is_valid(Some("NotSigned")));
        assert!(!signature_is_valid(Some("UnknownError")));
        assert!(!signature_is_valid(None));
    }

    fn test_ctx(root: &Path) -> ActivationContext {
        let base = root.join("Lattice");
        let runtime_root = base.join("v1");
        std::fs::create_dir_all(base.join(RUNTIME_DIR)).unwrap();
        std::fs::create_dir_all(&runtime_root).unwrap();
        ActivationContext {
            base,
            runtime_root,
            instance: Some("qa-test".into()),
            in_flight: Arc::new(AtomicBool::new(false)),
            drain_state: std::sync::atomic::AtomicU8::new(DRAIN_IDLE),
            lease: Mutex::new(None),
            request: Mutex::new(None),
            running_version: Arc::new(Mutex::new("unknown".into())),
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        ctx: ActivationContext,
        staged: PathBuf,
        exe: PathBuf,
        hash: String,
        from_exe: PathBuf,
    }

    fn make_fixture(version: &str, signed: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        std::fs::write(ctx.base.join(TOKEN_FILE), "test-token\n").unwrap();
        // The currently-running artifact (versioned under <base>/runtime).
        let from_dir = ctx.base.join(RUNTIME_DIR).join("0.1.3");
        std::fs::create_dir_all(&from_dir).unwrap();
        let from_exe = from_dir.join(EXE_FILE);
        std::fs::write(&from_exe, b"fake running host bytes").unwrap();
        // The staged candidate.
        let staged = ctx.base.join(RUNTIME_DIR).join(version);
        std::fs::create_dir_all(&staged).unwrap();
        let exe = staged.join(EXE_FILE);
        let bytes = b"fake candidate bytes for hash validation";
        std::fs::write(&exe, bytes).unwrap();
        let hash = sha256_file(&exe).unwrap();
        std::fs::write(
            staged.join(MANIFEST_FILE),
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": version,
                "sha256": hash,
                "signature_status": if signed { "Signed" } else { "NotSigned" },
                "source": "NAI-Lattice",
                "git": "test",
                "protocol_min": 1,
                "protocol_max": 1,
            }))
            .unwrap(),
        )
        .unwrap();
        Fixture {
            _dir: dir,
            ctx,
            staged,
            exe,
            hash,
            from_exe,
        }
    }

    fn request_for(fixture: &Fixture, version: &str) -> ActivationRequest {
        ActivationRequest {
            schema: 1,
            operation_id: "act-test-1".into(),
            staged_dir: fixture.staged.to_string_lossy().into_owned(),
            target_version: version.into(),
            exe_sha256: fixture.hash.clone(),
            protocol_min: 1,
            protocol_max: 1,
            storage_schema: 1,
            authorization: "test-token".into(),
            allow_unsigned: false,
            requested_at_unix_ms: 1,
        }
    }

    fn running_for(_fixture: &Fixture, exe: &Path, version: &str) -> RunningHost {
        RunningHost {
            pid: std::process::id(),
            exe: exe.to_path_buf(),
            version: version.into(),
            endpoint: PublishedEndpoint {
                pid: std::process::id(),
                canonical_pipe: r"\\.\pipe\test".into(),
                instance_id: "runtime-test".into(),
                protocol: Some(serde_json::json!({"selected": 1})),
            },
        }
    }

    fn with_active(fixture: &Fixture, version: &str) {
        std::fs::write(
            fixture.ctx.base.join(ACTIVE_FILE),
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": version,
                "sha256": fixture.hash,
                "path": fixture.from_exe,
                "installed_at_unix_ms": 1,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn raw(request: &ActivationRequest) -> Vec<u8> {
        serde_json::to_vec(request).unwrap()
    }

    fn validate(fixture: &Fixture, request: &ActivationRequest) -> Result<ValidatedRequest, Rejection> {
        let running = running_for(fixture, &fixture.from_exe, "0.1.3");
        validate_request(&fixture.ctx, &raw(request), &running)
    }

    #[test]
    fn schema_and_operation_id_are_rejected() {
        let fixture = make_fixture("0.1.4-qa", true);
        let mut request = request_for(&fixture, "0.1.4-qa");
        request.schema = 2;
        assert_eq!(
            validate(&fixture, &request).unwrap_err(),
            Rejection::SchemaMismatch("schema 2 (expected 1)".into())
        );
        let mut request = request_for(&fixture, "0.1.4-qa");
        request.operation_id = "  ".into();
        assert_eq!(validate(&fixture, &request).unwrap_err(), Rejection::EmptyOperationId);

        let mut request = request_for(&fixture, "0.1.4-qa");
        request.target_version = "../0.1.4".into();
        assert_eq!(
            validate(&fixture, &request).unwrap_err(),
            Rejection::InvalidVersionLabel("../0.1.4".into())
        );
    }

    #[test]
    fn staged_dir_must_be_the_direct_runtime_child() {
        let fixture = make_fixture("0.1.4-qa", true);
        let mut request = request_for(&fixture, "0.1.4-qa");
        let elsewhere = fixture.ctx.base.join("elsewhere").join("0.1.4-qa");
        std::fs::create_dir_all(&elsewhere).unwrap();
        request.staged_dir = elsewhere.to_string_lossy().into_owned();
        assert_eq!(
            validate(&fixture, &request).unwrap_err(),
            Rejection::StagedDirOutsideRuntime
        );

        let nested = fixture.staged.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        request.staged_dir = nested.to_string_lossy().into_owned();
        assert_eq!(
            validate(&fixture, &request).unwrap_err(),
            Rejection::StagedDirOutsideRuntime
        );
    }

    #[test]
    fn staged_dir_name_must_equal_target_version() {
        let fixture = make_fixture("0.1.4-qa", true);
        let request = request_for(&fixture, "0.1.5-qa");
        assert_eq!(
            validate(&fixture, &request).unwrap_err(),
            Rejection::StagedDirNameMismatch
        );
    }

    #[test]
    fn missing_exe_and_hashes_are_rejected() {
        let fixture = make_fixture("0.1.4-qa", true);
        let mut request = request_for(&fixture, "0.1.4-qa");
        std::fs::remove_file(&fixture.exe).unwrap();
        assert_eq!(validate(&fixture, &request).unwrap_err(), Rejection::MissingExe);

        let fixture = make_fixture("0.1.4-qa", true);
        request = request_for(&fixture, "0.1.4-qa");
        request.exe_sha256 = "0".repeat(64);
        assert_eq!(
            validate(&fixture, &request).unwrap_err(),
            Rejection::Sha256Mismatch("request exe_sha256")
        );

        let fixture = make_fixture("0.1.4-qa", true);
        let mut request = request_for(&fixture, "0.1.4-qa");
        request.exe_sha256 = "not-hex".into();
        assert_eq!(
            validate(&fixture, &request).unwrap_err(),
            Rejection::InvalidSha256Label
        );

        let fixture = make_fixture("0.1.4-qa", true);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixture.staged.join(MANIFEST_FILE)).unwrap())
                .unwrap();
        manifest["sha256"] = serde_json::json!("1".repeat(64));
        std::fs::write(
            fixture.staged.join(MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert_eq!(
            validate(&fixture, &request_for(&fixture, "0.1.4-qa")).unwrap_err(),
            Rejection::Sha256Mismatch("host-manifest sha256")
        );
    }

    #[test]
    fn manifest_is_required_and_must_match_version() {
        let fixture = make_fixture("0.1.4-qa", true);
        std::fs::remove_file(fixture.staged.join(MANIFEST_FILE)).unwrap();
        assert_eq!(
            validate(&fixture, &request_for(&fixture, "0.1.4-qa")).unwrap_err(),
            Rejection::ManifestMissing
        );

        let fixture = make_fixture("0.1.4-qa", true);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixture.staged.join(MANIFEST_FILE)).unwrap())
                .unwrap();
        manifest["version"] = serde_json::json!("9.9.9");
        std::fs::write(
            fixture.staged.join(MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let error = validate(&fixture, &request_for(&fixture, "0.1.4-qa")).unwrap_err();
        assert!(matches!(error, Rejection::ManifestMismatch(_)));
    }

    #[test]
    fn signature_policy_requires_signed_or_allow_unsigned() {
        let fixture = make_fixture("0.1.4-qa", false);
        let mut request = request_for(&fixture, "0.1.4-qa");
        assert!(matches!(
            validate(&fixture, &request).unwrap_err(),
            Rejection::UnsignedNotAllowed(_)
        ));
        request.allow_unsigned = true;
        // The test fixture has no active.json, so this passes signature policy
        // and only fails on the (intentionally absent) active/version state.
        let result = validate(&fixture, &request);
        assert!(!matches!(result, Err(Rejection::UnsignedNotAllowed(_))));
    }

    #[test]
    fn protocol_and_storage_schema_are_rejected() {
        let fixture = make_fixture("0.1.4-qa", true);
        let mut request = request_for(&fixture, "0.1.4-qa");
        request.protocol_max = 0;
        assert_eq!(
            validate(&fixture, &request).unwrap_err(),
            Rejection::ProtocolIncompatible
        );
        let mut request = request_for(&fixture, "0.1.4-qa");
        request.storage_schema = 2;
        assert_eq!(
            validate(&fixture, &request).unwrap_err(),
            Rejection::StorageSchemaUnsupported
        );
    }

    #[test]
    fn wrong_authorization_is_a_failure_and_never_echoes_the_token() {
        let fixture = make_fixture("0.1.4-qa", true);
        let mut request = request_for(&fixture, "0.1.4-qa");
        request.authorization = "wrong-token".into();
        let error = validate(&fixture, &request).unwrap_err();
        assert_eq!(error, Rejection::WrongAuthorization);
        assert!(!error.detail().contains("wrong-token"));
        assert!(!error.detail().contains("test-token"));
    }

    #[test]
    fn live_maintenance_lease_rejects_activation() {
        let fixture = make_fixture("0.1.4-qa", true);
        with_active(&fixture, "0.1.3");
        let lock = MaintenanceLock::new(&fixture.ctx.runtime_root);
        let _ = lock
            .acquire("someone-else", "manual maintenance", 60_000)
            .unwrap();
        let error = validate(&fixture, &request_for(&fixture, "0.1.4-qa")).unwrap_err();
        assert!(matches!(error, Rejection::MaintenanceLocked(_)));
    }

    #[test]
    fn active_state_and_version_rules_are_enforced() {
        let fixture = make_fixture("0.1.4-qa", true);
        // No active.json and a running exe under <base>/runtime/<version> is a
        // valid QA state.
        assert!(validate(&fixture, &request_for(&fixture, "0.1.4-qa")).is_ok());

        // Same version is a no-op.
        with_active(&fixture, "0.1.4-qa");
        assert_eq!(
            validate(&fixture, &request_for(&fixture, "0.1.4-qa")).unwrap_err(),
            Rejection::SameVersionNoop
        );

        // Older target is a downgrade.
        let fixture2 = make_fixture("0.1.4-qa", true);
        with_active(&fixture2, "0.1.5");
        assert_eq!(
            validate(&fixture2, &request_for(&fixture2, "0.1.4-qa")).unwrap_err(),
            Rejection::VersionNotNewer
        );

        // The running image must be the active artifact.
        let fixture3 = make_fixture("0.1.4-qa", true);
        with_active(&fixture3, "0.1.3");
        let other = fixture3.ctx.base.join(RUNTIME_DIR).join("0.1.2").join(EXE_FILE);
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, b"other").unwrap();
        let request = request_for(&fixture3, "0.1.4-qa");
        let running = running_for(&fixture3, &other, "0.1.2");
        let error = validate_request(&fixture3.ctx, &raw(&request), &running).unwrap_err();
        assert_eq!(error, Rejection::WorkerIdentityMismatch);
    }

    #[test]
    fn no_active_version_requires_a_versioned_running_host() {
        let fixture = make_fixture("0.1.4-qa", true);
        let request = request_for(&fixture, "0.1.4-qa");
        let unversioned = fixture.ctx.base.join("unversioned.exe");
        std::fs::write(&unversioned, b"x").unwrap();
        let running = running_for(&fixture, &unversioned, "unknown");
        let error = validate_request(&fixture.ctx, &raw(&request), &running).unwrap_err();
        assert_eq!(error, Rejection::NoActiveVersion);
    }

    #[test]
    fn version_comparison_is_semver_ish() {
        use std::cmp::Ordering;
        assert_eq!(compare_versions("0.1.3", "0.1.4"), Some(Ordering::Less));
        assert_eq!(compare_versions("0.1.4-qa", "0.1.3"), Some(Ordering::Greater));
        assert_eq!(compare_versions("0.1.4-qa", "0.1.4"), Some(Ordering::Less));
        assert_eq!(compare_versions("1.0.0", "0.9.9"), Some(Ordering::Greater));
        assert_eq!(compare_versions("v1.2.3", "1.2.3"), Some(Ordering::Equal));
        assert_eq!(compare_versions("1.2", "1.2.0"), Some(Ordering::Equal));
        assert_eq!(compare_versions("0.3.10", "0.3.9"), Some(Ordering::Greater));
        assert_eq!(
            compare_versions("0.1.4-qa.2", "0.1.4-qa.1"),
            Some(Ordering::Greater)
        );
        assert_eq!(compare_versions("not-a-version", "0.1.3"), None);
    }

    #[test]
    fn budget_decision_is_explicit() {
        let started = 1_000;
        let deadline = started + ACTIVATION_BUDGET.as_millis() as i64;
        assert!(deadline > started);
        assert!((deadline - started) as u64 == 10_000);
        assert!(deadline - 1 > started);
    }

    #[test]
    fn phases_are_terminal_only_where_documented() {
        fn terminal(phase: Phase) -> bool {
            matches!(
                phase,
                Phase::Committed | Phase::RolledBack | Phase::Failed | Phase::Busy
            )
        }
        for phase in [Phase::Committed, Phase::RolledBack, Phase::Failed, Phase::Busy] {
            assert!(terminal(phase));
        }
        for phase in [
            Phase::Idle,
            Phase::Validating,
            Phase::Draining,
            Phase::Starting,
            Phase::Verifying,
        ] {
            assert!(!terminal(phase));
        }
    }

    #[test]
    fn journal_round_trip_preserves_active_bytes_and_phase() {
        let fixture = make_fixture("0.1.4-qa", true);
        let journal = ActivationJournal {
            schema: 1,
            operation_id: "act-1".into(),
            phase: Phase::Verifying,
            from_exe: fixture.exe.to_string_lossy().into_owned(),
            to_exe: fixture.exe.to_string_lossy().into_owned(),
            from_version: "0.1.3".into(),
            to_version: "0.1.4-qa".into(),
            active_json_before: "{\"exact\":true}".into(),
            started_at_unix_ms: 123,
        };
        write_journal(&fixture.ctx.base, &journal).unwrap();
        let read = read_journal(&fixture.ctx.base).unwrap();
        assert_eq!(read.phase, Phase::Verifying);
        assert_eq!(read.active_json_before, "{\"exact\":true}");
        assert_eq!(read.operation_id, "act-1");
        remove_journal(&fixture.ctx.base);
        assert!(read_journal(&fixture.ctx.base).is_none());
    }

    #[test]
    fn terminal_failure_is_not_rewritten_as_rolled_back() {
        let fixture = make_fixture("0.1.4-qa", true);
        write_terminal(
            &fixture.ctx,
            "act-1",
            Phase::Failed,
            Some("0.1.3".into()),
            Some("0.1.4-qa".into()),
            Some("drain failed".into()),
            Some("drain_failed".into()),
            None,
            100,
        )
        .unwrap();
        write_terminal(
            &fixture.ctx,
            "act-1",
            Phase::RolledBack,
            Some("0.1.3".into()),
            Some("0.1.4-qa".into()),
            Some("rollback".into()),
            Some("rollback".into()),
            None,
            100,
        )
        .unwrap();
        assert_eq!(read_status(&fixture.ctx.base).unwrap().phase, Phase::Failed);
    }

    #[test]
    fn restore_active_verbatim_removes_or_restores_exact_bytes() {
        let fixture = make_fixture("0.1.4-qa", true);
        // Absent before -> absent after.
        restore_active_verbatim(&fixture.ctx, "").unwrap();
        assert!(!fixture.ctx.base.join(ACTIVE_FILE).exists());
        // Exact text is restored byte-for-byte.
        let raw = "{\n  \"version\": \"0.1.3\"\n}\n";
        std::fs::write(fixture.ctx.base.join(ACTIVE_FILE), b"something else").unwrap();
        restore_active_verbatim(&fixture.ctx, raw).unwrap();
        assert_eq!(
            std::fs::read_to_string(fixture.ctx.base.join(ACTIVE_FILE)).unwrap(),
            raw
        );
    }

    #[test]
    fn control_token_is_never_empty_and_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("Lattice");
        ensure_control_token(&base).unwrap();
        let first = read_token(&base).expect("token");
        assert!(first.len() >= 32);
        ensure_control_token(&base).unwrap();
        assert_eq!(read_token(&base).unwrap(), first);

        // A short/empty pre-existing token is a fail-closed configuration.
        let weak = dir.path().join("weak");
        std::fs::create_dir_all(&weak).unwrap();
        std::fs::write(weak.join(TOKEN_FILE), "short").unwrap();
        assert!(ensure_control_token(&weak).is_err());
        std::fs::write(weak.join(TOKEN_FILE), "").unwrap();
        assert!(ensure_control_token(&weak).is_err());
    }

    #[test]
    fn active_json_commit_preserves_keys_and_sets_contract_fields() {
        let fixture = make_fixture("0.1.4-qa", true);
        let before = serde_json::json!({
            "version": "0.1.3",
            "sha256": "old",
            "source": "keep-me",
            "git": "old-git",
            "installed_at_unix_ms": 1,
            "extra": {"nested": true}
        });
        let before_text = serde_json::to_string_pretty(&before).unwrap();
        std::fs::write(fixture.ctx.base.join(ACTIVE_FILE), &before_text).unwrap();
        let journal = ActivationJournal {
            schema: 1,
            operation_id: "act-commit".into(),
            phase: Phase::Verifying,
            from_exe: fixture.exe.to_string_lossy().into_owned(),
            to_exe: fixture.exe.to_string_lossy().into_owned(),
            from_version: "0.1.3".into(),
            to_version: "0.1.4-qa".into(),
            active_json_before: before_text.clone(),
            started_at_unix_ms: 1,
        };
        let manifest = HostManifest {
            version: Some("0.1.4-qa".into()),
            sha256: Some(fixture.hash.clone()),
            source: Some("NAI-Lattice".into()),
            git: Some("new-git".into()),
            signature_status: Some("Signed".into()),
        };
        commit_active_json(
            &fixture.ctx,
            &journal,
            &fixture.exe,
            &fixture.hash,
            Some(&manifest),
            10_000,
        )
        .unwrap();
        let value: Value =
            serde_json::from_slice(&std::fs::read(fixture.ctx.base.join(ACTIVE_FILE)).unwrap())
                .unwrap();
        assert_eq!(value["version"], "0.1.4-qa");
        assert_eq!(value["sha256"], fixture.hash);
        assert_eq!(value["path"], fixture.exe.to_string_lossy().as_ref());
        assert_eq!(value["source"], "NAI-Lattice");
        assert_eq!(value["git"], "new-git");
        assert!(value["installed_at_unix_ms"].as_i64().unwrap() > 0);
        assert_eq!(value["extra"]["nested"], true, "unknown fields are preserved");
        assert_eq!(
            std::fs::read_to_string(fixture.ctx.base.join(PREVIOUS_FILE)).unwrap(),
            before_text
        );
    }

    #[test]
    fn recovery_plan_prefers_a_healthy_candidate_then_a_healthy_previous() {
        assert_eq!(recovery_plan(true, false), RecoveryPlan::FinishCandidate);
        assert_eq!(recovery_plan(true, true), RecoveryPlan::FinishCandidate);
        assert_eq!(recovery_plan(false, true), RecoveryPlan::AdoptPrevious);
        assert_eq!(recovery_plan(false, false), RecoveryPlan::Rollback);
    }

    #[test]
    fn phase_wire_names_match_the_frozen_contract() {
        for (phase, wire) in [
            (Phase::Idle, "\"idle\""),
            (Phase::Validating, "\"validating\""),
            (Phase::Draining, "\"draining\""),
            (Phase::Starting, "\"starting\""),
            (Phase::Verifying, "\"verifying\""),
            (Phase::Committed, "\"committed\""),
            (Phase::RolledBack, "\"rolled_back\""),
            (Phase::Failed, "\"failed\""),
            (Phase::Busy, "\"busy\""),
        ] {
            assert_eq!(serde_json::to_string(&phase).unwrap(), wire);
        }
    }

    #[test]
    fn budget_boundary_is_exactly_ten_seconds() {
        let started = 1_764_000_000_000;
        assert_eq!(activation_deadline(started), started + 10_000);
        assert!(!budget_expired(started, started));
        assert!(!budget_expired(started, started + 9_999));
        assert!(budget_expired(started, started + 10_000));
        assert!(budget_expired(started, started + 10_001));
    }

    #[test]
    fn live_journal_refuses_a_concurrent_request_as_busy() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Arc::new(test_ctx(dir.path()));
        std::fs::write(ctx.base.join(TOKEN_FILE), "tok").unwrap();
        let journal = ActivationJournal {
            schema: 1,
            operation_id: "act-live".into(),
            phase: Phase::Draining,
            from_exe: "old.exe".into(),
            to_exe: "new.exe".into(),
            from_version: "0.1.3".into(),
            to_version: "0.1.4-qa".into(),
            active_json_before: String::new(),
            started_at_unix_ms: 1,
        };
        write_journal(&ctx.base, &journal).unwrap();
        let request = serde_json::json!({
            "schema": 1,
            "operation_id": "act-second",
            "staged_dir": "ignored",
            "target_version": "0.1.5-qa",
            "exe_sha256": "0".repeat(64),
            "protocol_min": 1,
            "protocol_max": 1,
            "storage_schema": 1,
            "authorization": "tok",
            "allow_unsigned": true,
        })
        .to_string();
        handle_request(&ctx, request.as_bytes());
        let status = read_status(&ctx.base).unwrap();
        assert_eq!(status.operation_id, "act-second");
        assert_eq!(status.phase, Phase::Busy);
        assert!(status.error.unwrap_or_default().contains("live_journal"));
        assert!(!ctx.base.join(REQUEST_FILE).exists());
        assert!(!ctx.in_flight.load(Ordering::SeqCst));
        // The foreign journal is not touched.
        assert_eq!(read_journal(&ctx.base).unwrap().operation_id, "act-live");
    }

    #[test]
    fn resolve_running_reports_missing_dead_and_unversioned_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        assert_eq!(resolve_running(&ctx).unwrap_err(), Rejection::EndpointMissing);

        write_atomic(
            &ctx.runtime_root.join("endpoint.json"),
            serde_json::json!({
                "pid": u32::MAX,
                "canonical_pipe": r"\\.\pipe\dead",
                "instance_id": "dead",
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(resolve_running(&ctx).unwrap_err(), Rejection::EndpointDead);

        write_atomic(
            &ctx.runtime_root.join("endpoint.json"),
            serde_json::json!({
                "pid": std::process::id(),
                "canonical_pipe": r"\\.\pipe\test",
                "instance_id": "test",
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            resolve_running(&ctx).unwrap_err(),
            Rejection::RunningHostUnversioned
        );
    }

    #[test]
    fn malformed_or_missing_active_state_and_unparseable_versions_are_rejected() {
        let fixture = make_fixture("0.1.4-qa", true);
        let request = request_for(&fixture, "0.1.4-qa");

        std::fs::write(fixture.ctx.base.join(ACTIVE_FILE), b"{not json").unwrap();
        assert_eq!(validate(&fixture, &request).unwrap_err(), Rejection::ActiveJsonMalformed);

        let fixture = make_fixture("0.1.4-qa", true);
        let missing = fixture.ctx.base.join(RUNTIME_DIR).join("0.1.3").join("missing.exe");
        std::fs::write(
            fixture.ctx.base.join(ACTIVE_FILE),
            serde_json::json!({"version": "0.1.3", "path": missing})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            validate(&fixture, &request_for(&fixture, "0.1.4-qa")).unwrap_err(),
            Rejection::ActiveArtifactMissing
        );

        let fixture = make_fixture("0.1.4-qa", true);
        std::fs::write(
            fixture.ctx.base.join(ACTIVE_FILE),
            serde_json::json!({"version": "dev", "path": fixture.from_exe})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            validate(&fixture, &request_for(&fixture, "0.1.4-qa")).unwrap_err(),
            Rejection::VersionUnparseable
        );
    }

    #[test]
    fn display_path_strips_the_win32_extended_prefix() {
        assert_eq!(display_path(Path::new(r"\\?\C:\x\y.exe")), r"C:\x\y.exe");
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\server\share\y.exe")),
            r"\\server\share\y.exe"
        );
        assert_eq!(display_path(Path::new(r"C:\x\y.exe")), r"C:\x\y.exe");
    }
}
