import { NafClient } from "./dist/index.js";
const pipe = String.raw`\\.\pipe\NAI.AmbientFabric.NAF1.2bb428`;
const c = new NafClient({ appId:"q4", clientVersion:"0.1.0", requestedCapabilities:["gastown.admin.metrics","gastown.review.list_jobs","gastown.admin.health"], producerId:"gastown.host", pipePath:pipe });
await c.connect();
console.log("metrics:", Buffer.from((await c.dispatch("gastown.admin.metrics",{})).body).toString());
console.log("jobs:", JSON.stringify(JSON.parse(Buffer.from((await c.dispatch("gastown.review.list_jobs",{})).body).toString()),null,1));
console.log("health:", Buffer.from((await c.dispatch("gastown.admin.health",{})).body).toString());
await c.close();
