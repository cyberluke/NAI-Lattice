# Lattice maintenance lifecycle

The per-user Lattice fabric is shared by every NAI product on the workstation.
Its lifecycle therefore has one owner — **NAI-Lattice** — and one operator
surface: `nai-lattice-cli maintenance`. Products may install and run the host,
but they must never purge, rotate, or "clean up" shared state on their own.

## 1. What lives where

| Path | Class | Authority |
|---|---|---|
| `<root>\ledger\` | **canonical** | Hash-chained event log. Source of truth for every projection. |
| `<root>\cas\` | **canonical** | Content-addressed artifacts referenced by ledger events. |
| `<root>\hot\`, `<root>\search\`, `<root>\vector\`, `read-model.json`, `multi-dim-index.json` | **derived** | Rebuildable projections. Safe to reset; not safely pruned by age. |
| `<root>\*.log`, `host-restart.log`, `*.tmp`, `*.partial` | **state** | Operational noise with a retention window. |
| `<root>\endpoint.json` | **state** | Re-published by the host on start. |
| `<base>\maintenance.json`, `maintenance.audit.jsonl` | **lifecycle** | Policy + append-only audit ledger. Survives a data purge. |
| `<base>\backups\` | **lifecycle** | Backup directories + manifests. |
| `<base>\runtime\<version>\` | **shared runtime** | Owned by the product runtime contract (`docs/PRODUCT_BUNDLING.md`). |
| `<base>\active.json` | **shared runtime** | Active-runtime pointer. Never touched by maintenance. |
| `<base>\releases\`, `<base>\legacy\` | **HELLDESK legacy** | Only via the HELLDESK owner's process (see §7). |

`<base>` = `%LOCALAPPDATA%\NAI\Lattice`; `<root>` = `<base>\v1`. The CLI derives
both from the canonical `RuntimeConfig` (honoring `NAI_LATTICE_ROOT`), so it
cannot be pointed at an arbitrary tree.

## 2. Retention policy

Defaults live in `<base>\maintenance.json`; CLI flags override the file for one
run.

```json
{
  "logs_days": 30,
  "audit_days": 90,
  "backup_days": 14,
  "runtime_keep": 2,
  "prune_runtimes": false,
  "auto_prune": true
}
```

* **Logs** older than `logs_days` are deleted by `prune`.
* **Audit records** older than `audit_days` are removed; unparseable lines are
  never silently discarded.
* **Backups** older than `backup_days` are deleted by `prune`.
* **Runtimes**: the active version is never removed. Pruning superseded runtime
  directories is **opt-in** (`prune_runtimes` or `--prune-runtimes`) because the
  product contract requires shared-runtime collection to be reference-counted
  across installed products; there is no product registry yet, so the operator
  confirms before it runs. `runtime_keep` non-active newest versions are kept.
* **Canonical ledger and CAS are never partially pruned.** Deleting sealed
  segments by age would break chain verification and artifact references. Use
  `purge --scope all` (backup by default) for a deliberate reset.
* **Derived stores are never partially deleted by age.** They are reset whole by
  `purge --scope derived`, and rebuilt from the ledger on demand.

## 3. Commands

```powershell
# Inventory: host health, endpoint, active runtime, sizes, retention, audit tail.
nai-lattice-cli maintenance status [--json]

# Cooperative host lifecycle (NAF control capability, see §4).
nai-lattice-cli maintenance stop  [--timeout 30]
nai-lattice-cli maintenance start [--host-exe <path>] [--timeout 30]

# Bounded housekeeping. Dry-run first; --yes to apply.
nai-lattice-cli maintenance prune --dry-run
nai-lattice-cli maintenance prune --yes --stop          # stops + restarts host
nai-lattice-cli maintenance prune --scheduled --yes --stop  # honors auto_prune

# Explicit resets.
nai-lattice-cli maintenance purge --scope derived --dry-run
nai-lattice-cli maintenance purge --scope derived --yes --stop
nai-lattice-cli maintenance purge --scope all --yes --stop    # backup by default
nai-lattice-cli maintenance purge --scope all --yes --no-backup --stop

# Safety net.
nai-lattice-cli maintenance backup [--to <dir>]
nai-lattice-cli maintenance restore --from <backup-dir> --yes --stop

