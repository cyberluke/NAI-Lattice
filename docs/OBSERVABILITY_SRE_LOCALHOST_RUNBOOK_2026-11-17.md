# NAI Lattice — local SRE operations and release gate

Source patch: [NAI Lattice PR #1](https://github.com/cyberluke/NAI-Lattice/pull/1). Consumer patch:
[NAI OS PR #7](https://github.com/cyberluke/NAI-OS/pull/7).
**Draft changes, no CI/CD, not Windows runtime verified or deployed.**

## Data plane

NAI OS logger, per-widget console bridge and N-Pilot sidecar logs are captured locally.
The canonical persisted stream is observability.log.v1, with trace_id, component,
level, event_type, caller, measured Unix timestamps and a process-local producer_seq.
The emitter batches onto the per-user NAF/1 named pipe. Producers enqueue to a
bounded in-memory ring, not synchronous NAF calls. If the fabric host stops, the
flusher retries on the same Tokio Windows IOCP reactor using fresh handshakes.

**No remote telemetry.** Logs remain on the machine unless the user explicitly
exports them. Message contents may nonetheless be sensitive and need redaction.

## Host and dashboard

- Control HTTP: 127.0.0.1 at host-selected GASTOWN_CTRL_PORT, default 8787.
- The host writes its actual control_http_port into per-user endpoint.json.
- NAI OS toolbar command reads the manifest port, checks loopback connectivity,
  then opens the existing Lattice dashboard WebView2 window or focuses it.
- Routes: GET /lattice (SRE UI), GET /lattice/icon (branding),
  GET /lattice/api (bounded search), GET /lattice/events (cursor-resumable SSE).
- Browser maintains exactly one EventSource; native auto-reconnection uses
  Last-Event-ID. The host bounds to six SSE workers and 64 total HTTP workers.
- Queries: component, minimum level, exact trace ID, text match; max 500 hits.
  Browser retains at most 1400 rows. All log text is assigned via textContent,
  never parsed as raw HTML.
- Ingestion health is real: every ten seconds NAI OS emits the current
  queued/dropped/flushed counters. Missing data is UNKNOWN, stale is STALE,
  dropped records are a real warning, not a synthetic success indicator.
- SSE failures and write timeout tear down the worker. A host outage may leave
  a bounded backlog, with explicitly counted losses at capacity.

## Local verification matrix (manual, not CI/CD)

1. In the NAI-Lattice working tree:
   cargo test -p lattice-observability
   cargo check -p nai-fabric-host
2. In the sibling NAI-OS working tree:
   cargo check --manifest-path src/Cargo.toml
   npm run type-check
   (Use project-specific sidecar TypeScript build scripts.)
3. Launch the fabric; inspect the actual endpoint manifest, not guessed
   environment variables or private pipe naming conventions.
4. Open Fancy Toolbar LATTICE button. Confirm real event searches, trace
   correlation, user-entered UTF-8 terms, actual queue metrics and precise
   report of an unavailable host.
5. Force 100 browser disconnects/reloads. Confirm no more than six concurrent
   SSE sessions, Last-Event-ID progression, no cursor-reset duplicate storm.
6. Flood 1000 log records/sec with identical messages and same-ms events.
   Check unique record identities, bounded queue, explicit drop counter.
7. Kill and restart the host mid-append. Log producers MUST continue without
   blocking. The sink must reconnect on the same async IOCP reactor and
   replay retained batches without misreading stale responses.
8. Verify CPU and memory remain bounded during 5 minutes of host outage.
   Validate the N-Pilot sidecar process count does not grow in parallel.
9. Restrict sensitive log data and test cross-user READ access. Read
   authentication is NOT yet implemented on these local HTTP routes.

## Security limitations and release ruling

Loopback binding and Host/Origin validation are useful but not equivalent to
Windows SID authorization. Another local Windows process can call HTTP
read routes. Do not expose the service over 0.0.0.0, external networking,
a public proxy or port forwarding. Before enterprise production, introduce
a narrow read-only capability bound to the correct Windows SID, no token in
URLs and no control-plane write token inside arbitrary WebView content.
The local control server's general management endpoints already use Bearer
authorization, but this DOES NOT protect log READ routes.

Only promote these draft PRs after local Windows compilation, runtime
performance/error recovery evidence, authorization negative tests, privacy
redaction and a reproducible versioned release. No claim of success from
a past agent narrative substitutes for these gates.
