# @cyberluke/mcp-nai-lattice

MCP server for NAI-Lattice: read-only semantic search over the local Qdrant code
index, plus the shared task ledger, mind-map feed, company policy and the
fabric's content-addressed artifact store.

**First production integration:** [NAI OS](https://github.com/cyberluke/NAI-OS)
ships the NAI Lattice fabric host as its shared per-user runtime.

## Install

```bash
npm install -g @cyberluke/mcp-nai-lattice
# or run it from a clone:
npm ci && npm run build -w @cyberluke/mcp-nai-lattice
```

Version 0.2.0 adds the generic artifact tools and is published on npm as
`@cyberluke/mcp-nai-lattice@0.2.0`.

The server speaks MCP over stdio:

```json
{
  "mcpServers": {
    "nai-lattice": {
      "command": "mcp-nai-lattice"
    }
  }
}
```

## Project registry

`list_indexed_projects` fuses live Qdrant collections with a **local,
machine-specific** registry. It is not shipped in the package and is ignored by
git. Copy the template and edit the paths:

```bash
cp projects.example.json projects.json
# or point the server at your own file:
#   REGISTRY_PATH=D:\path\to\projects.json
```

## Configuration

All settings are optional; the defaults target a local machine:

| Variable | Default | Purpose |
|---|---|---|
| `REGISTRY_PATH` | `<package>/projects.json` | Project registry |
| `LM_STUDIO_URL` | `http://127.0.0.1:1234/v1` | OpenAI-compatible embeddings endpoint |
| `EMBEDDING_MODEL` | `text-embedding-nomic-embed-text-v1.5` | Embedding model |
| `EMBEDDING_DIM` | `768` | Embedding dimension |
| `QDRANT_URL` | `http://localhost:6333` | Qdrant endpoint |
| `QDRANT_API_KEY` | unset | Optional Qdrant API key |

Semantic search is read-only: the server never writes to Qdrant and never
re-embeds the index. Task-ledger writes go through the `todo_*` tools.

## Tools

Policy and project discovery:

- `get_company_policy` — read the canonical AAF/Ambient Asset Federation doctrine.
- `list_indexed_projects` — live Qdrant collections fused with the local project
  registry, plus the shared team operating contract.
- `register_current_project` — add or update a workspace in `projects.json`.

Code search (requires LM Studio + Qdrant):

- `semantic_query` — dense + BM25 search over one workspace's code index.
- `cross_project_semantic_query` — one embedding, multiple workspace indexes.

Observability ledger (requires the fabric host):

- `log_tail` — newest structured `observability.log.v1` records.
- `log_search` — substring search over the live ledger.
- `log_traffic` — per-level/per-component traffic and health summary.

Fabric artifacts (generic BLAKE3 CAS; requires the fabric host):

- `nai_lattice_artifact_put` — store base64 bytes via NAF/1 `artifact.put`
  (max 8 MiB decoded; default media type `application/octet-stream`, default
  logical type `blob`). Returns `{ artifact_uri, deduplicated }`. Write tool,
  idempotent by content address.
- `nai_lattice_artifact_resolve` — open an `artifact://blake3/<hex>` URI via
  `artifact.open`. Returns `{ artifact_uri, size, sha256, blake3, inline }` and
  `dataBase64` only when the payload is at most 1 MiB; larger payloads return
  metadata only. Read-only.
- `nai_lattice_artifact_verify` — compare a stored artifact with a raw BLAKE3
  hex digest or an `artifact://blake3/<hex>` URI via `artifact.verify`. Returns
  `{ verified }`; an absent digest verifies as `false`. Read-only.

The artifact tools are product agnostic: they expose the fabric's CAS, not a
consumer-specific store. Failures are typed JSON text with stable codes
(`NAI_LATTICE_ARTIFACT_INVALID_REQUEST`, `NAI_LATTICE_ARTIFACT_UNAVAILABLE`,
`NAI_LATTICE_ARTIFACT_HOST_REJECTED`, `NAI_LATTICE_ARTIFACT_BAD_RESPONSE`,
`NAI_LATTICE_ARTIFACT_INTERNAL_ERROR`) rather than exceptions on the MCP stream.

Shared task ledger and handoff feed (authoritative `state/todo.jsonl`):

- `todo_add`, `todo_list`, `todo_update`, `todo_gate`, `todo_done_check`
- `todo_follow`, `todo_feed`

## License

MIT OR Apache-2.0.
