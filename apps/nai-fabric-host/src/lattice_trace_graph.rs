//! Truthful execution-chain graph for
//! `GET /lattice/api/v2/traces/{trace_id}/graph`.
//!
//! This is a CONSUMER of explicit span records, not a producer: a record is a
//! span only when its payload carries a non-empty `extras.span_id`. A record
//! without one is a point event. Edges come only from explicit evidence:
//!
//!  - `extras.parent_span_id` on a span (kind `parent`), and
//!  - an explicit `extras.call` object with `target_span_id` (kind `call`).
//!
//! Component co-occurrence, record order and timestamps never create an edge.
//! A parent/call target that is not in the bounded record set becomes an
//! `unresolved:<id>` node with null metrics and a diagnostic; no component is
//! invented for it.
//!
//! The trace's records are read through the existing bounded query path
//! (`lattice_query::query_on` with the exact `trace` filter and `nextAfter`
//! paging). One request examines at most 50 000 ledger records and delivers at
//! most 2 000 trace hits, so an old trace is findable without scanning an
//! unbounded ledger. `window` is the number of records actually examined.
//!
//! Caps: 500 nodes, 1 000 edges, 2 000 events. Any cap that cuts evidence sets
//! `truncated` and adds a `truncated` diagnostic.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use nai_lattice_runtime::LatticeRuntime;

/// Frozen response schema for the v2 graph contract.
const SCHEMA: u32 = 2;
/// Cumulative records one graph request may examine across every page
/// (contract: <= 50 000 examined / 2 000 hits).
const SCAN_BUDGET: u64 = 50_000;
/// Hard cap on trace hits delivered into the graph builder.
const HIT_CAP: usize = 2_000;
/// `query_on` clamps a page to 1 000 hits; matching it keeps paging honest.
const PAGE_LIMIT: usize = 1_000;
const MAX_NODES: usize = 500;
const MAX_EDGES: usize = 1_000;
const MAX_EVENTS: usize = 2_000;

// ===========================================================================
// Route parsing
// ===========================================================================

/// Extract the trace id from `/lattice/api/v2/traces/{trace_id}/graph`.
///
/// Percent escapes are decoded as path bytes (`+` stays `+`, unlike a query
/// string), so a trace id containing `%2F`, `%2B` or UTF-8 survives intact.
pub(crate) fn trace_id_from_graph_path(path: &str) -> Option<String> {
    let path = path.split('?').next().unwrap_or(path);
    let raw_id = path
        .strip_prefix("/lattice/api/v2/traces/")?
        .strip_suffix("/graph")?;
    // A trace id is exactly one path segment; an encoded slash (%2F) is still
    // allowed and decoded below.
    if raw_id.is_empty() || raw_id.contains('/') {
        return None;
    }
    let decoded = decode_path_segment(raw_id)?;
    let decoded = decoded.trim();
    (!decoded.is_empty()).then(|| decoded.to_string())
}

