import { NafClient } from "./dist/index.js";
const c = new NafClient({ appId:"bun-conn", clientVersion:"0.1.0", requestedCapabilities:["gastown.host.register","gastown.admin.health"], producerId:"gastown.host", pipePath:String.raw`\\.\pipe\NAI.AmbientFabric.NAF1.2bb428` });
try { await c.connect(); console.log("BUN_CONNECT_OK instance:", c.runtimeInstanceId); const h=await c.dispatch("gastown.admin.health",{}); console.log("health:", h.ok, Buffer.from(h.body).toString().slice(0,80)); await c.close(); } catch(e){ console.error("BUN_CONNECT_FAIL:", e.message, e.code??""); process.exit(1); }
