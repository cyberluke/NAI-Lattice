import { NafClient } from "./dist/index.js";
const pipe = String.raw`\\.\pipe\NAI.AmbientFabric.NAF1.2bb428`;
const ws = "C:/Users/lukes.COREI9/AppData/Local/NAI/GastownTest/workspace";
const c = new NafClient({ appId:"review-live", clientVersion:"0.1.0", requestedCapabilities:["gastown.review.enqueue_explicit","gastown.review.job_status","gastown.review.list_jobs","gastown.admin.health"], producerId:"gastown.host", pipePath:pipe });
await c.connect();
console.log("instance:", c.runtimeInstanceId);
const jobId = "live-review-"+Date.now();
const enq = await c.dispatch("gastown.review.enqueue_explicit", {
  job_id: jobId,
  workspace_canonical: ws,
  workspace_root: ws,
  source_manifest_hash: "host-owned",
  review_policy_version: 1,
}, jobId);
console.log("enqueue:", enq.ok, enq.ok ? Buffer.from(enq.body).toString() : enq.error);
// Poll job_status until Completed/Failed (workers run real model calls).
const start = Date.now();
let state = "Queued";
let last = "";
while (Date.now()-start < 120000) {
  const st = await c.dispatch("gastown.review.job_status", { job_id: jobId });
  const b = st.ok ? JSON.parse(Buffer.from(st.body).toString()) : {};
  last = JSON.stringify(b);
  state = b.state ?? state;
  if (state === "Completed" || state === "Failed") break;
  await new Promise(r=>setTimeout(r,2000));
}
console.log("final state:", state);
console.log("job:", last);
const jobs = await c.dispatch("gastown.review.list_jobs", {});
console.log("list_jobs:", jobs.ok ? Buffer.from(jobs.body).toString() : jobs.error);
await c.close();
if (state !== "Completed") { console.error("REVIEW_NOT_COMPLETED"); process.exit(1); }
console.log("LIVE_REAL_REVIEW_OK");