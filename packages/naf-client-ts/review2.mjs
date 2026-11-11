import { NafClient } from "./dist/index.js";
const pipe = String.raw`\\.\pipe\NAI.AmbientFabric.NAF1.2bb428`;
const ws = "C:/Users/lukes.COREI9/AppData/Local/NAI/GastownTest/workspace";
const c = new NafClient({ appId:"review-live2", clientVersion:"0.1.0", requestedCapabilities:["gastown.review.enqueue_explicit","gastown.review.job_status","gastown.admin.health","artifact.open"], producerId:"gastown.host", pipePath:pipe });
await c.connect();
const jobId = "live-review2-"+Date.now();
const enq = await c.dispatch("gastown.review.enqueue_explicit", { job_id:jobId, workspace_canonical:ws, workspace_root:ws, source_manifest_hash:"host-owned", review_policy_version:1 }, jobId);
console.log("enqueue:", enq.ok ? Buffer.from(enq.body).toString() : enq.error);
const start=Date.now(); let state="Queued", body="";
while(Date.now()-start<180000){ const s=await c.dispatch("gastown.review.job_status",{job_id:jobId}); body=Buffer.from(s.body).toString(); state=JSON.parse(body).state??state; if(state==="Completed"||state==="Failed")break; await new Promise(r=>setTimeout(r,2000)); }
console.log("final:", state, body);
if(state!=="Completed"){ process.exit(1); }
const uri=JSON.parse(body).report;
const art=await c.dispatch("artifact.open",{artifact_uri:uri});
const rpt = art.ok ? Buffer.from(JSON.parse(Buffer.from(art.body).toString()).data,"base64").toString("utf8") : art.error;
console.log("REPORT_DELIMITER");
console.log(rpt);
await c.close();
