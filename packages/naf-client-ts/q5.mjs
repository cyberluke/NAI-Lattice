import { NafClient } from "./dist/index.js";
const pipe = String.raw`\\.\pipe\NAI.AmbientFabric.NAF1.2bb428`;
const c = new NafClient({ appId:"q5", clientVersion:"0.1.0", requestedCapabilities:["gastown.review.list_jobs","gastown.review.job_status","artifact.open"], producerId:"gastown.host", pipePath:pipe });
await c.connect();
const jobs = JSON.parse(Buffer.from((await c.dispatch("gastown.review.list_jobs",{})).body).toString());
const bJob = jobs.find(j=>j.job_id.includes("13776"));
const aJob = jobs.find(j=>j.job_id.includes("51376"));
for(const jj of [aJob,bJob]){
  const st = JSON.parse(Buffer.from((await c.dispatch("gastown.review.job_status",{job_id:jj.job_id})).body).toString());
  const art = await c.dispatch("artifact.open",{artifact_uri:st.report});
  const rpt = !art.ok ? ("ERR:"+art.error) : Buffer.from(JSON.parse(Buffer.from(art.body).toString()).data,"base64").toString("utf8");
  const lines=rpt.split("\n");
  console.log("===== "+jj.job_id.substring(0,60));
  console.log(lines.slice(0,12).join("\n"));
  console.log(lines.filter(l=>l.includes("] —")||l.includes("VERDICT")).slice(0,4).join("\n"));
}
await c.close();
