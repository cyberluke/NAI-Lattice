# @cyberluke/mcp-nai-lattice

MCP server for NAI-Lattice: read-only semantic search over the local Qdrant code
index, plus the shared task ledger, mind-map feed and company policy.

## Install

```bash
npm install -g @cyberluke/mcp-nai-lattice
# or run it from a clone:
npm ci && npm run build -w @cyberluke/mcp-nai-lattice
```

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

## License

MIT OR Apache-2.0.
