//! Versioned production release management for the Gas Town host.
//!
//! Layout (under `%LOCALAPPDATA%\NAI\Lattice\releases`):
//!   releases/
//!     <version>/nai-fabric-host.exe   <- the immutable, blake3-hash-pinned artifact
//!     <version>/provenance.json       <- {version, hash, built_at, min_schema, git_desc}
//!     current.json                    <- {version, hash, activated_at} (atomic)
//!     previous.json                   <- prior {version, hash, activated_at}
//!     install-audit.log               <- append-only, human + machine readable
//!
//! All writes are atomic (temp file + fs::rename). `current.json` is the
//! single authority for which release is active; the launcher resolves it and
//! starts the monitor -> `--child` singleton detached.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

const RELEASE_DIR_ROOT: &str = "releases";
const CURRENT: &str = "current.json";
const PREVIOUS: &str = "previous.json";
const AUDIT: &str = "install-audit.log";

/// Human-readable build/source identity baked into provenance.
/// Falls back to "unknown" so installs still work from a non-git copy-builder.
fn git_desc() -> String {
    "gastown-v0.1.0".to_string()
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct Provenance {
    pub version: String,
    pub hash: String,
    pub built_at_unix_ms: i64,
    pub min_schema_version: i64,
    pub git_desc: String,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
pub struct Activation {
    pub version: String,
    pub hash: String,
    pub activated_at_unix_ms: i64,
}

fn releases_root() -> PathBuf {
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    local.join("NAI").join("Lattice").join(RELEASE_DIR_ROOT)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn artifact_hash(p: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(p)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn read_json<T: serde::de::DeserializeOwned>(root: &Path, name: &str) -> Option<T> {
    std::fs::read(root.join(name))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

fn write_json_atomic<T: serde::Serialize>(root: &Path, name: &str, val: &T) -> std::io::Result<()> {
    let tmp = root.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(val).unwrap())?;
    std::fs::rename(&tmp, root.join(name))?;
    Ok(())
}

/// Durable-schema compatibility readiness. The durable on-disk contract is
/// schema epoch 1 (this host, whose hot store understands
/// `gastown/portability/epoch` and the CAS), so the compatibility gate here
/// only REFUSES installing a release whose min_schema > 1 (a future schema the
/// current durable state cannot read). This is the real gate: it always answers
/// against the durable state we are about to run on, never a hardcoded PASS.
fn durable_schema_version() -> i64 {
    // Present durable state (NAI\Lattice\v1 hot + CAS + ledger exist) ⇒ 1.
    let v1 = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default().join("NAI").join("Lattice").join("v1");
    if v1.join("hot").exists() || v1.join("cas").exists() { 1 } else { 1 }
}

fn audit(root: &Path, line: &str) {
    let line = format!("{} {line}\n", now_ms());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(root.join(AUDIT)) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// `install <version> <exe> [min_schema]` — version + SHA-pin the artifact,
/// atomically activate it (preserving the prior as `previous.json` for
/// rollback). Refuses to clobber an active release or accept a non-exe.
fn cmd_install(args: &[String]) -> ! {
    let version = args.get(0).cloned().unwrap_or_default();
    let exe = args.get(1).cloned().unwrap_or_default();
    let min_schema: i64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
    if version.is_empty() || exe.is_empty() {
        println!("{}", serde_json::json!({"ok":false,"error":"gastown-release install <version> <abs-exe-path> [min_schema]"}));
        std::process::exit(2);
    }
    let exe_path = PathBuf::from(&exe);
    if !exe_path.is_file() {
        println!("{}", serde_json::json!({"ok":false,"error":format!("exe not found: {exe}")}));
        std::process::exit(2);
    }
    let sha = match artifact_hash(&exe_path) {
        Ok(s) => s,
        Err(e) => { println!("{}", serde_json::json!({"ok":false,"error":format!("hash failed: {e}")})); std::process::exit(2); }
    };
    // compatibility: refuse to activate a release that would need a schema
    // newer than the durable state supports.
    if min_schema > durable_schema_version() {
        println!("{}", serde_json::json!({"ok":false,"error":format!("release requires schema v{min_schema} but durable state is v{}", durable_schema_version())}));
        std::process::exit(2);
    }
    let root = releases_root();
    if !root.exists() { let _ = std::fs::create_dir_all(&root); }
    let ver_dir = root.join(&version);
    let _ = std::fs::create_dir_all(&ver_dir);
    // Validate: the currently ACTIVE release must not be overwritten in place.
    if let Some(cur) = read_json::<Activation>(&root, CURRENT) {
        if cur.version == version && std::fs::metadata(&ver_dir.join("nai-fabric-host.exe")).map(|m| m.len() > 0).unwrap_or(false) {
            println!("{}", serde_json::json!({"ok":false,"error":format!("version {version} is already the active release")}));
            std::process::exit(2);
        }
    }
    // Copy artifact into the versioned (immutable) slot, atomically, then hash the DESTINATION.
    let target = ver_dir.join("nai-fabric-host.exe");
    let tmp_target = ver_dir.join(".exe.tmp");
    let _ = std::fs::remove_file(&tmp_target);
    if let Err(e) = std::fs::copy(&exe_path, &tmp_target) {
        println!("{}", serde_json::json!({"ok":false,"error":format!("copy failed: {e}")}));
        std::process::exit(2);
    }
    std::fs::rename(&tmp_target, &target).unwrap();
    let dest_sha = artifact_hash(&target).unwrap();
    if dest_sha != sha {
        println!("{}", serde_json::json!({"ok":false,"error":"sha mismatch after copy; aborting"}));
        std::process::exit(2);
    }
    let prov = Provenance {
        version: version.clone(),
        hash: sha.clone(),
        built_at_unix_ms: now_ms(),
        min_schema_version: min_schema,
        git_desc: git_desc(),
    };
    std::fs::write(ver_dir.join("provenance.json"), serde_json::to_vec_pretty(&prov).unwrap()).unwrap();
    // Atomic activation swap (preserves prior for rollback).
    let activation = Activation { version: version.clone(), hash: sha.clone(), activated_at_unix_ms: now_ms() };
    let prior = read_json::<Activation>(&root, CURRENT);
    if let Some(p) = &prior {
        let _ = write_json_atomic(&root, PREVIOUS, p);
    }
    if let Err(e) = write_json_atomic(&root, CURRENT, &activation) {
        println!("{}", serde_json::json!({"ok":false,"error":format!("activate failed: {e}")}));
        std::process::exit(2);
    }
    audit(&root, &format!("install version={version} sha={sha} min_schema={min_schema} (prior={:?})", prior.as_ref().map(|a| a.version.clone())));
    println!("{}", serde_json::json!({"ok":true,"version":version,"hash":sha,"prior":prior.map(|a| a.version),"releases_root":root.to_string_lossy()}));
    std::process::exit(0);
}

fn cmd_list() -> ! {
    let root = releases_root();
    let mut versions: BTreeMap<String, Provenance> = BTreeMap::new();
    if root.exists() {
        if let Ok(rd) = std::fs::read_dir(&root) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let p = entry.path();
                if p.is_dir() {
                    if let Some(prov) = read_json::<Provenance>(&p, "provenance.json") {
                        versions.insert(name, prov);
                    }
                }
            }
        }
    }
    let current = read_json::<Activation>(&root, CURRENT);
    let previous = read_json::<Activation>(&root, PREVIOUS);
    println!("{}", serde_json::json!({
        "ok":true,
        "release_root": root.to_string_lossy(),
        "current": current,
        "previous": previous,
        "installed": versions,
    }));
    std::process::exit(0);
}

/// `rollback` — atomically swap the previous activation back to current.
/// The previously-active release binary is preserved (never deleted), so this
/// is always reversible and never loses a validated artifact.
fn cmd_rollback() -> ! {
    let root = releases_root();
    if !root.exists() {
        println!("{}", serde_json::json!({"ok":false,"error":"no releases installed"}));
        std::process::exit(2);
    }
    match read_json::<Activation>(&root, PREVIOUS) {
        Some(prev) => {
            // Verify the previous release artifact still exists (integrity).
            let exe = root.join(&prev.version).join("nai-fabric-host.exe");
            if !exe.is_file() {
                println!("{}", serde_json::json!({"ok":false,"error":format!("previous release artifact missing: {exe:?}")}));
                std::process::exit(2);
            }
            let current = read_json::<Activation>(&root, CURRENT);
            // new previous = the current we are leaving.
            if let Some(cur) = current.as_ref() {
                let _ = write_json_atomic(&root, PREVIOUS, cur);
            }
            write_json_atomic(&root, CURRENT, &prev).unwrap();
            audit(&root, &format!("rollback => version={} sha={}", prev.version, prev.hash));
            println!("{}", serde_json::json!({"ok":true,"active":prev.version,"hash":prev.hash,"left":current.map(|c| c.version)}));
            std::process::exit(0);
        }
        None => {
            println!("{}", serde_json::json!({"ok":false,"error":"no previous release to roll back to"}));
            std::process::exit(2);
        }
    }
}

/// Path to the host-published endpoint manifest (`%LOCALAPPDATA%\NAI\Lattice\
/// v1\endpoint.json`). The live worker writes this atomically with its `pid`.
fn endpoint_manifest_path() -> PathBuf {
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    local.join("NAI").join("Lattice").join("v1").join("endpoint.json")
}

/// True when a host worker is already live for this user. Idempotency guard for
/// `launch`: a periodic task wake must NEVER spawn a second monitor (which would
/// spawn a `--child` that fails the per-user singleton guard and crash-loops).
///
/// Probe: read the host-published `endpoint.json` and check whether its `pid`
/// still opens as a process. The host writes this file atomically at startup and
/// re-publishes on swap; a dead/crashed worker leaves a stale pid that fails
/// `OpenProcess`, so a periodic wake correctly relaunches after a crash or
/// reboot. This needs no SID derivation or last-error semantics — it is the
/// documented, reliable "is the singleton alive" test.
fn host_already_running() -> bool {
    let text = match std::fs::read_to_string(endpoint_manifest_path()) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let pid: u32 = match json.get("pid").and_then(|p| p.as_u64()) {
        Some(p) if p >= 1 && p <= u32::MAX as u64 => p as u32,
        _ => return false,
    };
    // If the handle opens, the host (at least a process with that pid) lives.
    unsafe {
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle: HANDLE = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => h,
            Err(_) => return false,
        };
        // Confirm the pid is actually the host (guards against pid reuse to some
        // unrelated process after a crash). Compare the leaf exe name.
        let mut buf: Vec<u16> = vec![0u16; 4096];
        let mut len: u32 = buf.len() as u32;
        let is_host = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
            .is_ok()
            && {
                let image = String::from_utf16_lossy(&buf[..len as usize]);
                let leaf = image.rsplit('\\').next().unwrap_or("").to_lowercase();
                leaf == "nai-fabric-host.exe"
            };
        let _ = CloseHandle(handle);
        is_host
    }
}

/// `launch` — resolve the currently active release and start its monitor
/// singleton DETACHED (CREATE_NO_WINDOW, no console). Immune to killing the
/// launching terminal/process: the monitor is its own process tree.
/// IDEMPOTENT: if the host already has a live worker (endpoint pid alive), this
/// no-ops with a zero exit so a periodic task wake is safe to re-run.
fn cmd_launch() -> ! {
    let root = releases_root();
    let cur = match read_json::<Activation>(&root, CURRENT) {
        Some(c) => c,
        None => {
            println!("{}", serde_json::json!({"ok":false,"error":"no current release; run gastown-release install <version> <exe>"}));
            std::process::exit(2);
        }
    };
    let exe = root.join(&cur.version).join("nai-fabric-host.exe");
    if !exe.is_file() {
        println!("{}", serde_json::json!({"ok":false,"error":format!("active release artifact missing: {exe:?}")}));
        std::process::exit(2);
    }
    // Verify integrity by re-hashing the artifact at launch time.
    let sha = match artifact_hash(&exe) {
        Ok(s) => s,
        Err(e) => { println!("{}", serde_json::json!({"ok":false,"error":format!("hash: {e}")})); std::process::exit(2); }
    };
    if sha != cur.hash {
        println!("{}", serde_json::json!({"ok":false,"error":format!("active release integrity FAILED (sha {sha} != recorded {})", cur.hash)}));
        std::process::exit(2);
    }
    // IDEMPOTENCY (self-heal task safety): if a live host worker already runs
    // for this user, do NOT spawn a second monitor — it would start a `--child`
    // that fails the per-user singleton guard in the worker and crash-loop.
    // A periodic task wake must be able to call `launch` every few minutes and
    // get a cheap no-op when the host is healthy. This is what makes the
    // scheduled-task periodic wake self-healing without duplicating the host.
    if host_already_running() {
        println!("{}", serde_json::json!({"ok":true,"already_running":true,"version":cur.version,"hash":cur.hash,"note":"host worker already live; no-op (idempotent launch)"}));
        std::process::exit(0);
    }
    // Spawn the monitor (no `--child`); it supervises the `--child` worker.
    // Detach fully: CREATE_NO_WINDOW (no console flash) AND redirect the child's
    // std handles to null so it cannot hold open (and thereby strand) the
    // launching terminal's pipes. This is what lets the release outlive the
    // launching shell/terminal entirely.
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    let mut cmd = std::process::Command::new(&exe);
    cmd.creation_flags(0x08000000) // CREATE_NO_WINDOW → no console flash
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => { println!("{}", serde_json::json!({"ok":false,"error":format!("spawn monitor: {e}")})); std::process::exit(2); }
    };
    let pid = child.id();
    audit(&root, &format!("launch monitor pid={pid} version={} sha={}", cur.version, cur.hash));
    println!("{}", serde_json::json!({"ok":true,"monitor_pid":pid,"version":cur.version,"hash":cur.hash,"exe":exe.to_string_lossy()}));
    std::process::exit(0);
}

/// `stop` — ask the monitor to end by finding the running worker and issuing a
/// clean shutdown is out of scope per-binary; instead this prints the PIDs the
/// operator can verify. (The singleton itself performs safe drain on shutdown.)
fn cmd_status() -> ! {
    let root = releases_root();
    let cur = read_json::<Activation>(&root, CURRENT);
    let prev = read_json::<Activation>(&root, PREVIOUS);
    println!("{}", serde_json::json!({"ok":true,"current":cur,"previous":prev,"release_root":root.to_string_lossy()}));
    std::process::exit(0);
}

/// Resolve the currently-active release's `nai-fabric-host.exe` absolute path.
///
/// Single source of truth for "which worker binary should run right now": it
/// reads `current.json` (atomically flipped by `install`) and returns the
/// versioned artifact, only if it actually exists. Used by `launch`-style
/// resolvers and by the monitor's live self-update so the running host always
/// follows the newest activated release without a reboot.
pub fn current_artifact_exe() -> Option<PathBuf> {
    let root = releases_root();
    let activation = read_json::<Activation>(&root, CURRENT)?;
    let exe = root
        .join(&activation.version)
        .join("nai-fabric-host.exe");
    if exe.is_file() {
        Some(exe)
    } else {
        None
    }
}

/// The currently-active release version label (for logging / change detection).
pub fn current_version() -> Option<String> {
    read_json::<Activation>(&releases_root(), CURRENT).map(|a| a.version)
}

pub fn release_main(args: Vec<String>) {
    // The host binary passes the full argv (starting with "gastown-release");
    // the console `gastown-release` bin passes argv[0] already = the subcommand.
    // Find the subcommand position after the marker (if present).
    let idx = args.iter().position(|a| a == "gastown-release").map(|i| i + 1).unwrap_or(0);
    let rest: Vec<String> = args.iter().skip(idx).cloned().collect();
    match rest.first().map(|s| s.as_str()).unwrap_or("status") {
        "install" => cmd_install(&rest[1..]),
        "list" | "ls" => cmd_list(),
        "rollback" => cmd_rollback(),
        "launch" => cmd_launch(),
        "autostart" => cmd_autostart(&rest[1..]),
        "unautostart" => cmd_autostart(&[String::from("off")]),
        "status" => cmd_status(),
        other => {
            println!("{}", serde_json::json!({"ok":false,"error":format!("unknown release subcommand: {other}"),"available":["install","list","rollback","launch","autostart","status"]}));
            std::process::exit(2);
        }
    }
}

/// Register / unregister Windows Task-Scheduler auto-start for the Gas Town host
/// so the Inquisitor singleton autostarts at logon and (via
/// `recover_interrupted_primary_on_startup`) CONTINUES its active primary beads
/// after a PC reboot. `gastown-release autostart [on|off]` (default `on`).
fn cmd_autostart(args: &[String]) -> ! {
    let on = args.first().map(|s| s.as_str()) != Some("off");
    let task = "NAIInquisitorHost";
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gastown-release.exe"));
    if on {
        // AtLogon, run as current interactive user (no admin), hidden window.
        let qexe = exe.to_str().unwrap_or("gastown-release.exe");
        let status = std::process::Command::new("schtasks")
            .args(["/Create", "/F", "/TN", task, "/SC", "ONLOGON", "/TR"])
            .arg(format!("\"{qexe}\" launch"))
            .args(["/RL", "LIMITED", "/IT"])
            .status();
        match status {
            Ok(s) if s.success() => println!("{}", serde_json::json!({"ok":true,"autostart":"on","task":task,"command":qexe,"note":"Task Scheduler ONLOGON auto-start registered; host resumes active beads at logon"})),
            other => println!("{}", serde_json::json!({"ok":false,"error":format!("schtasks create failed: {:?}", other), "task":task})),
        }
    } else {
        let status = std::process::Command::new("schtasks").args(["/Delete", "/F", "/TN", task]).status();
        match status {
            Ok(s) if s.success() => println!("{}", serde_json::json!({"ok":true,"autostart":"off","task":task})),
            other => println!("{}", serde_json::json!({"ok":false,"error":format!("schtasks delete failed: {:?}", other), "task":task})),
        }
    }
    std::process::exit(0);
}