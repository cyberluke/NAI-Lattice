# Runtime activation (manager handshake) — v1

Status: frozen for the 0.1.3 batch (2026-11-24). Owner: NAI-Lattice.
Implements R01 / NAIOS-001 without restarting NAI OS.

The control operation is owned by the **stable monitor**, not the worker: the
worker dies during the swap, so it must not own the operation or its response.
The manager (NAI OS, or the internal QA helper) writes a request file and polls a
status file. The monitor performs the swap.

This deliberately follows the shared bundling contract
(`docs/PRODUCT_BUNDLING.md`): the candidate lives in
`%LOCALAPPDATA%\NAI\Lattice\runtime\<version>\nai-fabric-host.exe` and the shared
pointer is `%LOCALAPPDATA%\NAI\Lattice\active.json`. The legacy `releases/`
layout is not used.

## Invariants

- **One serving monitor, one pipe, one writer.** The old worker is stopped before
  the candidate starts; the per-user singleton mutex is the enforcement.
- **No downgrade.** `target_version` must be newer than the active version.
- **Never claim staging as activation.** Success means the candidate answered a
  real NAF handshake + `runtime.health` with a matching identity and the endpoint
  manifest was republished.
- **10 s budget** from accepted request to verified new endpoint under an idle
  workspace. If drain/validation cannot meet it, the request fails explicitly and
  the old worker stays healthy.
- **No invented progress.** Every phase is written when it is actually reached.

## Files

All under `<base>` (`%LOCALAPPDATA%\NAI\Lattice`, or the parent of a scratch
`NAI_LATTICE_ROOT`).

### `activation.request.json` (manager → monitor)

```json
{
  "schema": 1,
  "operation_id": "act-1764000000000-abcdef",
  "staged_dir": "C:\\Users\\me\\AppData\\Local\\NAI\\Lattice\\runtime\\0.1.4-qa",
  "target_version": "0.1.4-qa",
  "exe_sha256": "<64 hex>",
  "protocol_min": 1,
  "protocol_max": 1,
  "storage_schema": 1,
  "authorization": "<contents of <base>/control.token>",
  "allow_unsigned": false,
  "requested_at_unix_ms": 1764000000000
}
```

### `activation.status.json` (monitor → manager)

```json
{
  "schema": 1,
  "operation_id": "act-...",
  "phase": "idle|validating|draining|starting|verifying|committed|rolled_back|failed|busy",
  "from_version": "0.1.3",
  "to_version": "0.1.4-qa",
  "detail": "candidate endpoint verified",
  "started_at_unix_ms": 1764000000000,
  "updated_at_unix_ms": 1764000006000,
  "error": null,
  "endpoint": {
    "pid": 1234,
    "canonical_pipe": "\\\\.\\pipe\\NAI.AmbientFabric.NAF1.1c840f",
    "instance_id": "nai-lattice-..."
  }
}
```

A new request is refused with `phase:"busy"` while a journal exists.

### `activation.journal.json` (crash recovery)

```json
{
  "schema": 1,
  "operation_id": "act-...",
  "phase": "draining|starting|verifying",
  "from_exe": "...\\runtime\\0.1.3\\nai-fabric-host.exe",
  "to_exe": "...\\runtime\\0.1.4-qa\\nai-fabric-host.exe",
  "from_version": "0.1.3",
  "to_version": "0.1.4-qa",
  "active_json_before": "{...verbatim...}",
  "started_at_unix_ms": 1764000000000
}
```

Written before the old worker is drained; removed only after the status file is
durable. On monitor start, an existing journal is resolved: if the candidate
endpoint is healthy and committed, finish; otherwise roll back to `from_exe` and
write `rolled_back`.

## Validation (before any mutation)

1. `schema == 1`; `operation_id` non-empty; no live journal.
2. `staged_dir` canonicalizes under `<base>\runtime`; directory name equals
   `target_version`; no traversal.
3. `<staged_dir>\nai-fabric-host.exe` exists; its SHA-256 equals `exe_sha256`
   **and** the `host-manifest.json` sha (when present).
4. `host-manifest.json` signature policy: `signature_status` must be `"Signed"`
   (the canonical producer value) or the legacy Authenticode `"Valid"` unless
   `allow_unsigned` is true (QA only). A mismatch is a hard failure.
5. Protocol range covers the current host (1..=1) and `storage_schema` is the
   supported schema (1).
6. `authorization` equals `<base>\control.token` (per-user secret; file ACL is the
   boundary). A wrong token is `failed`, never queued.
7. `target_version` is strictly newer than the active version.

## Phases

`validating` → `draining` → `starting` → `verifying` → `committed`

- **draining:** acquire the maintenance lease (owner `nai-fabric-host-activator`,
  TTL 10 min) and call `runtime.shutdown_if_idle` on the current pipe. A `busy`
  or `already_stopping` response ends as `busy`/`failed`; the old worker stays.
  Wait for the worker process to exit (clean exit 0). Any other exit aborts.
