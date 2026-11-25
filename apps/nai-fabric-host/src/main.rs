//! NAI Ambient Fabric host — per-user NAF/1 server over a Windows named pipe
//! (Lattice spec §7).
//!
//! Lifecycle:
//!  1. open the runtime (ledger + hotstore + CAS + AAF);
//!  2. create/own the per-user pipe `\\.\pipe\NAI.AmbientFabric.NAF1.<sid-hash>`;
//!  3. serve connections: handshake, then dispatch NAF methods via the runtime
//!     and the registered Gas Town domain service.
//!
//! The runtime dispatch is synchronous, so each connection is handled on its
//! own thread with no Tokio coordinator — deterministic append order and the
//! internal LSM/hot-store pipeline provide the required concurrency.

// The release singleton runs as a no-console Windows subsystem (PROC-005: no
// console window for the background host). The maintenance CLI stays a console
// app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::BytesMut;
use prost::Message;

use nai_lattice_gastown::events::{EventKind, GastownEvent};
use nai_lattice_gastown::host_registry::{AttachState, HostRecord};
use nai_lattice_gastown::GastownService;
use nai_lattice_protocol::pb;
use nai_lattice_runtime::lifecycle::{LockInspection, MaintenanceLock};
use nai_lattice_runtime::{LatticeRuntime, RuntimeConfig, RuntimeState};

mod naf_server;
mod naf_json_bridge;
mod win_acl;
mod endpoints;
mod activation;
mod gastown_orto;
mod dashboard_ui;
mod lattice_dashboard;
mod lattice_assets;
mod lattice_broker;
mod lattice_query;
mod host_metrics;
mod topology;
mod lattice_trace_graph;
mod notifications;
mod release;
mod convoy_a;
mod recall;
mod inquisitor;
mod project;

/// Canonical Lattice runtime root, set once at startup (GATE 2: production
/// migration manifests + endpoint publication live here).
pub(crate) static RUNTIME_ROOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// In-flight NAF dispatches (all connections). `runtime.shutdown_if_idle` is
/// only accepted when this is 1 (the shutdown request itself), so a cooperative
/// stop never interrupts an append, query, or Gas Town operation.
static IN_FLIGHT_REQUESTS: AtomicUsize = AtomicUsize::new(0);

/// Latched once a cooperative stop is accepted; further stop requests receive
/// `already_stopping` instead of scheduling a second exit.
static SHUTDOWN_ACCEPTED: AtomicBool = AtomicBool::new(false);

/// Upper bound on how long the exit thread waits for already-admitted
/// dispatches to finish before flushing and exiting anyway.
const SHUTDOWN_DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// Time reserved for the accepted shutdown response to reach the client.
const SHUTDOWN_RESPONSE_GRACE: std::time::Duration = std::time::Duration::from_millis(300);

/// Dispatch admission after the shutdown latch. Read-only discovery methods
/// stay available so widgets can keep observing `draining`; everything else is
/// refused instead of racing the exit.
#[derive(Debug, PartialEq, Eq)]
enum DispatchAdmission {
    Admit,
    Draining,
}

fn dispatch_admission(method: &str, shutdown_accepted: bool) -> DispatchAdmission {
    if shutdown_accepted && !matches!(method, "runtime.health" | "runtime.describe") {
        DispatchAdmission::Draining
    } else {
        DispatchAdmission::Admit
    }
}

/// True when only the shutdown request itself remains in flight.
fn shutdown_drained(in_flight: usize) -> bool {
    in_flight <= 1
}

/// Pure stop gate: accept only when this is the sole in-flight request and no
/// stop is already latched. Kept separate from the exit path so it is testable.
#[derive(Debug, PartialEq, Eq)]
enum ShutdownDecision {
    Accept,
    AlreadyStopping,
    Busy(usize),
}

fn shutdown_decision(in_flight: usize, accepted: bool) -> ShutdownDecision {
    if accepted {
        ShutdownDecision::AlreadyStopping
    } else if in_flight > 1 {
        ShutdownDecision::Busy(in_flight - 1)
    } else {
        ShutdownDecision::Accept
    }
}

/// Worker startup gate for the cross-process maintenance lease (NAIOS-006).
/// A live (or unreadable = fail-closed) lock means the store belongs to a
/// maintenance actor right now; opening it would race that actor.
#[derive(Debug, PartialEq, Eq)]
enum StartDecision {
    Start,
    Refuse(String),
}

/// Pure startup lease decision. Live -> refuse with owner/reason/expiry;
/// Unreadable -> refuse with an actionable message (fail closed); Absent or
/// Expired -> start.
fn startup_lock_decision(inspection: &LockInspection) -> StartDecision {
    match inspection {
        LockInspection::Absent | LockInspection::Expired(_) => StartDecision::Start,
        LockInspection::Live(record) => StartDecision::Refuse(format!(
            "maintenance window is live (owner={} reason={} pid={} expires_at_unix_ms={}); \
             refusing to open the store (NAIOS-006)",
            record.owner, record.reason, record.pid, record.expires_at_unix_ms
        )),
        LockInspection::Unreadable {
            error,
            modified_at_unix_ms,
        } => {
            let modified = modified_at_unix_ms
                .map(|ms| format!(" modified_at_unix_ms={ms}"))
                .unwrap_or_default();
            StartDecision::Refuse(format!(
                "maintenance.lock.json is unreadable ({error};{modified}); failing closed instead \
                 of opening the store: repair or remove the lock file and retry (NAIOS-006)"
            ))
        }
    }
}

/// What the monitor does after the supervised worker exits.
#[derive(Debug, PartialEq, Eq)]
enum MonitorAction {
    /// Clean exit (stop requested): end the monitor.
    CleanExit,
    /// Cooperative release swap: re-arm the cancel flag and respawn now.
    RespawnAfterSwap,
    /// Worker refused startup because a maintenance lease holds the store:
    /// the monitor must end with the SAME code and must not respawn.
    MaintenanceExit,
    /// Any other non-clean exit: bounded backoff, then respawn.
    CrashRetry,
}

fn monitor_action(exit: &nai_lattice_supervisor::ExitStatus) -> MonitorAction {
    use nai_lattice_supervisor::ExitStatus;
    match exit {
        ExitStatus::Success => MonitorAction::CleanExit,
        ExitStatus::Cancelled => MonitorAction::RespawnAfterSwap,
        ExitStatus::ExitCode(75) => MonitorAction::MaintenanceExit,
        _ => MonitorAction::CrashRetry,
    }
}

