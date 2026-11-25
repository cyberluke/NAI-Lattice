//! Host telemetry for `GET /lattice/api/host-metrics` (DASHBOARD_API §8).
//!
//! Three real sources, no synthesis:
//!  - GPU: `nvidia-smi --query-gpu=... --format=csv,noheader,nounits`, executed
//!    directly (no shell) with a short bounded wait; absent/failed -> `gpu: null`
//!    and `source: "none"`.
//!  - CPU: `GetSystemTimes` deltas between two samples (kernel time includes
//!    idle, so `total = kernel + user`). The first sample has no baseline and
//!    reports `utilizationPct: null`.
//!  - Memory: `GlobalMemoryStatusEx` (total/available physical bytes).
//!
//! One process-wide sampler behind a mutex caches the payload for at most
//! [`SAMPLE_TTL`] (2 s, contract §8); concurrent requests reuse the same sample
//! instead of spawning a process each. `gpu_metrics` is advertised only after
//! [`gpu_available`] observed a real GPU at startup.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Contract §8: values are measured at most every 2 s (cached).
const SAMPLE_TTL: Duration = Duration::from_secs(2);

/// The GPU probe is a bounded subprocess, never a hang.
const GPU_PROBE_TIMEOUT: Duration = Duration::from_millis(1_500);

const NVIDIA_QUERY: &[&str] = &[
    "--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu",
    "--format=csv,noheader,nounits",
];

/// Windows samples are 100 ns ticks, as reported by `GetSystemTimes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CpuTimes {
    idle: u64,
    kernel: u64,
    user: u64,
}

/// One real GPU reading from `nvidia-smi`. Optional fields stay `None` when the
/// driver reports `[N/A]` — never a synthesized zero.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GpuSample {
    pub(crate) name: String,
    pub(crate) utilization_pct: Option<f64>,
    pub(crate) memory_used_bytes: u64,
    pub(crate) memory_total_bytes: u64,
    pub(crate) temperature_c: Option<f64>,
}

/// A display adapter that is not reachable through `nvidia-smi` (for example an
/// Intel iGPU). Presence and driver/status are real; utilization and memory are
/// not exposed by this host API yet, so they stay null with an explicit detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdapterInfo {
    pub(crate) name: String,
    pub(crate) driver: Option<String>,
    pub(crate) status: Option<String>,
}

/// A Windows `ComputeAccelerator` (e.g. Intel AI Boost). Presence and status are
/// real; NPU utilization is not exposed by this host API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NpuInfo {
    pub(crate) name: String,
    pub(crate) status: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DeviceInventory {
    pub(crate) adapters: Vec<AdapterInfo>,
    pub(crate) npu: Vec<NpuInfo>,
}

static INVENTORY: OnceLock<DeviceInventory> = OnceLock::new();

struct Sampler {
    last_sample: Option<Instant>,
    cached: Option<Value>,
    prev_cpu: Option<CpuTimes>,
}

static SAMPLER: Mutex<Sampler> = Mutex::new(Sampler {
    last_sample: None,
    cached: None,
    prev_cpu: None,
});

/// Contract §8 payload. Serves the cached sample when it is younger than 2 s;
/// otherwise samples once (holding the sampler lock so concurrent callers share
/// the work instead of spawning their own `nvidia-smi`).
pub(crate) fn host_metrics() -> Value {
    let mut sampler = match SAMPLER.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let (Some(at), Some(cached)) = (sampler.last_sample, sampler.cached.as_ref()) {
        if at.elapsed() < SAMPLE_TTL {
            return cached.clone();
        }
    }
    let payload = sample(&mut sampler);
    sampler.last_sample = Some(Instant::now());
    sampler.cached = Some(payload.clone());
    payload
}

/// Startup probe used by `main.rs` to decide whether `gpu_metrics` is a real
/// capability of this host. One bounded `nvidia-smi` run, no cache needed.
pub(crate) fn gpu_available() -> bool {
    !nvidia_smi_samples().is_empty()
}

/// One measured sample. CPU utilization is `None` until a previous CPU sample
/// exists (the first call has no delta baseline).
fn sample(sampler: &mut Sampler) -> Value {
    let current_cpu = cpu_times();
    let utilization = match (sampler.prev_cpu, current_cpu) {
        (Some(prev), Some(cur)) => cpu_utilization(prev, cur),
        _ => None,
    };
    if current_cpu.is_some() {
        sampler.prev_cpu = current_cpu;
    }
    let memory = memory_status();
    let gpus = nvidia_smi_samples();
    payload_full(
        crate::now_unix_ms(),
        utilization,
        memory,
        &gpus,
        inventory(),
    )
}

