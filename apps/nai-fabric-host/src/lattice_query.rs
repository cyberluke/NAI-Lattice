//! Server-side query + trace correlation over the canonical Lattice ledger.
//!
//! Implements the frozen `docs/DASHBOARD_API.md` §2 (`GET /lattice/api/query`)
//! and §3 (`GET /lattice/api/traces`).
//!
//! Design constraints (from the contract):
//!  - the ledger is read through the SAME bounded `event.read_range` path the
//!    dashboard/SSE already use, over BOTH namespaces
//!    (`observability.log.v1` and the legacy `default` bucket), merged by
//!    `runtime_sequence`;
//!  - the ledger high-water mark is frozen at request start so a page can never
//!    silently pull records that belong to the next page;
//!  - every call performs bounded work: `limit` bounds the delivered page,
//!    `scan_budget` bounds the records examined, and `scanned`/`budgetHit`/
//!    `hasMore` make that bound visible;
//!  - nothing is fabricated: every hit/span comes from a persisted record, and
//!    an unknown value is `null`.
//!
//! The async runtime dispatch is synchronous from this module's point of view
//! (`LatticeRuntime::dispatch` runs the body on the runtime's own executor), so
//! these functions are safe to call from HTTP/SSE worker threads.

use std::collections::{BTreeMap, VecDeque};

use serde_json::{json, Value};

use nai_lattice_runtime::LatticeRuntime;

/// Canonical observability namespace (matches `lattice_dashboard`/CLI).
pub(crate) const OBS_NAMESPACE: &str = "observability.log.v1";
/// Legacy namespace the pre-fix observer emitted into. Still merged so no
/// record is lost from a historical window.
pub(crate) const FALLBACK_NAMESPACE: &str = "default";

/// The runtime clamps `event.read_range.limit` to 10_000; a larger request would
/// be silently truncated, so cap honestly on our side too.
const READ_PAGE_MAX: usize = 10_000;

pub(crate) const DEFAULT_QUERY_LIMIT: usize = 200;
pub(crate) const MAX_QUERY_LIMIT: usize = 1_000;
pub(crate) const DEFAULT_SCAN_BUDGET: u64 = 200_000;
pub(crate) const MAX_SCAN_BUDGET: u64 = 500_000;
pub(crate) const DEFAULT_TRACE_WINDOW: usize = 4_000;
pub(crate) const MAX_TRACE_WINDOW: usize = 8_000;

/// Deep-link trace filter bounds (DASHBOARD_API §3): a `?trace=` read may
/// examine at most 50 000 records and deliver at most 2 000 hits, total across
/// every page, so an old trace is findable without scanning an unbounded ledger.
const TRACE_FILTER_SCAN_BUDGET: u64 = 50_000;
const TRACE_FILTER_HIT_CAP: usize = 2_000;
/// The runtime clamps `query` pages to 1 000 hits; two pages reach the cap.
const TRACE_FILTER_PAGE_LIMIT: usize = 1_000;

const LEVEL_ORDER: &[&str] = &["trace", "debug", "info", "warn", "error"];

// ===========================================================================
// Options
// ===========================================================================

/// Parsed `/lattice/api/query` parameters (contract §2).
#[derive(Debug, Clone)]
pub(crate) struct QueryOpts {
    /// Minimum level (`trace|debug|info|warn|error`). Invalid values are
    /// ignored (no level filter) rather than silently filtering everything out.
    pub level_min: Option<String>,
    pub component: Option<String>,
    pub trace: Option<String>,
    /// Case-insensitive substring over `message` only.
    pub q: Option<String>,
    /// Inclusive `timestamp_utc_ms` bounds.
    pub since_ms: Option<u64>,
    pub until_ms: Option<u64>,
    /// Exclusive `runtime_sequence` cursor.
    pub after: u64,
    pub limit: usize,
    pub scan_budget: u64,
}

impl Default for QueryOpts {
    fn default() -> Self {
        Self {
            level_min: None,
            component: None,
            trace: None,
            q: None,
            since_ms: None,
            until_ms: None,
            after: 0,
            limit: DEFAULT_QUERY_LIMIT,
            scan_budget: DEFAULT_SCAN_BUDGET,
        }
    }
}

/// Parsed `/lattice/api/traces` parameters.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TraceOpts {
    pub window: usize,
}

impl Default for TraceOpts {
    fn default() -> Self {
        Self {
            window: DEFAULT_TRACE_WINDOW,
        }
    }
}

/// Parse the `/lattice/api/query?...` query string.
pub(crate) fn parse_query_opts(path: &str) -> QueryOpts {
    let mut o = QueryOpts::default();
    for (k, val) in query_pairs(path) {
        match k.as_str() {
            "level" => {
                let lv = val.trim().to_ascii_lowercase();
                o.level_min = LEVEL_ORDER.contains(&lv.as_str()).then_some(lv);
            }
            "component" => o.component = non_empty(val),
            "trace" => o.trace = non_empty(val),
            "q" => o.q = non_empty(val),
            "since_ms" => o.since_ms = val.parse::<u64>().ok(),
            "until_ms" => o.until_ms = val.parse::<u64>().ok(),
            "after" => o.after = val.parse::<u64>().unwrap_or(0),
            "limit" => {
                o.limit = val
                    .parse::<usize>()
                    .map(|n| n.clamp(1, MAX_QUERY_LIMIT))
                    .unwrap_or(DEFAULT_QUERY_LIMIT)
            }
            "scan_budget" => {
                o.scan_budget = val
                    .parse::<u64>()
                    .map(|n| n.clamp(1, MAX_SCAN_BUDGET))
                    .unwrap_or(DEFAULT_SCAN_BUDGET)
            }
            _ => {}
        }
    }
    o
}

/// Parse the `/lattice/api/traces?...` query string.
pub(crate) fn parse_trace_opts(path: &str) -> TraceOpts {
    let mut window = DEFAULT_TRACE_WINDOW;
    for (k, val) in query_pairs(path) {
        if k == "window" {
            window = val
                .parse::<usize>()
                .map(|n| n.clamp(1, MAX_TRACE_WINDOW))
                .unwrap_or(DEFAULT_TRACE_WINDOW);
        }
    }
    TraceOpts { window }
}

/// Decoded `key=value` pairs from a request path's query string.
pub(crate) fn query_pairs(path: &str) -> Vec<(String, String)> {
    let Some((_, qs)) = path.split_once('?') else {
        return Vec::new();
    };
    qs.split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((
                crate::lattice_dashboard::percent_decode(k)?,
                crate::lattice_dashboard::percent_decode(v)?,
            ))
        })
        .collect()
}

