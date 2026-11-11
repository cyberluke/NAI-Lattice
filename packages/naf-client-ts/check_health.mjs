import { NafClient } from "./dist/index.js";
const pipe = String.raw`\\.\pipe\NAI.AmbientFabric.NAF1.2bb428`;
const c = new NafClient({ appId:"check", clientVersion:"0.1.0", requestedCapabilities:["gastown.admin.metrics","gastown.admin.health"], producerId:"gastown.host", pipePath:pipe });
await c.connect();
const m = await c.dispatch("gastown.admin.metrics", {});
console.log("metrics:", Buffer.from(m.body).toString());
const h = await c.dispatch("gastown.admin.health", {});
console.log("health:", Buffer.from(h.body).toString());
await c.close();
