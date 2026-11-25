# Execution Chain Producer Contract — NAI OS / N-Pilot / NAF

- **Status:** normative producer contract; NAI-Lattice consumer implemented in
  the current worktree, producer propagation **blocked** on the owning repos
- **Date:** 2026-11-25
- **Owner of this document:** NAI-Lattice
- **Producers:** NAI OS, N-Pilot, NAF client/server dispatch
- **Consumers:** NAI-Lattice host (`/lattice/api/v2/traces/{trace_id}/graph`,
  schema 2), the Vue dashboard `Execution Chain` / `Request → Response` views
- **Related:** `docs/DASHBOARD_API.md` §3–§4, the internal NAI OS request log
  (NAIOS-003.4), the internal gap audit §2–§4, E01–E03

## 0. Scope and truth rules

This is a **producer** contract. NAI-Lattice consumes explicit evidence; it does
not invent a chain. The producer side of a real NAI OS → NAF → N-Pilot → tool /
model operation does not exist yet and is owned by those repositories.

The following are hard rules for every producer and consumer:

1. A record is a **span** only when it carries a non-empty `extras.span_id`.
   Everything else is a **point event**.
2. A **parent** edge exists only from an explicit `extras.parent_span_id`.
3. A **call** edge exists only from an explicit `extras.call` object with a
   non-empty `target_span_id`.
4. An **async** edge exists only from an explicit `extras.links[]` entry of
   kind `async`. A **response** edge is a separate explicit observation.
5. Sharing a `trace_id`, sharing a `request_id`, record order, component
   co-occurrence, timestamps, or message text **never** creates a call edge.
6. A response or completion event is not a callee. A ledger append is not an
   RPC hop.
7. Missing data stays missing: `null` / `N/A` / `unresolved`, never a guessed
   component, duration, parent, or status.
8. There is exactly one observability ledger (`observability.log.v1`). This
   contract must not be implemented as a second trace store, a second queue, or
   a second database.

## 1. Identity and W3C trace context

