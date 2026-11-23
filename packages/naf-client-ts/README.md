# @cyberluke/nai-lattice-client

Reusable TypeScript **NAF/1** client for NAI-Lattice: canonical framing and the
Windows named-pipe transport. The protocol source of truth is
`protos/naf/protocol.proto` in the NAI-Lattice repository.

**First production integration:** [NAI OS](https://github.com/cyberluke/NAI-OS)
ships the NAI Lattice fabric host as its shared per-user runtime.

## Install

```bash
npm install @cyberluke/nai-lattice-client
```

## Usage

```ts
import { NafClient } from "@cyberluke/nai-lattice-client";

// Discover the canonical pipe from:
// %LOCALAPPDATA%\NAI\Lattice\v1\endpoint.json -> canonical_pipe
const pipe = process.env.NAI_FABRIC_PIPE;
if (!pipe) throw new Error("Set NAI_FABRIC_PIPE from endpoint.json");

const client = new NafClient({
  appId: "my-app",
  clientVersion: "0.1.0",
  requestedCapabilities: ["event.read_range"],
  packageIdentity: "@cyberluke/nai-lattice-client@0.2.0",
  producerId: "my-app",
  pipePath: pipe,
});

await client.connect();

const res = await client.dispatch("event.read_range", {
  namespace: "observability.log.v1",
  event_type: "log.record",
  limit: 10,
});

if (res.status !== 0) {
  throw new Error(`read_range failed: ${res.status} ${res.error ?? ""}`);
}

const body = JSON.parse(new TextDecoder().decode(res.body));
console.log(body.events);
```

## API

- `new NafClient(options)` — `appId`, `clientVersion`, `requestedCapabilities`,
  `packageIdentity`, `producerId`, `pipePath`, optional `buildSha`.
- `connect()` — opens the canonical pipe and performs the NAF handshake.
- `runtimeInstanceId` — identity of the connected runtime instance.
- `dispatch(method, body, idempotencyKey?)` — sends one NAF request; returns
  `{ status, body, error }` where `status === 0` means success and `body` is a
  `Uint8Array` payload.

The client is read/write by capability only; request only the capabilities your
app needs.

## Related

- MCP server: [`@cyberluke/mcp-nai-lattice`](https://www.npmjs.com/package/@cyberluke/mcp-nai-lattice)
- Protocol crate: [`nai-lattice-protocol`](https://crates.io/crates/nai-lattice-protocol)
- Repository: <https://github.com/cyberluke/NAI-Lattice>

## License

MIT OR Apache-2.0.
