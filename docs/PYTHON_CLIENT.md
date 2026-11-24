# Python client (`nai-lattice-client`)

The Python NAF/1 client lives in
[`packages/naf-client-py`](../packages/naf-client-py). It speaks the same
canonical framing, handshake, `event.*`, and `artifact.*` methods as the
TypeScript client and the Rust host. Protocol source of truth:
[`protos/naf/protocol.proto`](../protos/naf/protocol.proto).

## Install

```bash
python -m pip install nai-lattice-client
```

Or from a checkout:

```bash
python -m pip install -e "packages/naf-client-py[test]"
```

Python 3.10+; `blake3==1.0.11` is pinned for byte-compatible frame integrity.

## Minimal use

```python
from nai_lattice_client import NafClient

with NafClient(app_id="my-product") as client:
    client.event_append(
        "my.product.events",
        "my.product.started",
        payload={"version": "1.0.0"},
        durability="durable",
    )
```

The client discovers the host through
`%LOCALAPPDATA%\NAI\Lattice\v1\endpoint.json`, falls back to the legacy
username-derived pipe alias, and never silently falls back to a local store.
`app_id` is required; `producer_id` defaults to it. Use
`nai.lattice.fabric` only for fabric-wide events; products own their own dotted
namespaces.

## Live smoke

With a host running:

```powershell
$env:NAI_LATTICE_LIVE="1"
python -m pytest packages/naf-client-py/tests -q -k live
```

Full API, endpoint discovery, conventions, and security notes:
[`packages/naf-client-py/README.md`](../packages/naf-client-py/README.md).