New instrumented paths use [W3C Trace Context](https://www.w3.org/TR/trace-context/):

- `traceparent: 00-<32 lowercase hex trace-id>-<16 lowercase hex parent-id>-<flags>`
  on NAF/HTTP/IPC boundaries that support headers;
- `tracestate` when a vendor state is needed;
- `trace_id` = 32 lowercase hex characters (16 bytes);
- `span_id` = 16 lowercase hex characters (8 bytes);
- flags bit 0 = sampled; a non-sampled trace may still carry metrics but must
  not be presented as a complete chain.

Legacy identifiers:

- A legacy `request_id` is not a `trace_id`. Keep it in `request_id` and, when a
  trace is assembled, in `legacy_request_ids[]`.
- A legacy ID may be longer or shorter than W3C and may contain arbitrary text.
  Preserve it verbatim; do not truncate, hash, or rewrite it into a W3C ID.
- The host accepts a non-empty `payload.trace_id` (or the envelope
  `correlation_id`) for backward compatibility. Producers must not rely on that
  leniency for new paths; validate W3C at the source.

Correlation rules:

- `trace_id` groups a distributed operation.
- `span_id` identifies one operation inside that trace.
- `parent_span_id` establishes hierarchy inside one trace.
- `request_id` groups a request's records but is **insufficient** for parentage
  or an exact call edge.
- `invocation_id` identifies one logical caller→callee invocation across
  processes; `attempt_id` identifies one retry of it.

## 2. Record shape

A traced record is a normal append to the canonical ledger:

```json
{
  "namespace": "observability.log.v1",
  "event_type": "log.record",
  "correlation_id": "<trace_id>",
  "timestamp_utc_ms": 1764000000000,
  "durability": "durable",
  "payload": {
    "ts_ms": 1764000000000,
    "level": "info",
    "component": "nai-os.agent",
    "trace_id": "<trace_id>",
    "caller": "nai-os.shell",
    "message": "tool.receipt read_file",
    "extras": {
      "span_id": "4bf92f3577b34da6",
      "parent_span_id": "00f067aa0ba902b7",
      "operation": "tool.exec",
      "span_kind": "client",
      "instance_id": "n-pilot-1",
      "status": "ok",
      "duration_ms": 650,
      "clock_domain": "n-pilot:pid-1234:monotonic",
      "call": {
        "target_span_id": "a3ce929d0e0e4736",
        "target_component": "nai-os.terminal",
        "operation": "process.exec",
        "transport": "naf/rpc"
      },
      "evidence_refs": ["session:...", "artifact:sha256:..."]
    }
  }
}
```

Required for every traced record:

| Field | Type | Rule |
|---|---|---|
| `payload.trace_id` | string | W3C 32 hex on new paths; legacy preserved if unavoidable |
| `payload.ts_ms` | u64 | Producer event time, epoch ms; envelope timestamp is only a fallback |
| `payload.level` | enum | `trace|debug|info|warn|error` |
| `payload.component` | string | Logical component (e.g. `nai-os.agent`); not a process name |
| `payload.message` | string | Human summary; must not be parsed as the chain source |

Required for a span (`extras` or flattened payload; **use `extras`**):

| Field | Type | Rule |
|---|---|---|
| `span_id` | string | Non-empty; unique within the trace |
| `operation` | string | Stable operation name (`turn.submitted`, `tool.exec`, …) |
| `span_kind` | enum | `server|client|producer|consumer|internal` |
| `status` | enum | `ok|error|failed|cancelled`; an `error` level also means error |
| `duration_ms` | number | Measured, finite, ≥ 0; omit when not measured |
| `instance_id` | string | Optional instance/process identity |
| `parent_span_id` | string | Optional; explicit parent only |
| `clock_domain` | string | Producer clock identity for cross-process timing |
| `links[]` | array | Optional explicit async links |
| `call` | object | Optional explicit call observation |
| `evidence_refs[]` | array | Optional stable, redacted evidence IDs |

A span with no `parent_span_id` is a **root span** in the host response. The host
adds a synthetic `trace:<trace_id>` node and a `parent` edge from that root; that
node is a rendering root, not a component and not a call.

### 2.1 What NAI-Lattice reads today

`apps/nai-fabric-host/src/lattice_trace_graph.rs` reads:

- `payload.trace_id`, falling back to the envelope `correlation_id`;
- `payload.ts_ms`, falling back to `timestamp_utc_ms`;
- `payload.level`, `payload.component`, `payload.message`;
- `extras.span_id`, `extras.parent_span_id`, `extras.operation`,
  `extras.duration_ms`, `extras.instance_id`, `extras.status`;
- `extras.call.target_span_id`, `.target_component`, `.operation`, `.transport`.

The host does **not** read `links[]`, `response`, `invocation_id`,
`attempt_id`, `clock_domain`, or `evidence_refs` beyond the ledger
`runtime_sequence` at this snapshot. Emit them anyway; they are required for the
next consumer revision and for cross-repo evidence.

## 3. Point events vs spans vs calls

| Entity | Persisted shape | UI surface | Counted as |
|---|---|---|---|
| Trace | grouped by `trace_id` | trace row | one trace |
| Span | record with `extras.span_id` | CHAIN node, WATERFALL bar, CALL TREE row | one span |
| Point event | record without `extras.span_id` | EVENTS row, WATERFALL tick | one event |
| Call | span with `extras.call` | CHAIN call edge | one explicit call |
| Parent relation | child `parent_span_id` | CALL TREE / CHAIN parent edge | one parent edge |
| Async link | `links[]` kind `async` | CHAIN async edge (reserved) | one async edge |
| Response | explicit response observation | separate edge (reserved) | not a call |

Rules:

- Put point events in `EVENTS`, never under `SPANS`.
- A point event gets a tick/diamond, never a full-length duration bar.
- A span with no recorded duration gets `N/A` and no invented interval.
- A legacy request-correlation trace (no `span_id`) gets point markers only and
  the exact text `CAUSAL CHAIN UNAVAILABLE — legacy correlation only`.
- Counts are separate: `EVENTS`, `SPANS`, `CALLS`. Inferred edges are shown
  separately and never inflate `CALLS`.
- A span record may be duplicated by a retry/queue; the consumer collapses
  duplicate `span_id` records into one node and keeps every evidence sequence.

## 4. Edge grammar

### 4.1 Parent edge

```json
{ "parent_span_id": "00f067aa0ba902b7" }
```

- Source = parent span (or `trace:<trace_id>` when the child has no parent).
- Target = child span.
- Semantic: nesting/hierarchy.
- Parent nesting **does not** imply a call. A child can be a local operation.
- If the parent is not in the bounded trace read, the node is
  `unresolved:<parent_span_id>`; its component stays `null` unless a call
  observation explicitly supplied `target_component`. The response carries an
  `unresolved_parent` diagnostic.

### 4.2 Call edge

```json
{
  "call": {
    "target_span_id": "a3ce929d0e0e4736",
    "target_component": "nai-os.terminal",
    "operation": "process.exec",
    "transport": "naf/rpc"
  }
}
```

- Source = caller span; target = callee span.
- Semantic: caller → callee communication.
- `target_span_id` is required; `target_component`, `operation`, `transport` are
  optional but strongly recommended.
- A call edge is explicit evidence, never inferred from co-occurrence.
- `inferred: false` on the wire; an inferred edge (if ever added) must be dashed,
  labelled, off by default, and never counted as an explicit call.

### 4.3 Async edge

```json
{
  "links": [
    { "kind": "async", "trace_id": "<trace_id>", "span_id": "a3ce929d0e0e4736" }
  ]
}
```

- Semantic: predecessor → successor causal link without parent nesting.
- Reserved in this contract; the current host does not parse `links[]`.
- The consumer must not turn an async link into a parent edge.

### 4.4 Response edge

A response/completion observation is a separate semantic type:

```json
{
  "response": {
    "source_span_id": "a3ce929d0e0e4736",
    "target_span_id": "4bf92f3577b34da6",
    "operation": "process.exec",
    "transport": "naf/rpc",
    "status": "ok"
  }
}
```

- Semantic: callee → caller completion/acknowledgement.
- Not a callee, not a new call, and not counted in `CALLS`.
- Reserved; the current host does not parse `response`.

### 4.5 Deduplication of client/server pairs

One logical call may be observed twice:

- a **client** span in the caller (send/await), and
- a **server** span in the callee (receive/execute).

Producers must:

- give both spans distinct `span_id`s;
- put a stable `invocation_id` on the call observation;
- emit **one** call observation with `target_span_id` pointing at the server
  span (or, if the client cannot know it, at a producer-assigned callee span);
- retain both records as evidence; the consumer counts one call, not two.

Retries use the same `invocation_id` with a new `attempt_id`. The UI may
aggregate attempts, but must not silently collapse distinct operations into one
without a visible count.

Current limitation: the host dedups duplicate edges by
`(kind, source, target, operation, transport)` and keeps the first evidence
sequence; it does not parse `invocation_id`. Producers must still emit it; the
consumer-side invocation dedup is not implemented (owner: NAI-Lattice).

## 5. Timing and clock domains

- `ts_ms` is wall-clock epoch ms. Use it for display and ordering.
- `duration_ms` must come from a local monotonic measurement. Do not derive it
  from two unsynchronized wall clocks.
- `clock_domain` identifies the clock (process + boot/monotonic identity). A
  consumer must not compute precise cross-process latency from different clock
  domains.
- `startOffsetMs` in the host response is `span.ts_ms - earliest span ts_ms` in
  the trace. It is a **wall-clock-relative** offset, not a clock-corrected
  monotonic offset. The UI labels it `+offset`; do not present it as exact
  cross-process latency.
- Missing duration is `null`/`N/A`. An ongoing span may show measured elapsed
  time labelled `ongoing`; it must not show a fabricated end or completion.
- Queue time, TTFT, prefill, stream completion, cancellation, and token
  throughput are emitted only when actually measured, with a stated denominator.

## 6. Coverage, truncation and diagnostics

The host graph response is schema **2**:

```json
{
  "schema": 2,
  "traceId": "<trace_id>",
  "generatedAt": 1764000000000,
  "window": 371,
  "truncated": false,
  "coverage": "legacy|spans|mixed",
  "nodes": [],
  "edges": [],
  "events": [],
  "diagnostics": []
}
```

- `window` is the number of ledger records **examined**, not a time range.
- `coverage`:
  - `legacy`: no record carries `span_id`; every record is a point event.
  - `spans`: every examined record is a span.
  - `mixed`: at least one record is a point event and at least one is a span.
- `truncated` is true when any bound cut evidence: 50 000 examined records,
  2 000 trace hits, 500 nodes, 1 000 edges, or 2 000 events.
- Diagnostics (stable codes):
  - `legacy_correlation_only` — no spans; render the legacy fallback;
  - `no_spans` — no records found for this trace in the bounded read;
  - `unresolved_parent` — a parent/call target was not observed;
  - `truncated` — the chain is incomplete; the message names the bound.
- `unresolved:<id>` nodes have `status:"unset"`, null duration/offset/instance,
  and a reason; they never get an invented component.
- A parent/call cycle is handled by the consumer with a visible cycle row/guard;
  producers must not create cycles, but the consumer must not hang on one.

The current dashboard normalizer maps only `explicit`, `legacy`, and `partial`.
The host's `spans` and `mixed` values currently normalize to `unknown` in
`packages/lattice-dashboard/src/query/traces.ts`; the chain still renders, but
the coverage label is not yet canonical. This is a known consumer defect, not a
producer licence to change the values.

## 7. Redaction and bounded emission

Producers must:

- never append credentials, tokens, private keys, raw prompts, hidden reasoning,
  or unrelated personal data;
- allowlist/redact before append; the dashboard inspector shows raw fields
  verbatim, so redaction cannot be deferred to the UI;
- use stable `evidence_refs` for authorized drill-down (session/artifact IDs),
  not the sensitive body;
- use a nonblocking, bounded emitter; no synchronous disk/network I/O on a hot
  path;
- record dropped/sampled counts and instrumentation coverage; a gap must be
  visible in the trace, not silently closed;
- use an idempotency key for retries and dedup at the canonical ledger;
- keep the N-Pilot scope local-only; no new cloud inference path.

The ledger is the only observability store. Do not add a second span database,
a second queue, or a second retention policy.

## 8. Persistence and query

- Append spans/events/call observations to the canonical ledger with
  `event.append` (durable) and the `observability.log.v1` namespace.
- Reconstruct after host restart from the ledger; do not rely on in-memory
  state.
- Handle out-of-order start/end, duplicates, retries, late arrival, incomplete
  and cancelled operations, missing data, and cross-clock ordering.
- The graph API reads through the existing bounded query path
  (`lattice_query::query_on` with the exact `trace` filter and `nextAfter`
  paging); no arbitrary SQL, no unbounded scan, no per-selection full-ledger
  scan.
- Historical reads are bounded and report `window`/`truncated`; active-trace
  incremental updates are not implemented in this batch (the UI loads the graph
  on selection and refreshes on demand).

## 9. Reference implementation status (NAI-Lattice)

### 9.1 Host graph — implemented in the worktree

- `apps/nai-fabric-host/src/lattice_trace_graph.rs` — schema-2 graph builder.
- `apps/nai-fabric-host/src/gastown_orto.rs` — route
  `GET /lattice/api/v2/traces/{trace_id}/graph`.
- `apps/nai-fabric-host/src/lattice_dashboard.rs` — `HOST_FEATURES` advertises
  `execution_chain_api`.
- Unit tests in the module cover: legacy point events never become a graph;
  explicit parent nodes/edges/offsets; component co-occurrence creates no call;
  missing parent becomes unresolved; explicit call object; unresolved call
  target; node/edge caps and truncation; unknown trace; route parser.
- **Important:** the delivered 0.1.3 host (`dist/0.1.3/host/nai-fabric-host.exe`,
  sha256 `9b47c4f2…14e286a8`) and the running QA host are an **older build**;
  they do not serve this route. The source worktree is ahead of the delivered
  artifact. A rebuild and re-delivery are required before the route is runtime
  evidence.

### 9.2 Dashboard chain — implemented in the worktree

- `packages/lattice-dashboard/src/query/traces.ts` — `fetchGraph` +
  schema-2 normalization; HTTP 404 is a truthful `unavailable`, not a fake
  graph.
- `modules/traces/useTraceGraph.ts` — one bounded read per selection with a
  generation guard; `legacy` is a state, not an error.
- `modules/traces/chain.ts` — counts (`EVENTS`/`SPANS`/`CALLS`), ticks vs bars,
  legacy markers, linear/branched chain layout, call-tree rows, waterfall
  geometry.
- `modules/traces/TraceDetail.vue` — `CHAIN | WATERFALL | CALL TREE | EVENTS`
  tabs and counts.
- `TraceChainGraph.vue`, `TraceWaterfall.vue`, `TraceCallTree.vue`,
  `TraceEvents.vue`, `ChainNodeCard.vue`, `ChainEdgeChip.vue`.
- `modules/traces/ExecutionChain.vue` — independently dockable
  `execution-chain` module, registered in `shell/registry.ts` and seeded into
  fresh/reset layouts (additive inside workspace v4; existing v4 layouts stay
  valid).
- Legacy fallback text is explicit:
  `CAUSAL CHAIN UNAVAILABLE — legacy correlation only`.
- **Important:** the delivered bundle (`dashboard_bundle_blake3`
  `8bb7ceae…bfa202`) predates these files. They are source-only at this
  snapshot; no runtime screenshot or built bundle of the chain UI exists in this
  audit.

### 9.3 Legacy fallback

For a trace with no `span_id` (the current QA fixtures are exactly this):

- the host returns `coverage:"legacy"`, empty `nodes`/`edges`, the point
  records in `events`, and `legacy_correlation_only`;
- the dashboard shows the unavailable message and lists the legacy point
  records; it does not draw a chain or a duration bar;
- a 404 from an older host is also treated as legacy/unavailable, not as an
  error to paper over.

## 10. What is not implemented here, and who owns it

| Missing capability | Owner |
|---|---|
| Real `traceparent`/`tracestate` propagation through NAI OS → NAF → N-Pilot | NAI OS + NAF client/server |
| Root/dispatch/native-window spans | NAI OS |
| Turn/agent/model/tool spans and call observations | N-Pilot |
| `invocation_id` / `attempt_id` dedup on the consumer | NAI-Lattice |
| Async `links[]` and response edge parsing | NAI-Lattice |
| W3C trace/span length validation in the host | NAI-Lattice |
| `evidence_refs[]` surfacing in the graph/UI | NAI-Lattice |
| Cross-process clock-domain uncertainty in offsets | NAI-Lattice |
| Live incremental graph updates for an active trace | NAI-Lattice |
| Selected-trace topology overlay | NAI-Lattice |
| Flame graph / sampled CPU profile / self-time / critical path | NAI-Lattice |
| Real ≥3-boundary operation, tool/model invocation, failure/concurrency | NAI OS + N-Pilot + QA |
| Persistence/reload proof after an isolated host restart | QA |
| QA fixtures with explicit `span_id`/`parent_span_id`/`call` | QA |
| Redaction enforcement at the producer | NAI OS + N-Pilot + NAF |
| Production signing/activation | Release owner |

Until the producer rows are implemented and a real operation is captured
(E01–E03), the chain is **consumer-ready but not producer-proven**. A legacy
trace must show the fallback; a fixture with explicit spans can prove the
rendering, but it is not a real NAI workflow.

## 11. Acceptance tests

Consumer (NAI-Lattice) — implemented tests in
`apps/nai-fabric-host/src/lattice_trace_graph.rs`:

- point-only trace → `legacy`, no nodes/edges, all events, diagnostic;
- explicit parent → exact nodes, parent edges, offsets, status;
- component co-occurrence → no call edge;
- missing parent → unresolved node, null component, diagnostic;
- explicit call → call edge with operation/transport, `inferred:false`;
- call to unobserved target → unresolved call edge;
- node/edge/event caps → `truncated` + diagnostic, no dangling edges;
- unknown trace → `legacy` + `no_spans`;
- route parser accepts only the exact graph shape.

Producer (blocked) — when available, capture:

1. a real operation with a root application/process and trace ID;
2. at least three distinct verified components A → B → C with paired
   invocation evidence/parent links;
3. at least one actual tool or local-model invocation and terminal completion;
4. one controlled failure/cancellation and one real concurrent branch;
5. persisted records, graph API responses, and UI screenshots of chain,
   waterfall, call tree, events, and edge → underlying log selection;
6. close/reopen and query the retained trace after an isolated host restart;
7. a legacy-only trace that shows the exact fallback, not a fabricated chain.

## 12. Versioning

- Graph response schema is `2`; fields are additive. A breaking change bumps
  the schema.
- `coverage` values `legacy|spans|mixed` are fixed for schema 2.
- Record fields live in `payload.extras`; adding a field is additive. Removing
  or renaming a field requires a producer/consumer version boundary.
- Legacy records are preserved and read as point events; they are never
  rewritten into synthetic spans.