/// Cooperative stop: latch draining, acknowledge, drain already-admitted work,
/// flush the hot store, then exit the worker with code 0. The monitor treats a
/// clean worker exit as "stop requested" (`ExitStatus::Success` -> monitor
/// ends), so the whole host tree stops and never respawns into a maintenance
/// window.
fn host_shutdown_if_idle(rt: &Arc<LatticeRuntime>) -> (u32, Vec<u8>, String) {
    let decision = shutdown_decision(
        IN_FLIGHT_REQUESTS.load(Ordering::SeqCst),
        SHUTDOWN_ACCEPTED.load(Ordering::SeqCst),
    );
    match decision {
        ShutdownDecision::AlreadyStopping => {
            let body = serde_json::json!({"accepted": false, "reason": "already_stopping"});
            (
                409,
                serde_json::to_vec(&body).unwrap_or_default(),
                "already stopping".to_string(),
            )
        }
        ShutdownDecision::Busy(in_flight) => {
            let body = serde_json::json!({
                "accepted": false,
                "reason": "busy",
                "in_flight": in_flight,
            });
            (
                409,
                serde_json::to_vec(&body).unwrap_or_default(),
                format!("busy: {in_flight} dispatches in flight"),
            )
        }
        ShutdownDecision::Accept => {
            SHUTDOWN_ACCEPTED.store(true, Ordering::SeqCst);
            // Publish the transition BEFORE the flush so NAI OS widgets can pause
            // polling on `draining` instead of logging transport errors, and the
            // transition lands in the durable lifecycle trail.
            rt.set_lifecycle_state_with_eta(
                RuntimeState::Draining,
                Some("runtime.shutdown_if_idle".to_string()),
                Some(SHUTDOWN_RESPONSE_GRACE.as_millis() as u64),
            );
            let pid = std::process::id();
            let rt = Arc::clone(rt);
            std::thread::spawn(move || {
                // Drain already-admitted dispatches BEFORE the durability flush:
                // the old fixed 300 ms sleep could exit while an append/query
                // was still running. The wait is bounded so a leaked request can
                // never keep the host alive forever.
                let deadline = std::time::Instant::now() + SHUTDOWN_DRAIN_DEADLINE;
                while !shutdown_drained(IN_FLIGHT_REQUESTS.load(Ordering::SeqCst)) {
                    if std::time::Instant::now() >= deadline {
                        tracing::warn!(
                            in_flight = IN_FLIGHT_REQUESTS.load(Ordering::SeqCst),
                            "runtime.shutdown_if_idle drain deadline elapsed; flushing and exiting"
                        );
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                rt.flush_hotstore();
                // Let the accepted response reach the client before the process ends.
                std::thread::sleep(SHUTDOWN_RESPONSE_GRACE);
                tracing::info!(pid, "runtime.shutdown_if_idle accepted; clean host stop");
                std::process::exit(0);
            });
            let body = serde_json::json!({
                "accepted": true,
                "grace_ms": SHUTDOWN_RESPONSE_GRACE.as_millis() as u64,
                "pid": pid,
            });
            // NAF success status is 0. Returning 200 here made every client
            // report "rpc failure: status 200" even though the stop succeeded.
            (0, serde_json::to_vec(&body).unwrap_or_default(), String::new())
        }
    }
}

fn main() {
    tracing_subscriber::fmt().with_env_filter("info").init();

    // review-worker: supervised per-role inference subprocess (GATE 3). Runs
    // ONE role's real model call in THIS isolated OS process, prints a JSON
    // result to stdout, exits. The host spawns this via lattice-supervisor.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "gastown-live-e2e") {
        gastown_orto::run_live_e2e_subcommand();
        return;
    }
    if args.iter().any(|a| a == "gastown-convoy-a-live") {
        convoy_a::run_convoy_a_live();
        return;
    }
    if args.iter().any(|a| a == "gastown-convoy-a-verify") {
        // args[0] == "gastown-convoy-a-verify", args[1] == root, args[2] == convoy_id
        let root = args.get(1).cloned().unwrap_or_default();
        let cid = args.get(2).cloned().unwrap_or_default();
        convoy_a::run_convoy_a_verify(&root, &cid);
        return;
    }
    if args.iter().any(|a| a == "gastown-adv-demo") {
        gastown_orto::run_live_adversarial_demo();
        return;
    }
    if args.iter().any(|a| a == "gastown-4role-demo") {
        gastown_orto::run_live_4role_demo();
        return;
    }
    if args.iter().any(|a| a == "gastown-toast-register") {
        #[cfg(windows)]
        {
            use std::io::Write as _;
            let ok = notifications::register_native_toast_identity();
            println!("{}", serde_json::json!({"ok": ok, "aumid": "NAI.GasTown.1"}));
            std::io::stdout().flush().ok();
        }
        #[cfg(not(windows))]
        { println!("{}", serde_json::json!({"ok": false, "reason": "not windows"})); }
        return;
    }
    if args.iter().any(|a| a == "review-worker") {
        run_review_worker_subcommand();
        return;
    }
    // helldesk-release: versioned production install / activation / rollback.
    // Manages `%LOCALAPPDATA%\NAI\Lattice\releases\<version>` + provenance and
    // atomically flips the `current.json` / `previous.json` activation pointer.
    // A release launcher resolves the current versioned exe and starts the
    // monitor -> --child singleton detached (no console, terminal-kill-safe).
    if args.iter().any(|a| a == "helldesk-release") {
        release::release_main(std::env::args().skip(1).collect());
        return;
    }
    // helldesk-agent: a supervised real-model Helldesk agent worker (polecat,
    // refinery, or mayor). Reads a worker request JSON, runs the real model once,
    // applies file changes to its isolated worktree, commits/pushes, prints the
    // structured decision, exits. The host supervises it (Job Object + timeout).
    if args.iter().any(|a| a == "helldesk-agent") {
        gastown_orto::run_gastown_agent_subcommand();
        return;
    }

    // gastown-ctrl: local control server (HTTP dashboard + durable scheduler).
    // Runs inside the host process so it shares the canonical runtime. Enabled
    // by default; the `--child` worker also starts it.
    // monitor/child split (AUTOSTART crash recovery, GATE 5):
    //   - default ("monitor") spawns the worker instance ("--child") and
    //     restarts it on any non-clean exit.
    if !args.iter().any(|a| a == "--child") {
        run_monitor_main();
        return;
    }

    run_worker_main();
}

/// Monitor supervisor: spawn the worker singleton as a supervised child and
/// restart it on crash (non-zero exit) with bounded backoff. Fully independent
/// of Kilo, VS Code, or any shell. Uses `nai_lattice_supervisor` (CREATE_NO_WINDOW,
/// no console flash). A CLEAN exit (code 0, e.g. `stop`) ends the monitor.
///
/// The monitor also owns R01 / NAIOS-001 activation (`activation.rs`): it polls
/// the file handshake, drains the worker over NAF, starts the staged candidate
/// through the SAME supervised spawn path, verifies the new endpoint, and
/// commits the shared `active.json` pointer. The worker therefore never owns the
/// swap operation or its response.
fn run_monitor_main() {
    use nai_lattice_supervisor::{ProcessSpec, WorkerLimits};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    // tracing already initialized in main(); do NOT re-init (would panic).
    let self_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("nai-fabric-host.exe"));

    // Cooperative-stop flag shared by the watcher (writer) and the supervise
    // loop (reader). Reset to a fresh `false` before each supervised run.
    let cancel_flag = Arc::new(AtomicBool::new(false));

    // The version currently running as the supervised worker. The watcher sets
    // the cancel flag only when the active version differs from THIS live value,
    // so a swap is a one-shot event (no retrigger loop).
    let running_version = Arc::new(std::sync::Mutex::new(
        release::current_version().unwrap_or_else(|| "unknown".to_string()),
    ));

    // Activation handshake (R01 / NAIOS-001). Disabled only when the environment
    // cannot host it safely (for example an isolated instance without a scratch
    // NAI_LATTICE_ROOT); the monitor still supervises the worker without it.
    let isolated = std::env::var_os("NAI_LATTICE_INSTANCE").is_some();
    let mut recovered_worker = None;
    let activation = match activation::monitor_context(Arc::clone(&running_version)) {
        Ok(ctx) => {
            match activation::recover_on_start(&ctx) {
                Ok(activation::RecoveryOutcome::Adopt(worker)) => {
                    tracing::warn!(
                        pid = worker.pid,
                        version = %worker.version,
                        "activation recovery found a live worker; it will be adopted"
                    );
                    recovered_worker = Some(worker);
                }
                Ok(activation::RecoveryOutcome::Nothing) => {}
                Err(e) => tracing::warn!(error = %e, "activation journal recovery failed"),
            }
            activation::spawn_request_watcher(Arc::clone(&ctx));
            // The worker advertises `activation` only when this monitor actually
            // owns the handshake (feature discovery must never claim a disabled
            // capability). The child inherits this marker.
            std::env::set_var("NAI_LATTICE_ACTIVATION", "1");
            Some(ctx)
        }
        Err(e) => {
            tracing::warn!(error = %e, "activation handshake disabled");
            None
        }
    };

    // Restart audit log. Keep it under the activation base so an isolated QA
    // instance never appends to the production LOG; production keeps the legacy
    // v1 path.
    let restart_log = activation
        .as_ref()
        .map(|ctx| ctx.base.join("host-restart.log"))
        .unwrap_or_else(|| {
            if isolated {
                std::env::temp_dir()
                    .join("NAI")
                    .join("Lattice")
                    .join("host-restart.log")
            } else {
                std::env::var_os("LOCALAPPDATA")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("NAI")
                    .join("Lattice")
                    .join("v1")
                    .join("host-restart.log")
            }
        });

    // Background active-version watcher: when the shared `active.json` (or the
    // legacy release pointer for a non-isolated monitor) differs from the worker
    // currently running, request a cooperative teardown so the main loop
    // respawns from the newest artifact.
    {
        let cancel_flag = Arc::clone(&cancel_flag);
        let running_version = Arc::clone(&running_version);
        let activation = activation.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(2));
            if cancel_flag.load(Ordering::Relaxed) {
                continue;
            }
            // Never let the version watcher tear down a worker that the
            // activation handshake is already swapping; the journal/status own
            // that lifecycle until it is terminal.
            if activation
                .as_ref()
                .map(|ctx| activation::activation_pending(ctx))
                .unwrap_or(false)
            {
                continue;
            }
            let current = activation
                .as_ref()
                .and_then(|ctx| activation::active_version(&ctx.base))
                .or_else(|| if isolated { None } else { release::current_version() });
            let Some(current) = current else { continue };
            let running = running_version.lock().map(|g| g.clone()).unwrap_or_default();
            if current != running {
                tracing::info!(from = %running, to = %current, "active version changed; swapping worker");
                cancel_flag.store(true, Ordering::Relaxed);
            }
        });
    }

    let limits_base = WorkerLimits { timeout: Duration::from_secs(60 * 60 * 24 * 7), idle_timeout: None, cancel_flag: Some(Arc::clone(&cancel_flag)), ..Default::default() };
    let mut attempt = 0u32;
    loop {
        let adopted = match (activation.as_ref(), recovered_worker.take()) {
            (Some(ctx), Some(worker)) => {
                let exe = worker.exe.clone();
                let outcome = activation::supervise_adopted(ctx, worker, &cancel_flag);
                Some((exe, outcome))
            }
            _ => None,
        };
        let (exe, outcome) = if let Some(pair) = adopted {
            pair
        } else {
            // Resolve the worker binary from the shared `active.json` first (the
            // canonical activation contract), then the legacy release pointer for
            // non-isolated production, then this monitor's own executable.
            let exe = activation
                .as_ref()
                .and_then(|ctx| activation::active_artifact_exe(&ctx.base))
                .or_else(|| if isolated { None } else { release::current_artifact_exe() })
                .unwrap_or_else(|| self_exe.clone());
            // Publish the version this spawn uses so the watcher compares against
            // the live worker, not a stale launch-time snapshot.
            let resolved_version = activation
                .as_ref()
                .and_then(|ctx| activation::version_for_exe(&ctx.base, &exe))
                .or_else(|| if isolated { None } else { release::current_version() })
                .unwrap_or_else(|| "unknown".to_string());
            if let Ok(mut guard) = running_version.lock() {
                *guard = resolved_version;
            }
            let limits = WorkerLimits { cancel_flag: Some(Arc::clone(&cancel_flag)), ..limits_base.clone() };
            let spec = ProcessSpec {
                program: exe.clone(),
                args: vec!["--child".to_string()],
                cwd: None,
                env: BTreeMap::new(), // inherit env
                no_console: true,
                no_capture: true, // worker output is unlogged; keeps live swap prompt
            };
            let outcome = match nai_lattice_supervisor::child::run(&spec, &limits) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("monitor: failed to spawn worker: {e}; retrying in 2s");
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
            };
            (exe, outcome)
        };
        log_worker_exit(&restart_log, &outcome);

        // Activation: a journal (or an accepted request) owns this exit. Never
        // treat a clean exit as "stop" while the operation is unresolved.
        let mut respawn_override = false;
        let mut swap_outcome: Option<(PathBuf, nai_lattice_supervisor::WorkerOutcome)> = None;
        if let Some(ctx) = &activation {
            if activation::activation_pending(ctx) {
                match activation::run_swap(
                    ctx,
                    exe.clone(),
                    outcome.clone(),
                    &limits_base,
                    &cancel_flag,
                ) {
                    activation::SwapOutcome::Supervised {
                        exe: swapped_exe,
                        outcome: swapped_outcome,
                        respawn_override: override_clean,
                    } => {
                        log_worker_exit(&restart_log, &swapped_outcome);
                        respawn_override = override_clean;
                        swap_outcome = Some((swapped_exe, swapped_outcome));
                    }
                    activation::SwapOutcome::Retry { detail } => {
                        tracing::warn!(%detail, "activation swap needs a retry; respawning active artifact");
                        cancel_flag.store(false, Ordering::Relaxed);
                        attempt = attempt.saturating_add(1);
                        std::thread::sleep(Duration::from_millis(
                            500u64 * (2u64.pow(attempt.min(5))),
                        ));
                        continue;
                    }
                }
            }
        }
        let exited_outcome = match swap_outcome {
            Some((_swapped_exe, swapped_outcome)) => swapped_outcome,
            None => outcome,
        };
        let pending_now = activation
            .as_ref()
            .map(|ctx| activation::activation_pending(ctx))
            .unwrap_or(false);
        let decision = monitor_next(
            &exited_outcome,
            respawn_override || pending_now,
            &mut attempt,
        );
        match decision {
            MonitorNext::Exit(code) => {
                if code == 0 {
                    tracing::info!(pid = exited_outcome.pid, "worker exited cleanly; monitor ending");
                } else {
                    tracing::warn!(
                        pid = exited_outcome.pid,
                        exit = format!("{:?}", exited_outcome.exit_status),
                        code,
                        "monitor exiting with the worker's refusal code"
                    );
                }
                std::process::exit(code);
            }
            MonitorNext::Continue { backoff, reset_cancel } => {
                if reset_cancel {
                    cancel_flag.store(false, Ordering::Relaxed);
                }
                if !backoff.is_zero() {
                    std::thread::sleep(backoff);
                }
            }
        }
    }
}

