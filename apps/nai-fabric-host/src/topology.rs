//! Component topology for `GET /lattice/api/topology` (DASHBOARD_API §9).
//!
//! Derived from the SAME bounded trace window the traces endpoint builds
//! (`lattice_query::window_groups`): every persisted record that carries a
//! `trace_id` is a span, and every span carries the component it recorded.
//!
//!  - node  = a component observed in the window; `count` is the number of
//!            component-bearing spans from it, `errors` the number of those
//!            whose level is exactly `error`;
//!  - edge  = a change of component between consecutive component-bearing
//!            spans INSIDE one trace (`A -> B` counted once per transition).
//!            A record without a component is not a component change and does
//!            not break the chain.
//!
//! Nothing is invented: an empty window yields empty arrays, and a component
//! that never appeared is never fabricated. `window` is the bounded record
//! window actually examined; `truncated` reports whether the cap cut older
//! records (proven by the shared bounded read, not inferred).

use std::collections::BTreeMap;

use serde_json::{json, Value};

use nai_lattice_runtime::LatticeRuntime;

use crate::lattice_query::{DEFAULT_TRACE_WINDOW, MAX_TRACE_WINDOW};

/// Parsed `/lattice/api/topology?...` parameters (contract §9).
#[derive(Debug, Clone, Copy)]
pub(crate) struct TopologyOpts {
    /// 1..=8000, default 4000 — identical bounds to the traces window.
    pub(crate) window: usize,
}

impl Default for TopologyOpts {
    fn default() -> Self {
        Self {
            window: DEFAULT_TRACE_WINDOW,
        }
    }
}

/// Parse the route query string. An invalid `window` falls back to the default,
/// exactly like `/lattice/api/traces`.
pub(crate) fn parse_topology_opts(path: &str) -> TopologyOpts {
    let mut opts = TopologyOpts::default();
    for (key, value) in crate::lattice_query::query_pairs(path) {
        if key == "window" {
            opts.window = value
                .parse::<usize>()
                .map(|n| n.clamp(1, MAX_TRACE_WINDOW))
                .unwrap_or(DEFAULT_TRACE_WINDOW);
        }
    }
    opts
}

/// Production entry point: resolves the process runtime once.
pub(crate) fn topology(opts: &TopologyOpts) -> Value {
    match crate::gastown_orto::RUNTIME.get() {
        Some(rt) => topology_on(rt, opts),
        // No runtime opened yet: no window was examined, so nothing is derived.
        None => topology_from(0, false, &BTreeMap::new()),
    }
}

/// Testable core: derive topology from a specific runtime.
pub(crate) fn topology_on(rt: &LatticeRuntime, opts: &TopologyOpts) -> Value {
    let window = opts.window.clamp(1, MAX_TRACE_WINDOW);
    let (groups, examined, truncated) = crate::lattice_query::window_groups(rt, window);
    topology_from(examined, truncated, &groups)
}

fn topology_from(
    examined: usize,
    truncated: bool,
    groups: &BTreeMap<String, Vec<crate::lattice_query::TraceRec>>,
) -> Value {
    let (nodes, edges) = derive(groups);
    json!({
        "schema": 1,
        "generatedAt": crate::now_unix_ms(),
        "window": examined,
        "truncated": truncated,
        "nodes": nodes,
        "edges": edges,
    })
}

#[derive(Debug, Default, Clone, Copy)]
struct NodeStats {
    count: u64,
    errors: u64,
}

