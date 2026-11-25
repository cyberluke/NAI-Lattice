# nai-lattice-cli

Operator CLI for the local NAI Lattice runtime. It opens the same runtime as the
fabric host and provides inspection, verification, recovery and log access.

**First production integration:** [NAI OS](https://github.com/cyberluke/NAI-OS).

## Commands

| Command | Purpose |
|---|---|
| `inspect` | Print runtime health and the registered capability list |
| `verify-ledger` | Verify the canonical ledger chain and print the high-water mark |
| `rebuild [--projection hot-state]` | Rebuild projections from the ledger |
| `verify <hash>` | Verify an artifact in the CAS |
| `gastown <sub>` | Gas Town operations (sessions, tasks, reviews) |
| `log <sub>` | Tail or query the observability log over the fabric pipe |
| `maintenance <sub>` | Enterprise lifecycle: status/stop/start, `prune`, `purge`, `backup`/`restore`, `audit`, `config` |

## Maintenance lifecycle

`maintenance` is the single owner of the per-user fabric lifecycle — retention,
purge, backup/restore, cooperative host stop/start and the audit ledger. Full
policy: [`docs/MAINTENANCE.md`](../../docs/MAINTENANCE.md).

```powershell
target\release\nai-lattice-cli.exe maintenance status
target\release\nai-lattice-cli.exe maintenance prune --dry-run
target\release\nai-lattice-cli.exe maintenance prune --yes --stop
target\release\nai-lattice-cli.exe maintenance purge --scope all --yes --stop
```

`prune` is the bounded scheduled housekeeping (logs, audit, backups; opt-in
superseded runtimes). `purge --scope derived|all|legacy` is the explicit reset;
`all` backs up by default and `restore --from` is the undo path. Every accepted
action is appended to `<base>\maintenance.audit.jsonl`. See
`scripts/register-maintenance-task.ps1` for the daily schedule.

## Build

```powershell
cargo build --release -p nai-lattice-cli
```

## Run

```powershell
target\release\nai-lattice-cli.exe inspect
target\release\nai-lattice-cli.exe verify-ledger
target\release\nai-lattice-cli.exe rebuild --projection hot-state
target\release\nai-lattice-cli.exe verify <artifact-hash>
target\release\nai-lattice-cli.exe gastown --help
target\release\nai-lattice-cli.exe log --help
```

`inspect`, `verify-ledger` and `rebuild` open the runtime directly; `log`
connects to the running fabric host over the canonical named pipe.

## License

MIT OR Apache-2.0.