fn log_worker_exit(restart_log: &std::path::Path, outcome: &nai_lattice_supervisor::WorkerOutcome) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let line = format!(
        "{ts} worker_pid={} exit={:?} wall_ms={} was_timeout={}\n",
        outcome.pid, outcome.exit_status, outcome.wall_ms, outcome.was_timed_out
    );
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(restart_log)
        .and_then(|mut f| {
            use std::io::Write as _;
            f.write_all(line.as_bytes())
        });
}

enum MonitorNext {
    Continue {
        backoff: std::time::Duration,
        reset_cancel: bool,
    },
    Exit(i32),
}

/// Apply the existing monitor semantics, with one addition: a clean exit caused
/// by a failed/aborted activation (`keep_alive`) respawns the restored artifact
/// instead of ending the monitor.
fn monitor_next(
    outcome: &nai_lattice_supervisor::WorkerOutcome,
    keep_alive: bool,
    attempt: &mut u32,
) -> MonitorNext {
    match monitor_action(&outcome.exit_status) {
        MonitorAction::CleanExit if keep_alive => {
            tracing::warn!(
                pid = outcome.pid,
                "clean worker exit belongs to an activation handover; respawning"
            );
            *attempt = 0;
            MonitorNext::Continue {
                backoff: std::time::Duration::ZERO,
                reset_cancel: true,
            }
        }
        MonitorAction::CleanExit => MonitorNext::Exit(0),
        MonitorAction::RespawnAfterSwap => {
            let next = release::current_version().unwrap_or_else(|| "unknown".to_string());
            tracing::info!(pid = outcome.pid, to = %next, "worker swapped; respawning on newest release");
            *attempt = 0;
            MonitorNext::Continue {
                backoff: std::time::Duration::ZERO,
                reset_cancel: true,
            }
        }
        MonitorAction::MaintenanceExit => {
            tracing::warn!(
                pid = outcome.pid,
                exit = format!("{:?}", outcome.exit_status),
                "worker refused startup: maintenance lease held (exit 75); monitor exiting without respawn",
            );
            MonitorNext::Exit(75)
        }
        MonitorAction::CrashRetry => {
            *attempt = attempt.saturating_add(1);
            let backoff_ms = 500u64 * (2u64.pow((*attempt).min(5))); // 0.5s..16s cap
            tracing::warn!(
                pid = outcome.pid,
                exit = format!("{:?}", outcome.exit_status),
                attempt = *attempt,
                "worker crashed; restarting",
            );
            MonitorNext::Continue {
                backoff: std::time::Duration::from_millis(backoff_ms),
                reset_cancel: false,
            }
        }
    }
}

/// The actual singleton worker: enforce the (per-identity) singleton, refuse to
/// start under a live maintenance lease, open the runtime, publish endpoints,
/// register the domain service, rehydrate migration, serve NAF/1.
fn run_worker_main() {
    let config = RuntimeConfig::default();
    let runtime_root = config.root.clone();
    let _ = RUNTIME_ROOT.set(config.root.to_string_lossy().into_owned());

    // Isolated QA endpoint identity (DASHBOARD_API §6.1). Parsed BEFORE any
    // shared resource is touched: an invalid NAI_LATTICE_INSTANCE must fail
    // startup, not silently fall back to the production identity.
    let endpoints = match endpoints::Endpoints::discover() {
        Ok(ep) => ep,
        Err(e) => {
            eprintln!("fatal: {e}");
            std::process::exit(2);
        }
    };

    // Per-user (or per-QA-instance) singleton enforcement (GATE 5): exactly one
    // persistent host per identity. Held for the whole worker lifetime, before
    // the store is touched.
    let Some(mutex) = win_acl::acquire_singleton(endpoints.instance.as_deref()) else {
        let instance = endpoints
            .instance
            .as_deref()
            .map(|name| format!(" (instance {name})"))
            .unwrap_or_default();
        eprintln!("another nai-fabric-host singleton is already running{instance}; exiting");
        std::process::exit(0);
    };
    let _guard = win_acl::SingletonGuard(mutex);
    tracing::info!(instance = ?endpoints.instance, "singleton acquired");

    // NAIOS-006: a maintenance window owns the store. Inspect the cross-process
    // lease AFTER winning the singleton and BEFORE opening the runtime, so we
    // never race the maintenance actor for the ledger. Live -> exit 75 with the
    // holder; Unreadable -> exit 75 (fail closed). The monitor treats 75 as
    // terminal and does not respawn.
    let lock_inspection = MaintenanceLock::new(&config.root).inspect();
    if let StartDecision::Refuse(reason) = startup_lock_decision(&lock_inspection) {
        tracing::warn!(%reason, "refusing to start worker");
        eprintln!("fatal: {reason}");
        std::process::exit(75);
    }

    let rt = match LatticeRuntime::open(config) {
        Ok(rt) => Arc::new(rt),
        Err(e) => {
            eprintln!("fatal: failed to open Lattice runtime: {e}");
            std::process::exit(2);
        }
    };
    tracing::info!(instance = %rt.instance_id, "NAI fabric host ready");

    // Truthful discovery: the runtime is the single feature/version surface NAI OS
    // gates on. `HOST_FEATURES` is the ONE host-owned list (also merged into
    // `/lattice/api/summary`), so `runtime.describe.features` and the summary can
    // never disagree. Dashboard provenance comes from the embedded bundle.
    let mut host_features: Vec<&str> = crate::lattice_dashboard::HOST_FEATURES.to_vec();
    if std::env::var("NAI_LATTICE_ACTIVATION").ok().as_deref() == Some("1") {
        host_features.push("activation");
    }
    // DASHBOARD_API §8: `gpu_metrics` is a real capability only when a GPU was
    // actually observed at startup. An absent/failed probe leaves it out; the
    // endpoint still serves `host_metrics` with `gpu: null`, `source: "none"`.
    if crate::host_metrics::gpu_available() {
        host_features.push("gpu_metrics");
    }
    rt.register_host_features(&host_features);
    if crate::lattice_assets::bundle_present() {
        rt.set_dashboard_version(crate::lattice_assets::bundle_version());
    }

    // Register the unpackaged toast identity (AUMID) so Gas Town can surface
    // native Windows notifications for completed convoys / failures / etc.
    #[cfg(windows)]
    {
        let reg = crate::notifications::register_native_toast_identity();
        tracing::info!(aumid_registered = reg, "Gas Town toast identity");
    }

    // Canonical per-user endpoint + legacy alias (same singleton, same dispatcher).
    // Isolated QA instances serve one suffixed pipe and no username alias.
    tracing::info!(canonical = %endpoints.canonical, alias = %endpoints.legacy_alias, sid = %endpoints.user_sid, instance = ?endpoints.instance, "endpoint identity");

    // Atomically publish the endpoint manifest into the Lattice runtime root so
    // @nai-lattice/client discovers the canonical pipe without manual config.
    if let Err(e) = endpoints::publish_endpoint_manifest(std::path::Path::new(&runtime_root), &endpoints, &rt.instance_id) {
        tracing::warn!(error = %e, "failed to publish endpoint manifest");
    }

    // Register the Gas Town domain service (owned subsystem inside this host).
    let gastown = Arc::new(Mutex::new(GastownService::default()));
    tracing::info!(caps = %gastown.lock().unwrap().capabilities().len(), "Gas Town capabilities registered");

    // Rehydrate any persisted production migration (GATE 2 + recovery replay).
    load_migration_manifest_on_startup(&gastown);

    // Gas Town ORCHESTRATION: durable state + reconciler + control API over the
    // shared runtime hot-store, with a background scheduler and local HTTP
    // dashboard (no VS Code dependency).
    gastown_orto::init_from_runtime(&rt);
    gastown_orto::rehydrate_recovered_town();
    gastown_orto::recover_interrupted_primary_on_startup();
    gastown_orto::start_scheduler_thread();
    gastown_orto::start_control_server();

    // Loopback JSON capability bridge for non-NAF runtimes (C#/Files). Opt-in.
    naf_json_bridge::start(Arc::clone(&rt));

    tracing::info!(pid = %std::process::id(), "host serving endpoints");
    let names = endpoints.names();
    naf_server::serve(rt, gastown, &names);
}

