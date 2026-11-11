import { NafClient } from "./dist/index.js";
const pipe = String.raw`\\.\pipe\NAI.AmbientFabric.NAF1.2bb428`;
const ws = "C:/Users/lukes.COREI9/AppData/Local/NAI/GastownTest/workspace";
const c = new NafClient({ appId:"auto-chain", clientVersion:"0.1.0", requestedCapabilities:["event.append","gastown.review.list_jobs","gastown.admin.metrics","gastown.admin.health"], producerId:"gastown.host", pipePath:pipe });
await c.connect();
const suffix = Date.now().toString();
const sid = "auto-sess-"+suffix; const turn = "auto-turn-1"; const host = "host-"+suffix;
// 1) chat.message (real task activity, host identity set like the Kilo plugin)
const a1 = await c.dispatch("event.append", { namespace:"gastown", event_type:"chat.message", entity_id:ws, workspace_id:ws, host_runtime_id:host, session_id:sid, turn_id:turn, producer_id:"gastown.host", producer_epoch:1, producer_sequence:1, event_id:"chat-"+suffix, event_timestamp_utc_ms:Date.now(), durability:"durable" }, "chat-"+suffix);
console.log("chat.message:", a1.ok, a1.ok?Buffer.from(a1.body).toString():a1.error);
await new Promise(r=>setTimeout(r,500));
// 2) session.idle (task completed / idle after activity)
const a2 = await c.dispatch("event.append", { namespace:"gastown", event_type:"session.idle", entity_id:ws, workspace_id:ws, host_runtime_id:host, session_id:sid, turn_id:turn, producer_id:"gastown.host", producer_epoch:1, producer_sequence:2, event_id:"idle-"+suffix, event_timestamp_utc_ms:Date.now(), durability:"durable" }, "idle-"+suffix);
console.log("session.idle:", a2.ok, a2.ok?Buffer.from(a2.body).toString():a2.error);
// 3) poll: host should AUTO-enqueue + run a real A/B/adjudicator review
const start=Date.now(); let jobs=[]; let found=false;
while(Date.now()-start<180000){
  const jr = await c.dispatch("gastown.review.list_jobs", {});
  jobs = jr.ok ? JSON.parse(Buffer.from(jr.body).toString()) : [];
  const auto = jobs.find(j=>j.job_id.startsWith("auto-"));
  if(auto && auto.state==="Completed"){ found=true; break; }
  await new Promise(r=>setTimeout(r,2000));
}
console.log("auto jobs now:", JSON.stringify(jobs));
const m = await c.dispatch("gastown.admin.metrics", {});
console.log("metrics:", Buffer.from(m.body).toString());
await c.close();
if(!found){ console.error("AUTO_REVIEW_NOT_TRIGGERED"); process.exit(1); }
console.log("AUTO_REAL_REVIEW_OK");