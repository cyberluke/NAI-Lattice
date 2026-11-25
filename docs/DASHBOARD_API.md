# Lattice dashboard API v1 (frozen contract)

Status: **frozen for the M1 batch** (2026-11-24). Owner: NAI-Lattice.
Consumers: `packages/lattice-dashboard` (Vue/Dockview shell), NAI OS WebView2 embed,
`mcp-nai-lattice` log methods.

This document is authoritative for endpoint shape. Both the Rust producer
(`apps/nai-fabric-host/src/lattice_query.rs`, `lattice_broker.rs`) and the Vue
consumer code against it. Changes require a schema version bump.

## 0. Invariants

- Loopback only. Same Host/Origin guard as the rest of the control plane.
- No tokens in URLs. No wildcard CORS. Responses are `application/json`,
  `Cache-Control: no-store`.
- Every count is measured from real records. An unknown value is `null`, never a
  synthesized zero.
- Bounded work per request: no endpoint may scan an unbounded ledger in one call.
  `scanned` and `hasMore` make the bound visible.

## 1. `GET /lattice/api/summary`

Stable compact payload for a NAI OS toolbar/status indicator and the shell header.
Cheap: bounded recent window only.

```json
{
  "schema": 1,
  "generatedAt": 1764000000000,
  "window": 2000,
  "levels": { "info": 1800, "error": 3 },
  "errors": 11,
  "components": { "nai-os.observability": 1200 },
  "distinctTraceIds": 42,
  "stream": {
    "activeSseClients": 2,
    "maxSseClients": 24,
    "pushTotal": 981,
    "subscribers": 2,
    "brokerScansTotal": 981,
    "lastScanUnixMs": 1764000000000,
    "droppedTotal": 0
  },
  "health": {
    "state": "healthy",
    "detail": null,
    "lastError": null,
    "sinceUnixMs": 1763999000000,
    "uptimeMs": 1000000
  },
  "version": {
    "host": "0.1.3",
    "dashboard": "0.1.0",
    "protocolMin": 1,
    "protocolMax": 1,
    "storageSchema": 1
  },
  "features": ["activation", "execution_chain_api", "gpu_metrics", "host_metrics", "shutdown_if_idle", "maintenance_api", "search_api", "sse_resume", "storage_api", "topology_api"]
}
```

`storage` (present once `storage_api` is advertised; read-only):

```json
"storage": {
  "schema": 1,
  "root": "C:\\Users\\me\\AppData\\Local\\NAI\\Lattice\\v1",
  "base": "C:\\Users\\me\\AppData\\Local\\NAI\\Lattice",
  "budgetBytes": 536870912000,
  "usedBytes": 123456789,
  "freeBytes": 901234567890,
  "reserveBytes": 337000000000,
  "pressure": "normal",
  "classes": {
    "ledger": 100, "cas": 200, "hot": 300, "search": 400,
    "vector": 500, "readModels": 600, "logs": 700,
    "backups": 800, "audit": 900
  },
  "retention": { "logsDays": 30, "auditDays": 90, "backupDays": 14, "runtimeKeep": 2 },
  "lastPrune": { "runUnixMs": 1764000000000, "bytesFreed": 12345 }
}
```

`budgetBytes = max(512 MiB, freeBytes − reserveBytes)` with
`reserveBytes = max(20 GiB, 15% of the volume's free bytes)`;
`pressure` is `critical` when `usedBytes > budgetBytes`, `elevated` at
≥ 90% of the budget, otherwise `normal`. If the volume cannot be queried, the
three byte fields are `null` and `pressure` is `"unknown"`. Class sizes are real
directory sizes; a missing class is `0`, never omitted. `lastPrune` is `null`
when no audit record exists. An additive `pins` field may appear:
`{count, versions[, error]}` for runtime versions pinned against pruning. The
same payload is returned by `runtime.storage.status`.

## 2. `GET /lattice/api/query`

Server-side typed historical query. This is the **source of truth** for search;
the UI table is a view over these pages. Deterministic ordering by
`runtime_sequence`.

