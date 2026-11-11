import { NafClient } from "./dist/index.js";
const pipe = String.raw`\\.\pipe\NAI.AmbientFabric.NAF1.2bb428`;
const c = new NafClient({ appId:"report-plugin", clientVersion:"0.1.0", requestedCapabilities:["gastown.review.list_jobs","gastown.review.job_status","artifact.open"], producerId:"gastown.host", pipePath:pipe });
await c.connect();
const jobs = JSON.parse(Buffer.from((await c.dispatch("gastown.review.list_jobs",{})).body).toString());
const pluginJob = jobs.find(j=>j.job_id.startsWith("auto-gastown.host-18260"));
console.log("plugin job:", pluginJob.job_id, pluginJob.state, "ws:", pluginJob.workspace);
const st = JSON.parse(Buffer.from((await c.dispatch("gastown.review.job_status",{job_id:pluginJob.job_id})).body).toString());
console.log("report:", st.report);
const art = await c.dispatch("artifact.open", { artifact_uri: st.report });
if(art.ok){ const rpt = Buffer.from(JSON.parse(Buffer.from(art.body).toString()).data,"base64").toString("utf8"); const lines=rpt.split("\n"); console.log("=== REPORT (first 34 lines) ==="); console.log(lines.slice(0,34).join("\n")); } else { console.log("open err", art.error); }
await c.close();
