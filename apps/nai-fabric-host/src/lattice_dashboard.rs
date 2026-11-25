//! Lattice observability dashboard — Bloomberg-terminal / ElasticSearch-admin style
//! live view of the canonical `observability.log.v1` ledger.
//!
//! Served by the fabric host control server on `GET /lattice` (embedded HTML),
//! `GET /lattice/events` (SSE live stream of `observability.log.v1` records + a
//! periodic `stats` line), `GET /lattice/api` (legacy JSON tail / search /
//! by-trace / component-breakdown for the dashboard's fetch calls and for the
//! mcp-nai-lattice log methods), and the frozen v1 endpoints in
//! `docs/DASHBOARD_API.md`: `GET /lattice/api/summary` (here),
//! `GET /lattice/api/query` + `/lattice/api/traces` (`lattice_query.rs`).
//!
//! Deliberately NON-BLOCKING for the host: the ONLY continuous ledger reader is
//! the shared scan broker (`lattice_broker.rs`) — one bounded read per ~1.25 s
//! for every SSE connection, not one scanner thread per connection. One-shot
//! JSON reads (`/lattice/api`, `/summary`) stay bounded recent windows.
//!
//! The embedded HTML below remains the rollback path until the Vue shell passes
//! its acceptance gate; its wire shape (`search`/`log`/`stats` SSE events and
//! the legacy JSON keys) is kept compatible.

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;

/// The Lattice dashboard icon (optimized 256px copy of "Neon Glass Network Hub").
/// Embedded so the installed host always serves it regardless of repo path.
pub(crate) const LATTICE_ICON_PNG: &[u8] = include_bytes!("../../../assets/icons/lattice-hub-256.png");
pub(crate) const LATTICE_ICON_MIME: &str = "image/png";

/// Successful SSE data-event writes (the existing `SSE PUSHES` contract).
static LIVE_PUSHES: AtomicUsize = AtomicUsize::new(0);
/// Established `/lattice/events` connections.
static ACTIVE_SSE: AtomicUsize = AtomicUsize::new(0);
/// Connections are capped by the shared broker (one SSE connection = one
/// subscriber). Reported as `maxSseClients` for compatibility.
const MAX_SSE_CLIENTS: usize = crate::lattice_broker::MAX_SUBSCRIBERS;