/// The original single-GPU shape, kept for tests and internal callers.
fn payload(
    generated_at: i64,
    cpu_utilization_pct: Option<f64>,
    memory: Option<(u64, u64)>,
    gpu: Option<&GpuSample>,
) -> Value {
    let gpus: Vec<GpuSample> = gpu.cloned().into_iter().collect();
    payload_full(
        generated_at,
        cpu_utilization_pct,
        memory,
        &gpus,
        &DeviceInventory::default(),
    )
}

/// §8 payload with every measured device. `gpu` stays the primary NVIDIA sample
/// for older readers; `gpus` lists all real adapters and `npu` the accelerator.
fn payload_full(
    generated_at: i64,
    cpu_utilization_pct: Option<f64>,
    memory: Option<(u64, u64)>,
    gpus: &[GpuSample],
    inventory: &DeviceInventory,
) -> Value {
    let primary = gpus.first();
    let mut devices: Vec<Value> = Vec::new();
    for (index, gpu) in gpus.iter().enumerate() {
        devices.push(gpu_device_json(index, gpu));
    }
    for adapter in &inventory.adapters {
        // Defense in depth: the probe filters these too, but a caller-built
        // inventory must not leak a virtual monitor or a duplicate NVIDIA row.
        if adapter.name.to_lowercase().contains("nvidia") || !is_real_adapter(&adapter.name) {
            continue;
        }
        devices.push(json!({
            "id": format!("windows-{}", slug(&adapter.name)),
            "vendor": vendor_of(&adapter.name),
            "name": adapter.name,
            "source": "windows",
            "utilizationPct": Value::Null,
            "memoryUsedBytes": Value::Null,
            "memoryTotalBytes": Value::Null,
            "temperatureC": Value::Null,
            "driver": adapter.driver,
            "status": adapter.status,
            "detail": "utilization and memory are not exposed by this host API",
        }));
    }
    let npu = inventory.npu.first().map(|npu| {
        json!({
            "id": "npu-0",
            "vendor": vendor_of(&npu.name),
            "name": npu.name,
            "source": "pnp",
            "status": npu.status,
            "utilizationPct": Value::Null,
            "detail": "NPU utilization is not exposed by this host API",
        })
    });
    let has_windows = !inventory.adapters.is_empty() || !inventory.npu.is_empty();
    let source = match (gpus.is_empty(), has_windows) {
        (false, true) => "nvidia-smi+windows",
        (false, false) => "nvidia-smi",
        (true, true) => "windows",
        (true, false) => "none",
    };
    json!({
        "schema": 1,
        "generatedAt": generated_at,
        "source": source,
        "cpu": {
            "utilizationPct": cpu_utilization_pct.map(number_value).unwrap_or(Value::Null),
        },
        "memory": memory
            .map(|(total, available)| json!({
                "totalBytes": total,
                "availableBytes": available,
            }))
            .unwrap_or(Value::Null),
        "gpu": primary.map(gpu_json).unwrap_or(Value::Null),
        "gpus": devices,
        "npu": npu.unwrap_or(Value::Null),
    })
}

fn gpu_device_json(index: usize, gpu: &GpuSample) -> Value {
    json!({
        "id": format!("nvidia-{index}"),
        "vendor": "nvidia",
        "name": gpu.name,
        "source": "nvidia-smi",
        "utilizationPct": gpu.utilization_pct.map(number_value).unwrap_or(Value::Null),
        "memoryUsedBytes": gpu.memory_used_bytes,
        "memoryTotalBytes": gpu.memory_total_bytes,
        "temperatureC": gpu.temperature_c.map(number_value).unwrap_or(Value::Null),
        "detail": Value::Null,
    })
}

fn gpu_json(gpu: &GpuSample) -> Value {
    json!({
        "name": gpu.name,
        "utilizationPct": gpu.utilization_pct.map(number_value).unwrap_or(Value::Null),
        "memoryUsedBytes": gpu.memory_used_bytes,
        "memoryTotalBytes": gpu.memory_total_bytes,
        "temperatureC": gpu.temperature_c.map(number_value).unwrap_or(Value::Null),
    })
}