fn decode_path_segment(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

// ===========================================================================
// Production entry points
// ===========================================================================

/// Production entry point: resolve the process runtime once.
pub(crate) fn graph(trace_id: &str) -> Value {
    match crate::gastown_orto::RUNTIME.get() {
        Some(rt) => graph_on(rt, trace_id),
        // No runtime opened yet: nothing was examined, so nothing is derived.
        None => build_graph(trace_id, &[], 0, false),
    }
}

/// Testable core: build the graph from a specific runtime.
pub(crate) fn graph_on(rt: &LatticeRuntime, trace_id: &str) -> Value {
    let (hits, examined, truncated) = collect_trace_hits(rt, trace_id);
    build_graph(trace_id, &hits, examined, truncated)
}

/// Page the exact server-side trace filter until the ledger is exhausted or a
/// frozen bound stops it. `truncated` is true only when a bound stopped a read
/// that was not proven complete.
fn collect_trace_hits(rt: &LatticeRuntime, trace_id: &str) -> (Vec<Value>, u64, bool) {
    let mut hits: Vec<Value> = Vec::new();
    let mut after: u64 = 0;
    let mut examined: u64 = 0;
    let mut truncated = false;

    loop {
        if hits.len() >= HIT_CAP {
            truncated = true;
            break;
        }
        let remaining = SCAN_BUDGET.saturating_sub(examined);
        if remaining == 0 {
            truncated = true;
            break;
        }
        let page = crate::lattice_query::query_on(
            rt,
            &crate::lattice_query::QueryOpts {
                trace: Some(trace_id.to_string()),
                after,
                limit: PAGE_LIMIT,
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
        if hits.len() > HIT_CAP {
            hits.truncate(HIT_CAP);
            truncated = true;
            break;
        }
        if hits.len() == HIT_CAP {
            // Exactly at the cap: complete only when the ledger also ended.
            truncated = !exhausted;
            break;
        }
        if exhausted {
            break;
        }
        let Some(next) = page.get("nextAfter").and_then(Value::as_u64) else {
            // No cursor and not exhausted: the ledger read failed. Never claim
            // completeness.
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

// ===========================================================================
// Record parsing (explicit fields only)
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Error,
    Unset,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Error => "error",
            Status::Unset => "unset",
        }
    }
}

#[derive(Debug, Clone)]
struct CallObs {
    target_span_id: String,
    target_component: Option<String>,
    operation: Option<String>,
    transport: Option<String>,
}

#[derive(Debug, Clone)]
struct SpanRec {
    span_id: String,
    seq: u64,
    ts: u64,
    component: Option<String>,
    message: String,
    parent_span_id: Option<String>,
    operation: Option<String>,
    duration_ms: Option<Value>,
    instance_id: Option<String>,
    status: Status,
    call: Option<CallObs>,
    /// Every examined record carrying this span id (duplicates keep their
    /// evidence; the first record supplies the node's fields).
    evidence: Vec<u64>,
}

fn extras_of(hit: &Value) -> Option<&Value> {
    hit.get("extras").filter(|v| v.is_object())
}

/// Field lookup: the explicit `extras` object first, then the flattened
/// payload. Both locations are producer-written evidence; nothing is derived.
fn field<'a>(hit: &'a Value, extras: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    extras
        .and_then(|e| e.get(key))
        .or_else(|| hit.get(key))
        .filter(|v| !v.is_null())
}

fn field_str<'a>(hit: &'a Value, extras: Option<&'a Value>, key: &str) -> Option<&'a str> {
    field(hit, extras, key).and_then(Value::as_str)
}

fn field_text(hit: &Value, extras: Option<&Value>, key: &str) -> Option<String> {
    field_str(hit, extras, key)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn field_number(hit: &Value, extras: Option<&Value>, key: &str) -> Option<Value> {
    let v = field(hit, extras, key)?;
    if !v.is_number() {
        return None;
    }
    // A measured duration is finite and non-negative; anything else is not a
    // duration and stays null.
    v.as_f64()
        .filter(|f| f.is_finite() && *f >= 0.0)
        .map(|_| v.clone())
}

fn record_seq(hit: &Value) -> Option<u64> {
    hit.get("runtime_sequence").and_then(Value::as_u64)
}

fn record_ts(hit: &Value) -> u64 {
    hit.get("ts_ms")
        .and_then(Value::as_u64)
        .or_else(|| hit.get("timestamp_utc_ms").and_then(Value::as_u64))
        .unwrap_or(0)
}

fn record_message(hit: &Value) -> String {
    hit.get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn record_level(hit: &Value) -> String {
    hit.get("level")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn record_component(hit: &Value) -> Option<String> {
    hit.get("component")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
}

/// Contract status precedence: a recorded error level or `failed`/`error`
/// status wins; `ok` or a recorded terminal event is ok; otherwise unset.
fn record_status(hit: &Value, extras: Option<&Value>, level: &str) -> Status {
    if level.eq_ignore_ascii_case("error") {
        return Status::Error;
    }
    match field_str(hit, extras, "status")
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("error") | Some("failed") => Status::Error,
        Some("ok") => Status::Ok,
        _ => match field(hit, extras, "terminal") {
            Some(Value::String(s)) if !s.trim().is_empty() => Status::Ok,
            Some(Value::Bool(true)) => Status::Ok,
            _ => Status::Unset,
        },
    }
}

/// A span is a record with a non-empty `extras.span_id` (or the same explicit
/// field on the flattened payload). Everything else is a point event.
fn span_of(hit: &Value) -> Option<SpanRec> {
    let seq = record_seq(hit)?;
    let extras = extras_of(hit);
    let span_id = field_text(hit, extras, "span_id")?;
    let level = record_level(hit);
    let call = call_of(hit, extras);
    Some(SpanRec {
        span_id,
        seq,
        ts: record_ts(hit),
        component: record_component(hit),
        message: record_message(hit),
        parent_span_id: field_text(hit, extras, "parent_span_id"),
        operation: field_text(hit, extras, "operation"),
        duration_ms: field_number(hit, extras, "duration_ms"),
        instance_id: field_text(hit, extras, "instance_id"),
        status: record_status(hit, extras, &level),
        call,
        evidence: vec![seq],
    })
}

/// An explicit call observation: only `extras.call` (a real object) with a
/// non-empty `target_span_id` creates a call edge.
fn call_of(hit: &Value, extras: Option<&Value>) -> Option<CallObs> {
    let call = field(hit, extras, "call")?;
    if !call.is_object() {
        return None;
    }
    let target_span_id = call
        .get("target_span_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let text = |key: &str| {
        call.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(CallObs {
        target_span_id,
        target_component: text("target_component"),
        operation: text("operation"),
        transport: text("transport"),
    })
}

fn span_id_of(hit: &Value) -> Option<String> {
    field_text(hit, extras_of(hit), "span_id")
}

fn truncate_label(s: &str) -> String {
    if s.chars().count() <= 200 {
        s.to_string()
    } else {
        s.chars().take(200).collect()
    }
}

// ===========================================================================
// Graph construction
// ===========================================================================

#[derive(Debug, Clone)]
struct Node {
    id: String,
    kind: &'static str,
    label: String,
    component: Option<String>,
    operation: Option<String>,
    status: Status,
    duration_ms: Option<Value>,
    start_offset_ms: Option<u64>,
    parent_span_id: Option<String>,
    instance_id: Option<String>,
    evidence: Vec<u64>,
}

#[derive(Debug, Clone)]
struct Edge {
    source: String,
    target: String,
    kind: &'static str,
    operation: Option<String>,
    transport: Option<String>,
    duration_ms: Option<Value>,
    evidence: Vec<u64>,
}

#[derive(Debug, Clone)]
struct UnresolvedRef {
    id: String,
    source_span_id: String,
    from_parent: bool,
    target_component: Option<String>,
    evidence: Vec<u64>,
}

fn build_graph(trace_id: &str, hits: &[Value], examined: u64, read_truncated: bool) -> Value {
    // ---- spans vs point events -------------------------------------------------
    let mut spans: Vec<SpanRec> = Vec::new();
    let mut span_pos: BTreeMap<String, usize> = BTreeMap::new();
    let mut all_spans = true;
    for hit in hits {
        match span_of(hit) {
            Some(span) => {
                if let Some(&i) = span_pos.get(&span.span_id) {
                    // Duplicate observation of one span: the first record owns
                    // the fields, every record contributes evidence.
                    spans[i].evidence.push(span.seq);
                } else {
                    span_pos.insert(span.span_id.clone(), spans.len());
                    spans.push(span);
                }
            }
            None => all_spans = false,
        }
    }
    // Deterministic trace order; sequence breaks equal timestamps.
    spans.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.seq.cmp(&b.seq)));

    let span_ids: BTreeSet<String> = spans.iter().map(|s| s.span_id.clone()).collect();

    // ---- unresolved references ------------------------------------------------
    let mut unresolved: Vec<UnresolvedRef> = Vec::new();
    let mut unresolved_pos: BTreeMap<String, usize> = BTreeMap::new();
    let mut ensure_unresolved =
        |id: &str,
         span: &SpanRec,
         from_parent: bool,
         target_component: Option<String>,
         unresolved: &mut Vec<UnresolvedRef>| {
            if let Some(&i) = unresolved_pos.get(id) {
                unresolved[i].evidence.push(span.seq);
                // An explicit target_component from a call observation is evidence;
                // never replace it with a guess.
                if unresolved[i].target_component.is_none() {
                    unresolved[i].target_component = target_component;
                }
                return;
            }
            unresolved_pos.insert(id.to_string(), unresolved.len());
            unresolved.push(UnresolvedRef {
                id: id.to_string(),
                source_span_id: span.span_id.clone(),
                from_parent,
                target_component,
                evidence: vec![span.seq],
            });
        };
    for span in &spans {
        if let Some(parent) = &span.parent_span_id {
            if !span_ids.contains(parent) {
                ensure_unresolved(parent, span, true, None, &mut unresolved);
            }
        }
        if let Some(call) = &span.call {
            if !span_ids.contains(&call.target_span_id) {
                ensure_unresolved(
                    &call.target_span_id,
                    span,
                    false,
                    call.target_component.clone(),
                    &mut unresolved,
                );
            }
        }
    }

    // ---- nodes ----------------------------------------------------------------
    let root_id = format!("trace:{trace_id}");
    let root_status = if spans.iter().any(|s| s.status == Status::Error) {
        Status::Error
    } else if !spans.is_empty() && spans.iter().all(|s| s.status == Status::Ok) {
        Status::Ok
    } else {
        Status::Unset
    };

    let baseline = spans.iter().filter(|s| s.ts > 0).map(|s| s.ts).min();
    let mut nodes: Vec<Node> = Vec::new();
    let mut nodes_capped = false;
    if !spans.is_empty() {
        nodes.push(Node {
            id: root_id.clone(),
            kind: "trace",
            label: trace_id.to_string(),
            component: None,
            operation: None,
            status: root_status,
            duration_ms: None,
            start_offset_ms: None,
            parent_span_id: None,
            instance_id: None,
            evidence: Vec::new(),
        });
    }
    for span in &spans {
        if nodes.len() >= MAX_NODES {
            nodes_capped = true;
            break;
        }
        let label = span
            .operation
            .clone()
            .or_else(|| (!span.message.is_empty()).then(|| truncate_label(&span.message)))
            .unwrap_or_else(|| span.span_id.clone());
        let start_offset_ms =
            baseline.and_then(|base| (span.ts > 0 && span.ts >= base).then(|| span.ts - base));
        nodes.push(Node {
            id: format!("span:{}", span.span_id),
            kind: "span",
            label,
            component: span.component.clone(),
            operation: span.operation.clone(),
            status: span.status,
            duration_ms: span.duration_ms.clone(),
            start_offset_ms,
            parent_span_id: span.parent_span_id.clone(),
            instance_id: span.instance_id.clone(),
            evidence: span.evidence.clone(),
        });
    }
    for unresolved_ref in &unresolved {
        if nodes.len() >= MAX_NODES {
            nodes_capped = true;
            break;
        }
        nodes.push(Node {
            id: format!("unresolved:{}", unresolved_ref.id),
            kind: "unresolved",
            label: unresolved_ref.id.clone(),
            // Only an explicit `target_component` from a call observation may
            // populate this; a missing parent never gets an invented component.
            component: unresolved_ref.target_component.clone(),
            operation: None,
            status: Status::Unset,
            duration_ms: None,
            start_offset_ms: None,
            parent_span_id: None,
            instance_id: None,
            evidence: unresolved_ref.evidence.clone(),
        });
    }
    let retained: BTreeSet<String> = nodes.iter().map(|n| n.id.clone()).collect();

    // ---- edges (explicit parent links and explicit call objects only) ---------
    let mut edges: Vec<Edge> = Vec::new();
    let mut edge_keys: BTreeSet<String> = BTreeSet::new();
    let mut edges_capped = false;

    let push_edge = |edge: Edge,
                     edges: &mut Vec<Edge>,
                     edge_keys: &mut BTreeSet<String>,
                     edges_capped: &mut bool| {
        if *edges_capped {
            return;
        }
        if !retained.contains(&edge.source) || !retained.contains(&edge.target) {
            // A node cap removed an endpoint; a dangling edge is not a graph.
            return;
        }
        let key = format!(
            "{}|{}|{}|{}|{}",
            edge.kind,
            edge.source,
            edge.target,
            edge.operation.as_deref().unwrap_or(""),
            edge.transport.as_deref().unwrap_or("")
        );
        if !edge_keys.insert(key) {
            return;
        }
        if edges.len() >= MAX_EDGES {
            *edges_capped = true;
            return;
        }
        edges.push(edge);
    };

    for span in &spans {
        let span_node = format!("span:{}", span.span_id);
        if !retained.contains(&span_node) {
            continue;
        }
        match span.parent_span_id.as_deref() {
            None => {
                push_edge(
                    Edge {
                        source: root_id.clone(),
                        target: span_node.clone(),
                        kind: "parent",
                        operation: span.operation.clone(),
                        transport: None,
                        duration_ms: None,
                        evidence: vec![span.seq],
                    },
                    &mut edges,
                    &mut edge_keys,
                    &mut edges_capped,
                );
            }
            Some(parent) => {
                let source = if span_ids.contains(parent) {
                    format!("span:{parent}")
                } else {
                    format!("unresolved:{parent}")
                };
                push_edge(
                    Edge {
                        source,
                        target: span_node.clone(),
                        kind: "parent",
                        operation: span.operation.clone(),
                        transport: None,
                        duration_ms: None,
                        evidence: vec![span.seq],
                    },
                    &mut edges,
                    &mut edge_keys,
                    &mut edges_capped,
                );
            }
        }
        if let Some(call) = &span.call {
            let target = if span_ids.contains(&call.target_span_id) {
                format!("span:{}", call.target_span_id)
            } else {
                format!("unresolved:{}", call.target_span_id)
            };
            push_edge(
                Edge {
                    source: span_node,
                    target,
                    kind: "call",
                    operation: call.operation.clone(),
                    transport: call.transport.clone(),
                    duration_ms: None,
                    evidence: vec![span.seq],
                },
                &mut edges,
                &mut edge_keys,
                &mut edges_capped,
            );
        }
    }

    // Deterministic, collision-resistant edge ids.
    let mut used_edge_ids: BTreeSet<String> = BTreeSet::new();
    let mut edges_json: Vec<Value> = Vec::with_capacity(edges.len());
    for edge in &edges {
        let base = match edge.kind {
            "parent" => format!("parent:{}->{}", edge.source, edge.target),
            _ => format!(
                "call:{}->{}:{}:{}",
                edge.source,
                edge.target,
                edge.operation.as_deref().unwrap_or(""),
                edge.transport.as_deref().unwrap_or("")
            ),
        };
        let mut id = base.clone();
        let mut n = 2usize;
        while !used_edge_ids.insert(id.clone()) {
            id = format!("{base}#{n}");
            n += 1;
        }
        edges_json.push(json!({
            "id": id,
            "source": edge.source,
            "target": edge.target,
            "kind": edge.kind,
            "operation": edge.operation,
            "transport": edge.transport,
            "durationMs": edge.duration_ms,
            "evidenceSequences": edge.evidence,
            "inferred": false,
        }));
    }

    // ---- events (every examined record) ---------------------------------------
    let mut events_capped = false;
    let mut events: Vec<Value> = Vec::with_capacity(hits.len().min(MAX_EVENTS));
    for hit in hits {
        if events.len() >= MAX_EVENTS {
            events_capped = true;
            break;
        }
        events.push(json!({
            "sequence": record_seq(hit).unwrap_or(0),
            "ts": record_ts(hit),
            "level": record_level(hit),
            "component": record_component(hit).unwrap_or_default(),
            "message": record_message(hit),
            "spanId": span_id_of(hit),
        }));
    }

    // ---- coverage and diagnostics ---------------------------------------------
    let coverage = if spans.is_empty() {
        "legacy"
    } else if all_spans {
        "spans"
    } else {
        "mixed"
    };
    let mut diagnostics: Vec<Value> = Vec::new();
    if spans.is_empty() {
        diagnostics.push(json!({
            "code": "legacy_correlation_only",
            "message": "No record in this trace carries extras.span_id; every record is a point event. Causal chain unavailable — legacy correlation only.",
            "spanId": "",
        }));
        if hits.is_empty() {
            diagnostics.push(json!({
                "code": "no_spans",
                "message": "No records for this trace were found in the bounded ledger read.",
                "spanId": "",
            }));
        }
    }
    for unresolved_ref in &unresolved {
        if !retained.contains(&format!("unresolved:{}", unresolved_ref.id)) {
            // The node cap removed this endpoint; the truncation diagnostic
            // already discloses the incomplete chain.
            continue;
        }
        let message = if unresolved_ref.from_parent {
            format!(
                "parent_span_id {} was not observed in the bounded trace read (referenced by span {}).",
                unresolved_ref.id, unresolved_ref.source_span_id
            )
        } else {
            format!(
                "call target span {} was not observed in the bounded trace read (declared by span {}).",
                unresolved_ref.id, unresolved_ref.source_span_id
            )
        };
        diagnostics.push(json!({
            "code": "unresolved_parent",
            "message": message,
            "spanId": unresolved_ref.source_span_id,
        }));
    }
    let truncated = read_truncated || nodes_capped || edges_capped || events_capped;
    if truncated {
        let mut reasons: Vec<&str> = Vec::new();
        if read_truncated {
            reasons.push("bounded trace read");
        }
        if nodes_capped {
            reasons.push("node cap");
        }
        if edges_capped {
            reasons.push("edge cap");
        }
        if events_capped {
            reasons.push("event cap");
        }
        diagnostics.push(json!({
            "code": "truncated",
            "message": format!(
                "The returned chain is incomplete ({}); examined {} records. Raise the bound or narrow the trace window.",
                reasons.join(", "), examined
            ),
            "spanId": "",
        }));
    }

    let nodes_json: Vec<Value> = nodes
        .iter()
        .map(|n| {
            json!({
                "id": n.id,
                "kind": n.kind,
                "label": n.label,
                "component": n.component,
                "operation": n.operation,
                "status": n.status.as_str(),
                "durationMs": n.duration_ms,
                "startOffsetMs": n.start_offset_ms,
                "parentSpanId": n.parent_span_id,
                "instanceId": n.instance_id,
                "evidenceSequences": n.evidence,
            })
        })
        .collect();

    json!({
        "schema": SCHEMA,
        "traceId": trace_id,
        "generatedAt": crate::now_unix_ms(),
        "window": examined,
        "truncated": truncated,
        "coverage": coverage,
        "nodes": nodes_json,
        "edges": edges_json,
        "events": events,
        "diagnostics": diagnostics,
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use nai_lattice_runtime::RuntimeConfig;

    const OBS: &str = crate::lattice_query::OBS_NAMESPACE;

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
    fn append(
        rt: &LatticeRuntime,
        trace_id: &str,
        level: &str,
        component: Option<&str>,
        message: &str,
        ts: u64,
        extras: Value,
    ) {
        let payload = json!({
            "ts_ms": ts,
            "level": level,
            "component": component,
            "trace_id": trace_id,
            "message": message,
            "extras": extras,
        });
        let args = json!({
            "namespace": OBS,
            "event_type": "log.record",
            "correlation_id": trace_id,
            "timestamp_utc_ms": ts,
            "durability": "durable",
            "payload": payload,
        });
        rt.dispatch(
            "event.append",
            &serde_json::to_vec(&args).unwrap(),
            "trace-graph-test",
            None,
        )
        .expect("append trace-graph fixture record");
    }

    fn node<'a>(out: &'a Value, id: &str) -> &'a Value {
        out["nodes"]
            .as_array()
            .expect("nodes array")
            .iter()
            .find(|n| n["id"] == id)
            .unwrap_or_else(|| panic!("missing node {id}"))
    }

    fn edges<'a>(out: &'a Value) -> &'a Vec<Value> {
        out["edges"].as_array().expect("edges array")
    }

    fn edge<'a>(out: &'a Value, source: &str, target: &str) -> &'a Value {
        edges(out)
            .iter()
            .find(|e| e["source"] == source && e["target"] == target)
            .unwrap_or_else(|| panic!("missing edge {source}->{target}"))
    }

    fn diagnostic_codes(out: &Value) -> Vec<String> {
        out["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .filter_map(|d| d["code"].as_str().map(str::to_string))
            .collect()
    }

    #[test]
    fn legacy_point_events_never_become_a_graph() {
        let (_dir, rt) = open_runtime();
        append(
            &rt,
            "legacy-1",
            "info",
            Some("nai.os"),
            "request received",
            1_764_600_000_000,
            json!({}),
        );
        append(
            &rt,
            "legacy-1",
            "warn",
            Some("nai.lattice"),
            "dispatch queued",
            1_764_600_000_005,
            json!({"event_type": "dispatch"}),
        );
        append(
            &rt,
            "legacy-1",
            "error",
            Some("nai.lattice"),
            "terminal failed",
            1_764_600_000_010,
            json!({}),
        );

        let out = graph_on(&rt, "legacy-1");
        assert_eq!(out["schema"], 2);
        assert_eq!(out["traceId"], "legacy-1");
        assert_eq!(out["coverage"], "legacy");
        assert_eq!(out["truncated"], false);
        assert!(out["nodes"].as_array().unwrap().is_empty());
        assert!(out["edges"].as_array().unwrap().is_empty());
        assert_eq!(out["events"].as_array().unwrap().len(), 3);
        assert!(
            out["events"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["spanId"].is_null()),
            "point events carry no span id"
        );
        let codes = diagnostic_codes(&out);
        assert!(codes.contains(&"legacy_correlation_only".to_string()));
        assert!(!codes.contains(&"truncated".to_string()));
    }

    #[test]
    fn explicit_parent_links_produce_exact_nodes_edges_and_offsets() {
        let (_dir, rt) = open_runtime();
        append(
            &rt,
            "T-parent",
            "info",
            Some("alpha"),
            "root request",
            1_764_700_000_000,
            json!({
                "span_id": "s-root",
                "operation": "turn.submitted",
                "duration_ms": 10,
                "span_kind": "server",
                "instance_id": "inst-1",
                "status": "ok",
            }),
        );
        append(
            &rt,
            "T-parent",
            "error",
            Some("beta"),
            "child failed",
            1_764_700_000_010,
            json!({
                "span_id": "s-child",
                "parent_span_id": "s-root",
                "operation": "tool.exec",
                "duration_ms": 5,
            }),
        );

        let out = graph_on(&rt, "T-parent");
        assert_eq!(out["coverage"], "spans");
        assert_eq!(out["truncated"], false);
        assert_eq!(out["nodes"].as_array().unwrap().len(), 3);
        assert_eq!(out["edges"].as_array().unwrap().len(), 2);

        let root = node(&out, "trace:T-parent");
        assert_eq!(root["kind"], "trace");
        assert_eq!(
            root["status"], "error",
            "a failed span makes the trace error"
        );
        assert!(root["durationMs"].is_null());
        assert!(root["startOffsetMs"].is_null());

        let parent = node(&out, "span:s-root");
        assert_eq!(parent["kind"], "span");
        assert_eq!(parent["component"], "alpha");
        assert_eq!(parent["operation"], "turn.submitted");
        assert_eq!(parent["status"], "ok");
        assert_eq!(parent["durationMs"], 10);
        assert_eq!(parent["startOffsetMs"], 0);
        assert!(parent["parentSpanId"].is_null());
        assert_eq!(parent["instanceId"], "inst-1");

        let child = node(&out, "span:s-child");
        assert_eq!(child["component"], "beta");
        assert_eq!(child["operation"], "tool.exec");
        assert_eq!(child["status"], "error", "an error record level is error");
        assert_eq!(child["durationMs"], 5);
        assert_eq!(child["startOffsetMs"], 10);
        assert_eq!(child["parentSpanId"], "s-root");

        let root_edge = edge(&out, "trace:T-parent", "span:s-root");
        assert_eq!(root_edge["kind"], "parent");
        assert_eq!(root_edge["inferred"], false);
        assert_eq!(
            root_edge["evidenceSequences"].as_array().unwrap().len(),
            1,
            "the edge evidence is the child record"
        );
        assert!(root_edge["evidenceSequences"][0].is_u64());

        let child_edge = edge(&out, "span:s-root", "span:s-child");
        assert_eq!(child_edge["kind"], "parent");
        assert_eq!(child_edge["operation"], "tool.exec");
        assert_eq!(child_edge["inferred"], false);

        assert!(
            !edges(&out).iter().any(|e| e["kind"] == "call"),
            "parent nesting alone never becomes a call"
        );
    }

    #[test]
    fn mixed_trace_keeps_point_events_out_of_the_graph() {
        let (_dir, rt) = open_runtime();
        append(
            &rt,
            "T-mixed",
            "info",
            Some("alpha"),
            "span with a real id",
            1_764_750_000_000,
            json!({"span_id": "s-one", "operation": "op.one"}),
        );
        append(
            &rt,
            "T-mixed",
            "warn",
            Some("alpha"),
            "point event only",
            1_764_750_000_001,
            json!({"event_type": "note"}),
        );

        let out = graph_on(&rt, "T-mixed");
        assert_eq!(out["coverage"], "mixed");
        assert_eq!(out["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(out["edges"].as_array().unwrap().len(), 1);
        assert_eq!(out["events"].as_array().unwrap().len(), 2);
        let point = out["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["message"] == "point event only")
            .expect("point event is preserved");
        assert!(
            point["spanId"].is_null(),
            "a point event stays an event, never a span"
        );
        assert!(
            !out["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["label"] == "point event only"),
            "a point event never becomes a node"
        );
    }

    #[test]
    fn component_cooccurrence_alone_creates_no_edge() {
        let (_dir, rt) = open_runtime();
        append(
            &rt,
            "T-cooc",
            "info",
            Some("alpha"),
            "one",
            1_764_800_000_000,
            json!({"span_id": "s-a", "operation": "a"}),
        );
        append(
            &rt,
            "T-cooc",
            "info",
            Some("beta"),
            "two",
            1_764_800_000_001,
            json!({"span_id": "s-b", "operation": "b"}),
        );

        let out = graph_on(&rt, "T-cooc");
        assert_eq!(out["coverage"], "spans");
        assert_eq!(out["edges"].as_array().unwrap().len(), 2);
        assert_eq!(edge(&out, "trace:T-cooc", "span:s-a")["kind"], "parent");
        assert_eq!(edge(&out, "trace:T-cooc", "span:s-b")["kind"], "parent");
        assert!(
            !edges(&out)
                .iter()
                .any(|e| e["source"] == "span:s-a" && e["target"] == "span:s-b"),
            "sharing a trace is not a call edge"
        );
    }

    #[test]
    fn missing_parent_becomes_an_unresolved_node_without_an_invented_component() {
        let (_dir, rt) = open_runtime();
        append(
            &rt,
            "T-unresolved",
            "info",
            Some("beta"),
            "orphan",
            1_764_900_000_000,
            json!({
                "span_id": "s-orphan",
                "parent_span_id": "missing-parent",
                "operation": "tool.exec",
                "duration_ms": 3,
            }),
        );

        let out = graph_on(&rt, "T-unresolved");
        assert_eq!(out["coverage"], "spans");
        assert_eq!(out["nodes"].as_array().unwrap().len(), 3);
        let unresolved = node(&out, "unresolved:missing-parent");
        assert_eq!(unresolved["kind"], "unresolved");
        assert!(unresolved["component"].is_null());
        assert!(unresolved["operation"].is_null());
        assert_eq!(unresolved["status"], "unset");
        assert!(unresolved["durationMs"].is_null());
        assert!(unresolved["startOffsetMs"].is_null());
        assert!(unresolved["parentSpanId"].is_null());
        assert!(unresolved["instanceId"].is_null());
        assert_eq!(
            unresolved["evidenceSequences"].as_array().unwrap().len(),
            1,
            "the unresolved node carries the referring span record as evidence"
        );
        assert!(unresolved["evidenceSequences"][0].is_u64());

        let parent_edge = edge(&out, "unresolved:missing-parent", "span:s-orphan");
        assert_eq!(parent_edge["kind"], "parent");
        assert_eq!(out["edges"].as_array().unwrap().len(), 1);

        let diagnostic = out["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["code"] == "unresolved_parent")
            .expect("unresolved_parent diagnostic");
        assert_eq!(diagnostic["spanId"], "s-orphan");
    }

    #[test]
    fn explicit_call_object_creates_a_typed_call_edge() {
        let (_dir, rt) = open_runtime();
        append(
            &rt,
            "T-call",
            "info",
            Some("alpha"),
            "dispatch",
            1_765_000_000_000,
            json!({
                "span_id": "s-caller",
                "operation": "dispatch",
                "duration_ms": 7,
                "call": {
                    "target_span_id": "s-callee",
                    "target_component": "beta",
                    "operation": "turn.run",
                    "transport": "naf/rpc",
                },
            }),
        );
        append(
            &rt,
            "T-call",
            "info",
            Some("beta"),
            "turn",
            1_765_000_000_001,
            json!({
                "span_id": "s-callee",
                "operation": "turn.run",
                "duration_ms": 3,
            }),
        );

        let out = graph_on(&rt, "T-call");
        assert_eq!(out["coverage"], "spans");
        let call = edge(&out, "span:s-caller", "span:s-callee");
        assert_eq!(call["kind"], "call");
        assert_eq!(call["operation"], "turn.run");
        assert_eq!(call["transport"], "naf/rpc");
        assert_eq!(call["inferred"], false);
        assert!(call["durationMs"].is_null());
        assert!(call["id"].as_str().unwrap().starts_with("call:"));

        // Both spans have no parent, so each is also rooted; the call edge is
        // additional explicit evidence, never a replacement.
        assert_eq!(out["edges"].as_array().unwrap().len(), 3);
        assert_eq!(
            edge(&out, "trace:T-call", "span:s-caller")["kind"],
            "parent"
        );
        assert_eq!(
            edge(&out, "trace:T-call", "span:s-callee")["kind"],
            "parent"
        );
    }

    #[test]
    fn call_to_an_unobserved_span_stays_unresolved() {
        let (_dir, rt) = open_runtime();
        append(
            &rt,
            "T-call-missing",
            "info",
            Some("alpha"),
            "dispatch",
            1_765_100_000_000,
            json!({
                "span_id": "s-caller",
                "call": {
                    "target_span_id": "never-seen",
                    "target_component": "beta",
                    "operation": "turn.run",
                    "transport": "naf/rpc",
                },
            }),
        );

        let out = graph_on(&rt, "T-call-missing");
        let unresolved = node(&out, "unresolved:never-seen");
        assert_eq!(unresolved["kind"], "unresolved");
        assert_eq!(
            unresolved["component"], "beta",
            "an explicit target_component is evidence, not a guess"
        );
        assert!(unresolved["durationMs"].is_null());
        let call = edge(&out, "span:s-caller", "unresolved:never-seen");
        assert_eq!(call["kind"], "call");
        assert_eq!(call["transport"], "naf/rpc");
        assert!(diagnostic_codes(&out).contains(&"unresolved_parent".to_string()));
    }

    #[test]
    fn bounded_graph_caps_nodes_and_reports_truncation() {
        let (_dir, rt) = open_runtime();
        // 520 root spans exceed the 500-node cap (root node included).
        for i in 0..520u64 {
            append(
                &rt,
                "T-cap",
                "info",
                Some("alpha"),
                &format!("span {i}"),
                1_765_200_000_000 + i,
                json!({
                    "span_id": format!("s-{i}"),
                    "operation": format!("op.{i}"),
                    "duration_ms": 1,
                }),
            );
        }

        let out = graph_on(&rt, "T-cap");
        assert_eq!(out["coverage"], "spans");
        assert_eq!(out["truncated"], true);
        assert_eq!(
            out["nodes"].as_array().unwrap().len(),
            MAX_NODES,
            "nodes are capped at 500"
        );
        assert!(
            out["edges"].as_array().unwrap().len() <= MAX_EDGES,
            "edges are capped at 1000"
        );
        assert_eq!(out["events"].as_array().unwrap().len(), 520);
        assert!(diagnostic_codes(&out).contains(&"truncated".to_string()));
        assert_eq!(out["nodes"][0]["id"], "trace:T-cap");
        // Every retained edge endpoint must exist: no dangling edges.
        let ids: BTreeSet<&str> = out["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap())
            .collect();
        for e in edges(&out) {
            assert!(ids.contains(e["source"].as_str().unwrap()));
            assert!(ids.contains(e["target"].as_str().unwrap()));
        }
    }

    #[test]
    fn unknown_trace_reports_no_spans_without_inventing_a_graph() {
        let (_dir, rt) = open_runtime();
        let out = graph_on(&rt, "trace-that-never-existed");
        assert_eq!(out["coverage"], "legacy");
        assert_eq!(out["truncated"], false);
        assert!(out["nodes"].as_array().unwrap().is_empty());
        assert!(out["edges"].as_array().unwrap().is_empty());
        assert!(out["events"].as_array().unwrap().is_empty());
        let codes = diagnostic_codes(&out);
        assert!(codes.contains(&"legacy_correlation_only".to_string()));
        assert!(codes.contains(&"no_spans".to_string()));
    }

    #[test]
    fn route_parser_accepts_only_the_exact_graph_shape() {
        assert_eq!(
            trace_id_from_graph_path("/lattice/api/v2/traces/a-flow-0/graph").as_deref(),
            Some("a-flow-0")
        );
        assert_eq!(
            trace_id_from_graph_path("/lattice/api/v2/traces/a%2Fb/graph?window=1").as_deref(),
            Some("a/b"),
            "path percent escapes decode as path bytes"
        );
        assert_eq!(
            trace_id_from_graph_path("/lattice/api/v2/traces/a+b/graph").as_deref(),
            Some("a+b"),
            "a plus sign is not a space in a path segment"
        );
        assert_eq!(
            trace_id_from_graph_path("/lattice/api/v2/traces//graph"),
            None
        );
        assert_eq!(
            trace_id_from_graph_path("/lattice/api/v2/traces/a/b/graph"),
            None,
            "a trace id is one path segment"
        );
        assert_eq!(
            trace_id_from_graph_path("/lattice/api/v2/traces/a-flow-0/graph/"),
            None
        );
        assert_eq!(
            trace_id_from_graph_path("/lattice/api/v2/traces/a-flow-0"),
            None
        );
        assert_eq!(
            trace_id_from_graph_path("/lattice/api/traces?trace=a-flow-0"),
            None
        );
        assert_eq!(
            trace_id_from_graph_path("/lattice/api/v2/dependencies"),
            None
        );
    }
}