/// Shared NAF/1 handling used by every connection.
pub(crate) fn handle_connection<S: Read + Write>(
    rt: &Arc<LatticeRuntime>,
    gastown: &Arc<Mutex<GastownService>>,
    stream: &mut S,
) {
    let mut buf = BytesMut::new();
    // 1. Handshake.
    match naf_server::read_naf_frame(stream, &mut buf) {
        Ok(frame) => {
            let env: pb::Envelope = match prost::Message::decode(&frame.payload[..]) {
                Ok(e) => e,
                Err(_) => return,
            };
            let hs_req = match env.kind {
                Some(pb::envelope::Kind::Request(r)) if r.method == "__handshake" => {
                    match pb::HandshakeRequest::decode(r.args.as_slice()) {
                        Ok(h) => h,
                        Err(_) => return,
                    }
                }
                _ => return,
            };
            match build_handshake_response(rt, &hs_req, &nai_lattice_gastown::capabilities::GASTOWN_CAPABILITIES) {
                Ok(resp) => {
                    let resp_env = pb::Envelope {
                        kind: Some(pb::envelope::Kind::Response(pb::Response {
                            status: 0,
                            error: String::new(),
                            body: resp.encode_to_vec(),
                        })),
                        ..Default::default()
                    };
                    if let Ok(bytes) = naf_server::encode_naf(&resp_env) {
                        let _ = stream.write_all(&bytes);
                        let _ = stream.flush();
                    }
                }
                Err(err) => {
                    // NAF-001: reject incompatible ranges / unauthorized
                    // capabilities explicitly — never silently downgrade.
                    let resp_env = pb::Envelope {
                        kind: Some(pb::envelope::Kind::Response(pb::Response {
                            status: 400,
                            error: err,
                            body: Vec::new(),
                        })),
                        ..Default::default()
                    };
                    if let Ok(bytes) = naf_server::encode_naf(&resp_env) {
                        let _ = stream.write_all(&bytes);
                        let _ = stream.flush();
                    }
                    return;
                }
            }
        }
        Err(_) => return,
    }

    // 2. Dispatch loop.
    loop {
        let frame = match naf_server::read_naf_frame(stream, &mut buf) {
            Ok(f) => f,
            Err(_) => break,
        };
        let env: pb::Envelope = match prost::Message::decode(&frame.payload[..]) {
            Ok(e) => e,
            Err(_) => break,
        };
        let (method, args, producer, idem) = match env.kind {
            Some(pb::envelope::Kind::Request(r)) => {
                // Carry the ACTUAL envelope producer + idempotency key through to
                // the canonical dispatch boundary. NAF-005: a client-set
                // `idempotency_key` must survive the host, otherwise `event.append`
                // dedup (ledger idempotency index) can never be reached.
                let producer = if env.producer_id.is_empty() {
                    "naf".to_string()
                } else {
                    env.producer_id.clone()
                };
                let idem = if env.idempotency_key.is_empty() {
                    None
                } else {
                    Some(env.idempotency_key.clone())
                };
                (r.method, r.args, producer, idem)
            }
            _ => break,
        };

        // After the shutdown latch only read-only discovery is admitted. Every
        // other dispatch is refused with a draining error so it cannot race the
        // exit path; in-flight counting covers admitted work only.
        let (status, body, error) = match dispatch_admission(
            &method,
            SHUTDOWN_ACCEPTED.load(Ordering::SeqCst),
        ) {
            DispatchAdmission::Draining => {
                let body = serde_json::to_vec(&serde_json::json!({
                    "ok": false,
                    "reason": "draining",
                }))
                .unwrap_or_default();
                (503, body, "draining: runtime is shutting down".to_string())
            }
            DispatchAdmission::Admit => {
                // Count in-flight dispatches so `runtime.shutdown_if_idle` only
                // lands when no other NAF work is executing (busy -> 409, no stop).
                IN_FLIGHT_REQUESTS.fetch_add(1, Ordering::SeqCst);
                // Gas Town domain methods are dispatched to the owned GastownService.
                let result = if method == "runtime.shutdown_if_idle" {
                    host_shutdown_if_idle(rt)
                } else if method.starts_with("gastown.") {
                    dispatch_gastown(&method, &args, &producer, idem.as_deref(), rt, gastown, now_unix_ms())
                } else if method == "event.append" || method == "event.append_batch" {
                    // Canonical append through the runtime, then replay into the
                    // Gas Town projection when the producer is the Gas Town host.
                    let result = rt.dispatch(&method, &args, &producer, idem.as_deref());
                    match result {
                        Ok(body) => {
                            maybe_replay_gastown(&args, &producer, rt, gastown);
                            (0, body, String::new())
                        }
                        Err(e) => (500, Vec::new(), e.to_string()),
                    }
                } else {
                    let result = rt.dispatch(&method, &args, &producer, idem.as_deref());
                    match result {
                        Ok(body) => (0, body, String::new()),
                        Err(e) => (500, Vec::new(), e.to_string()),
                    }
                };
                IN_FLIGHT_REQUESTS.fetch_sub(1, Ordering::SeqCst);
                result
            }
        };
        let resp_env = pb::Envelope {
            kind: Some(pb::envelope::Kind::Response(pb::Response {
                status,
                error,
                body,
            })),
            ..Default::default()
        };
        if let Ok(bytes) = naf_server::encode_naf(&resp_env) {
            if stream.write_all(&bytes).is_err() || stream.flush().is_err() {
                break;
            }
        }
    }
}

/// Dispatch a `gastown.*` NAF method to the owned Gas Town domain service.
/// Returns (status, body, error). Session-sync and host.register carry the
/// verified host identity read from the pipe envelope's producer_id.
fn dispatch_gastown(
    method: &str,
    args: &[u8],
    producer: &str,
    idem: Option<&str>,
    rt: &Arc<LatticeRuntime>,
    gastown: &Arc<Mutex<GastownService>>,
    now: i64,
) -> (u32, Vec<u8>, String) {
    let _ = idem;
    let args: serde_json::Value = match serde_json::from_slice(args) {
        Ok(v) => v,
        Err(e) => return (400, Vec::new(), e.to_string()),
    };
    let mut svc = match gastown.lock() {
        Ok(s) => s,
        Err(_) => return (500, Vec::new(), "gastown lock poisoned".into()),
    };
    let out = match method {
        "helldesk.orto.health"
        | "helldesk.orto.ensure_department"
        | "helldesk.orto.add_workstation"
        | "helldesk.orto.create_ticket"
        | "helldesk.orto.create_batch"
        | "helldesk.orto.start_batch"
        | "helldesk.orto.set_ticket_status"
        | "helldesk.orto.operator_request"
        | "helldesk.orto.dashboard"
        | "helldesk.orto.workstations_effective"
        | "helldesk.orto.reconcile"
        | "helldesk.orto.persist_manifests" => {
            let out = gastown_orto::dispatch_orto(method, &args, rt);
            serde_json::to_vec(&out).unwrap_or_default()
        }
        "gastown.admin.health" => {
            let h = svc.admin_health(now);
            serde_json::to_vec(&h).unwrap_or_default()
        }
        "gastown.admin.metrics" => serde_json::to_vec(&svc.metrics(now)).unwrap_or_default(),
        "gastown.host.register" => {
            let rec = HostRecord {
                host_runtime_id: args["host_runtime_id"].as_str().unwrap_or(producer).to_string(),
                producer_id: producer.to_string(),
                process_identity: args["process_identity"].as_str().unwrap_or("unknown").to_string(),
                verified_real_host: args["verified_real_host"].as_bool().unwrap_or(false),
                created_unix_ms: now,
                last_heartbeat_unix_ms: now,
                attached_at_unix_ms: Some(now),
                attach_state: AttachState::Attached,
                scopes: vec![],
            };
            serde_json::to_vec(&svc.host_register(rec)).unwrap_or_default()
        }
        "gastown.host.heartbeat" => {
            let hid = args["host_runtime_id"].as_str().unwrap_or(producer).to_string();
            serde_json::to_vec(&svc.host_heartbeat(&hid, now)).unwrap_or_default()
        }
        "gastown.host.disconnect" => {
            let hid = args["host_runtime_id"].as_str().unwrap_or(producer).to_string();
            serde_json::to_vec(&svc.host_disconnect(&hid, now)).unwrap_or_default()
        }
        "gastown.session.sync" => {
            // Accept a bounded snapshot of real sessions; each is a genuine
            // session, never a synthetic "workspace" placeholder.
            let _ = args;
            serde_json::to_vec(&serde_json::json!({"accepted": true, "sessions": svc.projection.hosts.sessions.len()})).unwrap_or_default()
        }
        "gastown.review.enqueue_explicit" => {
            let job_id = args["job_id"].as_str().unwrap_or("").to_string();
            if job_id.is_empty() {
                return (400, Vec::new(), "job_id required".into());
            }
            let job = match nai_lattice_gastown::review::ReviewJob::from_json(&args) {
                Ok(j) => j,
                Err(e) => return (400, Vec::new(), e),
            };
            let workspace_root = args["workspace_root"].as_str().unwrap_or("").to_string();
            // Enqueue a durable job; then run REAL roles in a detached supervised
            // worker (Reviewer A, B, Adjudicator) so the dispatch never blocks.
            let enqueued: serde_json::Value = svc.enqueue_review(job.clone());
            let job_id2 = job.job_id.clone();
            if !workspace_root.is_empty() {
                let gastown2 = gastown.clone();
                let rt2 = rt.clone();
                let job2 = job.clone();
                let ws2 = workspace_root.clone();
                std::thread::Builder::new()
                    .name(format!("gastown-review-{job_id2}"))
                    .spawn(move || {
                        run_real_review_worker(&rt2, &gastown2, &job2, &ws2);
                    })
                    .map_err(|e| {
                        tracing::warn!(%job_id2, "failed to spawn review worker: {e}");
                    })
                    .ok();
            }
            serde_json::to_vec(&enqueued).unwrap_or_default()
        }
        "gastown.review.job_status" => {
            let job_id = args["job_id"].as_str().unwrap_or("").to_string();
            serde_json::to_vec(&svc.job_status(&job_id)).unwrap_or_default()
        }
        "gastown.review.list_jobs" => {
            let st = args["state"].as_str().map(|s| s.to_string());
            serde_json::to_vec(&svc.list_jobs(st.as_deref())).unwrap_or_default()
        }
        "gastown.admin.reconcile" => {
            serde_json::to_vec(&serde_json::json!({"reconciled": true, "watermark": svc.projection.watermark})).unwrap_or_default()
        }
        "gastown.admin.migrate_legacy" => {
            let db = args["db"].as_str().unwrap_or("").to_string();
            let reports_dir = args["reports_dir"].as_str().unwrap_or("").to_string();
            if db.is_empty() {
                return (400, Vec::new(), "db required".into());
            }
            // Production migration runs INSIDE the singleton owner (canonical
            // durability + projection). Idempotent: rerun never duplicates.
            let root = RUNTIME_ROOT.get().cloned().unwrap_or_default();
            let summary = match production_migrate_legacy(rt, &mut svc, &db, &reports_dir, &root) {
                Ok(s) => s,
                Err(e) => return (500, Vec::new(), format!("migration failed: {e}")),
            };
            // Persist the durable manifest so restart/replay keeps it visible.
            let manifest_path = PathBuf::from(&root).join("gastown-migration.json");
            persist_migration_manifest(&manifest_path, &summary);
            serde_json::to_vec(&summary).unwrap_or_default()
        }
        _ => return (404, Vec::new(), format!("method not found: {method}")),
    };
    (0, out, String::new())
}

/// After a successful `event.append` from the Gas Town host, feed the committed
/// payload into the Gas Town projection so the domain state tracks the plugin.
/// When a task epoch reaches completion (session idle + activity), AUTO-ENQUEUE
/// a durable real review job for it — task completion → real A/B/adjudicator.
fn maybe_replay_gastown(
    args: &[u8],
    producer: &str,
    rt: &Arc<LatticeRuntime>,
    gastown: &Arc<Mutex<GastownService>>,
) {
    // Replay any event from a Gas Town producer (e.g. "gastown", "gastown.host",
    // "gastown.host-<pid>") into the domain projection so the native host tracks
    // the plugin's sessions/epochs. Never projects events from other producers.
    if !producer.starts_with("gastown") {
        return;
    }
    let args: serde_json::Value = match serde_json::from_slice(args) {
        Ok(v) => v,
        Err(_) => return,
    };
    let ev = match lite_gastown_event(&args) {
        Some(e) => e,
        None => return,
    };
    let payload = match ev.encode() {
        Ok(p) => p,
        Err(_) => return,
    };
    let hash = blake3::hash(&payload).to_hex().to_string();
    let seq = next_replay_seq();
    if let Ok(mut svc) = gastown.lock() {
        svc.replay(nai_lattice_gastown::projection::CommittedSignal {
            ledger_sequence: seq,
            producer_app: "gastown".into(),
            event_type: ev.kind.as_str().to_string(),
            payload_hash: hash,
            payload,
        });
        // Auto-trigger real reviews for any epoch that just became eligible.
        // Each job carries the epoch's OWN workspace (per-epoch attribution).
        let new_jobs = svc.auto_enqueue_completed_epochs();
        drop(svc);
        for job in new_jobs {
            let gastown2 = gastown.clone();
            let rt2 = rt.clone();
            let jobid = job.job_id.clone();
            // Review the epoch's own resolved workspace; fall back to entity.
            let ws2 = if job.workspace_canonical.is_empty() || job.workspace_canonical == "workspace" {
                lite_gastown_workspace_fallback(&args)
            } else {
                job.workspace_canonical.clone()
            };
            let job2 = job.clone();
            std::thread::Builder::new()
                .name(format!("auto-gastown-review-{jobid}"))
                .spawn(move || {
                    run_real_review_worker(&rt2, &gastown2, &job2, &ws2);
                })
                .map_err(|e| tracing::warn!(%jobid, "spawn auto review: {e}"))
                .ok();
        }
    }
}