/// Integers stay integers in JSON (`5`, not `5.0`), as the contract example
/// shows; genuine fractions survive (`12.5`).
fn number_value(value: f64) -> Value {
    if !value.is_finite() {
        return Value::Null;
    }
    if value.fract() == 0.0 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

/// CPU busy fraction from two cumulative `GetSystemTimes` snapshots.
/// `kernel` already contains `idle`, so total = kernel + user.
fn cpu_utilization(prev: CpuTimes, cur: CpuTimes) -> Option<f64> {
    let idle = cur.idle.checked_sub(prev.idle)?;
    let kernel = cur.kernel.checked_sub(prev.kernel)?;
    let user = cur.user.checked_sub(prev.user)?;
    let total = kernel.saturating_add(user);
    if total == 0 {
        return None;
    }
    let busy = total.saturating_sub(idle);
    let pct = (busy as f64 / total as f64) * 100.0;
    Some((pct.clamp(0.0, 100.0) * 10.0).round() / 10.0)
}

#[cfg(windows)]
fn cpu_times() -> Option<CpuTimes> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetSystemTimes;

    let mut idle = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: three valid, writable FILETIME pointers for the call duration.
    unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).ok()? };
    Some(CpuTimes {
        idle: filetime_ticks(idle),
        kernel: filetime_ticks(kernel),
        user: filetime_ticks(user),
    })
}

#[cfg(not(windows))]
fn cpu_times() -> Option<CpuTimes> {
    None
}

#[cfg(windows)]
fn filetime_ticks(ft: windows::Win32::Foundation::FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

#[cfg(windows)]
fn memory_status() -> Option<(u64, u64)> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    // SAFETY: MEMORYSTATUSEX is a plain POD struct; zero-initializing it and
    // setting `dwLength` is the Win32-documented call contract.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: `status` is a valid, correctly-sized MEMORYSTATUSEX.
    unsafe { GlobalMemoryStatusEx(&mut status).ok()? };
    (status.ullTotalPhys > 0).then_some((status.ullTotalPhys, status.ullAvailPhys))
}

#[cfg(not(windows))]
fn memory_status() -> Option<(u64, u64)> {
    None
}