/// One decoded query parameter (`?after=<seq>` first-connect resume for SSE).
pub(crate) fn query_param(path: &str, key: &str) -> Option<String> {
    query_pairs(path)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

pub(crate) fn query_param_u64(path: &str, key: &str) -> Option<u64> {
    query_param(path, key)?.trim().parse::<u64>().ok()
}

fn non_empty(v: String) -> Option<String> {
    (!v.is_empty()).then_some(v)
}

// ===========================================================================
// Bounded ledger reads
// ===========================================================================

/// One raw `event.read_range` page from a single namespace.
struct RawPage {
    events: Vec<Value>,
    /// Raw event count before any defensive filtering; used only to decide
    /// whether the namespace is exhausted.
    raw_len: usize,
}

fn keep_raw_events(all: Option<&Vec<Value>>) -> Vec<Value> {
    all.map(|a| {
        a.iter()
            .filter(|ev| has_sequence(ev))
            .cloned()
            .collect()
    })
    .unwrap_or_default()
}

/// Read one bounded page from ONE namespace, resuming after an exclusive cursor.
/// Never scans unbounded: `limit` is clamped to the runtime's hard read cap.
fn read_raw_page(rt: &LatticeRuntime, ns: &str, after: u64, limit: usize) -> Result<RawPage, ()> {
    let limit = limit.clamp(1, READ_PAGE_MAX);
    let args = json!({
        "namespace": ns,
        "after": after,
        "limit": limit,
        "tail": false,
    });
    let payload = serde_json::to_vec(&args).map_err(|_| ())?;
    let raw = rt
        .dispatch("event.read_range", &payload, "lattice-query", None)
        .map_err(|_| ())?;
    let v: Value = serde_json::from_slice(&raw).map_err(|_| ())?;
    let all = v.get("events").and_then(Value::as_array);
    Ok(RawPage {
        raw_len: all.map(Vec::len).unwrap_or(0),
        events: keep_raw_events(all),
    })
}

/// Read the newest bounded window from ONE namespace (`event.read_range`,
/// `tail: true`). Used for the bounded trace window.
fn read_raw_tail(rt: &LatticeRuntime, ns: &str, limit: usize) -> Result<RawPage, ()> {
    let limit = limit.clamp(1, READ_PAGE_MAX);
    let args = json!({
        "namespace": ns,
        "limit": limit,
        "tail": true,
    });
    let payload = serde_json::to_vec(&args).map_err(|_| ())?;
    let raw = rt
        .dispatch("event.read_range", &payload, "lattice-query", None)
        .map_err(|_| ())?;
    let v: Value = serde_json::from_slice(&raw).map_err(|_| ())?;
    let all = v.get("events").and_then(Value::as_array);
    Ok(RawPage {
        raw_len: all.map(Vec::len).unwrap_or(0),
        events: keep_raw_events(all),
    })
}

fn has_sequence(ev: &Value) -> bool {
    ev.get("runtime_sequence")
        .and_then(Value::as_u64)
        .is_some()
}

pub(crate) fn seq_of(rec: &Value) -> u64 {
    rec.get("runtime_sequence")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Normalize a raw ledger envelope into the dashboard record shape: the
/// `log.record` payload plus the authoritative ledger cursor/timestamp.
fn event_to_record(ev: &Value) -> Option<Value> {
    let seq = ev.get("runtime_sequence").and_then(Value::as_u64)?;
    let mut rec = match ev.get("payload") {
        Some(Value::Object(m)) => Value::Object(m.clone()),
        _ => json!({}),
    };
    rec["runtime_sequence"] = json!(seq);
    rec["timestamp_utc_ms"] = ev
        .get("timestamp_utc_ms")
        .cloned()
        .unwrap_or_else(|| json!(0));
    Some(rec)
}

/// One namespace's cursor-ordered read stream, buffered for a k-way merge.
struct Stream {
    ns: &'static str,
    cursor: u64,
    buf: VecDeque<Value>,
    done: bool,
}

impl Stream {
    fn new(ns: &'static str, after: u64) -> Self {
        Self {
            ns,
            cursor: after,
            buf: VecDeque::new(),
            done: false,
        }
    }
}

/// Refill one stream from its cursor. Records above the frozen high-water mark
/// terminate the stream (they belong to the next page). Returns the raw page
/// size so callers can account for reads.
fn refill(rt: &LatticeRuntime, s: &mut Stream, hwm: u64, page: usize) -> Result<usize, ()> {
    let p = read_raw_page(rt, s.ns, s.cursor, page)?;
    let raw_len = p.raw_len;
    let mut pushed = 0usize;
    for ev in p.events {
        let Some(rec) = event_to_record(&ev) else {
            continue;
        };
        let seq = seq_of(&rec);
        if seq > hwm {
            // Ascending stream: everything after this is also beyond the mark.
            s.done = true;
            break;
        }
        s.cursor = s.cursor.max(seq);
        s.buf.push_back(rec);
        pushed += 1;
    }
    if raw_len < page {
        s.done = true;
    }
    if pushed == 0 && raw_len >= page {
        // Defensive: a full page we cannot advance past would loop forever.
        s.done = true;
    }
    Ok(raw_len)
}

fn pick_min(streams: &[Stream]) -> Option<usize> {
    let mut best: Option<(usize, u64)> = None;
    for (i, s) in streams.iter().enumerate() {
        if let Some(front) = s.buf.front() {
            let seq = seq_of(front);
            if best.is_none_or(|(_, b)| seq < b) {
                best = Some((i, seq));
            }
        }
    }
    best.map(|(i, _)| i)
}

/// Read page size for one namespace refill: sized by the request's delivered
/// `limit` (NOT the much larger examined-record `scan_budget`), with a modest
/// floor/cap so a tiny page does not force one ledger dispatch per record while
/// a 200-row page never reads up to 10 000 records per namespace.
fn read_page_hint(limit: usize) -> usize {
    limit.clamp(256, 2_048)
}

/// Refill every empty, unfinished stream. `scan_budget` still bounds what the
/// merge loop EXAMINES (see [`query_on`]); `page_hint` bounds how many records
/// one namespace read may fetch at a time.
fn refill_all(
    rt: &LatticeRuntime,
    streams: &mut [Stream],
    hwm: u64,
    page_hint: usize,
) -> Result<(), ()> {
    let page = page_hint.clamp(1, READ_PAGE_MAX);
    for i in 0..streams.len() {
        if streams[i].buf.is_empty() && !streams[i].done {
            refill(rt, &mut streams[i], hwm, page)?;
        }
    }
    Ok(())
}

/// Fast path used by the SSE broker: up to `limit` records after `after`,
/// merged across both namespaces in globally ascending `runtime_sequence` order.
pub(crate) fn logs_after_on(rt: &LatticeRuntime, after: u64, limit: usize) -> Vec<Value> {
    let limit = limit.clamp(1, 4096);
    let mut streams = [
        Stream::new(OBS_NAMESPACE, after),
        Stream::new(FALLBACK_NAMESPACE, after),
    ];
    let mut out = Vec::with_capacity(limit.min(512));
    loop {
        if refill_all(rt, &mut streams, u64::MAX, read_page_hint(limit)).is_err() {
            break;
        }
        let Some(i) = pick_min(&streams) else { break };
        let Some(rec) = streams[i].buf.pop_front() else {
            break;
        };
        out.push(rec);
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Bounded newest window (ascending), merged across both namespaces.
/// Used by the compact summary; the legacy `/lattice/api` path keeps its own
/// byte-compatible reader in `lattice_dashboard`.
pub(crate) fn latest_logs_on(rt: &LatticeRuntime, limit: usize) -> Vec<Value> {
    let limit = limit.clamp(1, 4096);
    let mut out: Vec<Value> = Vec::new();
    for ns in [OBS_NAMESPACE, FALLBACK_NAMESPACE] {
        if let Ok(p) = read_raw_tail(rt, ns, limit) {
            out.extend(p.events.iter().filter_map(event_to_record));
        }
    }
    out.sort_by_key(seq_of);
    out.dedup_by_key(|r| seq_of(r));
    if out.len() > limit {
        out = out.split_off(out.len() - limit);
    }
    out
}

/// Frozen ledger high-water mark at request start.
fn runtime_high_water(rt: &LatticeRuntime) -> Option<u64> {
    rt.health()
        .get("ledger_high_water_mark")
        .and_then(Value::as_u64)
}

// ===========================================================================
// GET /lattice/api/query
// ===========================================================================

/// Production entry point: resolves the process runtime once.
pub(crate) fn query(opts: &QueryOpts) -> Value {
    match crate::gastown_orto::RUNTIME.get() {
        Some(rt) => query_on(rt, opts),
        None => empty_page(),
    }
}

fn empty_page() -> Value {
    json!({
        "schema": 1,
        "hits": [],
        "count": 0,
        "nextAfter": Value::Null,
        "hasMore": false,
        "exhausted": true,
        "scanned": 0,
        "budgetHit": false,
        "order": "asc",
        "generatedAt": crate::now_unix_ms(),
    })
}

/// Testable core: query a specific runtime (isolated fixtures use this).
pub(crate) fn query_on(rt: &LatticeRuntime, opts: &QueryOpts) -> Value {
    let limit = opts.limit.clamp(1, MAX_QUERY_LIMIT);
    let budget = opts.scan_budget.clamp(1, MAX_SCAN_BUDGET);
    // Frozen at request start: a record appended mid-request belongs to the
    // next page, never to this one.
    let hwm = runtime_high_water(rt).unwrap_or(u64::MAX);

    let mut streams = [
        Stream::new(OBS_NAMESPACE, opts.after),
        Stream::new(FALLBACK_NAMESPACE, opts.after),
    ];
    let mut hits: Vec<Value> = Vec::new();
    let mut examined: u64 = 0;
    let mut last_examined: Option<u64> = None;
    let mut exhausted = false;
    let mut budget_hit = false;
    let mut failed = false;

    loop {
        if refill_all(rt, &mut streams, hwm, read_page_hint(limit)).is_err() {
            // A ledger read failed: never claim the ledger end.
            failed = true;
            break;
        }
        let Some(i) = pick_min(&streams) else {
            exhausted = true;
            break;
        };
        if examined >= budget {
            budget_hit = true;
            break;
        }
        let Some(rec) = streams[i].buf.pop_front() else {
            exhausted = true;
            break;
        };
        examined += 1;
        let seq = seq_of(&rec);
        last_examined = Some(seq);
        if matches(&rec, opts) {
            hits.push(rec);
            if hits.len() >= limit {
                // The page is full. If the last hit already sits at the frozen
                // high-water mark there is nothing left to page to.
                if seq >= hwm {
                    exhausted = true;
                }
                break;
            }
        }
    }

    let last_hit = hits.last().map(seq_of);
    // Contract §2 pins `nextAfter` to the last delivered record. When the scan
    // budget stops the page before ANY hit, the only cursor that makes progress
    // is the last examined sequence — otherwise a filtered page would loop on
    // the same cursor forever.
    let next_after = last_hit
        .map(Value::from)
        .or_else(|| (budget_hit || failed).then(|| last_examined.map(Value::from)).flatten())
        .unwrap_or(Value::Null);
    let count = hits.len();
    json!({
        "schema": 1,
        "hits": hits,
        "count": count,
        "nextAfter": next_after,
        "hasMore": !exhausted,
        "exhausted": exhausted,
        "scanned": examined,
        "budgetHit": budget_hit,
        "order": "asc",
        "generatedAt": crate::now_unix_ms(),
    })
}

fn matches(rec: &Value, opts: &QueryOpts) -> bool {
    if let Some(min) = &opts.level_min {
        let level = rec.get("level").and_then(Value::as_str).unwrap_or("");
        if !crate::lattice_dashboard::level_at_least(level, min) {
            return false;
        }
    }
    if let Some(c) = &opts.component {
        if rec.get("component").and_then(Value::as_str) != Some(c.as_str()) {
            return false;
        }
    }
    if let Some(t) = &opts.trace {
        if rec.get("trace_id").and_then(Value::as_str) != Some(t.as_str()) {
            return false;
        }
    }
    if let Some(needle) = &opts.q {
        let msg = rec.get("message").and_then(Value::as_str).unwrap_or("");
        if !msg.to_lowercase().contains(&needle.to_lowercase()) {
            return false;
        }
    }
    let ts = rec
        .get("timestamp_utc_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if opts.since_ms.is_some_and(|s| ts < s) || opts.until_ms.is_some_and(|u| ts > u) {
        return false;
    }
    true
}

// ===========================================================================
// GET /lattice/api/traces
// ===========================================================================

/// Production entry point for trace correlation.
///
/// `trace = Some(id)` serves the deep-link filtered read (exactly that trace,
/// even older than the requested window); `None` is the byte-compatible bounded
/// recent-window read.
pub(crate) fn traces(opts: &TraceOpts, trace: Option<&str>) -> Value {
    match crate::gastown_orto::RUNTIME.get() {
        Some(rt) => traces_on(rt, opts, trace),
        None => {
            let trace = normalize_trace_filter(trace);
            let mut out = json!({
                "schema": 1,
                "window": 0,
                "traces": [],
                "generatedAt": crate::now_unix_ms(),
            });
            if let Some(id) = trace {
                out["trace"] = json!(id);
                out["truncated"] = json!(false);
            }
            out
        }
    }
}

/// Trim a `?trace=` value; blank/absent means "no filter".
fn normalize_trace_filter(trace: Option<&str>) -> Option<&str> {
    trace.map(str::trim).filter(|id| !id.is_empty())
}

/// The fields correlation needs from one persisted record. Shared with the
/// topology derivation (`topology.rs`) so both endpoints read the exact same
/// bounded window and the same component/level truth.
pub(crate) struct TraceRec {
    pub(crate) seq: u64,
    pub(crate) ts: u64,
    pub(crate) message: String,
    pub(crate) extras: Value,
    pub(crate) trace_id: String,
    /// The log record's `component` (contract §3 span `component`), or `None`
    /// when the producer did not record one. Never invented.
    pub(crate) component: Option<String>,
    /// The log record's `level`, kept verbatim (topology counts exact `error`
    /// records).
    pub(crate) level: String,
}

/// A non-empty component string from a payload, if the producer recorded one.
fn record_component(payload: Option<&Value>) -> Option<String> {
    payload
        .and_then(|p| p.get("component"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
}

fn trace_rec(ev: &Value) -> Option<TraceRec> {
    let seq = ev.get("runtime_sequence").and_then(Value::as_u64)?;
    let payload = ev.get("payload");
    // Correlation truth: the payload `trace_id`; when a producer only stamped
    // the envelope, the ledger's own `correlation_id` carries the same value
    // (see `lattice-observability::server_payload`). Never generated here.
    let trace_id = payload
        .and_then(|p| p.get("trace_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            ev.get("correlation_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })?
        .to_string();
    Some(TraceRec {
        seq,
        // Producer-recorded span time when present; the ledger event timestamp
        // is the append boundary and a batch can make it later than the span.
        ts: payload
            .and_then(|p| p.get("ts_ms"))
            .and_then(Value::as_u64)
            .or_else(|| ev.get("timestamp_utc_ms").and_then(Value::as_u64))
            .unwrap_or(0),
        message: payload
            .and_then(|p| p.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        extras: payload
            .and_then(|p| p.get("extras"))
            .cloned()
            .unwrap_or(Value::Null),
        component: record_component(payload),
        level: payload
            .and_then(|p| p.get("level"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        trace_id,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpanKind {
    Request,
    Tool,
    Terminal,
    Log,
}

impl SpanKind {
    fn as_str(self) -> &'static str {
        match self {
            SpanKind::Request => "request",
            SpanKind::Tool => "tool",
            SpanKind::Terminal => "terminal",
            SpanKind::Log => "log",
        }
    }
}

fn extras_str<'a>(extras: &'a Value, key: &str) -> Option<&'a str> {
    extras
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// Classify a persisted record into a span kind from its REAL fields.
fn classify(message: &str, extras: &Value) -> SpanKind {
    let m = message.to_ascii_lowercase();
    let et = extras
        .get("event_type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if m.contains("turn.submitted") || et == "turn.submitted" {
        SpanKind::Request
    } else if m.contains("tool.receipt") || et == "tool.receipt" {
        SpanKind::Tool
    } else if m.contains("turn.terminal") || et == "turn.terminal" {
        SpanKind::Terminal
    } else {
        SpanKind::Log
    }
}

fn truncate_label(s: &str) -> String {
    if s.chars().count() <= 200 {
        s.to_string()
    } else {
        s.chars().take(200).collect()
    }
}

/// Terminal kind as recorded (`extras.terminal`), else the message with the
/// `turn.terminal` prefix removed. Absent means empty, never invented.
fn terminal_kind(message: &str, extras: &Value) -> String {
    if let Some(t) = extras_str(extras, "terminal") {
        return t.to_string();
    }
    message
        .strip_prefix("turn.terminal")
        .unwrap_or(message)
        .trim()
        .to_string()
}

fn span_label(kind: SpanKind, message: &str, extras: &Value) -> String {
    let raw = match kind {
        SpanKind::Request => extras_str(extras, "prompt").unwrap_or(message),
        SpanKind::Tool => extras_str(extras, "tool").unwrap_or(message),
        SpanKind::Terminal => {
            let kind = terminal_kind(message, extras);
            if kind.is_empty() {
                message
            } else {
                return truncate_label(&kind);
            }
        }
        SpanKind::Log => message,
    };
    truncate_label(raw)
}

fn failure_tokens(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    ["failed", "cancelled", "canceled", "budget", "approval", "error"]
        .iter()
        .any(|t| l.contains(t))
}

fn duration_value(v: Option<&Value>) -> Option<Value> {
    let v = v?;
    if v.is_u64() || v.is_i64() || v.is_f64() {
        Some(v.clone())
    } else {
        None
    }
}

/// Trace correlation: the deep-link filter when `trace` is `Some`, otherwise the
/// bounded recent window (unchanged behavior and response shape).
pub(crate) fn traces_on(rt: &LatticeRuntime, opts: &TraceOpts, trace: Option<&str>) -> Value {
    match normalize_trace_filter(trace) {
        Some(trace_id) => filtered_traces_on(rt, trace_id),
        None => window_traces_on(rt, opts),
    }
}

/// Newest bounded ledger window shared by `/lattice/api/traces` and
/// `/lattice/api/topology`, merged across both namespaces by
/// `runtime_sequence`.
pub(crate) struct RecentWindow {
    /// Ascending by `runtime_sequence`, at most `window` records long.
    pub(crate) events: Vec<Value>,
    /// True when the ledger held more records than the requested window, i.e.
    /// the cap really cut older records. Proven by reading one record past the
    /// cap, never inferred from a full-looking page.
    pub(crate) truncated: bool,
}

/// Read the newest `window` records (bounded, deduped, ascending). One extra
/// record is requested so `truncated` is evidence, not a guess.
pub(crate) fn read_recent_window(rt: &LatticeRuntime, window: usize) -> RecentWindow {
    let window = window.clamp(1, MAX_TRACE_WINDOW);
    let probe = window.saturating_add(1).min(READ_PAGE_MAX);
    let mut raws: Vec<Value> = Vec::new();
    let mut namespace_overflow = false;
    for ns in [OBS_NAMESPACE, FALLBACK_NAMESPACE] {
        if let Ok(page) = read_raw_tail(rt, ns, probe) {
            // `read_raw_tail` returns the NEWEST page; a page larger than the
            // window proves older records were left behind.
            if page.raw_len > window {
                namespace_overflow = true;
            }
            raws.extend(page.events);
        }
    }
    raws.sort_by_key(|e| {
        e.get("runtime_sequence")
            .and_then(Value::as_u64)
            .unwrap_or(0)
    });
    raws.dedup_by_key(|e| {
        e.get("runtime_sequence")
            .and_then(Value::as_u64)
            .unwrap_or(0)
    });
    let merged_overflow = raws.len() > window;
    if merged_overflow {
        raws = raws.split_off(raws.len() - window);
    }
    RecentWindow {
        events: raws,
        truncated: namespace_overflow || merged_overflow,
    }
}

/// Group persisted records by `trace_id`, ordered `(ts, seq)` inside a trace.
/// The single correlation truth used by the trace builder and the topology
/// derivation; records without a `trace_id` are not spans and never grouped.
pub(crate) fn group_by_trace(events: &[Value]) -> BTreeMap<String, Vec<TraceRec>> {
    let mut groups: BTreeMap<String, Vec<TraceRec>> = BTreeMap::new();
    for ev in events {
        if let Some(rec) = trace_rec(ev) {
            groups.entry(rec.trace_id.clone()).or_default().push(rec);
        }
    }
    for recs in groups.values_mut() {
        recs.sort_by_key(|r| (r.ts, r.seq));
    }
    groups
}

/// `(groups, examined, truncated)` over the SAME bounded read the traces
/// endpoint uses. Topology derives its nodes/edges from these groups.
pub(crate) fn window_groups(
    rt: &LatticeRuntime,
    window: usize,
) -> (BTreeMap<String, Vec<TraceRec>>, usize, bool) {
    let read = read_recent_window(rt, window);
    let examined = read.events.len();
    (group_by_trace(&read.events), examined, read.truncated)
}

/// Bounded window aggregation: group persisted records by `trace_id` and
/// reconstruct request/tool/terminal/log spans from real fields.
fn window_traces_on(rt: &LatticeRuntime, opts: &TraceOpts) -> Value {
    let (groups, examined, _truncated) = window_groups(rt, opts.window);
    json!({
        "schema": 1,
        "window": examined,
        "traces": build_traces(groups),
        "generatedAt": crate::now_unix_ms(),
    })
}

/// Group correlated records into the frozen trace objects, newest activity
/// first (deterministic tie-break by trace id).
fn build_traces(groups: BTreeMap<String, Vec<TraceRec>>) -> Vec<Value> {
    let mut traces: Vec<Value> = Vec::with_capacity(groups.len());
    for (trace_id, mut recs) in groups {
        recs.sort_by_key(|r| (r.ts, r.seq));
        let first_ts = recs.first().map(|r| r.ts).unwrap_or(0);
        let last_ts = recs.last().map(|r| r.ts).unwrap_or(0);

        let mut spans: Vec<Value> = Vec::with_capacity(recs.len());
        let mut terminal: Option<String> = None;
        let mut terminal_duration: Option<Value> = None;
        // The observed window ends at the latest measured span end, not merely
        // the last record start (a terminal span can outlive its start record).
        let mut max_end = first_ts;
        // `ok` is only knowable once a terminal span exists: an open trace
        // reports JSON null instead of pretending success.
        let mut ok: Option<bool> = None;
        for rec in &recs {
            let kind = classify(&rec.message, &rec.extras);
            let label = span_label(kind, &rec.message, &rec.extras);
            // Any record may carry a real duration (tool receipts, terminal
            // spans, explicit spans); absent is null, never a guessed length.
            let duration = duration_value(rec.extras.get("duration_ms"));
            max_end = max_end.max(rec.ts);
            if let Some(ms) = duration
                .as_ref()
                .and_then(|value| value.as_u64().or_else(|| value.as_f64().map(|f| f.max(0.0).round() as u64)))
            {
                max_end = max_end.max(rec.ts.saturating_add(ms));
            }
            if kind == SpanKind::Terminal && terminal.is_none() {
                let kind_text = terminal_kind(&rec.message, &rec.extras);
                let failed = failure_tokens(&kind_text) || failure_tokens(&rec.message);
                ok = Some(!failed);
                terminal_duration = duration.clone();
                terminal = Some(if kind_text.is_empty() {
                    truncate_label(&rec.message)
                } else {
                    kind_text
                });
            }
            spans.push(json!({
                "ts": rec.ts,
                "kind": kind.as_str(),
                "label": label,
                "runtime_sequence": rec.seq,
                "durationMs": duration.unwrap_or(Value::Null),
                // Contract §3: the log record's component, or null. Topology is
                // derived from exactly these real components.
                "component": rec
                    .component
                    .as_deref()
                    .map(Value::from)
                    .unwrap_or(Value::Null),
            }));
        }

        let state = if terminal.is_some() { "closed" } else { "open" };
        // The trace duration is the observed window (first start → last end),
        // not the terminal span's own duration.
        let observed = max_end.saturating_sub(first_ts);
        let trace_duration = if observed > 0 {
            Some(Value::from(observed))
        } else {
            terminal_duration.or_else(|| Some(Value::from(0)))
        };
        traces.push(json!({
            "traceId": trace_id,
            "firstTs": first_ts,
            "lastTs": last_ts,
            "durationMs": trace_duration.unwrap_or(Value::Null),
            "state": state,
            "terminal": terminal.map(Value::from).unwrap_or(Value::Null),
            "ok": ok,
            "spans": spans,
        }));
    }

    // Newest activity first (deterministic tie-break by trace id).
    traces.sort_by(|a, b| {
        let la = a.get("lastTs").and_then(Value::as_u64).unwrap_or(0);
        let lb = b.get("lastTs").and_then(Value::as_u64).unwrap_or(0);
        lb.cmp(&la).then_with(|| {
            a.get("traceId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .cmp(b.get("traceId").and_then(Value::as_str).unwrap_or(""))
        })
    });
    traces
}

/// `?trace=<id>` deep link: page the SAME server-side filtered query the API
/// exposes (`query_on` with the trace filter) until the ledger is exhausted or
/// the frozen bound stops it. A trace older than the recent window is still
/// found; an unknown id yields an empty list (never a fabricated span).
fn filtered_traces_on(rt: &LatticeRuntime, trace_id: &str) -> Value {
    let (hits, examined, truncated) =
        collect_trace_hits(rt, trace_id, TRACE_FILTER_HIT_CAP, TRACE_FILTER_SCAN_BUDGET);
    let mut recs: Vec<TraceRec> = hits.iter().filter_map(trace_rec_from_hit).collect();
    // Defensive: the query filter is exact; never emit a foreign trace here.
    recs.retain(|rec| rec.trace_id == trace_id);
    let mut groups: BTreeMap<String, Vec<TraceRec>> = BTreeMap::new();
    for rec in recs {
        groups.entry(rec.trace_id.clone()).or_default().push(rec);
    }
    json!({
        "schema": 1,
        "window": examined,
        "traces": build_traces(groups),
        "trace": trace_id,
        "truncated": truncated,
        "generatedAt": crate::now_unix_ms(),
    })
}

/// Follow `nextAfter` until exhausted or bounded.
///
/// `scan_budget` is CUMULATIVE across pages (the contract's "at most 50 000
/// examined records" bound), so paging cannot multiply it. Returns
/// `(hits, examined, truncated)`; `truncated` is true only when a bound stopped
/// a read that was not known to be complete.
fn collect_trace_hits(
    rt: &LatticeRuntime,
    trace_id: &str,
    hit_cap: usize,
    scan_budget: u64,
) -> (Vec<Value>, u64, bool) {
    let hit_cap = hit_cap.max(1);
    let page_limit = hit_cap.min(TRACE_FILTER_PAGE_LIMIT);
    let mut hits: Vec<Value> = Vec::new();
    let mut after: u64 = 0;
    let mut examined: u64 = 0;
    let mut truncated = false;

    loop {
        if hits.len() >= hit_cap {
            truncated = true;
            break;
        }
        let remaining = scan_budget.saturating_sub(examined);
        if remaining == 0 {
            truncated = true;
            break;
        }
        let page = query_on(
            rt,
            &QueryOpts {
                trace: Some(trace_id.to_string()),
                after,
                limit: page_limit,
                scan_budget: remaining,
                ..Default::default()
            },
        );
        examined =
            examined.saturating_add(page.get("scanned").and_then(Value::as_u64).unwrap_or(0));
        let exhausted = page
            .get("exhausted")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(batch) = page.get("hits").and_then(Value::as_array) {
            hits.extend(batch.iter().cloned());
        }
        if hits.len() > hit_cap {
            // Delivery was cut by the bound even if the scan happened to end.
            hits.truncate(hit_cap);
            truncated = true;
            break;
        }
        if hits.len() == hit_cap {
            // Exactly at the cap: complete only when the ledger also ended.
            truncated = !exhausted;
            break;
        }
        if exhausted {
            break;
        }
        let Some(next) = page.get("nextAfter").and_then(Value::as_u64) else {
            // No cursor and not exhausted: the ledger read failed. Report the
            // bounded result as truncated rather than claiming completeness.
            truncated = true;
            break;
        };
        if next <= after {
            truncated = true;
            break;
        }
        after = next;
    }

    (hits, examined, truncated)
}

/// Build a correlation record from a flattened `/lattice/api/query` hit
/// (`query_on` copies the log payload and adds the ledger cursor/timestamp).
fn trace_rec_from_hit(hit: &Value) -> Option<TraceRec> {
    let seq = hit.get("runtime_sequence").and_then(Value::as_u64)?;
    let trace_id = hit
        .get("trace_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?
        .to_string();
    Some(TraceRec {
        seq,
        ts: hit
            .get("ts_ms")
            .and_then(Value::as_u64)
            .or_else(|| hit.get("timestamp_utc_ms").and_then(Value::as_u64))
            .unwrap_or(0),
        message: hit
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        extras: hit.get("extras").cloned().unwrap_or(Value::Null),
        component: record_component(Some(hit)),
        level: hit
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        trace_id,
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use nai_lattice_runtime::RuntimeConfig;

    fn open_runtime() -> (tempfile::TempDir, LatticeRuntime) {
        let dir = tempfile::tempdir().unwrap();
        let rt = LatticeRuntime::open(RuntimeConfig {
            root: dir.path().to_path_buf(),
            ledger_segment_bytes: 64 * 1024 * 1024,
            ..Default::default()
        })
        .expect("open isolated runtime");
        (dir, rt)
    }

    #[allow(clippy::too_many_arguments)]
    fn append_log(
        rt: &LatticeRuntime,
        ns: &str,
        level: &str,
        component: &str,
        trace_id: Option<&str>,
        message: &str,
        ts_ms: u64,
        extras: Value,
    ) {
        let args = json!({
            "namespace": ns,
            "event_type": "log.record",
            "correlation_id": trace_id.unwrap_or(""),
            // Logical record time == ledger event time for the fixture, so
            // `timestamp_utc_ms` filters and trace durations are deterministic.
            "timestamp_utc_ms": ts_ms,
            // The real observability producer appends durable records; the
            // fixture uses the same durability as production so the paging test
            // exercises the exact write path. (ProjectionOnly is equally valid
            // since the ledger now advances the chain for every written frame.)
            "durability": "durable",
            "payload": {
                "ts_ms": ts_ms,
                "level": level,
                "component": component,
                "trace_id": trace_id,
                "message": message,
                "extras": extras,
            },
        });
        rt.dispatch(
            "event.append",
            &serde_json::to_vec(&args).unwrap(),
            "lattice-query-test",
            None,
        )
        .expect("append fixture record");
    }

    /// All persisted sequence numbers, read back independently of the pager.
    fn persisted_sequences(rt: &LatticeRuntime) -> Vec<u64> {
        let mut all = Vec::new();
        for ns in [OBS_NAMESPACE, FALLBACK_NAMESPACE] {
            let mut after = 0u64;
            loop {
                let page = read_raw_page(rt, ns, after, READ_PAGE_MAX).expect("read page");
                if page.events.is_empty() {
                    break;
                }
                let mut last = after;
                for ev in &page.events {
                    let seq = ev.get("runtime_sequence").unwrap().as_u64().unwrap();
                    last = last.max(seq);
                    all.push(seq);
                }
                if last <= after || page.raw_len < READ_PAGE_MAX {
                    break;
                }
                after = last;
            }
        }
        all.sort_unstable();
        all.dedup();
        all
    }

    /// Wait (bounded) for the runtime's asynchronous open-time lifecycle record
    /// to land, so fixture accounting is deterministic. This is fixture setup,
    /// not test logic: the ledger's own `runtime.state` audit record is real.
    fn settle_runtime(rt: &LatticeRuntime) {
        let mut prev = persisted_sequences(rt).len();
        for _ in 0..80 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            let now = persisted_sequences(rt).len();
            if now == prev {
                return;
            }
            prev = now;
        }
    }

    /// Map the fixture's own marker (`extras.i`) to its persisted sequence, so a
    /// concurrent lifecycle record can never shift the accounting.
    fn fixture_sequence_map(rt: &LatticeRuntime) -> std::collections::BTreeMap<u64, u64> {
        let mut out = std::collections::BTreeMap::new();
        for ns in [OBS_NAMESPACE, FALLBACK_NAMESPACE] {
            let mut after = 0u64;
            loop {
                let page = read_raw_page(rt, ns, after, READ_PAGE_MAX).expect("read page");
                if page.events.is_empty() {
                    break;
                }
                let mut last = after;
                for ev in &page.events {
                    let seq = ev.get("runtime_sequence").unwrap().as_u64().unwrap();
                    last = last.max(seq);
                    if let Some(i) = ev.pointer("/payload/extras/i").and_then(Value::as_u64) {
                        out.insert(i, seq);
                    }
                }
                if last <= after || page.raw_len < READ_PAGE_MAX {
                    break;
                }
                after = last;
            }
        }
        out
    }

    struct FixtureRow {
        i: u64,
        level: &'static str,
        component: &'static str,
        trace: Option<String>,
        message: String,
        ts: u64,
        expected_seq: u64,
    }

    /// ≥250 records interleaved across both namespaces, deterministic filters.
    /// Returns `(rows, time_base, full ledger sequence snapshot)`.
    fn append_fixture(rt: &LatticeRuntime) -> (Vec<FixtureRow>, u64, Vec<u64>) {
        const LEVELS: [&str; 7] = ["debug", "info", "warn", "error", "info", "info", "trace"];
        const COMPONENTS: [&str; 4] = ["alpha", "beta", "gamma", "alpha"];
        settle_runtime(rt);
        let base = 1_764_000_000_000u64;
        let mut rows = Vec::new();
        for i in 0..260u64 {
            let ns = if i % 3 == 0 {
                FALLBACK_NAMESPACE
            } else {
                OBS_NAMESPACE
            };
            let level = LEVELS[(i as usize) % LEVELS.len()];
            let component = COMPONENTS[(i as usize) % COMPONENTS.len()];
            let trace = (i % 5 == 0).then(|| format!("t-{}", i % 30));
            let message = format!("event {i} marker-{}", i % 7);
            let ts = base + i;
            let extras = json!({
                "i": i,
                "event_type": if i % 37 == 0 { "tool.receipt" } else { "log" },
            });
            append_log(rt, ns, level, component, trace.as_deref(), &message, ts, extras);
            rows.push(FixtureRow {
                i,
                level,
                component,
                trace,
                message,
                ts,
                expected_seq: 0,
            });
        }
        // Resolve each fixture record's ledger sequence by its own marker.
        let map = fixture_sequence_map(rt);
        assert_eq!(map.len(), rows.len(), "fixture must persist every record");
        for row in rows.iter_mut() {
            row.expected_seq = *map
                .get(&row.i)
                .unwrap_or_else(|| panic!("fixture record {} missing from ledger", row.i));
        }
        (rows, base, persisted_sequences(rt))
    }

    fn seqs_of(v: &Value) -> Vec<u64> {
        v.get("hits")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(seq_of).collect())
            .unwrap_or_default()
    }

    fn assert_ascending(seqs: &[u64]) {
        for w in seqs.windows(2) {
            assert!(w[0] < w[1], "page not strictly ascending: {w:?}");
        }
    }

    #[test]
    fn query_pages_every_record_exactly_once_under_small_budget() {
        let (_dir, rt) = open_runtime();
        let (_rows, _base, expected) = append_fixture(&rt);

        let mut after = 0u64;
        let mut seen: Vec<u64> = Vec::new();
        let mut pages = 0;
        let mut saw_budget_hit = false;
        loop {
            pages += 1;
            assert!(pages < 200, "pager did not terminate");
            let out = query_on(
                &rt,
                &QueryOpts {
                    after,
                    limit: 50,
                    scan_budget: 40,
                    ..Default::default()
                },
            );
            assert_eq!(out["schema"], 1);
            assert_eq!(out["order"], "asc");
            let page = seqs_of(&out);
            assert_ascending(&page);
            assert_eq!(out["count"].as_u64().unwrap() as usize, page.len());
            assert!(page.len() <= 50);
            assert!(out["scanned"].as_u64().unwrap() <= 40);
            if out["budgetHit"].as_bool().unwrap() {
                saw_budget_hit = true;
                assert_eq!(out["exhausted"], false);
            }
            seen.extend(page.iter().copied());
            if out["exhausted"].as_bool().unwrap() {
                assert_eq!(out["hasMore"], false);
                break;
            }
            assert_eq!(out["hasMore"], true);
            after = out["nextAfter"].as_u64().expect("progress cursor");
        }

        assert!(
            saw_budget_hit,
            "a 40-record budget must hit on a 260-record ledger"
        );
        assert_eq!(seen, expected, "every record exactly once, ascending");
        let mut deduped = seen.clone();
        deduped.dedup();
        assert_eq!(deduped.len(), seen.len(), "no duplicate delivery");
    }

    #[test]
    fn query_filters_are_exact_and_exhaustive() {
        let (_dir, rt) = open_runtime();
        let (rows, base, ledger_snapshot) = append_fixture(&rt);

        let cases: Vec<(&str, QueryOpts, Box<dyn Fn(&FixtureRow) -> bool>)> = vec![
            (
                "level>=warn",
                QueryOpts {
                    level_min: Some("warn".into()),
                    ..Default::default()
                },
                Box::new(move |r: &FixtureRow| r.level == "warn" || r.level == "error"),
            ),
            (
                "component=beta",
                QueryOpts {
                    component: Some("beta".into()),
                    ..Default::default()
                },
                Box::new(move |r: &FixtureRow| r.component == "beta"),
            ),
            (
                "trace=t-0",
                QueryOpts {
                    trace: Some("t-0".into()),
                    ..Default::default()
                },
                Box::new(move |r: &FixtureRow| r.trace.as_deref() == Some("t-0")),
            ),
            (
                "q=marker-3",
                QueryOpts {
                    q: Some("MARKER-3".into()),
                    ..Default::default()
                },
                Box::new(move |r: &FixtureRow| r.message.to_lowercase().contains("marker-3")),
            ),
            (
                "time window",
                QueryOpts {
                    since_ms: Some(base + 50),
                    until_ms: Some(base + 60),
                    ..Default::default()
                },
                Box::new(move |r: &FixtureRow| r.ts >= base + 50 && r.ts <= base + 60),
            ),
        ];

        for (name, opts, pred) in cases {
            let mut expected: Vec<u64> = rows
                .iter()
                .filter(|r| pred(r))
                .map(|r| r.expected_seq)
                .collect();
            expected.sort_unstable();
            assert!(!expected.is_empty(), "{name}: fixture must exercise the filter");
            let out = query_on(
                &rt,
                &QueryOpts {
                    limit: MAX_QUERY_LIMIT,
                    scan_budget: MAX_SCAN_BUDGET,
                    ..opts
                },
            );
            assert_eq!(seqs_of(&out), expected, "{name}: exact filtered result");
            assert_eq!(out["exhausted"], true, "{name}: ledger fully examined");
            assert_eq!(out["budgetHit"], false, "{name}: budget was ample");
            assert_eq!(
                out["scanned"].as_u64().unwrap(),
                ledger_snapshot.len() as u64,
                "{name}: every record examined"
            );
        }
    }

    /// T03/Q01 acceptance gate: a 10k-record store must return every retained
    /// line exactly once through cursor pagination, with exact filtered counts.
    /// Ignored by default because it writes 10k records; run explicitly:
    /// `cargo test -p nai-fabric-host ten_thousand -- --ignored --nocapture`.
    #[test]
    #[ignore = "10k acceptance fixture: run explicitly with --ignored"]
    fn ten_thousand_record_fixture_is_fully_recoverable() {
        let (_dir, rt) = open_runtime();
        const N: u64 = 10_000;
        let base = 1_764_100_000_000u64;
        for i in 0..N {
            let args = json!({
                "namespace": if i % 4 == 0 { FALLBACK_NAMESPACE } else { OBS_NAMESPACE },
                "event_type": "log.record",
                "timestamp_utc_ms": base + i,
                "payload": {
                    "ts_ms": base + i,
                    "level": if i % 100 == 0 { "warn" } else { "info" },
                    "component": if i % 3 == 0 { "beta" } else { "alpha" },
                    "trace_id": Value::Null,
                    "message": format!("record {i}"),
                    "extras": {},
                },
            });
            rt.dispatch(
                "event.append",
                &serde_json::to_vec(&args).unwrap(),
                "lattice-10k-test",
                None,
            )
            .expect("append 10k fixture record");
        }
        rt.flush_hotstore();
        // The runtime appends its own boot lifecycle record (`runtime.state`) to
        // the same observability namespace, so the ledger holds N + 1 records.
        // Compare against the ledger's real high-water mark instead of assuming
        // the fixture is the only producer.
        let hwm = rt.health()["ledger_high_water_mark"]
            .as_u64()
            .expect("ledger high-water mark");
        assert_eq!(hwm, N + 1, "10k fixture records + the boot lifecycle record");

        let mut after = 0u64;
        let mut seen: Vec<u64> = Vec::new();
        let mut pages = 0;
        loop {
            pages += 1;
            assert!(pages < 100, "pager must terminate");
            let out = query_on(
                &rt,
                &QueryOpts {
                    after,
                    limit: 1000,
                    scan_budget: 500_000,
                    ..Default::default()
                },
            );
            let page = seqs_of(&out);
            assert_ascending(&page);
            seen.extend(page.iter().copied());
            if out["exhausted"].as_bool().unwrap() {
                assert_eq!(out["hasMore"], false);
                break;
            }
            after = out["nextAfter"].as_u64().expect("progress cursor");
        }
        assert_eq!(seen.len(), hwm as usize, "every retained line recovered");
        let mut unique = seen.clone();
        unique.dedup();
        assert_eq!(unique.len(), hwm as usize, "no duplicate rows across pages");
        assert_eq!(
            seen,
            (1..=hwm).collect::<Vec<_>>(),
            "pagination covers the ledger contiguously"
        );

        // Filtered counts over the same store are exact (1% warn).
        let warn = query_on(
            &rt,
            &QueryOpts {
                level_min: Some("warn".into()),
                limit: 1000,
                scan_budget: 500_000,
                ..Default::default()
            },
        );
        assert_eq!(warn["count"].as_u64().unwrap(), 100);
        assert_eq!(warn["exhausted"], true);
    }

    #[test]
    fn query_with_no_matches_still_makes_progress_until_exhausted() {
        let (_dir, rt) = open_runtime();
        let (_rows, _base, ledger_snapshot) = append_fixture(&rt);
        let mut after = 0u64;
        let mut iterations = 0;
        loop {
            iterations += 1;
            assert!(iterations < 100, "zero-hit pager did not terminate");
            let out = query_on(
                &rt,
                &QueryOpts {
                    component: Some("does-not-exist".into()),
                    after,
                    limit: 50,
                    scan_budget: 37,
                    ..Default::default()
                },
            );
            assert_eq!(out["count"], 0);
            assert!(out["hits"].as_array().unwrap().is_empty());
            if out["exhausted"].as_bool().unwrap() {
                assert_eq!(out["hasMore"], false);
                break;
            }
            assert_eq!(out["budgetHit"], true);
            after = out["nextAfter"]
                .as_u64()
                .expect("budget-limited scan must return a progress cursor");
        }
        assert_eq!(
            iterations as usize,
            ledger_snapshot.len().div_ceil(37)
        );
    }

    #[test]
    fn read_page_hint_is_sized_by_limit_not_scan_budget() {
        assert_eq!(read_page_hint(1), 256, "floor avoids one dispatch per record");
        assert_eq!(read_page_hint(200), 256);
        assert_eq!(read_page_hint(1_000), 1_000);
        assert_eq!(read_page_hint(MAX_QUERY_LIMIT), 1_000);
        assert_eq!(read_page_hint(10_000), 2_048, "cap keeps a page modest");
    }

    #[test]
    fn a_small_page_examines_only_the_delivered_rows() {
        let (_dir, rt) = open_runtime();
        append_fixture(&rt);
        let out = query_on(
            &rt,
            &QueryOpts {
                limit: 200,
                scan_budget: MAX_SCAN_BUDGET,
                ..Default::default()
            },
        );
        assert_eq!(out["count"], 200);
        assert_eq!(
            out["scanned"].as_u64().unwrap(),
            200,
            "a full page stops examination at the delivered rows, not at scan_budget"
        );
        assert_eq!(out["budgetHit"], false);
        assert_eq!(out["hasMore"], true);
        assert_eq!(
            out["nextAfter"].as_u64(),
            seqs_of(&out).last().copied(),
            "nextAfter remains the last delivered sequence"
        );

        let rest = query_on(
            &rt,
            &QueryOpts {
                after: out["nextAfter"].as_u64().expect("progress cursor"),
                limit: MAX_QUERY_LIMIT,
                scan_budget: MAX_SCAN_BUDGET,
                ..Default::default()
            },
        );
        assert_eq!(rest["exhausted"], true);
        assert_eq!(rest["hasMore"], false);
    }

    fn append_trace(
        rt: &LatticeRuntime,
        level: &str,
        tid: &str,
        message: &str,
        ts: u64,
        extras: Value,
    ) {
        append_log(
            rt,
            OBS_NAMESPACE,
            level,
            "npilot",
            Some(tid),
            message,
            ts,
            extras,
        );
    }

    #[test]
    fn traces_group_real_request_tool_terminal_records() {
        let (_dir, rt) = open_runtime();
        settle_runtime(&rt);
        let t0 = 1_764_100_000_000u64;
        // T1: request → tool → terminal(completed, 500 ms).
        append_trace(
            &rt,
            "info",
            "T1",
            "npilot.turn.submitted",
            t0,
            json!({"prompt": "build it"}),
        );
        append_trace(
            &rt,
            "info",
            "T1",
            "npilot.tool.receipt",
            t0 + 10,
            json!({"tool": "shell"}),
        );
        append_trace(
            &rt,
            "info",
            "T1",
            "turn.terminal completed",
            t0 + 600,
            json!({"terminal": "completed", "duration_ms": 500}),
        );
        // T2: request → terminal(failed) with no explicit duration.
        let t1 = t0 + 5_000;
        append_trace(&rt, "info", "T2", "npilot.turn.submitted", t1, json!({}));
        append_trace(
            &rt,
            "error",
            "T2",
            "turn.terminal failed",
            t1 + 100,
            json!({"terminal": "failed"}),
        );
        // T3: still open.
        let t2 = t0 + 10_000;
        append_trace(&rt, "info", "T3", "npilot.turn.submitted", t2, json!({}));
        // A record with no trace id must not synthesize a trace.
        append_log(
            &rt,
            OBS_NAMESPACE,
            "info",
            "npilot",
            None,
            "turn.submitted orphan",
            t2 + 1,
            json!({}),
        );

        let out = traces_on(&rt, &TraceOpts { window: 4_000 }, None);
        assert_eq!(out["schema"], 1);
        assert_eq!(
            out["window"].as_u64().unwrap() as usize,
            persisted_sequences(&rt).len(),
            "bounded window contains every persisted record"
        );
        let traces = out["traces"].as_array().unwrap();
        assert_eq!(traces.len(), 3, "orphan must not create a trace");

        let by_id = |id: &str| -> Value {
            traces
                .iter()
                .find(|t| t["traceId"] == id)
                .cloned()
                .unwrap_or_else(|| panic!("missing trace {id}"))
        };

        let t1v = by_id("T1");
        assert_eq!(t1v["state"], "closed");
        assert_eq!(t1v["terminal"], "completed");
        assert_eq!(t1v["ok"], true);
        assert_eq!(t1v["durationMs"], 1100, "observed window spans first start to last measured end");
        let kinds: Vec<&str> = t1v["spans"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, vec!["request", "tool", "terminal"]);
        assert_eq!(t1v["spans"][1]["label"], "shell");

        let t2v = by_id("T2");
        assert_eq!(t2v["state"], "closed");
        assert_eq!(t2v["terminal"], "failed");
        assert_eq!(t2v["ok"], false);
        assert_eq!(t2v["durationMs"], 100, "fallback to lastTs-firstTs");

        let t3v = by_id("T3");
        assert_eq!(t3v["state"], "open");
        assert_eq!(t3v["terminal"], Value::Null);
        assert_eq!(
            t3v["ok"],
            Value::Null,
            "an open trace has no success verdict (JSON null)"
        );
        assert_eq!(t3v["spans"].as_array().unwrap().len(), 1);
        assert_eq!(t3v["durationMs"], 0);
        assert!(
            out.get("trace").is_none() && out.get("truncated").is_none(),
            "the unfiltered response keeps its exact previous shape"
        );
    }

    #[test]
    fn trace_spans_carry_the_record_component() {
        let (_dir, rt) = open_runtime();
        settle_runtime(&rt);
        let base = 1_764_200_000_000u64;
        append_log(
            &rt,
            OBS_NAMESPACE,
            "info",
            "nai-os.agent",
            Some("C1"),
            "npilot.turn.submitted",
            base,
            json!({}),
        );
        append_log(
            &rt,
            OBS_NAMESPACE,
            "error",
            "nai-os.terminal",
            Some("C1"),
            "turn.terminal failed",
            base + 5,
            json!({"terminal": "failed"}),
        );
        // No trace id -> not a span, and never a topology node source.
        append_log(
            &rt,
            OBS_NAMESPACE,
            "info",
            "orphan.component",
            None,
            "not correlated",
            base + 6,
            json!({}),
        );

        for out in [
            traces_on(&rt, &TraceOpts { window: 4_000 }, None),
            traces_on(&rt, &TraceOpts { window: 4_000 }, Some("C1")),
        ] {
            let traces = out["traces"].as_array().expect("traces array");
            assert_eq!(traces.len(), 1, "only the correlated trace exists");
            let spans = traces[0]["spans"].as_array().expect("spans array");
            assert_eq!(spans.len(), 2);
            assert_eq!(
                spans[0]["component"], "nai-os.agent",
                "the request span carries its record's component"
            );
            assert_eq!(
                spans[1]["component"], "nai-os.terminal",
                "the terminal span carries its record's component"
            );
        }
    }

    #[test]
    fn trace_deep_link_finds_trace_older_than_window() {
        let (_dir, rt) = open_runtime();
        settle_runtime(&rt);
        let base = 1_764_300_000_000u64;
        // Old trace first (request -> terminal), then enough recent noise that a
        // small `?window=` cannot contain it.
        append_trace(
            &rt,
            "info",
            "OLD-1",
            "npilot.turn.submitted",
            base,
            json!({"prompt": "deep link"}),
        );
        append_trace(
            &rt,
            "info",
            "OLD-1",
            "turn.terminal completed",
            base + 7,
            json!({"terminal": "completed", "duration_ms": 7}),
        );
        for i in 0..30u64 {
            append_trace(
                &rt,
                "info",
                "RECENT",
                &format!("recent {i}"),
                base + 100 + i,
                json!({"recent": i}),
            );
        }

        // The small window really excludes the old trace.
        let windowed = traces_on(&rt, &TraceOpts { window: 5 }, None);
        assert!(
            windowed["traces"]
                .as_array()
                .unwrap()
                .iter()
                .all(|t| t["traceId"] != "OLD-1"),
            "fixture must place OLD-1 outside the recent window"
        );

        // Exactly what the HTTP route does: parse the path (percent-decoding
        // included), then hand the filter to `traces_on`.
        let path = "/lattice/api/traces?window=5&trace=OLD%2D1";
        let opts = parse_trace_opts(path);
        let trace = query_param(path, "trace");
        assert_eq!(trace.as_deref(), Some("OLD-1"), "trace is percent-decoded");
        let found = traces_on(&rt, &opts, trace.as_deref());

        assert_eq!(found["schema"], 1);
        assert_eq!(found["trace"], "OLD-1");
        assert_eq!(found["truncated"], false);
        let traces = found["traces"].as_array().expect("traces array");
        assert_eq!(traces.len(), 1, "exactly the requested trace");
        assert_eq!(traces[0]["traceId"], "OLD-1");
        assert_eq!(traces[0]["state"], "closed");
        assert_eq!(traces[0]["terminal"], "completed");
        assert_eq!(traces[0]["ok"], true);
        let kinds: Vec<&str> = traces[0]["spans"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, vec!["request", "terminal"]);
    }

    #[test]
    fn unknown_trace_deep_link_returns_empty_and_echoes_id() {
        let (_dir, rt) = open_runtime();
        settle_runtime(&rt);
        append_trace(
            &rt,
            "info",
            "KNOWN",
            "npilot.turn.submitted",
            1_764_400_000_000,
            json!({}),
        );
        let out = traces_on(&rt, &TraceOpts { window: 1_000 }, Some("missing-trace"));
        assert_eq!(out["trace"], "missing-trace", "the filter is echoed");
        assert_eq!(out["truncated"], false, "the ledger was fully examined");
        assert!(
            out["traces"].as_array().unwrap().is_empty(),
            "an unknown trace must never invent spans"
        );
    }

    #[test]
    fn trace_filter_bounds_delivery_and_examination() {
        let (_dir, rt) = open_runtime();
        settle_runtime(&rt);
        let base = 1_764_500_000_000u64;
        // Noise first: a tiny examined budget must stop before any match.
        for i in 0..25u64 {
            append_trace(
                &rt,
                "info",
                "NOISE",
                &format!("noise {i}"),
                base + i,
                json!({"noise": i}),
            );
        }
        for i in 0..5u64 {
            append_trace(
                &rt,
                "info",
                "CAP",
                &format!("cap {i}"),
                base + 100 + i,
                json!({"i": i}),
            );
        }

        // Examined budget: 10 records total across pages, no match yet -> stop.
        let (hits, examined, truncated) = collect_trace_hits(&rt, "CAP", 2_000, 10);
        assert_eq!(examined, 10, "the budget is cumulative across pages");
        assert_eq!(hits.len(), 0);
        assert!(truncated, "a stopped scan must report truncation");

        // Hit cap: 5 matching records, cap 3 -> exactly 3 delivered.
        let (hits, examined, truncated) = collect_trace_hits(&rt, "CAP", 3, 50_000);
        assert_eq!(hits.len(), 3, "the hit cap bounds delivery");
        assert!(examined >= 3);
        assert!(truncated, "hitting the hit cap must report truncation");
        assert!(hits
            .iter()
            .all(|hit| hit["trace_id"].as_str() == Some("CAP")));

        // Ample bounds: everything found, nothing truncated.
        let (hits, examined, truncated) =
            collect_trace_hits(&rt, "CAP", TRACE_FILTER_HIT_CAP, TRACE_FILTER_SCAN_BUDGET);
        assert_eq!(hits.len(), 5);
        assert!(examined >= 30);
        assert!(!truncated);
    }

    #[test]
    fn trace_options_are_bounded() {
        assert_eq!(parse_trace_opts("/lattice/api/traces").window, 4_000);
        assert_eq!(
            parse_trace_opts("/lattice/api/traces?window=999999").window,
            8_000
        );
        assert_eq!(parse_trace_opts("/lattice/api/traces?window=0").window, 1);
        assert_eq!(normalize_trace_filter(None), None);
        assert_eq!(normalize_trace_filter(Some("   ")), None);
        assert_eq!(normalize_trace_filter(Some(" t-1 ")), Some("t-1"));
    }

    #[test]
    fn query_options_are_bounded_and_decoded() {
        let o = parse_query_opts(
            "/lattice/api/query?level=WARN&component=nai-os&q=GPU%20hot&limit=999999&scan_budget=9999999&after=42&since_ms=10&until_ms=99",
        );
        assert_eq!(o.level_min.as_deref(), Some("warn"));
        assert_eq!(o.component.as_deref(), Some("nai-os"));
        assert_eq!(o.q.as_deref(), Some("GPU hot"));
        assert_eq!(o.limit, MAX_QUERY_LIMIT);
        assert_eq!(o.scan_budget, MAX_SCAN_BUDGET);
        assert_eq!(o.after, 42);
        assert_eq!(o.since_ms, Some(10));
        assert_eq!(o.until_ms, Some(99));
        // Invalid level is ignored, not applied as a filter.
        assert_eq!(
            parse_query_opts("/lattice/api/query?level=nope").level_min,
            None
        );
    }
}