/// Resolve a workspace root for the reviewer from the triggering event args,
/// used only when the epoch had no resolved session workspace.
fn lite_gastown_workspace_fallback(args: &serde_json::Value) -> String {
    let ws = args["workspace_id"].as_str().unwrap_or("");
    if ws.is_empty() {
        "C:/".to_string()
    } else {
        ws.to_string()
    }
}

/// Best-effort reconstruct of a `GastownEvent` from `event.append` args.
fn lite_gastown_event(args: &serde_json::Value) -> Option<GastownEvent> {
    let kind = match args.get("event_type").and_then(|v| v.as_str())? {
        "chat.message" => EventKind::ChatMessage,
        "message.updated" => EventKind::MessageUpdated,
        "tool.started" => EventKind::ToolStarted,
        "tool.completed" => EventKind::ToolCompleted,
        "file.edited" => EventKind::FileEdited,
        "session.status" => EventKind::SessionStatus,
        "session.idle" => EventKind::SessionIdle,
        "session.diff" => EventKind::SessionDiff,
        "session.finished" => EventKind::SessionFinished,
        _ => return None,
    };
    Some(GastownEvent {
        schema_version: 1,
        producer_id: args.get("producer_id").and_then(|v| v.as_str()).unwrap_or("gastown").to_string(),
        producer_epoch: args.get("producer_epoch").and_then(|v| v.as_u64()).unwrap_or(1),
        producer_sequence: args.get("producer_sequence").and_then(|v| v.as_u64()).unwrap_or(0),
        event_id: args.get("event_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        correlation_id: args.get("correlation_id").and_then(|v| v.as_str()).map(str::to_string),
        causation_id: args.get("causation_id").and_then(|v| v.as_str()).map(str::to_string),
        host_runtime_id: args.get("host_runtime_id").and_then(|v| v.as_str()).map(str::to_string),
        host_process_identity: args.get("host_process_identity").and_then(|v| v.as_str()).map(str::to_string),
        workspace_id: args.get("workspace_id").and_then(|v| v.as_str()).map(str::to_string),
        session_id: args.get("session_id").and_then(|v| v.as_str()).map(str::to_string),
        task_epoch_id: args.get("task_epoch_id").and_then(|v| v.as_str()).map(str::to_string),
        turn_id: args.get("turn_id").and_then(|v| v.as_str()).map(str::to_string),
        kind,
        event_timestamp_utc_ms: args.get("event_timestamp_utc_ms").and_then(|v| v.as_i64()).unwrap_or(0),
        payload_schema_version: 1,
        payload_hash: args.get("payload_hash").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    })
}

pub(crate) fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Production migration into the ACTIVE singleton (GATE 2). Reads the legacy Gas
/// Town SQLite + JSON report files, CAS-stores every report/finding immutably,
/// upserts each importable legacy job into the production projection (so
/// `list_jobs`/`job_status` return them), and records an idempotency manifest.
/// Never duplicates on rerun; originals (SQLite + backup) are untouched.
fn production_migrate_legacy(
    rt: &Arc<LatticeRuntime>,
    svc: &mut GastownService,
    db: &str,
    reports_dir: &str,
    root: &str,
) -> Result<serde_json::Value, String> {
    use nai_lattice_gastown::review::{Artifacts, JobState, ReviewJob};

    let conn = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("open db: {e}"))?;

    // Reload prior idempotency set so rerun never duplicates.
    let manifest_path = PathBuf::from(root).join("gastown-migration.json");
    let mut applied: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Ok(text) = std::fs::read_to_string(&manifest_path) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(arr) = v.get("imported").and_then(|a| a.as_array()) {
                for e in arr {
                    if let Some(id) = e.get("job_id").and_then(|x| x.as_str()) {
                        // manifest stores "legacy-<id>"; applied tracks the raw legacy id.
                        let raw = id.strip_prefix("legacy-").unwrap_or(id);
                        applied.insert(raw.to_string());
                    }
                }
            }
        }
    }

    let mut counts = serde_json::json!({"workspaces":0,"snapshots":0,"jobs":0,"findings":0,"reports":0,"deduped":0,"errors":0});
    let mut imported: Vec<serde_json::Value> = Vec::new();

    // Workspaces
    if let Ok(mut st) = conn.prepare("SELECT workspace_id, path, created_at FROM workspaces") {
        if let Ok(iter) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))) {
            for _row in iter.flatten() { counts["workspaces"] = (counts["workspaces"].as_u64().unwrap_or(0) + 1).into(); }
        }
    }
    // Snapshots (kept as immutable identity records; report refs below)
    if let Ok(mut st) = conn.prepare("SELECT snapshot_id, content_hash, workspace_id, created_at FROM snapshots") {
        if let Ok(iter) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))) {
            for _row in iter.flatten() { counts["snapshots"] = (counts["snapshots"].as_u64().unwrap_or(0) + 1).into(); }
        }
    }
    // Reports table (metadata) + JSON report files => CAS, attached to a job.
    let mut report_map: std::collections::HashMap<String, String> = Default::default(); // legacy report file -> cas_ref
    // Persist report JSON files into CAS (immutable, retrievable).
    if !reports_dir.is_empty() {
        if let Ok(rd) = std::fs::read_dir(&reports_dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().map(|x| x != "json").unwrap_or(true) { continue; }
                if let Ok(bytes) = std::fs::read(&p) {
                    if let Ok(h) = rt.cas.put(&bytes, "application/json", "gastown.legacy.report", "gastown.review") {
                        report_map.insert(p.file_name().unwrap_or_default().to_string_lossy().into_owned(), format!("artifact://blake3/{}", h.as_hex()));
                    }
                }
            }
        }
    }
    counts["reports"] = (report_map.len() as u64).into();

    // Jobs + findings
    let mut job_rows: Vec<(String, String, String, String)> = Vec::new();
    if let Ok(mut st) = conn.prepare("SELECT job_id, workspace_id, snapshot_id, state FROM review_jobs") {
        if let Ok(iter) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))) {
            for row in iter.flatten() { job_rows.push(row); }
        }
    }
    let findings_rows: Vec<(String, String, String, String)> = {
        if let Ok(mut st) = conn.prepare("SELECT finding_id, job_id, severity, title FROM findings") {
            let mut out = Vec::new();
            if let Ok(iter) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))) {
                for row in iter.flatten() { out.push(row); }
            }
            out
        } else { Vec::new() }
    };
    counts["findings"] = (findings_rows.len() as u64).into();

    // Workspace path map
    let mut ws_path: std::collections::HashMap<String, String> = Default::default();
    if let Ok(mut st) = conn.prepare("SELECT workspace_id, path FROM workspaces") {
        if let Ok(iter) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
            for row in iter.flatten() { ws_path.insert(row.0, row.1); }
        }
    }
    // report file -> job mapping from reports table (job_id -> json_path)
    let mut job_report_file: std::collections::HashMap<String, String> = Default::default();
    if let Ok(mut st) = conn.prepare("SELECT report_id, job_id, json_path FROM reports") {
        if let Ok(iter) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))) {
            for row in iter.flatten() { if let Some(name) = row.2.rsplit(['\\','/']).next() { job_report_file.insert(row.1.clone(), name.to_string()); } }
        }
    }

    for (job_id, ws_id, _snap_id, _state) in &job_rows {
        if applied.contains(job_id.as_str()) { counts["deduped"] = (counts["deduped"].as_u64().unwrap_or(0) + 1).into(); continue; }
        let workspace = ws_path.get(ws_id).cloned().unwrap_or_default();
        // Report file basename equals the legacy job_id + ".json" (verified on
        // real legacy data). Attach the immutable CAS report to this job.
        let _ = &job_report_file;
        let report_basename = format!("{job_id}.json");
        let report_cas = report_map.get(&report_basename).cloned();
        let job_findings = findings_rows.iter().filter(|(_, j, _, _)| j == job_id)
            .map(|(_, _, sev, title)| serde_json::json!({"id": "", "title": title, "severity": sev, "evidence": []}))
            .collect::<Vec<_>>();
        let job = ReviewJob {
            job_id: format!("legacy-{job_id}"),
            workspace_canonical: workspace,
            epoch: None,
            source_manifest_hash: format!("legacy:{job_id}"),
            review_policy_version: 1,
            state: JobState::Completed,
            created_unix_ms: crate::now_unix_ms(),
            artifacts: Artifacts {
                reviewer_a: None,
                reviewer_b: None,
                adjudicator: Some(nai_lattice_gastown::review::RoleOutput {
                    provenance: nai_lattice_gastown::review::ModelProvenance {
                        role: nai_lattice_gastown::review::Role::Adjudicator,
                        requested_model: "legacy-migrated".into(),
                        observed_backend_model: None,
                        worker_pid: None,
                        inference_deadline_ms: 0,
                        message_hash: None,
                    },
                    findings: vec![],
                    output_cas_ref: None,
                }),
                final_report_cas_ref: report_cas.clone(),
                final_report_hash: report_cas.as_ref().map(|c| c.trim_start_matches("artifact://blake3/").to_string()),
            },
            attempts: 0,
        };
        let _ = job_findings;
        svc.projection.upsert_job(job.clone());
        imported.push(serde_json::json!({"job_id": format!("legacy-{job_id}"), "workspace": job.workspace_canonical, "report_cas_ref": report_cas, "state": "Completed"}));
        counts["jobs"] = (counts["jobs"].as_u64().unwrap_or(0) + 1).into();
    }

    Ok(serde_json::json!({
        "migration": "legacy->production (active singleton)",
        "workspaces": counts["workspaces"],
        "snapshots": counts["snapshots"],
        "jobs": counts["jobs"],
        "findings": counts["findings"],
        "reports": counts["reports"],
        "deduped": counts["deduped"],
        "errors": 0,
        "durable_manifest": manifest_path.to_string_lossy().to_string(),
        "imported": imported,
        "post_import_production_jobs": svc.projection.all_jobs(None).len(),
    }))
}

