# nai-lattice-aaf

Ambient Asset Federation resolver: reuse compatible existing bytes instead of copying or downloading.

Part of [NAI Lattice](https://github.com/cyberluke/NAI-Lattice): a local-first,
event-sourced, multi-dimensional data fabric for Windows. This crate is one
building block of the workspace.

## Install

```toml
[dependencies]
nai-lattice-aaf = "0.1"
```

## Status

Active.

## Role

Federation resolver: reuse compatible bytes before acquiring new ones.

## Workspace dependencies

- `nai-lattice-core`
- `nai-lattice-cas`

## Links

- Repository: <https://github.com/cyberluke/NAI-Lattice>
- Product contract: [docs/PRODUCT_BUNDLING.md](https://github.com/cyberluke/NAI-Lattice/blob/main/docs/PRODUCT_BUNDLING.md)
- Security and signing: [SECURITY.md](https://github.com/cyberluke/NAI-Lattice/blob/main/SECURITY.md)
- License: MIT OR Apache-2.0