- **starting:** spawn `to_exe` with the same worker arguments the monitor uses.
- **verifying:** poll for a new `endpoint.json` (new pid/instance) and a NAF
  handshake + `runtime.health` on the new pipe. Identity must match; protocol and
  storage schema must be compatible. Deadline 10 s total.
- **committed:** under the manager mutex, atomically rewrite `active.json`
  (same schema; version, hash, path, installed timestamp) and preserve the prior
  as `<base>\previous.json`; republish `activation.status.json`; remove the
  journal; release the lease.
- **rollback:** on any error after `draining`, spawn `from_exe`, verify the old
  endpoint, restore `active.json` verbatim from the journal, write
  `phase:"rolled_back"` with the diagnostic, remove the journal, release the
  lease. Never delete the previous runtime directory.

## CLI/QA helper

The internal QA helper (`-StagedDir <dir> -Version <v> [-AllowUnsigned]`) writes
the request with a fresh operation id and polls the status until a terminal
phase, printing the JSON. QA only: it is not shipped to products. It resolves the
base like the host (`-Base`, else the parent of `NAI_LATTICE_ROOT`, else
`%LOCALAPPDATA%\NAI\Lattice`), reads `<base>\control.token` without printing it,
and exits 0 only for `committed` (2 for `rolled_back`/`failed`/`busy`, 3 for a
poll timeout).

## Out of scope

- Production activation of a signed candidate (release authorization, V02/H02).
- Cross-process activation from a worker that lacks this monitor (0.1.1/0.1.2
  need the one legacy restart; from 0.1.3 onwards the handshake works).

## Implementation notes (0.1.3 host lane; reported deviations, frozen schema unchanged)

These are the points where the frozen text above could not be implemented
literally without contradicting an existing frozen contract. They are recorded
here instead of silently diverging:

1. **Lease order is drain → lease → candidate, not lease → drain.**
   NAIOS-006 makes a *live* maintenance lease a hard startup refusal for the
   worker, and `runtime.health` reports `maintenance` under that lease, which the
   NAF client correctly refuses. Acquiring the lease before the drain therefore
   made both the old handshake and the candidate startup fail. The host now:
   validates, writes the journal, drains the healthy old worker, takes the lease
   while the old worker is stopping, releases it immediately before starting the
   candidate, and commits under the manager mutex. Crash recovery force-releases
   a lease left by this operation (owner + operation id in the reason; the
   fencing token is never written to the journal).
2. **`signature_status` accepts both contracts.** The canonical
   `packaging/bundle-host-for-product.ps1` now writes `Signed` for a signed exe;
   older manifests used the Authenticode status `Valid`. The host treats **both**
   `"Signed"` and `"Valid"` as signed, so canonical and legacy bundles are
   accepted. `-AllowUnsigned` remains the QA escape hatch.
3. **A second request during our own in-flight operation is queued, not answered
   with `busy`.** Writing `busy` to the single status file would overwrite the
   in-flight operation's phase and make its manager lose the response. While this
   monitor is executing an accepted request, a newer request is left on disk and
   processed after the terminal status. A request that finds a *foreign* journal
   while no operation is in flight is answered `busy` without touching that
   journal.
4. **`active.json` gains `path`.** The frozen "committed" step names version,
   hash, path, and installed timestamp. The host preserves every existing field,
   sets `version`, `sha256`, `path` (absolute candidate exe), and
   `installed_at_unix_ms`, and copies `source`/`git` from `host-manifest.json`
   when present. `previous.json` is the exact prior `active.json` text.
5. **An isolated instance must also set `NAI_LATTICE_ROOT`.** Otherwise its
   monitor would poll the production base and could rewrite production
   `active.json`; activation is disabled for that configuration (the worker still
   runs). The QA layout is `<base>/v1` as `NAI_LATTICE_ROOT`, so `<base>` is the
   runtime root's parent.
6. **Manager mutex naming.** Production uses the shared
   `Local\NAI.Lattice.Host.Manager.v1`; an isolated instance appends
   `.<instance>` so QA activation never blocks a live product manager or vice
   versa.
7. **Recovery adopts a healthy worker.** If a monitor restart finds a healthy
   candidate (or a still-healthy previous worker), it commits/announces the
   recovery and *polls the existing process* instead of spawning a duplicate that
   would lose the singleton and end the monitor. A crash-recovered candidate must
   still be `Signed`/`Valid` and hash-match its manifest because the original request's
   `allow_unsigned` is not in the frozen journal; an unsigned QA candidate is
   rolled back rather than finished.
8. **The 10 s budget is measured from request acceptance** (validation, drain,
   start, and verification all count toward it). A drain that accepts but does
   not exit in time is reported `failed` while the journal is retained, so the
   eventual clean exit is respawned rather than treated as a stop.
9. **Rollback restores `active.json` before spawning the previous artifact**
   (the document lists spawn → verify → restore). Restoring first means a crash
   inside the rollback window cannot leave the pointer at a candidate that was
   never committed; the previous worker is still verified before `rolled_back`
   is published.