Query parameters:

| Param | Type | Default | Meaning |
|---|---|---|---|
| `level` | enum | – | minimum level: `trace|debug|info|warn|error` |
| `component` | string | – | exact component |
| `trace` | string | – | exact `trace_id` |
| `q` | string | – | case-insensitive substring over `message` |
| `since_ms` | u64 | – | inclusive `timestamp_utc_ms` lower bound |
| `until_ms` | u64 | – | inclusive `timestamp_utc_ms` upper bound |
| `after` | u64 | 0 | exclusive `runtime_sequence` cursor |
| `limit` | int | 200 | 1..=1000 |
| `scan_budget` | int | 200000 | max ledger events examined this call (1..=500000) |

Response:

```json
{
  "schema": 1,
  "hits": [ { "runtime_sequence": 123, "timestamp_utc_ms": 1764000000000,
              "level": "info", "component": "x", "trace_id": "t",
              "message": "...", "extras": {} } ],
  "count": 200,
  "nextAfter": 4567,
  "hasMore": true,
  "exhausted": false,
  "scanned": 200000,
  "budgetHit": true,
  "order": "asc",
  "generatedAt": 1764000000000
}
```

- `hits` are ordered oldest → newest (ascending `runtime_sequence`) so a client can
  append pages without re-sorting.
- `nextAfter` is the last delivered `runtime_sequence` (null when none).
  When the scan stops on `scan_budget` before any matching record, it is the
  last examined `runtime_sequence` instead, so a filtered page can still make
  progress; it is null only when nothing was delivered and nothing remains.
- `hasMore` is true when the budget was hit before the ledger end; continue with
  `after=nextAfter`.
- `exhausted` is true when the scan reached the current ledger end.
- Filtering happens before pagination: a page contains up to `limit` matching
  records, and `scanned` counts examined records.

## 3. `GET /lattice/api/traces`

Real request→response grouping from correlation fields, not client-side guessing.

```json
{
  "schema": 1,
  "window": 4000,
  "traces": [{
    "traceId": "abc",
    "firstTs": 1764000000000,
    "lastTs": 1764000001200,
    "durationMs": 1200,
    "state": "closed",
    "terminal": "completed",
    "ok": true,
    "spans": [{ "ts": 1764000000000, "kind": "request|tool|terminal|log",
                "label": "...", "runtime_sequence": 12,
                "durationMs": null }]
  }],
  "generatedAt": 1764000000000
}
```

`state` is `open` when no terminal span was observed. Absent duration is `null`.
`ok` is `true`/`false` only for a closed trace whose terminal span was observed;
it is `null` for an open trace, because an outcome was never recorded.

