# ADR-001 — SSE subscriber cap and shared scan broker

- **Status:** accepted for the 0.1.3 preview; NAI OS integration must re-measure
- **Date:** 2026-11-25
- **Owner:** NAI-Lattice (transport)
- **Supersedes:** the legacy per-connection scanner model and its hard cap of six
- **Contract:** `docs/DASHBOARD_API.md` §4, T01/T02/T05, G03

## 1. Context

The 0.1.1 dashboard opened one ledger scanner thread per `/lattice/events`
connection and enforced `MAX_SSE_CLIENTS = 6`. Six was a connection cap, not a
measured resource model: each client paid for its own scanner, cursor and
buffer, so the only safe limit was the one originally tested.

The 0.1.3 host replaces that model with one process-wide broker
(`apps/nai-fabric-host/src/lattice_broker.rs`):

- one background pump thread ticks every `TICK = 1250 ms`;
- one bounded `logs_after(min_cursor, 4096)` ledger read serves the whole
  subscriber set per tick;
- each subscriber owns a `sync_channel(256)` so a slow consumer never blocks
  the pump or another subscriber;
- the initial snapshot is the only per-subscriber ledger read (bounded to 4096
  records; a cursor-less first connect gets the newest 200);
- `stats` is computed once per four ticks and fanned out;
- a connection has a hard 600 s lifetime and resumes with `Last-Event-ID`
  (or `?after=<seq>`);
- over the cap the route answers `503` with `Retry-After: 3`; it never leaves a
  silently dead stream.

The relevant question is therefore no longer "how many scanner threads?" but
"how many bounded HTTP subscribers can one pump and its queues hold without
changing the resource model?"

## 2. Measured model

The only measured multi-client run in this batch is the isolated QA gate:

```text
node scripts/qa/sse-load.mjs http://127.0.0.1:8793 12 6
```

Result (recorded in the internal engineering status ledger §T05 and the
verification log):

| Measurement | Result |
|---|---|
| Load clients | 12 |
| Already-connected browser subscriber | 1 |
| Total subscribers | 13 |
| Broker scans during the 6 s window | 5 |
| Ticks in the window (`ceil(6000 / 1250)`) | 5 |
| Dropped records | 0 |
| Client errors | 0 |
| Delivered `data:` lines | 96 |

`brokerScansTotal` advanced 27 → 32: exactly one scan per tick for all 13
subscribers. That is the property the gate exists to prove. It is **not** a
24-subscriber stress test and it does not cover the full T05/G03 matrix (§6).

## 3. Decision

1. **24 physical subscribers per host process.** A physical subscriber is one
   accepted HTTP `GET /lattice/events` connection. The cap is enforced by the
   broker's subscriber gauge and by the registry check, so it cannot be exceeded
   by a race between `at_capacity()` and `subscribe()`.
2. **One shared scan pump per process.** No Vue component, panel, or logical
   subscription owns a ledger lease or a scanner thread. Additional panels use
   the shell's existing `TelemetryFeed` event source.
3. **Bounded per-subscriber queue.** `sync_channel(256)` messages. The pump uses
   `try_send` and never blocks.
4. **Explicit refusal over the cap.** `503 Service Unavailable` with
   `Retry-After: 3` and `Connection: close`. No silent stream, no fake live
   fallback.
5. **No per-panel upstream leases.** Logical panel subscriptions are local
   selectors over the shared feed; duplicating, moving, or re-docking a panel
   must not create another `/lattice/events` connection.
6. **No SharedWorker in this batch.** Each dashboard page, browser tab, or
   WebView2 view is one physical subscriber. Cross-window multiplexing is not
   assumed; a future implementation must be validated in the real WebView2
   profile before it is claimed.

The limit is a **physical** cap, not a logical-panel cap. `maxSseClients` is
reported as 24 for compatibility with the old counter name; `subscribers` is the
broker gauge and is the authoritative number for the shared pump.

## 4. Consequences

### 4.1 Resource cost

Per connection the host pays for:

- one HTTP connection thread and its socket buffers;
- one 256-message `SyncSender` queue (messages, not records; a batch is one
  message);
- one subscriber registry entry and cursor.

Per process the host pays for one pump thread and one bounded ledger read per
tick. The 24 cap bounds the connection-side cost; it does not make the ledger
read cost depend on the number of subscribers.

