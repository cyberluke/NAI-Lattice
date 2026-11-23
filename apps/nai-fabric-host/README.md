# nai-fabric-host

The per-user **NAI Ambient Fabric host**. It serves NAF/1 over a Windows named
pipe and owns the local runtime: canonical ledger, hot store, CAS, AAF and the
Gas Town domain service.

## Architecture

- **Monitor process** (default): supervises exactly one worker and restarts it
  on a crash. A clean worker exit ends the monitor.
- **Worker process** (`--child`): acquires the per-user singleton, opens the
  runtime, publishes `endpoint.json`, binds the named pipe and the control HTTP
  port, and serves requests.
- **Live release swap**: the monitor watches
  `%LOCALAPPDATA%\NAI\Lattice\releases\current.json`; activating a new release
  drains the worker and respawns it from the new artifact without a reboot.

## Build

```powershell
cargo build --release -p nai-fabric-host
```

## Run

```powershell
# Monitor (recommended): spawns the --child worker and supervises it
target\release\nai-fabric-host.exe

# Worker directly (debugging only)
target\release\nai-fabric-host.exe --child

# Detached release launch (no console, survives the shell)
target\release\helldesk-release.exe launch
```

## Endpoint

The live worker publishes `%LOCALAPPDATA%\NAI\Lattice\v1\endpoint.json`:

| Field | Meaning |
|---|---|
| `canonical_pipe` | `\\.\pipe\NAI.AmbientFabric.NAF1.<sid-hash>` |
| `legacy_alias` | legacy pipe alias for older clients |
| `control_http_port` | dashboard/control port (default `8787`) |
| `pid` | live worker PID |
| `instance_id` | runtime instance identity |
| `protocol` | NAF handshake range |

The control HTTP dashboard is at `http://127.0.0.1:8787/` (loopback only;
mutations require the user-scoped control token).

## Runtime data

`%LOCALAPPDATA%\NAI\Lattice\v1\` holds the ledger, hot store, CAS, vector index,
search index and read model. Treat it as the durable source of truth; do not
delete or edit it manually.

## Product bundling

Products that ship this host must follow
[`docs/PRODUCT_BUNDLING.md`](https://github.com/cyberluke/NAI-Lattice/blob/main/docs/PRODUCT_BUNDLING.md):
bundle the exe + `host-manifest.json`, install into the shared
`runtime\<version>` root, maintain `active.json`, and verify SHA-256 plus the
Authenticode signer thumbprint before executing.

## Signing

Release binaries are signed with the NAI sovereign code-signing chain. See
[`SECURITY.md`](https://github.com/cyberluke/NAI-Lattice/blob/main/SECURITY.md)
for the current thumbprints, trust provisioning and verification commands.

## License

MIT OR Apache-2.0.