Each span also carries `component` (the log record's component, or `null`). The
topology endpoint is derived from those real components.

`?window=` (1..=8000, default 4000) bounds how many recent records are examined.
`?trace=<id>` returns exactly that trace even when it is older than the window
(bounded scan: at most 50 000 examined records / 2 000 hits; `truncated` is then
true). Without matches, `traces` is empty — the UI must say “not found”, never
invent a span. The response echoes `trace` when the filter was applied.

## 4. `GET /lattice/events` (SSE)

- Events: `search` (initial page), `log` (delta), `stats`, `state`
  (lifecycle transition), `status` (connection truth), plus `: heartbeat` comments.
- `id:` on every data event = `runtime_sequence` cursor (monotonic).
- `retry: 3000` is emitted once at stream open.
- Resume: client reconnects with `Last-Event-ID`; the server replays from that
  cursor and never resets to zero. `?after=<seq>` is also accepted, both for a
  first connect and for a client that re-creates its `EventSource` after a
  controlled backoff (the bundled shell does the latter; cursor semantics are
  identical).
- All connected clients are served by **one shared scan broker** (one ledger read
  per tick for the whole set), not one scanner thread per connection.
- Backpressure is bounded per subscriber. Dropped events are reported in the next
  `status`/`stats` event (`droppedTotal`); the client renders `gap` truthfully
  and resumes from its last cursor.
- `state` payload: `{ "state": "draining", "detail": "...", "etaMs": 5000,
  "sinceUnixMs": 1764000000000 }`.
- Refusal: when the global subscriber cap is reached, respond `503` with
  `Retry-After: 3` (never a silently dead stream).

## 5. `GET /lattice/api` (legacy, kept)

Existing tail + search + stats shape used by `mcp-nai-lattice` and the legacy
dashboard. Kept byte-compatible: `schema`, `hits` (newest-first), `count`,
`stats`. New consumers use §1–§3.

## 6. `GET /lattice` (Vue shell)

- `/lattice` and `/lattice/` serve the embedded Vite bundle index (`Cache-Control: no-cache, must-revalidate`).
- `/lattice/assets/...` serves only verified manifest entries (`public, max-age=31536000, immutable`); `/lattice/popout.html` is the Dockview popout.
- `/lattice/legacy` is the explicit legacy-dashboard rollback; `/lattice/next` remains a compatibility alias.
- Missing bundle: `/lattice` serves the legacy dashboard; `/lattice/next` 302s to `/lattice` (no loop).
- `?embed=1` → chrome-free shell that fills the WebView2 viewport.
- Deep links: `panel=log-tail|traces|components`, `trace=<id>`, `component=<name>`,
  `time=<epoch_ms>`.
- CSP: `default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline';
  connect-src 'self'; img-src 'self' data:; frame-ancestors 'self'; base-uri 'none'`.
  No external origins; the bundle is fully self-hosted.

The canonical dashboard URL is `/lattice`; the legacy page is `/lattice/legacy`.

### 6.1 QA instances (isolated preview)

Setting `NAI_LATTICE_INSTANCE=<name>` (plus a scratch `NAI_LATTICE_ROOT` and a
distinct `HELLDESK_CTRL_PORT`) runs a fully isolated host for QA: the singleton
mutex and canonical pipe get the same `<name>` suffix, the legacy username alias
is disabled, and `endpoint.json` records `"instance": "<name>"`. Production
behavior is unchanged when the variable is unset. Nothing in an isolated
instance is discoverable by normal clients or product spawns.

```powershell
$env:NAI_LATTICE_ROOT      = "$env:TEMP\nai-lattice-qa\v1"
$env:NAI_LATTICE_INSTANCE  = 'qa-dashboard'
$env:HELLDESK_CTRL_PORT    = '8790'
target\release\nai-fabric-host.exe
# then open http://127.0.0.1:8790/lattice
```

## 7. Bundle manifest

`packages/lattice-dashboard/dist/bundle-manifest.json`, produced by the Vite build:

```json
{ "schema": 1, "name": "lattice-dashboard", "version": "0.1.0",
  "builtAt": "2026-11-24T20:00:00Z", "entry": "index.html",
  "files": [{ "path": "assets/index-abc.js", "bytes": 12345, "blake3": "..." }] }
```

The host embeds the manifest and files at compile time; a missing or malformed
manifest means "no bundle" and the route falls back to legacy. The host refuses
paths outside the manifest.

## 8. Host telemetry — `GET /lattice/api/host-metrics`

```json
{
  "schema": 1,
  "generatedAt": 1764000000000,
  "source": "nvidia-smi",
  "cpu": { "utilizationPct": 12.5 },
  "memory": { "totalBytes": 68559708160, "availableBytes": 41234567890 },
  "gpu": {
    "name": "NVIDIA GeForce RTX 4090",
    "utilizationPct": 5,
    "memoryUsedBytes": 2807037952,
    "memoryTotalBytes": 24142446592,
    "temperatureC": 54
  }
}
```

`gpu` is `null` when no NVIDIA sampler is available; `source` is then
`"none"`. `cpu.utilizationPct` is `null` until a previous sample exists;
`memory` is `null` if the OS query fails; GPU `utilizationPct`/`temperatureC`
may be `null` and the temperature may be fractional. Values are measured at most
every 2 s (cached). Nothing is synthesized. The host registers `host_metrics`
when the endpoint is served and `gpu_metrics` only after a real GPU sample
succeeds.

The payload also carries the full device inventory: `gpus` is an array of every
real adapter (`id`, `vendor`, `name`, `source`, nullable metrics, `detail`), and
`npu` is the Windows ComputeAccelerator (for example Intel AI Boost) or `null`.
NVIDIA entries come from `nvidia-smi`; other adapters and the NPU come from a
one-time Windows inventory and keep utilization/memory `null` with an explicit
`detail` when the host API does not expose them. `source` is
`nvidia-smi+windows` when both are present. `gpu` remains the primary NVIDIA
sample for older readers.

## 9. Topology — `GET /lattice/api/topology?window=4000`

```json
{
  "schema": 1,
  "generatedAt": 1764000000000,
  "window": 4000,
  "truncated": false,
  "nodes": [{ "id": "nai-os.agent", "label": "nai-os.agent", "count": 65, "errors": 11 }],
  "edges": [{ "source": "nai-os.agent", "target": "nai-os.terminal", "count": 22 }]
}
```

Derived from real trace spans: each span carries `component`; an edge is a
consecutive component change inside one trace. `window` is the bounded record
window actually examined; `truncated` reports the cap. Empty arrays (not fake
nodes) when the window has no component-bearing traces. Component-less spans do
not break the edge chain (the next component-bearing span connects to the last
known component). Feature: `topology_api`.

## 10. Popouts, FDC3 channels and the module SDK

- **Popouts (U04, opt-in):** the workspace exposes a `POP` action that detaches
  the active panel into a Dockview popout window (same origin, normal browser
  window). NAI OS remains the desktop window manager; no OS-level integration is
  assumed. A failed/blocked popout shows a visible notice and leaves the panel in
  place.
- **FDC3 channel isolation (I02):** the adapter joins or creates the app channel
  `nai.lattice` and raises `nai.lattice.logSelection` / `nai.lattice.trace`
  there. Private contexts stay on the default channel; the shell status shows the
  active channel. No agent → the honest “no desktop agent” state.
- **Module SDK (I03):** `window.naiLattice` v1 exposes
  `{ version, channel, focusTrace(id), raiseSelection(context) }` for a
  same-origin module; it never fabricates a desktop agent.

## 11. Execution-chain graph — `GET /lattice/api/v2/traces/{trace_id}/graph`

Schema 2. Reads explicit span/call records only; no edge is inferred from
component co-occurrence, sequence or timestamps.

```json
{
  "schema": 2, "traceId": "...", "generatedAt": 0, "window": 4000,
  "truncated": false, "coverage": "spans|mixed|legacy",
  "nodes": [{ "id": "trace:<id>|span:<span_id>|unresolved:<parent_id>",
              "kind": "trace|span|unresolved", "label": "...",
              "component": null, "operation": null, "status": "ok|error|unset",
              "durationMs": null, "startOffsetMs": null,
              "parentSpanId": null, "instanceId": null,
              "evidenceSequences": [] }],
  "edges": [{ "id": "...", "source": "...", "target": "...",
              "kind": "parent|call|async|response", "operation": null,
              "transport": null, "durationMs": null,
              "evidenceSequences": [], "inferred": false }],
  "events": [{ "sequence": 0, "ts": 0, "level": "info",
               "component": null, "message": "...", "spanId": null }],
  "diagnostics": [{ "code": "legacy_correlation_only|unresolved_parent|no_spans|truncated",
                    "message": "...", "spanId": null }]
}
```

- A **span** is a record with a non-empty `extras.span_id`; every record is also
  an event. A **call** edge requires an explicit `extras.call` or a real
  `parent_span_id`. A missing parent becomes an `unresolved` node plus a
  diagnostic — never an invented component.
- `coverage: legacy` means zero spans: no nodes/edges, events preserved, and
  the UI must show `CAUSAL CHAIN UNAVAILABLE — legacy correlation only`.
- Bounds: 500 nodes, 1 000 edges, 2 000 events, ≤50 000 examined / 2 000 hits;
  `truncated` reports any cap.