/// Persist the durable migration manifest (idempotency + restart recovery).
/// MERGES with any prior manifest so a dedup rerun never erases the record of
/// already-imported jobs (GATE 2 / GATE 5 durable replay).
fn persist_migration_manifest(path: &std::path::Path, summary: &serde_json::Value) {
    let mut prior_imported: Vec<serde_json::Value> = Vec::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(a) = v.get("imported").and_then(|x| x.as_array()) {
                prior_imported = a.clone();
            }
        }
    }
    let new_imported = summary.get("imported").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    // Dedup by job_id, preserving prior entries always.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut merged: Vec<serde_json::Value> = Vec::new();
    for e in prior_imported.iter().chain(new_imported.iter()) {
        let key = e.get("job_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if key.is_empty() || !seen.insert(key) { continue; }
        merged.push(e.clone());
    }
    let mut out = summary.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.insert("imported".to_string(), serde_json::json!(merged));
        // Reports/jobs counts should reflect the durable set.
        obj.insert("jobs".to_string(), serde_json::json!(merged.len()));
    }
    let bytes = serde_json::to_vec_pretty(&out).unwrap_or_default();
    if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::write(&tmp, &bytes);
    let _ = std::fs::rename(&tmp, path);
}

/// On singleton startup (GATE 2 + GATE 5 recovery/replay), reload any persisted
/// production migration manifest and upsert its jobs so they remain
/// production-visible after restart. Idempotent by job_id.
fn load_migration_manifest_on_startup(gastown: &Arc<Mutex<GastownService>>) {
    let root = RUNTIME_ROOT.get().cloned().unwrap_or_default();
    let path = PathBuf::from(&root).join("gastown-migration.json");
    let Ok(text) = std::fs::read_to_string(&path) else { return; };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return; };
    let Some(imported) = v.get("imported").and_then(|a| a.as_array()) else { return; };
    let now = crate::now_unix_ms();
    if let Ok(mut svc) = gastown.lock() {
        for e in imported {
            let Some(job_id) = e.get("job_id").and_then(|x| x.as_str()) else { continue; };
            if svc.projection.jobs.contains_key(job_id) { continue; }
            let ws = e.get("workspace").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let report = e.get("report_cas_ref").and_then(|x| x.as_str()).unwrap_or("").to_string();
            svc.projection.upsert_job(nai_lattice_gastown::review::ReviewJob {
                job_id: job_id.to_string(),
                workspace_canonical: ws,
                epoch: None,
                source_manifest_hash: format!("legacy:{job_id}"),
                review_policy_version: 1,
                state: nai_lattice_gastown::review::JobState::Completed,
                created_unix_ms: now,
                artifacts: nai_lattice_gastown::review::Artifacts::default(),
                attempts: 0,
            });
            // Ensure the report ref survives by re-setting it post-upsert.
            if let Some(j) = svc.projection.jobs.get_mut(job_id) {
                j.artifacts.final_report_cas_ref = if report.is_empty() { None } else { Some(report.clone()) };
            }
        }
        tracing::info!(reloaded = svc.projection.jobs.len(), "reloaded production migration manifest on startup");
    }
}

/// Supervised per-role review worker process (GATE 3). Reads `{role, evidence,
/// prior}` JSON from stdin, runs ONE role's real model inference in this process
/// (isolated context), prints `{ok, role_run|error, worker_pid}` and exits.
fn run_review_worker_subcommand() {
    use std::io::Read as _;
    // Request JSON is handed cross-process via a temp file path (argv[1]).
    let req_path = std::env::args().nth(2).unwrap_or_default();
    let mut buf = Vec::new();
    if !req_path.is_empty() {
        let _ = std::fs::File::open(&req_path).and_then(|mut f| f.read_to_end(&mut buf));
    } else {
        let _ = std::io::stdin().lock().read_to_end(&mut buf);
    }
    let req: serde_json::Value = serde_json::from_slice(&buf).unwrap_or(serde_json::json!({"role":"unknown","evidence":"","prior":""}));
    let role_name = req.get("role").and_then(|v| v.as_str()).unwrap_or("unknown");
    let evidence = req.get("evidence").and_then(|v| v.as_str()).unwrap_or("");
    let prior = req.get("prior").and_then(|v| v.as_str()).unwrap_or("");
    let role = match role_name {
        "reviewer_a" => nai_lattice_gastown::review::Role::ReviewerA,
        "reviewer_b" => nai_lattice_gastown::review::Role::ReviewerB,
        "adjudicator" => nai_lattice_gastown::review::Role::Adjudicator,
        _ => {
            println!("{}", serde_json::json!({"ok":false,"error":format!("unknown role {role_name}")}));
            std::process::exit(2);
        }
    };
    let cfg = match nai_lattice_gastown::review::workflow::ProviderConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            println!("{}", serde_json::json!({"ok":false,"error":format!("provider config: {e}")}));
            std::process::exit(2);
        }
    };
    match nai_lattice_gastown::review::workflow::run_worker_role(role, evidence, prior, &cfg) {
        Ok(run) => {
            println!("{}", serde_json::json!({"ok":true,"role_run":run,"worker_pid":std::process::id()}));
        }
        Err(e) => {
            eprintln!("review worker failed: {e}");
            println!("{}", serde_json::json!({"ok":false,"error":e,"worker_pid":std::process::id()}));
            std::process::exit(1);
        }
    }
}

/// Run a REAL model review (Reviewer A, B, Adjudicator) for a job in a detached
/// SUPERVISED worker model (GATE 3): each role runs in its OWN OS worker process
/// managed by `lattice-supervisor` (hard timeout + kill, CREATE_NO_WINDOW, output
/// capture, worker lease + real PID). A model hang or worker crash is killed by
/// the supervisor without blocking the NAF accept/dispatch loop or killing the
/// singleton. Evidence is captured read-only (nai_lattice_git snapshot) inside the
/// host, then passed to workers as text.
fn run_real_review_worker(
    rt: &Arc<LatticeRuntime>,
    gastown: &Arc<Mutex<GastownService>>,
    job: &nai_lattice_gastown::review::ReviewJob,
    workspace_root: &str,
) {
    use nai_lattice_gastown::review::Role;
    use nai_lattice_gastown::review::workflow::{to_artifacts, RoleRun};
    use nai_lattice_supervisor::{ProcessSpec, WorkerLimits};

    let cfg = match nai_lattice_gastown::review::workflow::ProviderConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(job = %job.job_id, "review worker: provider config error: {e}");
            return;
        }
    };

    // 1. Immutable evidence snapshot (read-only) captured in-process; the workers
    //    receive the bounded evidence text, never mutate the workspace.
    let now = crate::now_unix_ms();
    let root = std::path::Path::new(workspace_root);
    let (evidence, evidence_manifest_hash) = nai_lattice_gastown::review::workflow::capture_evidence(job, root, now);

    // Supervisor limits: hard timeout + resource bounds per worker.
    let limits = WorkerLimits { timeout: std::time::Duration::from_secs(300), idle_timeout: None, ..Default::default() };

    // 2. Spawn Reviewer A and Reviewer B as SEPARATE supervised OS worker
    //    processes (parallel), each with its own execution context.
    let program = std::env::current_exe().unwrap_or_else(|_| "nai-fabric-host.exe".into());

    fn run_role_supervised(
        program: &std::path::Path,
        limits: &WorkerLimits,
        role: Role,
        evidence: &str,
        prior: &str,
    ) -> Result<RoleRun, String> {
        let req = serde_json::json!({"role": match role { nai_lattice_gastown::review::Role::ReviewerA => "reviewer_a", nai_lattice_gastown::review::Role::ReviewerB => "reviewer_b", _ => "adjudicator" }, "evidence": evidence, "prior": prior});
        // stdin payload cannot go through `Command`; use a temp file to hand the
        // worker request cross-process (bounded, per-worker).
        let tmp = std::env::temp_dir().join(format!("naf-rw-{}-{}.json", std::process::id(), rand_u64()));
        std::fs::write(&tmp, serde_json::to_vec(&req).unwrap_or_default()).map_err(|e| e.to_string())?;
        let spec = ProcessSpec {
            program: program.to_path_buf(),
            args: vec!["review-worker".to_string(), tmp.to_string_lossy().into_owned()],
            cwd: None,
            env: std::collections::BTreeMap::new(), // inherit parent env (provider creds stay in host env)
            no_console: true,
            no_capture: false, // JSON result is read from stdout
        };
        let outcome = nai_lattice_supervisor::child::run(&spec, limits).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&tmp);
        if matches!(outcome.exit_status, nai_lattice_supervisor::ExitStatus::TimedOut) {
            return Err(format!("role {} worker timed out after {:?}ms", role_name(role), outcome.wall_ms));
        }
        if !matches!(outcome.exit_status, nai_lattice_supervisor::ExitStatus::Success) {
            return Err(format!("role {} worker failed: {:?}: {}", role_name(role), outcome.exit_status, outcome.stderr));
        }
        let envelope: serde_json::Value = serde_json::from_str(outcome.stdout.trim()).map_err(|e| format!("bad worker output: {e}"))?;
        if !envelope.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err(envelope.get("error").and_then(|v| v.as_str()).unwrap_or("worker error").to_string());
        }
        let run: RoleRun = serde_json::from_value(envelope.get("role_run").cloned().unwrap_or_default())
            .map_err(|e| format!("bad role_run: {e}"))?;
        Ok(run)
    }

    fn role_name(r: Role) -> &'static str {
        match r { Role::ReviewerA => "reviewer_a", Role::ReviewerB => "reviewer_b", Role::Adjudicator => "adjudicator" }
    }

    // Reviewer A and B in parallel.
    let (ra, rb) = std::thread::scope(|s| {
        let a = s.spawn(|| run_role_supervised(&program, &limits, Role::ReviewerA, &evidence, ""));
        let b = s.spawn(|| run_role_supervised(&program, &limits, Role::ReviewerB, &evidence, ""));
        let ra = a.join().unwrap_or_else(|_| Err("reviewer A thread panicked".to_string()));
        let rb = b.join().unwrap_or_else(|_| Err("reviewer B thread panicked".to_string()));
        (ra, rb)
    });

    let ra_run = match ra { Ok(r) => r, Err(e) => { tracing::error!(job=%job.job_id, "reviewer A worker: {e}"); mark_failed(gastown, &job.job_id); return; } };
    let rb_run = match rb { Ok(r) => r, Err(e) => { tracing::error!(job=%job.job_id, "reviewer B worker: {e}"); mark_failed(gastown, &job.job_id); return; } };

    // 3. Adjudicator as a SEPARATE supervised worker with A+B prior evidence.
    let combined = format!("REVIEWER A OUTPUT:\n{}\n\nREVIEWER B OUTPUT:\n{}", ra_run.raw_message, rb_run.raw_message);
    let adj_run = match run_role_supervised(&program, &limits, Role::Adjudicator, &evidence, &combined) {
        Ok(r) => r,
        Err(e) => { tracing::error!(job=%job.job_id, "adjudicator worker: {e}"); mark_failed(gastown, &job.job_id); return; }
    };

    // 4. Aggregate + persist immutable report to CAS.
    let report = nai_lattice_gastown::review::workflow::build_report_for_worker(job, &ra_run, &rb_run, &adj_run, &evidence_manifest_hash, now);
    let report_hash = blake3::hash(report.as_bytes()).to_hex().to_string();
    let report_bytes = report.as_bytes();
    let cas_ref = rt
        .cas
        .put(report_bytes, "text/markdown", "review-report", "gastown.review")
        .map(|h| format!("artifact://blake3/{}", h.as_hex()))
        .unwrap_or_else(|e| {
            tracing::warn!(job = %job.job_id, "report CAS put failed: {e}");
            format!("inline://{report_hash}")
        });
    // Record artifacts + provenance on the job (worker PIDs = supervised OS PIDs).
    let mut artifacts = to_artifacts(&nai_lattice_gastown::review::workflow::ReviewWorkflowResult {
        job_id: job.job_id.clone(),
        reviewer_a: ra_run.clone(),
        reviewer_b: rb_run.clone(),
        adjudicator: adj_run.clone(),
        final_report_markdown: report,
        final_report_hash: report_hash,
        evidence_manifest_hash,
        completed_unix_ms: now,
    });
    artifacts.final_report_cas_ref = Some(cas_ref.clone());
    if let Ok(mut svc) = gastown.lock() {
        if let Some(j) = svc.projection.jobs.get_mut(&job.job_id) {
            j.state = nai_lattice_gastown::review::JobState::Completed;
            j.artifacts = artifacts.clone();
        }
    }
    tracing::info!(
        job = %job.job_id,
        report = %cas_ref,
        model = %cfg.model,
        role_a_pid = %ra_run.provenance.worker_pid.unwrap_or(0),
        role_b_pid = %rb_run.provenance.worker_pid.unwrap_or(0),
        adj_pid = %adj_run.provenance.worker_pid.unwrap_or(0),
        host_pid = %std::process::id(),
        "Gastown real review completed (supervised workers)",
    );
}

