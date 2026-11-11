import { NafClient } from "./dist/index.js";
const pipe = String.raw`\\.\pipe\NAI.AmbientFabric.NAF1.2bb428`;
const c = new NafClient({ appId:"latency", clientVersion:"0.1.0", requestedCapabilities:["event.append"], producerId:"gastown.host", pipePath:pipe });
await c.connect();
const ls=[]; 
for(let i=0;i<6;i++){
  const t0=Date.now();
  const r=await c.dispatch("event.append",{namespace:"gastown",event_type:"chat.message",entity_id:"C:/lat",workspace_id:"C:/lat",host_runtime_id:"lat-host",session_id:"s-lat",turn_id:"t"+i,producer_id:"gastown.host",event_timestamp_utc_ms:Date.now(),durability:"batched"},"lat-"+i);
  ls.push(Date.now()-t0);
}
ls.sort((a,b)=>a-b);
const p50=ls[Math.floor(ls.length/2)];
console.log("ingest append latencies(ms):", ls.join(","));
console.log("median p50:", p50, "max:", Math.max(...ls));
await c.close();
