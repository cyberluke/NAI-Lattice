import { NafClient } from "./dist/index.js";
const pipe = String.raw`\\.\pipe\NAI.AmbientFabric.NAF1.2bb428`;
const c = new NafClient({ appId:"report2", clientVersion:"0.1.0", requestedCapabilities:["gastown.review.list_jobs","artifact.open","gastown.admin.metrics"], producerId:"gastown.host", pipePath:pipe });
await c.connect();
const jobs = JSON.parse(Buffer.from((await c.dispatch("gastown.review.list_jobs",{})).body).toString());
const auto = jobs.find(j=>j.job_id.startsWith("auto-"));
// find the report uri via job_status (report field)
const st = JSON.parse(Buffer.from((await c.dispatch("gastown.review.job_status",{job_id:auto.job_id})).body).toString());
console.log("auto job:", auto.job_id, "state:", st.state, "report:", st.report);
const art = await c.dispatch("artifact.open", { artifact_uri: st.report });
if(art.ok){ const rpt = Buffer.from(JSON.parse(Buffer.from(art.body).toString()).data,"base64").toString("utf8"); const lines=rpt.split("\n"); console.log("REPORT HEAD:"); console.log(lines.slice(0,12).join("\n")); console.log("REPORT FINDINGS:"); console.log(lines.filter(l=>l.includes("] —")||l.includes("VERDICT")&&l.includes(":")||l.includes("ModelProvenance")).slice(0,8).join("\n")); } else { console.log("open err", art.error); }
await c.close();