The initial snapshot is a per-connection read of at most 4096 records. A burst of
24 simultaneous first connects can therefore cause up to 24 bounded snapshot
reads before the shared ticks begin. That cost is bounded but has not been
measured; it is part of the NAI OS re-measurement.

### 4.2 Dropped-event semantics

If a subscriber's queue is full when a delta is ready:

- the pump skips that subscriber's **whole delta** for the tick;
- the skipped record count is added to `droppedTotal`;
- the subscriber's cursor advances to the last sequence in the skipped batch, so
  one slow consumer cannot make the pump re-read the same records forever;
- the next `status` event reports the new cursor and `droppedTotal`;
- the client renders a visible gap and resumes from its last actually delivered
  `id:` on reconnect;
- the server does not replay the skipped batch from an in-memory buffer. Retained
  history is available through `/lattice/api/query`; the client must not present
  the gap as complete data.

This is deliberate: the alternative (blocking the pump or dropping individual
records silently) would let one panel stall every other panel. The cost is that
a slow consumer can lose a batch and must reconcile via the query path.

### 4.3 Monitoring contract

`GET /lattice/api/summary` exposes:

| Field | Unit / semantics |
|---|---|
| `stream.activeSseClients` | gauge: accepted `/lattice/events` connections |
| `stream.maxSseClients` | constant: current physical cap (24) |
| `stream.subscribers` | gauge: broker-registered subscribers |
| `stream.brokerScansTotal` | monotonic since host start: one per pump tick with subscribers |
| `stream.droppedTotal` | monotonic records skipped by full queues |
| `stream.pushTotal` | monotonic successful SSE data-event writes (not heartbeats, not UI ticks) |
| `stream.lastScanUnixMs` | wall-clock timestamp of the last pump scan |

`activeSseClients` and `subscribers` can differ transiently while a connection is
opening or tearing down. Neither is a count of panels. `brokerScansTotal` is a
counter, not a rate; compare two samples over a known interval.

### 4.4 Idle behavior

With no subscribers the pump returns before reading the ledger and before
dispatching `runtime.health`. It does not advance its tick counter. An idle host
does not pay for SSE.

## 5. Limits and open measurements

The measured model supports 13 subscribers. It does **not** yet support these
claims:

- 24 concurrent subscribers (the cap is a safety bound, not a tested capacity);
- 12 simultaneous modules plus duplicates and three independent same-profile
  views;
- a separate WebView2 profile or a cross-window SharedWorker;
- 100 open/close cycles without leaks;
- a killed/reconnected broker, a maintenance switch, or a slow subscriber under
  sustained pressure beyond the unit test;
- 1 000 events/s for 10 minutes, a 10 000/s burst, or the 100 000-record
  historical query;
- RSS/heap/thread counts before and after closing all modules.

Until those are measured, the 24 cap must not be cited as T05/G03 acceptance.
The NAI OS integration must repeat the gate against the real WebView2 window
model and record the same counters.

## 6. Rollback

If the measured model fails, restore `MAX_SUBSCRIBERS = 6` in
`apps/nai-fabric-host/src/lattice_broker.rs`. The shared pump, queue, refusal
status and client resume semantics remain unchanged; only the cap and the
reported `maxSseClients` change. The cap is a compile-time constant in this
batch, so rollback requires a rebuild and a normal host activation. No protocol
or storage migration is involved.

A future runtime-configurable cap must still enforce the same refusal and
monitoring contract; it must not silently accept connections over the effective
limit.

## 7. Status

**Accepted for the 0.1.3 preview.** The deviation from six is justified only by
the measured shared-broker model above, not by the fact that
`MAX_SSE_CLIENTS = 24` avoids a connection error. NAI OS integration must
re-measure before this ADR is treated as accepted for production.

## 8. References

- `apps/nai-fabric-host/src/lattice_broker.rs` — cap, tick, queue, drop, stats.
- `apps/nai-fabric-host/src/lattice_dashboard.rs` — `lattice_sse`, `503`,
  `SseLease`, stream counters.
- `docs/DASHBOARD_API.md` §4 — SSE contract.
- `scripts/qa/sse-load.mjs` — gate (1..23 load clients; one browser subscriber
  makes 24 the maximum testable total).
- Internal engineering status ledger §T05 and the verification log — recorded
  13-subscriber run.
