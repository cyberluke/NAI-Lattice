// Minimal NAF/1 client example. No machine-specific defaults.
//
//   $env:NAI_FABRIC_PIPE = '\\.\pipe\NAI.AmbientFabric.NAF1.<sid-hash>'
//   node examples/live-fabric.mjs
//
// The pipe name is published by the running host in
// %LOCALAPPDATA%\NAI\Lattice\v1\endpoint.json (`canonical_pipe`).
import { NafClient } from "../dist/index.js";

const pipe = process.env.NAI_FABRIC_PIPE;
if (!pipe) {
  console.error("Set NAI_FABRIC_PIPE to the canonical_pipe from endpoint.json.");
  process.exit(2);
}

const client = new NafClient({
  appId: "nai-lattice-example",
  clientVersion: "0.2.0",
  requestedCapabilities: ["event.read_range"],
  packageIdentity: "@cyberluke/nai-lattice-client@0.2.0",
  producerId: "nai-lattice-example",
  pipePath: pipe,
});

await client.connect();
console.log("connected; runtime instance:", client.runtimeInstanceId);

const res = await client.dispatch("event.read_range", {
  namespace: "observability.log.v1",
  event_type: "log.record",
  limit: 10,
});

if (res.status !== 0) {
  console.error("read_range failed:", res.status, res.error);
  process.exit(1);
}

const body = JSON.parse(new TextDecoder().decode(res.body));
for (const event of body.events ?? []) {
  const payload = event.payload ?? {};
  console.log(payload.level ?? "info", payload.component ?? "", payload.message ?? "");
}