/// Counts one live SSE connection for the dashboard; the broker enforces the
/// hard subscriber cap and answers `503 + Retry-After` over it.
struct SseLease;
impl SseLease {
    fn acquire() -> Self {
        ACTIVE_SSE.fetch_add(1, Ordering::AcqRel);
        Self
    }
}
impl Drop for SseLease {
    fn drop(&mut self) {
        ACTIVE_SSE.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Esimated per-minute push cadence for the "traffic" illusion isn't fabricated:
/// these are REAL counters from the ledger scan window, not fake "traffic".
pub(crate) fn push_count() -> usize {
    LIVE_PUSHES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Testable core of the bounded newest-window read against an explicit runtime.
pub(crate) fn latest_logs_on(rt: &nai_lattice_runtime::LatticeRuntime, limit: usize) -> Vec<Value> {
    // IMPORTANT: do NOT constrain `event_type` here. The CQRS read model's fast
    // tail path only triggers for an UNFILTERED event.read_range (all types). If
    // we hard-filter `log.record`, the runtime falls back to the slow scan path
    // AND — worse — when the current observer emits health/infrastructure records
    // (not `log.record`), the bounded tail returns only the STALE historical
    // log.record lines, making the live dashboard look frozen (the exact 7h-stale
    // desync we're fixing). The dashboard filters in-process over the bounded
    // real tail instead.
    // Namespace-consolidated read: NAI OS temporarily emitted observer records
    // (health / telemetry) under the runtime default namespace during the
    // pre-fix window (~9h), while canonical observers went to
    // `observability.log.v1`. The dashboard is the user's picture of the whole
    // observability stream, so we read BOTH namespaces and merge by runtime
    // sequence (desc). This keeps the UI live on the authoritative ledger without
    // mutating stored records; once every producer sits under the canonical
    // namespace the `default` bucket simply falls out of the recent window.
    let mut out: Vec<serde_json::Value> = Vec::new();
    for ns in [
        crate::gastown_orto::lattice_obs_namespace().to_string(),
        "default".to_string(),
    ] {
        let args = serde_json::json!({
            "namespace": ns,
            "limit": limit.min(4096),
            "tail": true,
        });
        let raw = rt.dispatch(
            "event.read_range",
            &serde_json::to_vec(&args).unwrap_or_default(),
            "lattice-dashboard",
            None,
        );
        let Ok(raw) = raw else { continue };
        let Ok(v): Result<serde_json::Value, _> = serde_json::from_slice(&raw) else {
            continue;
        };
        if let Some(events) = v.get("events").and_then(|e| e.as_array()) {
            for ev in events {
                // The payload already carries {ts_ms, level, component, trace_id,
                // caller, message, extras}; add cursor + correlation for the UI.
                let mut rec = ev.get("payload").cloned().unwrap_or(serde_json::json!({}));
                if let Some(seq) = ev.get("runtime_sequence").and_then(|s| s.as_u64()) {
                    rec["runtime_sequence"] = serde_json::json!(seq);
                }
                rec["timestamp_utc_ms"] = ev
                    .get("timestamp_utc_ms")
                    .cloned()
                    .unwrap_or(serde_json::json!(0));
                out.push(rec);
            }
        }
    }
    // Merge both namespaces into one newest-first stream by runtime sequence so a
    // stale `default` record never outranks a fresh canonical one (or vice versa).
    out.sort_by(|a, b| {
        let sa = a.get("runtime_sequence").and_then(|s| s.as_u64()).unwrap_or(0);
        let sb = b.get("runtime_sequence").and_then(|s| s.as_u64()).unwrap_or(0);
        sb.cmp(&sa)
    });
    out.truncate(limit.clamp(1, 4096));
    out
}

/// Aggregates computed from ONE bounded window.
struct WindowAggregate {
    window: usize,
    levels: std::collections::BTreeMap<String, usize>,
    components: std::collections::BTreeMap<String, usize>,
    distinct_trace_ids: usize,
    errors: usize,
    /// Newest measured in-process NAI OS emitter health record (never faked).
    emitter: Option<(u64, Value)>,
}

fn aggregate_window(logs: &[Value]) -> WindowAggregate {
    let mut levels: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut components: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut trace_ids: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // Measured from the latest in-process NAI OS emitter health record.
    // Absent means UNKNOWN, never synthesize zero backpressure.
    let mut newest_emitter: Option<(u64, serde_json::Value)> = None;
    for l in logs {
        if l.get("component").and_then(|x| x.as_str()) == Some("nai-os.observability") {
            if let Some(metrics) = l.pointer("/extras/emitter") {
                let ts = l.get("ts_ms").and_then(|x| x.as_u64()).unwrap_or(0);
                if newest_emitter.as_ref().is_none_or(|(prior, _)| ts > *prior) {
                    newest_emitter = Some((ts, metrics.clone()));
                }
            }
        }
        if let Some(lv) = l.get("level").and_then(|x| x.as_str()) {
            *levels.entry(lv.to_string()).or_insert(0) += 1;
        }
        if let Some(c) = l.get("component").and_then(|x| x.as_str()) {
            *components.entry(c.to_string()).or_insert(0) += 1;
        }
        if let Some(t) = l.get("trace_id").and_then(|x| x.as_str()) {
            if !t.is_empty() {
                trace_ids.insert(t.to_string());
            }
        }
    }
    let errors = levels.get("error").copied().unwrap_or(0)
        + levels.get("warn").copied().unwrap_or(0);
    WindowAggregate {
        window: logs.len(),
        levels,
        components,
        distinct_trace_ids: trace_ids.len(),
        errors,
        emitter: newest_emitter,
    }
}

/// Stream/broker counters shared by `stats()` and `summary()`.
pub(crate) fn stream_stats() -> Value {
    let b = crate::lattice_broker::snapshot_stats();
    serde_json::json!({
        "activeSseClients": ACTIVE_SSE.load(Ordering::Relaxed),
        "maxSseClients": MAX_SSE_CLIENTS,
        "pushTotal": push_count(),
        "subscribers": b.subscribers,
        "brokerScansTotal": b.scans,
        "lastScanUnixMs": b.last_scan_unix_ms,
        "droppedTotal": b.dropped_total,
    })
}

/// Aggregate counters for the traffic/health panel, from a bounded recent scan.
pub(crate) fn stats() -> Value {
    match crate::gastown_orto::RUNTIME.get() {
        Some(rt) => stats_on(rt),
        None => stats_from(&WindowAggregate {
            window: 0,
            levels: Default::default(),
            components: Default::default(),
            distinct_trace_ids: 0,
            errors: 0,
            emitter: None,
        }),
    }
}

/// Testable core of [`stats`] against an explicit runtime.
pub(crate) fn stats_on(rt: &nai_lattice_runtime::LatticeRuntime) -> Value {
    let logs = latest_logs_on(rt, 2000);
    stats_from(&aggregate_window(&logs))
}

fn stats_from(agg: &WindowAggregate) -> Value {
    serde_json::json!({
        "schema": 1,
        "window": agg.window,
        "levels": agg.levels,
        "errors": agg.errors,
        "components": agg.components,
        "distinctTraceIds": agg.distinct_trace_ids,
        "livePushes": push_count(),
        "activeSseClients": ACTIVE_SSE.load(Ordering::Relaxed),
        "maxSseClients": MAX_SSE_CLIENTS,
        "stream": stream_stats(),
        "emitter": agg.emitter.as_ref().map(|(ts, metrics)| serde_json::json!({
            "observedAt": ts,
            "metrics": metrics,
        })),
        "generatedAt": crate::now_unix_ms(),
    })
}

/// Search / tail JSON for the dashboard fetch API and the MCP log methods.
/// Filters are applied in-process over the bounded recent window (newest-first).
pub(crate) fn search(opts: &SearchOpts) -> Value {
    match crate::gastown_orto::RUNTIME.get() {
        Some(rt) => search_on(rt, opts),
        None => search_over(Vec::new(), opts),
    }
}

/// Testable core of [`search`] against an explicit runtime.
pub(crate) fn search_on(rt: &nai_lattice_runtime::LatticeRuntime, opts: &SearchOpts) -> Value {
    search_over(latest_logs_on(rt, 6000), opts)
}

fn search_over(mut logs: Vec<Value>, opts: &SearchOpts) -> Value {
    logs.retain(|l| {
        let level = l.get("level").and_then(|x| x.as_str()).unwrap_or("");
        if let Some(min) = &opts.level_min {
            if !level_at_least(level, min) {
                return false;
            }
        }
        if let Some(c) = &opts.component {
            let comp = l.get("component").and_then(|x| x.as_str()).unwrap_or("");
            if comp != c {
                return false;
            }
        }
        if let Some(t) = &opts.trace {
            let ct = l.get("trace_id").and_then(|x| x.as_str()).unwrap_or("");
            if ct != t {
                return false;
            }
        }
        if let Some(needle) = &opts.contains {
            let msg = l.get("message").and_then(|x| x.as_str()).unwrap_or("");
            let hay = format!("{level} {msg}").to_lowercase();
            if !hay.contains(&needle.to_lowercase()) {
                return false;
            }
        }
        true
    });
    logs.truncate(opts.limit.unwrap_or(100).clamp(1, 500));
    serde_json::json!({
        "schema": 1,
        "hits": logs,
        "count": logs.len(),
        "generatedAt": crate::now_unix_ms(),
    })
}

pub(crate) fn level_at_least(level: &str, min: &str) -> bool {
    const ORDER: &[&str] = &["trace", "debug", "info", "warn", "error"];
    let a = ORDER.iter().position(|l| *l == level).unwrap_or(2);
    let b = ORDER.iter().position(|l| *l == min).unwrap_or(2);
    a >= b
}

/// Search options mirroring the MCP log methods.
#[derive(Default)]
pub(crate) struct SearchOpts {
    pub level_min: Option<String>,
    pub component: Option<String>,
    pub trace: Option<String>,
    pub contains: Option<String>,
    pub limit: Option<usize>,
}

/// Parse the `/lattice/api` query string into `SearchOpts`. The full path
/// (including `?...`) is passed in; `%20`-style escaping is decoded minimally.
pub(crate) fn parse_lattice_search_opts(path: &str) -> SearchOpts {
    let mut o = SearchOpts::default();
    let Some((_, qs)) = path.split_once('?') else { return o };
    for pair in qs.split('&') {
        let Some((k, v)) = pair.split_once('=') else { continue };
        let val = percent_decode(v).unwrap_or_default();
        match k {
            "level" => o.level_min = (!val.is_empty()).then(|| val.clone()),
            "component" => o.component = (!val.is_empty()).then(|| val.clone()),
            "trace" => o.trace = (!val.is_empty()).then(|| val.clone()),
            "contains" => o.contains = (!val.is_empty()).then(|| val.clone()),
            "limit" => o.limit = val.parse::<usize>().ok().map(|n| n.clamp(1,500)),
            _ => {}
        }
    }
    o
}

/// Minimal percent-decode (`%XX` → byte; `+` → space).
pub(crate) fn percent_decode(v: &str) -> Option<String> {
    // Percent decoding is BYTE-based: building chars from %C5%99 separately
    // corrupts Czech/Vietnamese UTF-8 text and silently breaks log searches.
    let raw = v.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'%' if i + 2 < raw.len() => {
                let h = std::str::from_utf8(&raw[i + 1..i + 3]).ok()?;
                let b = u8::from_str_radix(h, 16).ok()?;
                out.push(b);
                i += 3;
            }
            b'+' => { out.push(b' '); i += 1; }
            b => { out.push(b); i += 1; }
        }
    }
    String::from_utf8(out).ok()
}

// ===========================================================================
// GET /lattice/api/summary (contract §1)
// ===========================================================================

/// The single host-owned feature list: Lattice API surfaces that exist in THIS
/// binary. `main.rs` registers exactly this list on the runtime and
/// [`features_summary`] merges it into `/lattice/api/summary`, so
/// `runtime.describe.features` and `summary.features` cannot drift. It is never
/// a claim about unimplemented work; runtime-owned features (lifecycle,
/// maintenance, shared storage) are advertised by the runtime itself.
pub(crate) const HOST_FEATURES: &[&str] = &[
    // DASHBOARD_API §8: served by this binary for every host (GPU presence is a
    // separate, truthfully conditional `gpu_metrics` advertisement in main.rs).
    "host_metrics",
    "search_api",
    "sse_resume",
    "shutdown_if_idle",
    "summary_api",
    // DASHBOARD_API §9: derived from the same bounded trace window.
    "topology_api",
    "traces_api",
    // Gap audit §2.4: truthful v2 execution-chain graph (explicit spans only).
    "execution_chain_api",
];

fn health_summary(health: &Value) -> Value {
    let pick = |keys: &[&str]| -> Value {
        keys.iter()
            .find_map(|k| health.get(*k).cloned())
            .unwrap_or(Value::Null)
    };
    serde_json::json!({
        "state": pick(&["state"]),
        "detail": pick(&["detail"]),
        "lastError": pick(&["lastError", "last_error"]),
        "sinceUnixMs": pick(&["sinceUnixMs", "since_unix_ms"]),
        "uptimeMs": pick(&["uptimeMs", "uptime_ms"]),
    })
}

fn version_summary() -> Value {
    serde_json::json!({
        "host": env!("CARGO_PKG_VERSION"),
        "dashboard": crate::lattice_assets::bundle_version(),
        "protocolMin": 1,
        "protocolMax": 1,
        "storageSchema": 1,
    })
}

/// Features advertised by THIS host: runtime capabilities from
/// `runtime.describe` (authoritative, passed through verbatim) plus
/// [`HOST_FEATURES`]. Every entry is a real capability — nothing is invented;
/// if `describe` has no `features` array (an older runtime) the host list
/// stands alone.
fn features_summary(rt: Option<&nai_lattice_runtime::LatticeRuntime>) -> Value {
    let mut out: Vec<Value> = Vec::new();
    if let Some(rt) = rt {
        if let Ok(raw) = rt.dispatch("runtime.describe", b"{}", "lattice-summary", None) {
            if let Ok(v) = serde_json::from_slice::<Value>(&raw) {
                if let Some(arr) = v.get("features").and_then(Value::as_array) {
                    out.extend(arr.iter().cloned());
                }
            }
        }
    }
    for feature in HOST_FEATURES {
        if !out.iter().any(|v| v.as_str() == Some(feature)) {
            out.push(Value::from(*feature));
        }
    }
    Value::Array(out)
}

/// Contract §1 `storage`: produced by the runtime's shared storage policy
/// (`runtime.storage.status`), passed through verbatim. Omitted only when the
/// method fails, so an older runtime degrades to the previous summary shape
/// instead of inventing numbers.
fn storage_summary(rt: Option<&nai_lattice_runtime::LatticeRuntime>) -> Option<Value> {
    let rt = rt?;
    let raw = rt
        .dispatch("runtime.storage.status", b"{}", "lattice-summary", None)
        .ok()?;
    serde_json::from_slice(&raw).ok()
}

fn summary_from(
    agg: &WindowAggregate,
    health: Value,
    features: Value,
    storage: Option<Value>,
) -> Value {
    let mut out = serde_json::json!({
        "schema": 1,
        "generatedAt": crate::now_unix_ms(),
        "window": agg.window,
        "levels": agg.levels,
        "errors": agg.errors,
        "components": agg.components,
        "distinctTraceIds": agg.distinct_trace_ids,
        "stream": stream_stats(),
        "health": health,
        "version": version_summary(),
        "features": features,
    });
    if let Some(storage) = storage {
        if let Some(object) = out.as_object_mut() {
            object.insert("storage".to_string(), storage);
        }
    }
    out
}

/// Compact summary for the NAI OS toolbar / shell header. Bounded recent
/// window only; every count is measured, every unknown is `null`.
pub(crate) fn summary() -> Value {
    match crate::gastown_orto::RUNTIME.get() {
        Some(rt) => summary_on(rt),
        None => summary_from(
            &aggregate_window(&[]),
            health_summary(&Value::Null),
            features_summary(None),
            None,
        ),
    }
}

/// Testable core of [`summary`] against an explicit runtime.
pub(crate) fn summary_on(rt: &nai_lattice_runtime::LatticeRuntime) -> Value {
    let logs = latest_logs_on(rt, 2000);
    let agg = aggregate_window(&logs);
    summary_from(
        &agg,
        health_summary(&rt.health()),
        features_summary(Some(rt)),
        storage_summary(Some(rt)),
    )
}

// ===========================================================================
// SSE (shared broker)
// ===========================================================================

/// Broker-backed, cursor-resumable SSE for `/lattice/events`.
///
/// Every connection is one logical subscriber of the single shared scan pump;
/// the route signature is unchanged. Over the subscriber cap the broker refuses
/// with `503 + Retry-After: 3` — never a silently dead stream.
pub(crate) fn lattice_sse<W: Write>(w: &mut W, after: Option<u64>) {
    let sub = match crate::lattice_broker::broker().subscribe(after) {
        Ok(sub) => sub,
        Err(crate::lattice_broker::SubscribeError::AtCapacity) => {
            let _ = w.write_all(
                format!(
                    "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nRetry-After: {}\r\nConnection: close\r\n\r\n",
                    crate::lattice_broker::RETRY_AFTER_SECS
                )
                .as_bytes(),
            );
            return;
        }
    };
    let _lease = SseLease::acquire();
    if w.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache, no-transform\r\nConnection: keep-alive\r\nX-Accel-Buffering: no\r\nX-Content-Type-Options: nosniff\r\n\r\n").is_err() {
        return;
    }
    // Emitted exactly once at stream open (contract §4).
    if w.write_all(b"retry: 3000\r\n\r\n").is_err() || w.flush().is_err() {
        return;
    }
    let started = std::time::Instant::now();
    let lifetime = crate::lattice_broker::stream_lifetime();
    while started.elapsed() < lifetime {
        match sub.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(msg) => {
                if write_broker_msg(w, msg).is_err() {
                    break;
                }
                if w.flush().is_err() {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // Dropping `sub` unregisters the subscriber from the shared pump.
}

fn write_broker_msg<W: Write>(
    w: &mut W,
    msg: crate::lattice_broker::BrokerMsg,
) -> std::io::Result<()> {
    use crate::lattice_broker::BrokerMsg;
    match msg {
        BrokerMsg::Search { id, payload } => {
            let id = id.map(|i| format!("id: {i}\r\n")).unwrap_or_default();
            write!(w, "{id}event: search\r\ndata: {payload}\r\n\r\n")?;
            LIVE_PUSHES.fetch_add(1, Ordering::Relaxed);
        }
        BrokerMsg::Log { id, payload } => {
            write!(w, "id: {id}\r\nevent: log\r\ndata: {payload}\r\n\r\n")?;
            LIVE_PUSHES.fetch_add(1, Ordering::Relaxed);
        }
        BrokerMsg::Stats(payload) => {
            write!(w, "event: stats\r\ndata: {payload}\r\n\r\n")?;
            LIVE_PUSHES.fetch_add(1, Ordering::Relaxed);
        }
        BrokerMsg::State(payload) => {
            write!(w, "event: state\r\ndata: {payload}\r\n\r\n")?;
            LIVE_PUSHES.fetch_add(1, Ordering::Relaxed);
        }
        BrokerMsg::Status {
            cursor,
            subscribers,
            dropped_total,
            broker_scans,
        } => {
            let data = serde_json::to_string(&serde_json::json!({
                "cursor": cursor,
                "subscribers": subscribers,
                "droppedTotal": dropped_total,
                "brokerScans": broker_scans,
            }))
            .unwrap_or_default();
            write!(w, "event: status\r\ndata: {data}\r\n\r\n")?;
            LIVE_PUSHES.fetch_add(1, Ordering::Relaxed);
        }
        // Comments are not "pushes": they carry no data and advance no cursor.
        BrokerMsg::Heartbeat => {
            write!(w, ": heartbeat\r\n\r\n")?;
        }
    }
    Ok(())
}

/// Embedded Bloomberg/ES-admin style HTML dashboard. Dark, dense, monospace;
/// real data only. Panels: header (live stats ticker), a traffic/health strip,
/// a per-component breakdown, and a live scrolling log tail with level coloring +
/// a traceId search box.
pub(crate) const LATTICE_UI_HTML: &str = r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8">
<title>NAI Lattice · Observability</title>
<link rel="icon" href="/lattice/icon" type="image/png">
<style>
:root{--bg:#0a0d12;--panel:#11151c;--panel2:#0d1117;--line:#1f2630;--fg:#e6edf3;--muted:#8b98a5;--accent:#19c2d8;--ok:#26de81;--warn:#ffce54;--bad:#ff6b6b;--search:#f7b731}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:13px ui-monospace,Consolas,Menlo,monospace}
.hd{display:flex;align-items:center;gap:14px;padding:9px 16px;border-bottom:1px solid var(--line);background:var(--panel);position:sticky;top:0;z-index:20;flex-wrap:wrap}
.hd h1{margin:0;font-size:14px;letter-spacing:1px;color:var(--accent);display:flex;align-items:center;gap:9px;text-shadow:0 0 13px #19c2d855}.hd .tick{color:var(--muted);font-size:11px}
 .tw{display:grid;grid-template-columns:repeat(6,1fr);gap:8px;padding:12px 16px;border-bottom:1px solid var(--line)}
.kpi{background:linear-gradient(125deg,var(--panel),#0a1720);border:1px solid #23505a;border-radius:6px;padding:8px 10px;box-shadow:inset 0 1px #ffffff0d}
.kpi .lab{color:var(--muted);font-size:10px;text-transform:uppercase;letter-spacing:.5px}.kpi .val{font-size:20px;font-weight:700;margin-top:2px}
.grid{display:grid;grid-template-columns:minmax(0,1.6fr) minmax(0,1fr);gap:10px;padding:12px 16px;align-items:start}
@media(max-width:1000px){.grid{grid-template-columns:1fr}}
.panel{background:var(--panel);border:1px solid var(--line);border-radius:6px;overflow:hidden}
.panel h2{margin:0;font-size:11px;letter-spacing:.5px;color:var(--accent);border-bottom:1px solid var(--line);padding:7px 10px;display:flex;justify-content:space-between;align-items:center}
#tail{height:60vh;overflow:auto;font-size:12px;line-height:1.5;padding:6px 8px;background:var(--panel2)}
.ln{white-space:pre-wrap;word-break:break-word;padding:0 4px;border-left:3px solid transparent}
.ln .ts{color:var(--muted);padding-right:8px}.ln .lv{font-weight:700;padding-right:8px}
.ln.l-info{border-left-color:var(--accent)}.ln.l-warn{border-left-color:var(--warn);color:var(--warn)}
.ln.l-error{border-left-color:var(--bad);color:var(--bad);background:#ff6b6b0d}
.ln .comp{color:var(--muted);padding-right:8px}.ln .trace{color:var(--search);font-size:11px}
#components{font-size:12px}.row{display:flex;justify-content:space-between;padding:5px 10px;border-bottom:1px solid var(--line);align-items:center}
.bar{height:6px;border-radius:3px;background:var(--line);flex:1;margin:0 8px;overflow:hidden}
.bar i{display:block;height:100%;background:var(--accent)}
 .searchbar{display:flex;gap:6px;align-items:center;flex-wrap:wrap;padding:8px 10px;border-bottom:1px solid var(--line)}
input{background:var(--panel2);border:1px solid var(--line);color:var(--fg);border-radius:4px;padding:5px 8px;font-size:12px;width:220px}
button{background:var(--accent);border:0;color:#041;border-radius:4px;padding:5px 10px;font-weight:700;cursor:pointer}
.s-ok{color:var(--ok)}.s-warn{color:var(--warn)}.s-bad{color:var(--bad)}
/* ——— request/response trace explorer ——— */
#traces{height:26vh;overflow:auto;font-size:12px;background:var(--panel2);padding:4px 8px}
.tv{border:1px solid #1c2430;border-radius:4px;margin:4px 0;overflow:hidden}
.tv .tv-hd{display:flex;gap:8px;align-items:center;padding:4px 8px;background:#0d141b;cursor:pointer;user-select:none}
.tv .tv-hd:hover{background:#101822}
.tv .tv-rid{color:var(--search);font-size:11px;font-weight:700;min-width:120px}
.tv .tv-req{color:var(--fg);flex:1;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.tv .tv-ms{color:var(--muted);font-size:11px}
.tv .tv-dur{color:var(--ok);font-weight:700;font-size:11px}
.tv .tv-bad{color:var(--bad);font-size:11px}
.tv .tv-body{display:none;padding:2px 10px 6px;border-top:1px solid #1c2430}
.tv .tv-body.show{display:block}
.tv .span{display:flex;gap:8px;padding:2px 0;align-items:baseline;border-left:2px solid #1c2430;margin-left:6px;padding-left:8px}
.tv .span .kw{color:var(--accent);font-weight:700;min-width:130px;font-size:11px}
.tv .span .sc{color:var(--muted);font-size:11px}
.tv .span.s-tool{border-left-color:var(--warn)}
.tv .span.s-term{border-left-color:var(--ok)}
.tv .span.s-term.bad{border-left-color:var(--bad)}
</style></head><body>
<div class="hd"><h1><img src="/lattice/icon" alt="" width="26" height="26">NAI LATTICE</h1><span class="tick">observability.log.v1</span>
  <span class="tick" id="clock"></span><span class="tick" id="conn" style="color:var(--accent)">connecting…</span>
  <span class="s-ok" id="event-cursor">cursor</span>
</div>
<div class="tw">
  <div class="kpi"><div class="lab">Live log lines</div><div class="val" id="k-window">–</div></div>
  <div class="kpi"><div class="lab">Warn+Error</div><div class="val" id="k-errors" style="color:var(--warn)">–</div></div>
  <div class="kpi"><div class="lab">Distinct traceId</div><div class="val" id="k-traces">–</div></div>
  <div class="kpi"><div class="lab">Components</div><div class="val" id="k-comp">–</div></div>
  <div class="kpi"><div class="lab">SSE pushes</div><div class="val" id="k-push">–</div></div>
  <div class="kpi"><div class="lab">Active streams</div><div class="val" id="k-active">–</div></div>
</div>
<div class="searchbar"><button onclick="refresh()">↻</button><button id="pause" onclick="togglePause()">Ⅱ PAUSE</button><button onclick="clearTail()">CLEAR</button>
  <input id="q" placeholder="grep message / error code" onkeydown="if(event.key==='Enter')refresh()"/>
  <input id="component" placeholder="component (exact)" onkeydown="if(event.key==='Enter')refresh()" />
  <input id="trace" placeholder="traceId (exact)" onkeydown="if(event.key==='Enter')refresh()" />
  <select id="lvl" onchange="refresh()"><option value="">all levels</option><option>error</option><option>warn</option><option>info</option><option>debug</option><option>trace</option></select>
</div>
<div class="grid">
  <div class="panel"><h2>LOG TAIL <span class="tick" id="tail-meta"></span></h2><div id="tail"></div></div>
  <div><div class="panel"><h2>INGESTION / BACKPRESSURE</h2><div class="row" id="ingest" style="display:block;line-height:1.8">Waiting for measured producer health…</div></div><div class="panel" style="margin-top:10px"><h2>COMPONENTS</h2><div id="components"></div></div></div>
</div>
<div class="panel" style="margin:0 16px 16px"><h2>REQUEST → RESPONSE TRACES
  <span class="tick" id="trace-meta"></span>
</h2><div id="traces"><div class="row" style="color:var(--muted)">Collecting request/response shapes (turn.submitted → tool.receipt → turn.terminal, keyed by request_id==trace_id)…</div></div></div>
<script>
const $=id=>document.getElementById(id);
const fmtTs=ms=>{const d=new Date(Number(ms));if(!Number.isFinite(d.getTime()))return '--:--:--';const p=n=>String(n).padStart(2,'0');return `${p(d.getMonth()+1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${String(d.getMilliseconds()).padStart(3,'0')}`};
const seen=new Set();
const MAX_ROWS=1400;
let paused=false;
let stream=null;
function clearTail(){seen.clear();$('tail').replaceChildren();$('tail-meta').textContent='cleared';}
function togglePause(){paused=!paused;$('pause').textContent=paused?'▶ RESUME':'Ⅱ PAUSE';$('pause').style.background=paused?'var(--warn)':'var(--accent)';}
function filters(){
 return {q:($('q').value||'').toLowerCase(),lv:$('lvl').value,
  component:$('component').value.trim(),trace:$('trace').value.trim()};
}
function filtered(ln,f){
 if(f.lv&&ln.level!==f.lv)return false;
 if(f.component&&ln.component!==f.component)return false;
 if(f.trace&&ln.trace_id!==f.trace)return false;
 if(f.q&&!String(ln.message||'').toLowerCase().includes(f.q))return false;
 return true;
}
function addSpan(parent,cls,value){
 const span=document.createElement('span');span.className=cls;span.textContent=String(value??'');parent.append(span);
}
function renderTail(hits,force=false){
 if(paused&&!force)return;
 const tail=$('tail'),f=filters();
 const stick=tail.scrollHeight-tail.scrollTop-tail.clientHeight<72;
let added=0;
  for(const ln of hits){
   feedTrace(ln); // accumulate request/response shapes regardless of filter view
   if(!filtered(ln,f))continue;
  const seq=String(ln.runtime_sequence??('legacy:'+ln.ts_ms+':'+ln.component+':'+ln.message));
  if(seen.has(seq))continue;
  const lv=['trace','debug','info','warn','error'].includes(ln.level)?ln.level:'info';
  const div=document.createElement('div');div.className='ln l-'+lv;div.dataset.key=seq;
  addSpan(div,'ts',fmtTs(ln.timestamp_utc_ms||ln.ts_ms));
  addSpan(div,'lv',lv.toUpperCase());
  addSpan(div,'comp',ln.component);
  if(ln.trace_id)addSpan(div,'trace','['+ln.trace_id+'] ');
  div.append(document.createTextNode(String(ln.message||'')));
  tail.append(div);seen.add(seq);added++;
  while(tail.childElementCount>MAX_ROWS){
   const oldest=tail.firstElementChild;
   if(!oldest)break;
   seen.delete(oldest.dataset.key);
   oldest.remove();
  }
 }
 if(added&&stick)tail.scrollTop=tail.scrollHeight;
 $('tail-meta').textContent=tail.childElementCount+' rows · '+(paused?'paused':'live');
 renderTraces();
}
function renderStats(s){
 $('k-window').textContent=s.window??'—';
 $('k-errors').textContent=s.errors??'—';
 $('k-traces').textContent=s.distinctTraceIds??'—';
 $('k-comp').textContent=Object.keys(s.components||{}).length;
 $('k-push').textContent=s.livePushes??'—';
 $('k-active').textContent=(s.activeSseClients??'—')+' / '+(s.maxSseClients??'—');
 const h=s.emitter,el=$('ingest');el.replaceChildren();
 if(!h||!h.metrics){el.textContent='NO PRODUCER TELEMETRY · runtime unknown';el.style.color='var(--warn)';}
 else{
  const age=Math.max(0,Date.now()-Number(h.observedAt||0));
  const m=h.metrics;
  el.style.color=age>35000?'var(--warn)':'var(--fg)';
  el.textContent='Queued '+(m.queued??'?')+'  |  Dropped '+(m.dropped??'?')+
   '  |  Flushed '+(m.flushed??'?')+'  |  Age '+Math.round(age/1000)+'s';
  if(Number(m.dropped)>0)el.style.color='var(--bad)';
 }
 const comps=$('components');comps.replaceChildren();
 const top=Object.entries(s.components||{}).sort((a,b)=>Number(b[1])-Number(a[1])).slice(0,25);
 const max=Math.max(1,...top.map(([,n])=>Number(n)||0));
 for(const [name,n] of top){
  const row=document.createElement('div');row.className='row';
  addSpan(row,'',name);
  const bar=document.createElement('div');bar.className='bar';
  const fill=document.createElement('i');fill.style.width=Math.max(0,Math.min(100,Number(n)/max*100))+'%';
  bar.append(fill);row.append(bar);addSpan(row,'',n);comps.append(row);
 }
 if(!top.length)addSpan(comps,'row','no records yet');
}

/* ——— request→response trace accumulation ———
   Keyed by trace_id (which NAI-OS sets == request_id for npilot.turn.* events).
   For each trace we remember:
     req   — the turn.submitted record (prompt + shape)
     tools — tool.receipt spans (in order)
     term  — the turn.terminal record (terminal kind + duration_ms)
   When a trace sees terminal, it is "closed". We keep the last 30 closed + any
   open ones, newest-first, so the dashboard shows every request/response shape
   as a controlled trace without unbounded memory. */
const traceMap=new Map();
function feedTrace(ln){
 if(!ln||!ln.trace_id)return;
 const tid=ln.trace_id;
 const msg=String(ln.message||'');
 const isReq=/npilot\.lattice|turn\.submitted/.test(msg);
	const isTerm=/turn\.terminal/.test(msg);
	const isTool=/tool\.receipt/.test(msg)||ln.extras?.event_type==='tool.receipt';
	if(!isReq&&!isTerm&&!isTool)return;
	const t=traceMap.get(tid)||{rid:tid,req:null,tools:[],term:null,firstTs:ln.ts_ms,lastTs:ln.ts_ms};
	t.lastTs=Math.max(t.lastTs,ln.ts_ms||t.firstTs);
	if(isReq&&!t.req)t.req=ln;
	if(isTerm&&!t.term)t.term=ln;
	if(isTool)t.tools.push(ln);
	traceMap.set(tid,t);
	// bounds: drop oldest closed traces beyond 30, keep open ones
	let closed=[...traceMap.values()].filter(x=>x.term);
	if(closed.length>30){
		closed.sort((a,b)=>(a.lastTs||0)-(b.lastTs||0));
		for(const old of closed.slice(0,closed.length-30))if(old.term)traceMap.delete(old.rid);
	}
}
function renderTraces(){
	const el=$('traces');el.replaceChildren();
	const items=[...traceMap.values()].sort((a,b)=>(b.lastTs||0)-(a.lastTs||0)).slice(0,40);
	if(!items.length){el.innerHTML='<div class="row" style="color:var(--muted)">No request→response traces in the live window yet — send an N-Pilot turn and it appears here, keyed by request_id.</div>';$('trace-meta').textContent='0 traces';return;}
	let nClosed=0,nOpen=0;
	for(const t of items){
		if(t.term)nClosed++;else nOpen++;
		const root=document.createElement('div');root.className='tv';
		const hd=document.createElement('div');hd.className='tv-hd';
		addSpan(hd,'tv-rid',t.rid.slice(0,18)+(t.rid.length>18?'…':''));
		const reqHint=t.req?(String(t.req.extras?.prompt||t.req.extras?.request_id||'').slice(0,60)||'(prompt)'):'(not seen yet)';
		addSpan(hd,'tv-req',reqHint);
		addSpan(hd,'tv-ms',fmtTs(t.firstTs));
		if(t.term){
			const termMsg=String(t.term.extras?.terminal||t.term.message||'').replace(/^turn\.terminal\s*/,'');
			const ok=!/(failed|cancelled|budget|approval|error)/i.test(termMsg);
			const dur=t.term.extras?.duration_ms;
			addSpan(hd,'tv-dur',(ok?'✓ ':'✗ ')+String(termMsg||'term'));
			if(!ok)hd.classList.add('tv-bad');
			if(dur)addSpan(hd,'tv-ms',dur+'ms');
		}else addSpan(hd,'tv-ms','… running');
		const body=document.createElement('div');body.className='tv-body';
		if(t.req){
			const r=document.createElement('div');r.className='span s-req';
			addSpan(r,'kw','request');
			const shape=JSON.stringify(t.req.extras||{});
			addSpan(r,'sc',shape.length>140?shape.slice(0,140)+'…':shape);
			body.append(r);
		}
		if(t.tools.length){
			for(const tl of t.tools.slice(-6)){
				const d=document.createElement('div');d.className='span s-tool';
				addSpan(d,'kw','tool');
				addSpan(d,'sc',String(tl.extras?.tool||tl.message||'').slice(0,90));
				body.append(d);
			}
		}
		if(t.term){
			const d=document.createElement('div');d.className='span s-term'+(t.term.extras?.terminal&&/failed|cancelled|budget|approval|error/i.test(String(t.term.extras.terminal))?' bad':'');
			addSpan(d,'kw','terminal');
			const es=t.term.extras||{};
			addSpan(d,'sc','kind='+String(es.terminal||'')+' ms='+(es.duration_ms??'?'));
			body.append(d);
		}
		hd.addEventListener('click',()=>body.classList.toggle('show'));
		root.append(hd,body);el.append(root);
	}
	$('trace-meta').textContent=(nClosed+' closed · '+nOpen+' running · '+items.length+' shown');
}
async function refresh(){
 const f=filters();
 const qp=new URLSearchParams({limit:'350'});
 if(f.q)qp.set('contains',f.q);
 if(f.lv)qp.set('level',f.lv);
 if(f.component)qp.set('component',f.component);
 if(f.trace)qp.set('trace',f.trace);
 try{
  const response=await fetch('/lattice/api?'+qp.toString(),{cache:'no-store'});
  if(!response.ok)throw new Error('HTTP '+response.status);
  const json=await response.json();
  clearTail();renderTail((json.hits||[]).slice().reverse(),true);
  renderStats(json.stats||{});
 }catch(error){$('tail-meta').textContent='QUERY ERROR '+String(error);}
}
setInterval(()=>{const d=new Date();const p=n=>String(n).padStart(2,'0');$('clock').textContent=`${p(d.getMonth()+1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`},1000);
function connect(){
 if(stream){stream.close();stream=null;}
 stream=new EventSource('/lattice/events');
 stream.onopen=()=>{$('conn').textContent='● live';$('conn').style.color='var(--ok)';};
 stream.onerror=()=>{$('conn').textContent='↻ reconnecting';$('conn').style.color='var(--warn)';};
 for(const kind of ['log','search','stats']){
  stream.addEventListener(kind,event=>{
   try{
    const data=JSON.parse(event.data);
    if(kind==='stats')renderStats(data);
    else renderTail(kind==='search'?data:(data.hits||[]));
   }catch(e){console.warn('[lattice] malformed SSE',kind,e);}
  });
 }
 // EventSource automatically reconnects with Last-Event-ID. Never allocate
 // an extra untracked stream on a timer: each costs a server worker.
}
window.addEventListener('pagehide',()=>{if(stream)stream.close();});
connect();
refresh();
</script></body></html>"#;
#[cfg(test)]
mod dashboard_parser_tests {
    use super::*;
    use nai_lattice_runtime::RuntimeConfig;

    #[test]
    fn percent_decoder_preserves_utf8_and_plus() {
        assert_eq!(percent_decode("Praha+%C5%99%C3%ADjen"), Some("Praha říjen".into()));
        assert_eq!(percent_decode("%E1%BB%87"), Some("ệ".into()));
        assert_eq!(percent_decode("%FF"), None);
    }
    #[test]
    fn search_limits_are_hard_bounded() {
        let opts = parse_lattice_search_opts("/lattice/api?limit=9999999&contains=GPU%20hot");
        assert_eq!(opts.limit, Some(500));
        assert_eq!(opts.contains.as_deref(), Some("GPU hot"));
    }

    fn open_runtime() -> (tempfile::TempDir, nai_lattice_runtime::LatticeRuntime) {
        let dir = tempfile::tempdir().unwrap();
        let rt = nai_lattice_runtime::LatticeRuntime::open(RuntimeConfig {
            root: dir.path().to_path_buf(),
            ledger_segment_bytes: 64 * 1024 * 1024,
            ..Default::default()
        })
        .expect("open isolated runtime");
        // The runtime appends its open-time lifecycle record asynchronously;
        // wait for it so window counts are deterministic fixture setup.
        let mut prev = latest_logs_on(&rt, 200).len();
        for _ in 0..80 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            let now = latest_logs_on(&rt, 200).len();
            if now == prev {
                break;
            }
            prev = now;
        }
        (dir, rt)
    }

    fn append(rt: &nai_lattice_runtime::LatticeRuntime, level: &str, component: &str, message: &str) {
        let args = serde_json::json!({
            "namespace": "observability.log.v1",
            "event_type": "log.record",
            "durability": "durable",
            "payload": {
                "ts_ms": 1_764_000_000_000u64,
                "level": level,
                "component": component,
                "message": message,
                "extras": {},
            },
        });
        rt.dispatch(
            "event.append",
            &serde_json::to_vec(&args).unwrap(),
            "dashboard-test",
            None,
        )
        .expect("append");
    }

    #[test]
    fn sse_writer_emits_contract_event_shapes() {
        use crate::lattice_broker::BrokerMsg;

        fn render(msg: BrokerMsg) -> String {
            let mut buf: Vec<u8> = Vec::new();
            write_broker_msg(&mut buf, msg).expect("write");
            String::from_utf8(buf).expect("utf8")
        }

        let search = render(BrokerMsg::Search {
            id: Some(7),
            payload: std::sync::Arc::from("[{\"runtime_sequence\":7}]"),
        });
        assert!(search.starts_with("id: 7\r\nevent: search\r\ndata: "), "{search}");
        assert!(search.ends_with("\r\n\r\n"), "event terminator");

        let log = render(BrokerMsg::Log {
            id: 8,
            payload: std::sync::Arc::from("{\"hits\":[]}"),
        });
        assert!(log.starts_with("id: 8\r\nevent: log\r\ndata: "), "{log}");

        let stats = render(BrokerMsg::Stats(std::sync::Arc::from("{}")));
        assert!(stats.starts_with("event: stats\r\ndata: "), "{stats}");
        assert!(!stats.contains("id:"), "stats must not advance a cursor");

        let state = render(BrokerMsg::State(std::sync::Arc::from(
            "{\"state\":\"draining\"}",
        )));
        assert!(state.starts_with("event: state\r\ndata: "), "{state}");
        assert!(!state.contains("id:"));

        let status = render(BrokerMsg::Status {
            cursor: 42,
            subscribers: 2,
            dropped_total: 1,
            broker_scans: 9,
        });
        assert!(status.starts_with("event: status\r\ndata: "), "{status}");
        assert!(!status.contains("id:"));
        let body = status
            .split("data: ")
            .nth(1)
            .unwrap_or_default()
            .trim();
        let v: Value = serde_json::from_str(body).expect("status json");
        assert_eq!(v["cursor"], 42);
        assert_eq!(v["subscribers"], 2);
        assert_eq!(v["droppedTotal"], 1);
        assert_eq!(v["brokerScans"], 9);

        assert_eq!(render(BrokerMsg::Heartbeat), ": heartbeat\r\n\r\n");
    }

    #[test]
    fn legacy_search_shape_is_unchanged_and_newest_first() {
        let (_dir, rt) = open_runtime();
        let baseline = latest_logs_on(&rt, 200).len();
        append(&rt, "info", "alpha", "one");
        append(&rt, "warn", "alpha", "two");
        append(&rt, "error", "beta", "three");

        let out = search_on(&rt, &SearchOpts { limit: Some(2), ..Default::default() });
        let obj = out.as_object().expect("legacy response is an object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["count", "generatedAt", "hits", "schema"]);
        assert_eq!(out["schema"], 1);
        assert_eq!(out["count"], 2);
        let hits = out["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 2);
        let seq: Vec<u64> = hits
            .iter()
            .map(|h| h["runtime_sequence"].as_u64().unwrap())
            .collect();
        assert!(seq[0] > seq[1], "legacy /lattice/api stays newest-first");
        for hit in hits {
            for key in ["runtime_sequence", "timestamp_utc_ms", "level", "component", "message"] {
                assert!(hit.get(key).is_some(), "legacy hit keeps {key}");
            }
        }

        // Filters keep their exact legacy semantics.
        let filtered = search_on(
            &rt,
            &SearchOpts { component: Some("beta".into()), limit: Some(50), ..Default::default() },
        );
        assert_eq!(filtered["count"], 1);
        let stats = stats_on(&rt);
        for key in [
            "schema",
            "window",
            "levels",
            "errors",
            "components",
            "distinctTraceIds",
            "livePushes",
            "activeSseClients",
            "maxSseClients",
            "emitter",
            "generatedAt",
        ] {
            assert!(stats.get(key).is_some(), "legacy stats keeps {key}");
        }
        let stream = stats["stream"].as_object().expect("stream object");
        for key in [
            "activeSseClients",
            "maxSseClients",
            "pushTotal",
            "subscribers",
            "brokerScansTotal",
            "lastScanUnixMs",
            "droppedTotal",
        ] {
            assert!(stream.contains_key(key), "stream reports {key}");
        }
        assert_eq!(stats["window"], 3 + baseline);
    }

    #[test]
    fn summary_reports_contract_keys_and_runtime_health() {
        let (_dir, rt) = open_runtime();
        let baseline = latest_logs_on(&rt, 200).len();
        append(&rt, "info", "alpha", "one");
        append(&rt, "error", "beta", "boom");
        let out = summary_on(&rt);
        for key in [
            "schema",
            "generatedAt",
            "window",
            "levels",
            "errors",
            "components",
            "distinctTraceIds",
            "stream",
            "health",
            "version",
            "features",
            "storage",
        ] {
            assert!(out.get(key).is_some(), "summary exposes {key}");
        }
        assert_eq!(out["schema"], 1);
        assert_eq!(out["window"], 2 + baseline);
        assert_eq!(out["errors"], 1, "warn+error count over the window");
        assert_eq!(out["health"]["state"], "healthy");
        for key in ["state", "detail", "lastError", "sinceUnixMs", "uptimeMs"] {
            assert!(out["health"].get(key).is_some(), "health exposes {key}");
        }
        assert_eq!(out["version"]["host"], env!("CARGO_PKG_VERSION"));
        assert_eq!(out["version"]["protocolMin"], 1);
        assert_eq!(out["version"]["protocolMax"], 1);
        assert_eq!(out["version"]["storageSchema"], 1);
        assert!(out["version"]["dashboard"].is_string());
        // `features` is the runtime's own advertisement plus the host API
        // surfaces this binary really serves; nothing is invented.
        let describe: Value = serde_json::from_slice(
            &rt.dispatch("runtime.describe", b"{}", "summary-test", None)
                .expect("describe"),
        )
        .expect("describe json");
        let advertised = out["features"].as_array().expect("features array");
        if let Some(runtime_features) = describe.get("features").and_then(Value::as_array) {
            for f in runtime_features {
                assert!(
                    advertised.contains(f),
                    "runtime feature {f} must be passed through"
                );
            }
        }
        for host_feature in HOST_FEATURES {
            assert!(
                advertised.iter().any(|v| v == host_feature),
                "host-served feature {host_feature} must be advertised"
            );
        }
        // `storage` is the shared policy payload (contract §1), produced by
        // `runtime.storage.status` and passed through verbatim.
        let storage = out.get("storage").expect("summary exposes storage");
        assert_eq!(storage["schema"], 1);
        for class in [
            "ledger", "cas", "hot", "search", "vector", "readModels", "logs", "backups",
            "audit",
        ] {
            assert!(
                storage["classes"].get(class).is_some(),
                "storage.classes exposes {class}"
            );
        }
        assert_eq!(storage["retention"]["logsDays"], 30);
        assert_eq!(storage["retention"]["auditDays"], 90);
        assert_eq!(storage["retention"]["backupDays"], 14);
        assert_eq!(storage["retention"]["runtimeKeep"], 2);
        assert!(storage["root"].is_string());
        assert!(storage["base"].is_string());
    }

    #[test]
    fn summary_and_runtime_describe_share_the_same_host_feature_set() {
        let (_dir, rt) = open_runtime();
        rt.register_host_features(HOST_FEATURES);
        let describe: Value = serde_json::from_slice(
            &rt.dispatch("runtime.describe", b"{}", "feature-test", None)
                .expect("describe"),
        )
        .expect("describe json");
        let advertised = describe["features"].as_array().expect("features array");
        for feature in HOST_FEATURES {
            assert!(
                advertised.iter().any(|v| v.as_str() == Some(feature)),
                "runtime.describe must advertise the host feature {feature}"
            );
        }
        let summary = summary_on(&rt);
        assert_eq!(
            summary["features"], describe["features"],
            "summary.features and runtime.describe.features must be the same host set"
        );
    }
}
