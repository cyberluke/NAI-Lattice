# nai-lattice-client

Reusable Python **NAF/1** client for NAI-Lattice: canonical framing, the
Windows named-pipe transport, handshake negotiation, and the typed
`event.*` / `artifact.*` methods. The protocol source of truth is
[`protos/naf/protocol.proto`](https://github.com/cyberluke/NAI-Lattice/blob/main/protos/naf/protocol.proto)
in the NAI-Lattice repository.

This package is deliberately product-neutral. It has **no default application
identity**, **no product-specific event namespace**, and **no local-store
fallback**. Callers pass `app_id` explicitly.

Version `0.2.1` matches the sibling TypeScript client
([`packages/naf-client-ts`](https://github.com/cyberluke/NAI-Lattice/tree/main/packages/naf-client-ts)).

## Install

The current source tree is installable; the 0.2.1 distribution is built and
checked but not yet uploaded to the package index from this workspace.

```bash
# Current source install (until the package is published):
python -m pip install -e "packages/naf-client-py[test]"

# Published package, once available:
python -m pip install nai-lattice-client
```

Requires Python 3.10+ and `blake3==1.0.11` (pinned for byte-compatible frame
integrity).

## Quickstart

Start `nai-fabric-host` first; it publishes the per-user endpoint manifest
while it is running.

```python
from nai_lattice_client import NafClient

with NafClient(app_id="my-product") as client:
    event = client.event_append(
        "my.product.events",
        "my.product.started",
        payload={"version": "1.0.0"},
        durability="durable",
    )
    print(event["event_id"])

    put = client.artifact_put(b"hello", "text/plain", "my.product.note")
    assert client.artifact_open(put["artifact_uri"]) == b"hello"
    assert client.artifact_verify(put["artifact_uri"]) is True
```

`NafClient` is synchronous and safe to share between threads; one dispatch is
serialized at a time per client instance.

## Endpoint discovery

The host publishes:

```
%LOCALAPPDATA%\NAI\Lattice\v1\endpoint.json
```

with `canonical_pipe` (and optionally `legacy_alias`). Resolution order:

1. explicit `pipe_path=...` (useful for tests or non-default hosts);
2. `endpoint_path=...` manifest path;
3. `%LOCALAPPDATA%\NAI\Lattice\v1\endpoint.json`;
4. the username-derived legacy alias served by the same host singleton.

Helpers `default_endpoint_path()`, `discover_endpoint(path=None)`, and
`legacy_pipe_name()` expose the same logic. A client must not guess the
canonical pipe from `USERNAME` when the manifest is available.

## API

```python
NafClient(
    *,
    app_id: str,                         # required
    pipe_path: str | None = None,
    endpoint_path: str | os.PathLike[str] | None = None,
    client_version: str | None = None,   # defaults to package __version__
    producer_id: str | None = None,      # defaults to app_id
    requested_capabilities: tuple[str, ...] | list[str] | None = None,
    package_identity: str = "",
    build_sha: str = "",
    nonce: bytes = b"",
    timeout_ms: int = 15_000,
    transport_factory: Callable[[str], Transport] | None = None,
)
```

- `connect()` / `close()` / context manager — open the pipe, perform the NAF
  handshake, and close the handle.
- `dispatch(method, args=None, idempotency_key=None)` — send one request and
  return the JSON-decoded response body. Non-zero host status raises
  `NafError` with `code="RPC_ERROR"` and the host `status`, `error`, `body`.
- `event_append(namespace, event_type, *, entity_id, payload, correlation_id,
  durability="batched", idempotency_key, timestamp_utc_ms)` — append a
  canonical event. `durability` is `"batched"` or `"durable"`.
- `event_read_range(*, after, limit, namespace, event_type, entity_id,
  correlation_id, tail)` — read canonical events, resuming after an exclusive
  `after` sequence.
- `artifact_put(data, media_type, logical_type)` — store bytes in the CAS;
  returns `artifact_uri` and `deduplicated`.
- `artifact_open(artifact_uri)` — return the exact artifact bytes.
- `artifact_verify(expected_hash_or_uri)` — return the host's `verified` bool;
  the canonical `artifact://blake3/<hash>` URI is accepted.

Handshake results are exposed as `runtime_version`, `runtime_build_sha`,
`runtime_instance_id`, `naf_version`, `storage_schema_version`,
`capabilities`, `health`, and `endpoint`.

Low-level frame/codec helpers are exported for tests and tooling:
`encode_wire_frame`, `decode_wire_frame`, `encode_envelope`,
`decode_envelope`, `encode_handshake_request`, `decode_handshake_response`,
`encode_frame_message`, `decode_frame_message`, `crc32c`, `blake3_digest`,
plus constants such as `FRAME_MAGIC`, `MAX_FRAME_BYTES`, `NAF_VERSION`, and
the `KIND_*` values.

## Conventions

- **`app_id` is required.** Use a stable dotted product/component identifier,
  for example `my-product` or `my-product.worker`.
- **`producer_id` defaults to `app_id`.** Override it only when a product needs
  distinct producer identities per component.
- **Namespaces are caller-owned.** `nai.lattice.fabric` is the product-neutral
  namespace for events about the shared fabric itself. `nai.lattice.test` is
  reserved for tests and smoke validation. Products should use their own
  dotted namespace for product events.
- **Idempotency keys are stable per operation.** Reusing a key returns the
  original event id and sets `duplicated: true`; generate one key per logical
  operation, not per retry attempt.
- **Request only the capabilities you need.** The default tuple covers
  `event.append`, `event.append_batch`, `event.read_range`, and the artifact
  methods. A host may reject a handshake for an unsupported capability.
- **No silent fallback.** If the host is absent, `connect()` raises
  `NafError(code="PIPE_UNAVAILABLE")`; the client does not buffer events in a
  local database.

## Security

- Transport is a **local Windows named pipe** owned by the per-user Lattice
  host. There is no TCP listener and no network path in this package.
- Pipe access is bounded by the host's per-user pipe ACL. The client does not
  weaken it.
- CRC32C + BLAKE3 framing detects corruption and accidental mutation; it is
  **not** a remote authentication mechanism.
- The ledger is durable and auditable. Do not send credentials, tokens, or
  customer secrets in event payloads or artifact metadata unless your product
  policy explicitly permits it.
- Capability-scoped dispatch is the authority boundary: the host decides what
  an app may do. Request the least privilege that works.

## Tests

Hermetic tests cover framing, protobuf compatibility (including the canonical
TS→Rust handshake golden), endpoint discovery, request shapes, and typed
errors. The live smoke is opt-in:

```powershell
$env:NAI_LATTICE_LIVE="1"
python -m pytest packages/naf-client-py/tests -q -k live
```

The live test writes `nai.lattice.test` events and a small artifact through a
running `nai-fabric-host`.

## Related

- TypeScript client:
  [`packages/naf-client-ts`](https://github.com/cyberluke/NAI-Lattice/tree/main/packages/naf-client-ts)
- Protocol source:
  [`protos/naf/protocol.proto`](https://github.com/cyberluke/NAI-Lattice/blob/main/protos/naf/protocol.proto)
- Repository: <https://github.com/cyberluke/NAI-Lattice>

## License

MIT OR Apache-2.0.