/// Count components and consecutive component transitions from real spans.
fn derive(
    groups: &BTreeMap<String, Vec<crate::lattice_query::TraceRec>>,
) -> (Vec<Value>, Vec<Value>) {
    let mut nodes: BTreeMap<String, NodeStats> = BTreeMap::new();
    let mut edges: BTreeMap<(String, String), u64> = BTreeMap::new();

    for recs in groups.values() {
        let mut previous: Option<&str> = None;
        for rec in recs {
            let Some(component) = rec.component.as_deref() else {
                // Not a component change: the previous component stays the
                // anchor for the next component-bearing span.
                continue;
            };
            let stats = nodes.entry(component.to_string()).or_default();
            stats.count = stats.count.saturating_add(1);
            if rec.level.eq_ignore_ascii_case("error") {
                stats.errors = stats.errors.saturating_add(1);
            }
            if let Some(prev) = previous {
                if prev != component {
                    let key = (prev.to_string(), component.to_string());
                    let count = edges.entry(key).or_insert(0);
                    *count = count.saturating_add(1);
                }
            }
            previous = Some(component);
        }
    }

    // Deterministic, useful ordering: busiest node first, then id; busiest
    // transition first, then source/target.
    let mut node_list: Vec<(String, NodeStats)> = nodes.into_iter().collect();
    node_list.sort_by(|a, b| {
        b.1.count
            .cmp(&a.1.count)
            .then_with(|| a.0.cmp(&b.0))
    });
    let nodes = node_list
        .into_iter()
        .map(|(id, stats)| {
            let label = id.clone();
            json!({
                "id": id,
                "label": label,
                "count": stats.count,
                "errors": stats.errors,
            })
        })
        .collect();

    let mut edge_list: Vec<((String, String), u64)> = edges.into_iter().collect();
    edge_list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let edges = edge_list
        .into_iter()
        .map(|((source, target), count)| {
            json!({
                "source": source,
                "target": target,
                "count": count,
            })
        })
        .collect();

    (nodes, edges)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OBS: &str = crate::lattice_query::OBS_NAMESPACE;

    fn open_runtime() -> (tempfile::TempDir, LatticeRuntime) {
        let dir = tempfile::tempdir().unwrap();
        let rt = LatticeRuntime::open(nai_lattice_runtime::RuntimeConfig {
            root: dir.path().to_path_buf(),
            ledger_segment_bytes: 64 * 1024 * 1024,
            ..Default::default()
        })
        .expect("open isolated runtime");
        (dir, rt)
    }

    /// Wait for the runtime's asynchronous boot `runtime.state` record so the
    /// fixture sits after it deterministically (the record carries a component
    /// but no trace id, so it is never a topology node).
    fn settle_runtime(rt: &LatticeRuntime) {
        let mut prev = examined_records(rt);
        for _ in 0..80 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            let now = examined_records(rt);
            if now == prev {
                return;
            }
            prev = now;
        }
    }

    fn examined_records(rt: &LatticeRuntime) -> usize {
        crate::lattice_query::read_recent_window(rt, 8_000).events.len()
    }

    #[allow(clippy::too_many_arguments)]
    fn append_span(
        rt: &LatticeRuntime,
        level: &str,
        component: Option<&str>,
        trace_id: &str,
        message: &str,
        ts: u64,
    ) {
        let mut payload = json!({
            "ts_ms": ts,
            "level": level,
            "trace_id": trace_id,
            "message": message,
            "extras": {},
        });
        if let Some(component) = component {
            payload["component"] = json!(component);
        } else {
            payload["component"] = Value::Null;
        }
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
            "topology-test",
            None,
        )
        .expect("append topology fixture record");
    }

    fn node<'a>(out: &'a Value, id: &str) -> &'a Value {
        out["nodes"]
            .as_array()
            .expect("nodes array")
            .iter()
            .find(|n| n["id"] == id)
            .unwrap_or_else(|| panic!("missing node {id}"))
    }

    fn edge<'a>(out: &'a Value, source: &str, target: &str) -> &'a Value {
        out["edges"]
            .as_array()
            .expect("edges array")
            .iter()
            .find(|e| e["source"] == source && e["target"] == target)
            .unwrap_or_else(|| panic!("missing edge {source}->{target}"))
    }

    #[test]
    fn two_traces_produce_exact_nodes_edges_and_counts() {
        let (_dir, rt) = open_runtime();
        settle_runtime(&rt);
        let baseline = examined_records(&rt);

        // T1: alpha -> beta -> beta -> gamma (consecutive duplicates are one node,
        // not an edge).
        append_span(&rt, "info", Some("alpha"), "T1", "request", 1_764_600_000_000);
        append_span(&rt, "info", Some("beta"), "T1", "tool", 1_764_600_000_010);
        append_span(&rt, "warn", Some("beta"), "T1", "retry", 1_764_600_000_020);
        append_span(&rt, "error", Some("gamma"), "T1", "terminal", 1_764_600_000_030);
        // T2: beta -> alpha.
        append_span(&rt, "error", Some("beta"), "T2", "request", 1_764_600_001_000);
        append_span(&rt, "info", Some("alpha"), "T2", "terminal", 1_764_600_001_010);

        let out = topology_on(&rt, &TopologyOpts { window: 4_000 });
        assert_eq!(out["schema"], 1);
        assert_eq!(
            out["window"].as_u64().unwrap() as usize,
            baseline + 6,
            "the bounded window examined the runtime's real records plus six spans"
        );
        assert_eq!(out["truncated"], false);

        let nodes = out["nodes"].as_array().expect("nodes array");
        assert_eq!(nodes.len(), 3, "exactly the components that appeared");
        // beta: 3 spans (info, warn, error) with 1 error.
        assert_eq!(node(&out, "beta")["count"], 3);
        assert_eq!(node(&out, "beta")["errors"], 1);
        // alpha: 2 spans, no error.
        assert_eq!(node(&out, "alpha")["count"], 2);
        assert_eq!(node(&out, "alpha")["errors"], 0);
        // gamma: 1 span, an error.
        assert_eq!(node(&out, "gamma")["count"], 1);
        assert_eq!(node(&out, "gamma")["errors"], 1);
        // label mirrors the real component id.
        assert_eq!(node(&out, "beta")["label"], "beta");
        // Deterministic order: busiest first, then id.
        let ids: Vec<&str> = nodes.iter().map(|n| n["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["beta", "alpha", "gamma"]);

        let edges = out["edges"].as_array().expect("edges array");
        assert_eq!(edges.len(), 3, "only consecutive component changes");
        assert_eq!(edge(&out, "alpha", "beta")["count"], 1);
        assert_eq!(edge(&out, "beta", "gamma")["count"], 1);
        assert_eq!(edge(&out, "beta", "alpha")["count"], 1);
        // beta -> beta is the same component, so it is not an edge.
        assert!(
            !edges
                .iter()
                .any(|e| e["source"] == "beta" && e["target"] == "beta"),
            "a repeated component is not an edge"
        );
    }

    #[test]
    fn window_with_no_component_bearing_traces_is_empty() {
        let (_dir, rt) = open_runtime();
        settle_runtime(&rt);
        // Records with a component but no trace id are not spans.
        append_span(&rt, "info", Some("lattice.runtime"), "", "boot", 1_764_700_000_000);
        append_span(&rt, "info", None, "", "no trace", 1_764_700_000_001);

        let out = topology_on(&rt, &TopologyOpts { window: 4_000 });
        assert_eq!(out["schema"], 1);
        assert_eq!(out["truncated"], false);
        assert!(
            out["nodes"].as_array().unwrap().is_empty(),
            "no component-bearing trace => no nodes (never fabricated)"
        );
        assert!(
            out["edges"].as_array().unwrap().is_empty(),
            "no component-bearing trace => no edges"
        );
    }

    #[test]
    fn window_cap_is_reported_as_truncated() {
        let (_dir, rt) = open_runtime();
        settle_runtime(&rt);
        for i in 0..12u64 {
            append_span(
                &rt,
                "info",
                Some(if i % 2 == 0 { "alpha" } else { "beta" }),
                "CAP",
                &format!("span {i}"),
                1_764_800_000_000 + i,
            );
        }

        let out = topology_on(&rt, &TopologyOpts { window: 5 });
        assert_eq!(out["window"], 5, "the cap is the window actually examined");
        assert_eq!(out["truncated"], true, "older records were cut by the cap");
        let counted: u64 = out["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["count"].as_u64().unwrap())
            .sum();
        assert_eq!(counted, 5, "every examined span is counted exactly once");

        // A window that contains the whole ledger is not truncated.
        let ledger = examined_records(&rt) as u64;
        assert!(ledger > 5, "the fixture must exceed the small window");
        let all = topology_on(&rt, &TopologyOpts { window: 8_000 });
        assert_eq!(all["truncated"], false);
        assert_eq!(all["window"].as_u64().unwrap(), ledger);
    }

    #[test]
    fn records_without_a_component_do_not_break_the_edge_chain() {
        let (_dir, rt) = open_runtime();
        settle_runtime(&rt);
        append_span(&rt, "info", Some("alpha"), "T", "one", 1_764_900_000_000);
        append_span(&rt, "info", None, "T", "uncorrelated component", 1_764_900_000_001);
        append_span(&rt, "info", Some("beta"), "T", "two", 1_764_900_000_002);

        let out = topology_on(&rt, &TopologyOpts { window: 4_000 });
        assert_eq!(out["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(
            edge(&out, "alpha", "beta")["count"],
            1,
            "a component-less record is not a component change"
        );
        assert_eq!(out["edges"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn topology_options_are_bounded_like_the_traces_window() {
        assert_eq!(parse_topology_opts("/lattice/api/topology").window, 4_000);
        assert_eq!(
            parse_topology_opts("/lattice/api/topology?window=25").window,
            25
        );
        assert_eq!(parse_topology_opts("/lattice/api/topology?window=0").window, 1);
        assert_eq!(
            parse_topology_opts("/lattice/api/topology?window=999999").window,
            8_000
        );
        assert_eq!(
            parse_topology_opts("/lattice/api/topology?window=nope").window,
            4_000
        );
        assert_eq!(TopologyOpts::default().window, 4_000);
    }

    #[test]
    fn no_runtime_reports_an_empty_examined_window() {
        let out = topology_from(0, false, &BTreeMap::new());
        assert_eq!(out["schema"], 1);
        assert_eq!(out["window"], 0);
        assert_eq!(out["truncated"], false);
        assert!(out["nodes"].as_array().unwrap().is_empty());
        assert!(out["edges"].as_array().unwrap().is_empty());
    }
}
