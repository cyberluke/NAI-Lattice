# nai-lattice-embeddings

AAF-federated embeddings and rerank provider backed by the LUSI NPU gateway.

Part of the [NAI Lattice](https://github.com/cyberluke/NAI-Lattice) workspace.

**First production integration:** [NAI OS](https://github.com/cyberluke/NAI-OS)
ships the NAI Lattice fabric host as its shared per-user runtime.

## Install

```toml
[dependencies]
nai-lattice-embeddings = "0.1"
```

## Status

Active.

## Role

Federated embeddings and rerank provider (LUSI NPU gateway).

## Workspace dependencies

- `nai-lattice-core`
- `nai-lattice-vector`

## Links

- Repository: <https://github.com/cyberluke/NAI-Lattice>
- Product contract: [docs/PRODUCT_BUNDLING.md](https://github.com/cyberluke/NAI-Lattice/blob/main/docs/PRODUCT_BUNDLING.md)
- Security and signing: [SECURITY.md](https://github.com/cyberluke/NAI-Lattice/blob/main/SECURITY.md)
- License: MIT OR Apache-2.0