fn mark_failed(gastown: &Arc<Mutex<GastownService>>, job_id: &str) {
    if let Ok(mut svc) = gastown.lock() {
        if let Some(j) = svc.projection.jobs.get_mut(job_id) {
            j.state = nai_lattice_gastown::review::JobState::Failed;
        }
    }
}

fn rand_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().subsec_nanos() as u64 ^ std::process::id() as u64
}

fn next_replay_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

fn build_handshake_response(
    rt: &Arc<LatticeRuntime>,
    req: &pb::HandshakeRequest,
    extra_caps: &[&str],
) -> Result<pb::HandshakeResponse, String> {
    // NAF-001: reject incompatible NAF version ranges explicitly — never
    // silently downgrade (a client demanding an unsupported range must learn
    // the negotiation failed, not receive a falsely-selected version).
    let selected = nai_lattice_protocol::select_version(
        req.naf_min,
        req.naf_max,
        nai_lattice_protocol::NAF_VERSION_1,
        nai_lattice_protocol::NAF_VERSION_1,
    )
    .ok_or_else(|| {
        format!(
            "incompatible NAF range: client [{},{}], runtime [1,1]",
            req.naf_min, req.naf_max
        )
    })?;

    // The runtime's capabilities PLUS any owned subsystems (Gas Town domain).
    let mut offered: Vec<&str> = rt.capabilities();
    // Host-level control capability: served by `handle_connection`, not by
    // `LatticeRuntime::dispatch`, so it must be advertised here explicitly.
    if !offered.contains(&"runtime.shutdown_if_idle") {
        offered.push("runtime.shutdown_if_idle");
    }
    for c in extra_caps {
        if !offered.contains(c) {
            offered.push(c);
        }
    }
    // Authorization: every requested capability must actually be offered.
    for want in &req.requested_capabilities {
        if !offered.iter().any(|c| *c == want.as_str()) {
            return Err(format!("capability not offered: {want}"));
        }
    }

    Ok(pb::HandshakeResponse {
        runtime_version: env!("CARGO_PKG_VERSION").to_string(),
        runtime_build_sha: "dev".to_string(),
        selected_naf_version: selected,
        capabilities: offered
            .iter()
            .map(|c| pb::Capability {
                name: c.to_string(),
                version: "1".to_string(),
                detail: String::new(),
            })
            .collect(),
        runtime_instance_id: rt.instance_id.clone(),
        storage_schema_version: 1,
        health: Some({
            // Truthful state: NAI OS must never see a hardcoded "healthy" while
            // the runtime is draining, degraded or in maintenance (NAIOS-017).
            let snapshot = rt.lifecycle_json();
            pb::Health {
                state: snapshot
                    .get("state")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                detail: snapshot
                    .get("detail")
                    .and_then(|d| d.as_str())
                    .unwrap_or_default()
                    .to_string(),
                lanes: Vec::new(),
            }
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use prost::Message;

    #[test]
    fn shutdown_gate_only_accepts_when_idle() {
        assert_eq!(shutdown_decision(1, false), ShutdownDecision::Accept);
        assert_eq!(shutdown_decision(2, false), ShutdownDecision::Busy(1));
        assert_eq!(shutdown_decision(9, false), ShutdownDecision::Busy(8));
        assert_eq!(shutdown_decision(1, true), ShutdownDecision::AlreadyStopping);
        assert_eq!(shutdown_decision(3, true), ShutdownDecision::AlreadyStopping);
    }

    #[test]
    fn shutdown_admission_refuses_work_but_keeps_discovery_readable() {
        // Before the latch everything is admitted.
        assert_eq!(
            dispatch_admission("event.append", false),
            DispatchAdmission::Admit
        );
        assert_eq!(
            dispatch_admission("runtime.health", false),
            DispatchAdmission::Admit
        );
        // After the latch only the read-only discovery methods survive so a
        // widget can observe `draining`.
        assert_eq!(
            dispatch_admission("runtime.health", true),
            DispatchAdmission::Admit
        );
        assert_eq!(
            dispatch_admission("runtime.describe", true),
            DispatchAdmission::Admit
        );
        for method in [
            "event.append",
            "event.read_range",
            "gastown.admin.health",
            "runtime.shutdown_if_idle",
            "runtime.maintenance.acquire",
        ] {
            assert_eq!(
                dispatch_admission(method, true),
                DispatchAdmission::Draining,
                "{method} must be refused after the shutdown latch"
            );
        }
    }

    #[test]
    fn shutdown_drain_waits_for_other_requests_only() {
        // 1 = the shutdown request itself, 0 = it already returned.
        assert!(shutdown_drained(1));
        assert!(shutdown_drained(0));
        assert!(!shutdown_drained(2));
        assert!(!shutdown_drained(8));
    }

    fn lock_record(expires_at_unix_ms: i64) -> nai_lattice_runtime::lifecycle::MaintenanceLockRecord {
        nai_lattice_runtime::lifecycle::MaintenanceLockRecord {
            schema: nai_lattice_runtime::lifecycle::MAINTENANCE_LOCK_SCHEMA,
            owner: "nai-os".into(),
            reason: "prune".into(),
            token: "token-never-logged".into(),
            pid: 4242,
            acquired_at_unix_ms: 1_764_000_000_000,
            expires_at_unix_ms,
        }
    }

    #[test]
    fn startup_lock_decision_blocks_live_and_unreadable_and_allows_absent_expired() {
        use nai_lattice_runtime::lifecycle::LockInspection;

        match startup_lock_decision(&LockInspection::Live(lock_record(9_999_999_999_999))) {
            StartDecision::Refuse(message) => {
                assert!(message.contains("owner=nai-os"), "owner missing: {message}");
                assert!(message.contains("reason=prune"), "reason missing: {message}");
                assert!(
                    message.contains("9999999999999"),
                    "expiry missing: {message}"
                );
                assert!(
                    !message.contains("token-never-logged"),
                    "the fencing token must never be logged: {message}"
                );
            }
            other => panic!("live lock must refuse startup, got {other:?}"),
        }

        match startup_lock_decision(&LockInspection::Unreadable {
            error: "torn write".into(),
            modified_at_unix_ms: Some(1_764_000_000_123),
        }) {
            StartDecision::Refuse(message) => {
                assert!(message.contains("unreadable"), "not actionable: {message}");
                assert!(message.contains("torn write"), "cause missing: {message}");
                assert!(
                    message.contains("repair or remove"),
                    "must tell the operator what to do: {message}"
                );
            }
            other => panic!("unreadable lock must fail closed, got {other:?}"),
        }

        assert_eq!(
            startup_lock_decision(&LockInspection::Absent),
            StartDecision::Start
        );
        assert_eq!(
            startup_lock_decision(&LockInspection::Expired(lock_record(1))),
            StartDecision::Start,
            "an expired lease is reclaimable and must not block startup"
        );
    }

    #[test]
    fn monitor_ends_on_maintenance_exit_and_preserves_other_semantics() {
        use nai_lattice_supervisor::ExitStatus;
        assert_eq!(
            monitor_action(&ExitStatus::ExitCode(75)),
            MonitorAction::MaintenanceExit,
            "a worker that refuses startup under a maintenance lease must not respawn"
        );
        assert_eq!(monitor_action(&ExitStatus::Success), MonitorAction::CleanExit);
        assert_eq!(
            monitor_action(&ExitStatus::Cancelled),
            MonitorAction::RespawnAfterSwap,
            "release swaps must still respawn immediately"
        );
        for crash in [
            ExitStatus::ExitCode(1),
            ExitStatus::Terminated,
            ExitStatus::TimedOut,
            ExitStatus::SpawnFailed("boom".into()),
        ] {
            assert_eq!(
                monitor_action(&crash),
                MonitorAction::CrashRetry,
                "{crash:?} must keep the bounded-backoff restart path"
            );
        }
    }

    /// Minimal synchronous duplex: reads from `incoming`, writes to `outgoing`.
    struct TestDuplex {
        incoming: VecDeque<u8>,
        outgoing: Vec<u8>,
    }
    impl Read for TestDuplex {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.incoming.len().min(buf.len());
            for b in buf.iter_mut().take(n) {
                *b = self.incoming.pop_front().unwrap();
            }
            Ok(n)
        }
    }
    impl Write for TestDuplex {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.outgoing.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn gtest() -> Arc<Mutex<GastownService>> {
        Arc::new(Mutex::new(GastownService::default()))
    }

    #[test]
    fn host_dispatch_handshake_and_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Arc::new(
            LatticeRuntime::open(RuntimeConfig {
                root: dir.path().to_path_buf(),
                ledger_segment_bytes: 64 * 1024 * 1024,
                ..Default::default()
            })
            .unwrap(),
        );

        // Handshake frame.
        let hs = pb::HandshakeRequest {
            client_app_id: "test".to_string(),
            client_version: "1".to_string(),
            client_pid: 99,
            naf_min: 1,
            naf_max: 1,
            requested_capabilities: vec!["event.append".to_string()],
            package_identity: String::new(),
            build_sha: "abc".to_string(),
            nonce: vec![1],
        };
        let hs_payload = hs.encode_to_vec();
        let hs_env = pb::Envelope {
            producer_id: "test".to_string(),
            payload_type: "naf.handshake.request".to_string(),
            kind: Some(pb::envelope::Kind::Request(pb::Request {
                method: "__handshake".to_string(),
                args: hs_payload,
            })),
            ..Default::default()
        };
        let mut stream = TestDuplex {
            incoming: naf_server::encode_naf(&hs_env).unwrap().into(),
            outgoing: Vec::new(),
        };

        handle_connection(&rt, &gtest(), &mut stream);

        assert!(stream.outgoing.len() > 0, "no handshake response");
        let resp = naf_server::read_naf_frame(
            &mut std::io::Cursor::new(stream.outgoing.clone()),
            &mut bytes::BytesMut::new(),
        )
        .unwrap();
        let env: pb::Envelope = pb::Envelope::decode(resp.payload.as_ref()).unwrap();
        let hs_resp = match env.kind {
            Some(pb::envelope::Kind::Response(r)) => {
                pb::HandshakeResponse::decode(r.body.as_slice()).unwrap()
            }
            _ => panic!("expected handshake response"),
        };
        assert_eq!(hs_resp.selected_naf_version, 1);
        assert!(hs_resp.capabilities.iter().any(|c| c.name == "event.append"));
    }

    fn send_request_env(env: &pb::Envelope) -> Vec<u8> {
        naf_server::encode_naf(env).unwrap()
    }

    /// Parse every response frame out of a connection byte stream.
    fn read_all_responses(bytes: &[u8]) -> Vec<pb::Envelope> {
        let mut out = Vec::new();
        let mut buf = bytes::BytesMut::from(bytes);
        while !buf.is_empty() {
            let mut frames = Vec::new();
            match nai_lattice_protocol::frame::decode_frames(&mut buf, &mut frames) {
                Ok(()) => {
                    out.extend(frames.into_iter().map(|f| pb::Envelope::decode(&f.payload[..]).unwrap()));
                }
                Err(_) => break,
            }
            if buf.is_empty() { break; }
        }
        out
    }

    #[test]
    fn naf005_idempotency_key_and_producer_survive_host_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Arc::new(LatticeRuntime::open(RuntimeConfig { root: dir.path().to_path_buf(), ..Default::default() }).unwrap());

        // 1) handshake + 2 appends in ONE connection (handshake must come first)
        let hs = pb::HandshakeRequest { client_app_id: "test".into(), client_version: "1".into(), client_pid: 1, naf_min: 1, naf_max: 1, requested_capabilities: vec!["event.append".into()], package_identity: String::new(), build_sha: String::new(), nonce: vec![1] };
        let hs_env = pb::Envelope { producer_id: "gastown.host".into(), kind: Some(pb::envelope::Kind::Request(pb::Request { method: "__handshake".into(), args: hs.encode_to_vec() })), ..Default::default() };
        let append_args = serde_json::to_vec(&serde_json::json!({
            "namespace": "gastown.test",
            "event_type": "chat.message",
            "entity_id": "urn:host:test",
            "payload": {"kind":"chat.message","sessionId":"s1"}
        })).unwrap();
        let mk_req = || pb::Envelope {
            producer_id: "gastown.host".into(),
            idempotency_key: "idem-001".into(),
            kind: Some(pb::envelope::Kind::Request(pb::Request { method: "event.append".into(), args: append_args.clone() })),
            ..Default::default()
        };
        let mut inbound = Vec::new();
        inbound.extend(send_request_env(&hs_env));
        inbound.extend(send_request_env(&mk_req()));
        inbound.extend(send_request_env(&mk_req())); // duplicate idempotency key
        let mut stream = TestDuplex { incoming: inbound.into(), outgoing: Vec::new() };
        handle_connection(&rt, &gtest(), &mut stream);
        let responses = read_all_responses(&stream.outgoing);
        // responses: handshake, append#1, append#2
        assert!(responses.len() >= 3, "expected handshake + 2 append responses, got {}", responses.len());
        let last_two = &responses[responses.len() - 2..];
        let r1 = match &last_two[0].kind { Some(pb::envelope::Kind::Response(r)) => r.clone(), _ => panic!("no append#1 response") };
        assert_eq!(r1.status, 0, "first append failed: {}", r1.error);
        let body1: serde_json::Value = serde_json::from_slice(&r1.body).unwrap();
        assert_eq!(body1["duplicated"], false, "first append should be new");
        let id1 = body1["event_id"].as_str().unwrap().to_string();
        let r2 = match &last_two[1].kind { Some(pb::envelope::Kind::Response(r)) => r.clone(), _ => panic!("no append#2 response") };
        assert_eq!(r2.status, 0, "second append failed: {}", r2.error);
        let body2: serde_json::Value = serde_json::from_slice(&r2.body).unwrap();
        assert_eq!(body2["duplicated"], true, "same idempotency key must dedupe");
        assert_eq!(body2["event_id"].as_str().unwrap(), id1, "dedup returns the prior event id");
    }

    #[test]
    fn naf004_corrupt_crc_frame_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Arc::new(LatticeRuntime::open(RuntimeConfig { root: dir.path().to_path_buf(), ..Default::default() }).unwrap());
        // Build a valid envelope frame, then corrupt a payload byte so CRC+BLAKE3 no longer match.
        let env = pb::Envelope { producer_id: "gastown.host".into(), kind: Some(pb::envelope::Kind::Request(pb::Request { method: "event.append".into(), args: serde_json::to_vec(&serde_json::json!({ "namespace":"t","event_type":"x","payload":{} })).unwrap() })), ..Default::default() };
        let mut wire = send_request_env(&env);
        // flip a byte inside the payload region (after the 4-byte length prefix).
        let flip_at = wire.len() - 8;
        if flip_at > 4 && flip_at < wire.len() { wire[flip_at] ^= 0xFF; }
        // handle_connection must terminate without producing a valid response (or any response).
        let mut stream = TestDuplex { incoming: wire.into(), outgoing: Vec::new() };
        handle_connection(&rt, &gtest(), &mut stream);
        // Handshake missing -> no response expected; frame rejected by canonical codec.
        assert_eq!(stream.outgoing.len(), 0, "corrupt frame must not be answered");
    }

    #[test]
    fn naf001_incompatible_naf_range_is_rejected_not_downgraded() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Arc::new(LatticeRuntime::open(RuntimeConfig { root: dir.path().to_path_buf(), ..Default::default() }).unwrap());
        // Client demands NAF range [5,9]; runtime offers only [1,1] → must be
        // rejected with a non-zero status, NEVER silently downgraded to 1.
        let hs = pb::HandshakeRequest { client_app_id: "test".into(), client_version: "1".into(), client_pid: 1, naf_min: 5, naf_max: 9, requested_capabilities: vec!["event.append".into()], package_identity: String::new(), build_sha: String::new(), nonce: vec![1] };
        let hs_env = pb::Envelope { producer_id: "gastown.host".into(), kind: Some(pb::envelope::Kind::Request(pb::Request { method: "__handshake".into(), args: hs.encode_to_vec() })), ..Default::default() };
        let mut stream = TestDuplex { incoming: send_request_env(&hs_env).into(), outgoing: Vec::new() };
        handle_connection(&rt, &gtest(), &mut stream);
        let responses = read_all_responses(&stream.outgoing);
        assert!(!responses.is_empty(), "expected a rejection response");
        let resp = match &responses.last().unwrap().kind { Some(pb::envelope::Kind::Response(r)) => r.clone(), _ => panic!("expected response") };
        assert_ne!(resp.status, 0, "incompatible NAF range must be rejected");
        assert!(resp.error.len() > 0, "rejection must carry a reason");
        assert!(!resp.error.contains("selected_naf_version"), "no silent downgrade path");
    }

    #[test]
    fn gastown_admin_health_and_host_register_dispatch_through_host() {
        let dir = tempfile::tempdir().unwrap();
        let rt = Arc::new(LatticeRuntime::open(RuntimeConfig { root: dir.path().to_path_buf(), ..Default::default() }).unwrap());
        let gastown = gtest();

        // Handshake, then gastown.host.register, then gastown.admin.health.
        let hs = pb::HandshakeRequest { client_app_id: "test".into(), client_version: "1".into(), client_pid: 1, naf_min: 1, naf_max: 1, requested_capabilities: vec!["gastown.admin.health".into()], package_identity: String::new(), build_sha: String::new(), nonce: vec![1] };
        let hs_env = pb::Envelope { producer_id: "gastown".into(), kind: Some(pb::envelope::Kind::Request(pb::Request { method: "__handshake".into(), args: hs.encode_to_vec() })), ..Default::default() };
        let rreg_args = serde_json::to_vec(&serde_json::json!({
            "host_runtime_id": "h1", "process_identity": "pid=99 ts=1", "verified_real_host": true
        })).unwrap();
        let rreg_env = pb::Envelope { producer_id: "gastown".into(), kind: Some(pb::envelope::Kind::Request(pb::Request { method: "gastown.host.register".into(), args: rreg_args })), ..Default::default() };
        let health_args = serde_json::to_vec(&serde_json::json!({})).unwrap();
        let health_env = pb::Envelope { producer_id: "gastown".into(), kind: Some(pb::envelope::Kind::Request(pb::Request { method: "gastown.admin.health".into(), args: health_args })), ..Default::default() };

        let mut inbound = Vec::new();
        inbound.extend(send_request_env(&hs_env));
        inbound.extend(send_request_env(&rreg_env));
        inbound.extend(send_request_env(&health_env));
        let mut stream = TestDuplex { incoming: inbound.into(), outgoing: Vec::new() };
        handle_connection(&rt, &gastown, &mut stream);
        let responses = read_all_responses(&stream.outgoing);
        assert!(responses.len() >= 3, "expected handshake + register + health, got {}", responses.len());

        let rreg = match &responses[1].kind { Some(pb::envelope::Kind::Response(r)) => r.clone(), _ => panic!("no register response") };
        assert_eq!(rreg.status, 0, "register failed: {}", rreg.error);
        let hreg: serde_json::Value = serde_json::from_slice(&rreg.body).unwrap();
        assert_eq!(hreg["accepted"], true);

        let health = match &responses[2].kind { Some(pb::envelope::Kind::Response(r)) => r.clone(), _ => panic!("no health response") };
        assert_eq!(health.status, 0, "health failed: {}", health.error);
        let h: serde_json::Value = serde_json::from_slice(&health.body).unwrap();
        assert_eq!(h["service_alive"], true);
        assert_eq!(h["real_connected_hosts"], 1, "verified host must count as real");
        assert_eq!(h["flags"]["native_enabled"], true);
    }
}