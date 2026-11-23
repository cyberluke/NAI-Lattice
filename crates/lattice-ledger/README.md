# nai-lattice-ledger

Durable, replayable canonical event ledger: segments, BLAKE3 chains, crash recovery.

Part of [NAI Lattice](https://github.com/cyberluke/NAI-Lattice): a local-first,
event-sourced, multi-dimensional data fabric for Windows. This crate is one
building block of the workspace.

**First production integration:** [NAI OS](https://github.com/cyberluke/NAI-OS)
ships the NAI Lattice fabric host as its shared per-user runtime.

## Install

```toml
[dependencies]
nai-lattice-ledger = "0.1"
```

## Status

Active.

## Role

Durable, replayable event ledger with BLAKE3 chains and crash recovery.

## Workspace dependencies

- `nai-lattice-core`
- `nai-lattice-protocol`

## Links

- Repository: <https://github.com/cyberluke/NAI-Lattice>
- Product contract: [docs/PRODUCT_BUNDLING.md](https://github.com/cyberluke/NAI-Lattice/blob/main/docs/PRODUCT_BUNDLING.md)
- Security and signing: [SECURITY.md](https://github.com/cyberluke/NAI-Lattice/blob/main/SECURITY.md)
- License: MIT OR Apache-2.0
