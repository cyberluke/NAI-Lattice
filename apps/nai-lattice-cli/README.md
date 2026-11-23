# nai-lattice-cli

Operator CLI for the local NAI Lattice runtime. It opens the same runtime as the
fabric host and provides inspection, verification, recovery and log access.

## Commands

| Command | Purpose |
|---|---|
| `inspect` | Print runtime health and the registered capability list |
| `verify-ledger` | Verify the canonical ledger chain and print the high-water mark |
| `rebuild [--projection hot-state]` | Rebuild projections from the ledger |
| `verify <hash>` | Verify an artifact in the CAS |
| `gastown <sub>` | Gas Town operations (sessions, tasks, reviews) |
| `log <sub>` | Tail or query the observability log over the fabric pipe |

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
