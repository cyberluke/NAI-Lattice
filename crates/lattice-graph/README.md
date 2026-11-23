# nai-lattice-graph

Graph index and relationship queries over canonical Lattice events.

Part of [NAI Lattice](https://github.com/cyberluke/NAI-Lattice): a local-first,
event-sourced, multi-dimensional data fabric for Windows. This crate is one
building block of the workspace.

**First production integration:** [NAI OS](https://github.com/cyberluke/NAI-OS)
ships the NAI Lattice fabric host as its shared per-user runtime.

## Install

```toml
[dependencies]
nai-lattice-graph = "0.1"
```

## Status

**Scaffold.** The package name and role are reserved; the public API is not implemented yet. Do not depend on it for production.

## Role

Reserved graph index over canonical Lattice events.

## Workspace dependencies

- `nai-lattice-core`

## Links

- Repository: <https://github.com/cyberluke/NAI-Lattice>
- Product contract: [docs/PRODUCT_BUNDLING.md](https://github.com/cyberluke/NAI-Lattice/blob/main/docs/PRODUCT_BUNDLING.md)
- Security and signing: [SECURITY.md](https://github.com/cyberluke/NAI-Lattice/blob/main/SECURITY.md)
- License: MIT OR Apache-2.0
