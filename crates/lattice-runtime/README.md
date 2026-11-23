# nai-lattice-runtime

NAI Lattice runtime: wires ledger + hotstore + CAS + AAF and dispatches NAF/1 capabilities.

Part of [NAI Lattice](https://github.com/cyberluke/NAI-Lattice): a local-first,
event-sourced, multi-dimensional data fabric for Windows. This crate is one
building block of the workspace.

## Install

```toml
[dependencies]
nai-lattice-runtime = "0.1"
```

## Status

Active.

## Role

Composition root: wires ledger, hotstore, CAS, AAF and NAF dispatch.

## Workspace dependencies

- `nai-lattice-core`
- `nai-lattice-protocol`
- `nai-lattice-ledger`
- `nai-lattice-cas`
- `nai-lattice-hotstore`
- `nai-lattice-hotstore-surrealkv`
- `nai-lattice-aaf`
- `nai-lattice-vector`
- `nai-lattice-search`

## Links

- Repository: <https://github.com/cyberluke/NAI-Lattice>
- Product contract: [docs/PRODUCT_BUNDLING.md](https://github.com/cyberluke/NAI-Lattice/blob/main/docs/PRODUCT_BUNDLING.md)
- Security and signing: [SECURITY.md](https://github.com/cyberluke/NAI-Lattice/blob/main/SECURITY.md)
- License: MIT OR Apache-2.0