# Policy + audit.
nai-lattice-cli maintenance config [--init] [--json]
nai-lattice-cli maintenance audit  [--limit 20]
```

Mutating commands refuse to run while the host is running unless `--stop` is
passed; with `--stop` the host is stopped cooperatively, the maintenance runs,
and the host is restarted (`--no-restart` leaves it down) — including when the
maintenance itself fails. The scheduled task is
`prune --scheduled --yes --stop`; `--scheduled` makes the run a no-op when
`auto_prune` is `false`.

**Host identity.** Mutating commands resolve the pipe from
`<runtime root>\endpoint.json`, never from the per-user default, and verify the
running host's `runtime.describe.root` before stopping it. A scratch
`NAI_LATTICE_ROOT` drill therefore cannot stop the production host.

**Maintenance lease.** Mutating commands take
`<runtime root>\maintenance.lock.json` for the whole operation (owner
`nai-lattice-cli`, 15 min TTL, fencing token). The lease file is excluded from
purge/restore/backup so the holder cannot delete its own lock; a crashed run is
recovered by TTL expiry.

**Backups.** `--to` must be outside the runtime root and empty/non-existent.
`restore --from` requires a full backup (`"scope":"all"`) whose data tree matches
the manifest file/byte counts; a derived/legacy backup is refused. A
`pre-restore` full backup is taken before the root is cleared.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | Success (including a dry-run or "already stopped/running"). |
| 2 | Usage error or unknown option (including `prune` without `--yes`). |
| 3 | Host busy / refused the stop / did not stop / serves a different root / legacy host. |
| 4 | Unsafe request (no `--yes`, backup inside the data root, non-empty destination, non-full restore, lease held, active runtime unreadable). |
| 5 | I/O or other failure. |

## 4. Cooperative stop/start contract

The host advertises and serves `runtime.shutdown_if_idle` over the canonical
NAF pipe:

1. The request is **idle-gated**: it is accepted only when it is the sole
   in-flight dispatch. A busy host returns `{"accepted":false,"reason":"busy"}`
   and is **not** stopped.
2. On acceptance the host flushes the hot store, acknowledges, and exits the
   worker with code 0 after a short grace period.
3. The monitor treats a clean worker exit as a requested stop and ends itself;
   it does not respawn into a maintenance window.
4. `maintenance start` relaunches the monitor (`DETACHED_PROCESS`), then waits
   for the pipe + `runtime.health`.

Hosts that predate this capability reject the handshake with
`capability not offered: runtime.shutdown_if_idle`. The CLI reports that
explicitly (exit 3) instead of pretending to stop. Restart NAI OS (or the
product supervisor) once to activate the current runtime; after that the
lifecycle is self-contained.

Do not run two maintenance operations concurrently. `prune`/`purge`/`restore`
take the host down; the scheduled task uses `-MultipleInstances IgnoreNew`, and
the CLI is short-lived by design.

## 5. Backups, restore and audit

A backup is a directory `<base>\backups\<UTC stamp>-<label>\` containing
`manifest.json` and `data\` (for `--scope all`, the whole runtime root). The
manifest records scope, source root, file/byte counts, CLI version, the active
runtime pointer and the endpoint snapshot.

`purge --scope all` backs up **by default** (use `--no-backup` only when the
data is disposable). `restore --from <dir> --yes` requires a full backup
(`"scope":"all"`) whose `data\` matches the manifest file/byte counts, refuses a
source/destination overlap, takes a `pre-restore` full backup, then stops the
host, clears the runtime root and restores `data\` exactly. A derived or legacy
backup is refused — it does not contain the canonical ledger/CAS.

Every accepted action appends one JSON line to
`<base>\maintenance.audit.jsonl`:

```json
{"ts_ms":1795545725000,"ts_utc":"2026-11-24T18:42:05Z","action":"purge",
 "result":"ok","cli_version":"0.1.3",
 "detail":{"scope":"all","items":9,"bytes":123456789,"backup":"...","host_stopped":true}}
```

The audit ledger sits outside the runtime root, so a full data purge cannot
erase the record of that purge. Scheduled pruning also rotates the audit itself
(`audit_days`).

## 6. Scheduling

Register a per-user daily prune (runs only while the user is logged on, because
the fabric host is per-user):

```powershell
pwsh -File .\scripts\register-maintenance-task.ps1
# ... or: -Time 04:30 -CliPath C:\tools\nai-lattice-cli.exe
# remove: -Unregister
```

Recommended policy:

* **Daily**: `prune --scheduled --yes --stop` (logs/audit/backups; restarts the
  host). `auto_prune: false` turns the scheduled run into a no-op.
* **Weekly**: review `maintenance status` and the audit tail.
* **Monthly**: verify one backup (manifest + restore into a scratch
  `NAI_LATTICE_ROOT`) and review disk usage.
* **Purge**: never scheduled. It is an operator action under change control.

## 7. Multi-product rules

* `active.json` and `runtime\` are **not** maintenance-owned. The CLI never
  deletes `active.json`; runtime pruning is opt-in and never removes the active
  version.
* A product uninstall removes only its own package. It never runs a purge.
* `purge --scope legacy` covers HELLDESK's `releases\`/`legacy\` artifacts.
  Product code must not delete these; run it only in coordination with the
  HELLDESK owner. Optional `releases\current.json` is legacy/migration state and
  is not written by maintenance.
* Data reset: `purge --scope all` stops the host, backs up, removes the runtime
  root contents, and restarts the host. Products observe the normal "missing
  data → rebuild" path; no product-specific cleanup is required.

## 8. Full-reset runbook (change control)

1. Announce the maintenance window; no product should be mid-append.
2. `maintenance status --json` — record sizes, active runtime, host pid.
3. `maintenance purge --scope all --dry-run` — review the exact targets.
4. `maintenance purge --scope all --yes --stop` — stops, backs up, purges,
   restarts.
5. `maintenance status` — confirm the host is healthy and the active runtime is
   unchanged.
6. Verify provenance (`active.json` sha/version) and that a new audit record
   exists with `scope: "all"` and the backup path.
7. Retain the backup per policy; if the reset was accidental, `restore --from
   <backup> --yes --stop`.

## 9. Troubleshooting

| Symptom | Meaning / action |
|---|---|
| `cooperative stop unavailable ... capability not offered` | Running host predates `runtime.shutdown_if_idle`. Restart NAI OS/its supervisor once; the new runtime serves it. |
| `host refused the stop: busy` | Real work in flight. Retry when idle; never force-kill (the ledger has crash recovery, but a clean stop is the contract). |
| `the fabric host is running` (purge/prune without `--stop`) | Stop it first or pass `--stop` (the command restarts it afterwards). |
| Purge deleted derived stores | Expected: they rebuild from the ledger on the next start. Run `maintenance status` and wait for the projection worker. |
| `refusing to purge without --yes` | Destructive commands are intentionally gated. Preview with `--dry-run`. |
| Backup destination rejected | `backup --to` refuses destinations inside the runtime root. Choose a separate volume/directory. |
| Disk full during backup | The command fails before deleting anything; free space and retry. `purge` never deletes before its backup completes. |