/// Run `nvidia-smi` once and return its stdout. Direct `Command` (no shell),
/// bounded by [`GPU_PROBE_TIMEOUT`]; on timeout the child is killed. Any
/// failure (absent binary, non-zero exit) is `None`.
fn run_nvidia_smi() -> Option<String> {
    use std::process::{Command, Stdio};

    let mut command = Command::new("nvidia-smi");
    command
        .args(NVIDIA_QUERY)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: a background host must never flash a console.
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + GPU_PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut text = String::new();
                use std::io::Read as _;
                child.stdout.take()?.read_to_string(&mut text).ok()?;
                return Some(text);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// The first GPU sample (legacy single-GPU helper).
fn nvidia_smi_sample() -> Option<GpuSample> {
    parse_nvidia_smi(&run_nvidia_smi()?)
}

/// Every real NVIDIA GPU reported by `nvidia-smi` (one CSV line per GPU).
fn nvidia_smi_samples() -> Vec<GpuSample> {
    run_nvidia_smi()
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .filter_map(parse_nvidia_smi_line)
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the first non-empty line (the first GPU) of `nvidia-smi` CSV output.
fn parse_nvidia_smi(raw: &str) -> Option<GpuSample> {
    raw.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .and_then(parse_nvidia_smi_line)
}

/// `name, utilization.gpu, memory.used, memory.total, temperature.gpu`
/// (`nounits` => utilization is %, memory is MiB, temperature is °C).
fn parse_nvidia_smi_line(line: &str) -> Option<GpuSample> {
    let parts: Vec<&str> = line.split(',').map(str::trim).collect();
    if parts.len() < 5 {
        return None;
    }
    // A GPU name may itself contain a comma; only the four metrics are fixed.
    let split = parts.len() - 4;
    let name = parts[..split].join(", ");
    if name.is_empty() {
        return None;
    }
    let utilization_pct = parse_smi_number(parts[split]);
    let memory_used_bytes = mib_to_bytes(parse_smi_number(parts[split + 1])?);
    let memory_total_bytes = mib_to_bytes(parse_smi_number(parts[split + 2])?);
    if memory_total_bytes == 0 {
        return None;
    }
    let temperature_c = parse_smi_number(parts[split + 3]);
    Some(GpuSample {
        name,
        utilization_pct,
        memory_used_bytes,
        memory_total_bytes,
        temperature_c,
    })
}

/// `[N/A]`, `[Not Supported]`, empty or non-finite driver values are unknown,
/// not zero.
fn parse_smi_number(raw: &str) -> Option<f64> {
    let text = raw.trim();
    if text.is_empty() || text.starts_with('[') {
        return None;
    }
    text.parse::<f64>().ok().filter(|value| value.is_finite())
}

fn mib_to_bytes(mib: f64) -> u64 {
    (mib.max(0.0) * 1024.0 * 1024.0).round() as u64
}

/// Static device inventory probed once (not per sample): every Windows display
/// adapter and ComputeAccelerator (NPU). `nvidia-smi` stays the live NVIDIA
/// source; this fills in the rest of the machine truthfully.
fn inventory() -> &'static DeviceInventory {
    INVENTORY.get_or_init(probe_inventory)
}

fn probe_inventory() -> DeviceInventory {
    // Fixed script, no user input. `@()` keeps single results arrays; the JSON
    // is parsed below. Virtual/meta monitors are filtered out.
    let script = r#"$ad=@(Get-CimInstance Win32_VideoController -ErrorAction SilentlyContinue | Select-Object Name,DriverVersion,Status);$np=@(Get-PnpDevice -PresentOnly -Class ComputeAccelerator -ErrorAction SilentlyContinue | Select-Object FriendlyName,Status);[pscustomobject]@{adapters=$ad;npu=$np}|ConvertTo-Json -Depth 4 -Compress"#;
    run_inventory_probe(script)
        .and_then(|text| parse_inventory(&text))
        .unwrap_or_default()
}

fn run_inventory_probe(script: &str) -> Option<String> {
    for exe in ["powershell.exe", "pwsh.exe"] {
        if let Some(text) = run_inventory_with(exe, script) {
            return Some(text);
        }
    }
    None
}

fn run_inventory_with(exe: &str, script: &str) -> Option<String> {
    use std::process::{Command, Stdio};
    let mut command = Command::new(exe);
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut text = String::new();
                use std::io::Read as _;
                child.stdout.take()?.read_to_string(&mut text).ok()?;
                return Some(text);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn parse_inventory(text: &str) -> Option<DeviceInventory> {
    let value: Value = serde_json::from_str(text.trim()).ok()?;
    let mut out = DeviceInventory::default();
    for item in array_or_one(value.get("adapters")) {
        let name = item
            .get("Name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let Some(name) = name else { continue };
        if !is_real_adapter(name) || out.adapters.iter().any(|a| a.name.eq_ignore_ascii_case(name)) {
            continue;
        }
        out.adapters.push(AdapterInfo {
            name: name.to_string(),
            driver: item
                .get("DriverVersion")
                .and_then(Value::as_str)
                .map(str::to_string),
            status: item.get("Status").and_then(Value::as_str).map(str::to_string),
        });
    }
    for item in array_or_one(value.get("npu")) {
        let name = item
            .get("FriendlyName")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let Some(name) = name else { continue };
        out.npu.push(NpuInfo {
            name: name.to_string(),
            status: item.get("Status").and_then(Value::as_str).map(str::to_string),
        });
    }
    Some(out)
}

fn array_or_one(value: Option<&Value>) -> Vec<&Value> {
    match value {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(item) => vec![item],
        None => Vec::new(),
    }
}

fn is_real_adapter(name: &str) -> bool {
    let lower = name.to_lowercase();
    !lower.is_empty()
        && !lower.contains("virtual")
        && !lower.contains("meta")
        && !lower.contains("remote")
        && !lower.contains("basic display")
}

fn vendor_of(name: &str) -> &'static str {
    let lower = name.to_lowercase();
    if lower.contains("nvidia") {
        "nvidia"
    } else if lower.contains("intel") {
        "intel"
    } else if lower.contains("amd") || lower.contains("radeon") {
        "amd"
    } else {
        "other"
    }
}

fn slug(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RTX_LINE: &str = "NVIDIA GeForce RTX 4090, 5, 2807, 23028, 54";

    #[test]
    fn nvidia_smi_line_maps_to_contract_fields() {
        let gpu = parse_nvidia_smi_line(RTX_LINE).expect("real 4090 line");
        assert_eq!(gpu.name, "NVIDIA GeForce RTX 4090");
        assert_eq!(gpu.utilization_pct, Some(5.0));
        assert_eq!(gpu.memory_used_bytes, 2807 * 1024 * 1024);
        assert_eq!(gpu.memory_total_bytes, 23028 * 1024 * 1024);
        assert_eq!(gpu.temperature_c, Some(54.0));

        // Only the FIRST GPU is used, even if several are printed.
        let two = format!("{RTX_LINE}\nNVIDIA GeForce RTX 3060, 1, 100, 12288, 40\n");
        let first = parse_nvidia_smi(&two).expect("first GPU");
        assert_eq!(first.name, "NVIDIA GeForce RTX 4090");
    }

    #[test]
    fn nvidia_smi_unknown_metrics_stay_null_never_zero() {
        let gpu = parse_nvidia_smi_line("NVIDIA L40S, [N/A], 1024, 46068, [Not Supported]")
            .expect("name and memory are real");
        assert_eq!(gpu.utilization_pct, None);
        assert_eq!(gpu.temperature_c, None);
        assert_eq!(gpu.memory_used_bytes, 1024 * 1024 * 1024);
        assert_eq!(gpu.memory_total_bytes, 46068 * 1024 * 1024);
    }

    #[test]
    fn nvidia_smi_malformed_or_absent_yields_none() {
        assert!(parse_nvidia_smi("").is_none());
        assert!(parse_nvidia_smi("   \n\n").is_none());
        assert!(parse_nvidia_smi("only, four, fields, here").is_none());
        assert!(parse_nvidia_smi("NVIDIA X, 5, nope, 1024, 50").is_none());
        assert!(parse_nvidia_smi("NVIDIA X, 5, 10, 0, 50").is_none());
    }

    #[test]
    fn cpu_utilization_uses_kernel_plus_user_and_idle_delta() {
        let prev = CpuTimes {
            idle: 1_000,
            kernel: 2_000,
            user: 1_000,
        };
        // idle +500, kernel +600, user +400 => total 1_000, busy 500 => 50 %.
        let cur = CpuTimes {
            idle: 1_500,
            kernel: 2_600,
            user: 1_400,
        };
        assert_eq!(cpu_utilization(prev, cur), Some(50.0));

        // Fully idle: every tick was idle => 0 %.
        let idle = CpuTimes {
            idle: 2_000,
            kernel: 3_000,
            user: 1_000,
        };
        assert_eq!(cpu_utilization(prev, idle), Some(0.0));

        // Fully busy: no idle ticks => 100 %.
        let busy = CpuTimes {
            idle: 1_000,
            kernel: 2_500,
            user: 1_500,
        };
        assert_eq!(cpu_utilization(prev, busy), Some(100.0));

        // No elapsed time (identical snapshots) => unknown, not 0.
        assert_eq!(cpu_utilization(prev, prev), None);
        // A decreasing counter is not a usable delta.
        let backwards = CpuTimes {
            idle: 500,
            kernel: 1_000,
            user: 500,
        };
        assert_eq!(cpu_utilization(prev, backwards), None);
    }

    #[test]
    fn payload_matches_the_frozen_shape_with_a_gpu() {
        let gpu = GpuSample {
            name: "NVIDIA GeForce RTX 4090".into(),
            utilization_pct: Some(5.0),
            memory_used_bytes: 2_807_037_952,
            memory_total_bytes: 24_142_446_592,
            temperature_c: Some(54.0),
        };
        let out = payload(
            1_764_000_000_000,
            Some(12.5),
            Some((68_559_708_160, 41_234_567_890)),
            Some(&gpu),
        );
        assert_eq!(out["schema"], 1);
        assert_eq!(out["generatedAt"], 1_764_000_000_000i64);
        assert_eq!(out["source"], "nvidia-smi");
        assert_eq!(out["cpu"]["utilizationPct"], 12.5);
        assert_eq!(out["memory"]["totalBytes"], 68_559_708_160u64);
        assert_eq!(out["memory"]["availableBytes"], 41_234_567_890u64);
        assert_eq!(out["gpu"]["name"], "NVIDIA GeForce RTX 4090");
        assert_eq!(out["gpu"]["utilizationPct"], 5, "whole percent stays an int");
        assert_eq!(out["gpu"]["memoryUsedBytes"], 2_807_037_952u64);
        assert_eq!(out["gpu"]["memoryTotalBytes"], 24_142_446_592u64);
        assert_eq!(out["gpu"]["temperatureC"], 54);
    }

    #[test]
    fn payload_without_gpu_or_baseline_is_null_not_fabricated() {
        let out = payload(1_764_000_000_000, None, None, None);
        assert_eq!(out["schema"], 1);
        assert_eq!(out["source"], "none");
        assert_eq!(out["cpu"]["utilizationPct"], Value::Null);
        assert_eq!(out["memory"], Value::Null);
        assert_eq!(out["gpu"], Value::Null);
        assert_eq!(
            out.as_object().map(|o| o.len()),
            Some(8),
            "§8 keys plus the additive gpus/npu inventory"
        );
        for key in ["schema", "generatedAt", "source", "cpu", "memory", "gpu", "gpus", "npu"] {
            assert!(out.get(key).is_some(), "payload exposes {key}");
        }
    }

    #[test]
    fn payload_with_a_gpu_but_no_cpu_or_memory_is_honest() {
        let gpu = GpuSample {
            name: "NVIDIA L40S".into(),
            utilization_pct: None,
            memory_used_bytes: 1_073_741_824,
            memory_total_bytes: 48_318_382_080,
            temperature_c: None,
        };
        let out = payload(7, None, None, Some(&gpu));
        assert_eq!(out["source"], "nvidia-smi");
        assert_eq!(out["cpu"]["utilizationPct"], Value::Null);
        assert_eq!(out["memory"], Value::Null);
        assert_eq!(out["gpu"]["utilizationPct"], Value::Null);
        assert_eq!(out["gpu"]["temperatureC"], Value::Null);
    }

    #[test]
    fn nvidia_smi_samples_reads_every_gpu_line() {
        let raw = format!("{RTX_LINE}\nNVIDIA L40S, 1, 1024, 46068, 40\n");
        let samples: Vec<GpuSample> = raw
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .filter_map(parse_nvidia_smi_line)
            .collect();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[1].name, "NVIDIA L40S");
    }

    #[test]
    fn payload_lists_every_adapter_and_the_npu() {
        let gpu = GpuSample {
            name: "NVIDIA GeForce RTX 4090".into(),
            utilization_pct: Some(0.0),
            memory_used_bytes: 1,
            memory_total_bytes: 2,
            temperature_c: Some(58.5),
        };
        let inventory = DeviceInventory {
            adapters: vec![
                AdapterInfo {
                    name: "Intel(R) Graphics".into(),
                    driver: Some("32.0".into()),
                    status: Some("OK".into()),
                },
                AdapterInfo {
                    name: "Meta Virtual Monitor".into(),
                    driver: None,
                    status: None,
                },
                AdapterInfo {
                    name: "NVIDIA GeForce RTX 4090".into(),
                    driver: None,
                    status: None,
                },
            ],
            npu: vec![NpuInfo {
                name: "Intel(R) AI Boost".into(),
                status: Some("OK".into()),
            }],
        };
        let out = payload_full(1, Some(10.0), None, &[gpu], &inventory);
        assert_eq!(out["source"], "nvidia-smi+windows");
        let devices = out["gpus"].as_array().expect("gpus array");
        assert_eq!(
            devices.len(),
            2,
            "nvidia + intel; virtual and duplicate nvidia skipped"
        );
        assert_eq!(devices[1]["name"], "Intel(R) Graphics");
        assert_eq!(devices[1]["utilizationPct"], Value::Null);
        assert_eq!(out["npu"]["name"], "Intel(R) AI Boost");
        assert_eq!(out["npu"]["status"], "OK");
    }

    #[test]
    fn parse_inventory_accepts_arrays_and_single_objects() {
        let single = r#"{"adapters":{"Name":"Intel(R) Graphics","DriverVersion":"1","Status":"OK"},"npu":{"FriendlyName":"Intel(R) AI Boost","Status":"OK"}}"#;
        let inv = parse_inventory(single).expect("single objects");
        assert_eq!(inv.adapters.len(), 1);
        assert_eq!(inv.npu.len(), 1);
        let arrays = r#"{"adapters":[{"Name":"Intel(R) Graphics","Status":"OK"},{"Name":"Virtual Desktop Monitor"}],"npu":[]}"#;
        let inv = parse_inventory(arrays).expect("arrays");
        assert_eq!(inv.adapters.len(), 1, "virtual monitor filtered");
        assert!(inv.npu.is_empty());
    }
}
