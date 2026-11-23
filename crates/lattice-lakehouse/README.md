# nai-lattice-lakehouse

Parquet/Arrow lakehouse with immutable snapshot manifests and streaming delta synchronisation.

Part of the [NAI Lattice](https://github.com/cyberluke/NAI-Lattice) workspace.

**First production integration:** [NAI OS](https://github.com/cyberluke/NAI-OS)
ships the NAI Lattice fabric host as its shared per-user runtime.

## Install

```toml
[dependencies]
nai-lattice-lakehouse = "0.1"
```

## Status

Active.

## Role

Parquet/Arrow lakehouse with immutable snapshots and delta sync.

## Workspace dependencies

- `nai-lattice-core`
- `nai-lattice-aaf`
- `nai-lattice-cas`

## Links

- Repository: <https://github.com/cyberluke/NAI-Lattice>
- Product contract: [docs/PRODUCT_BUNDLING.md](https://github.com/cyberluke/NAI-Lattice/blob/main/docs/PRODUCT_BUNDLING.md)
- Security and signing: [SECURITY.md](https://github.com/cyberluke/NAI-Lattice/blob/main/SECURITY.md)
- License: MIT OR Apache-2.0
