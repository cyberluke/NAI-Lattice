import { NafClient } from "./dist/index.js";
const pipe = String.raw`\\.\pipe\NAI.AmbientFabric.NAF1.2bb428`;
const c = new NafClient({ appId:"report-read", clientVersion:"0.1.0", requestedCapabilities:["artifact.open","artifact.verify","gastown.review.job_status"], producerId:"gastown.host", pipePath:pipe });
await c.connect();
const job = await c.dispatch("gastown.review.job_status", { job_id: "live-review-1794226538997" });
const jb = JSON.parse(Buffer.from(job.body).toString());
console.log("job report uri:", jb.report);
const art = await c.dispatch("artifact.open", { artifact_uri: jb.report });
if (art.ok) {
  const bytes = Buffer.from(JSON.parse(Buffer.from(art.body).toString()).data, "base64");
  console.log("report bytes:", bytes.length);
  console.log(bytes.toString("utf8"));
} else {
  console.log("artifact.open error:", art.error);
}
const ver = await c.dispatch("artifact.verify", { artifact_uri: jb.report });
console.log("artifact.verify:", ver.ok, ver.ok ? Buffer.from(ver.body).toString() : ver.error);
await c.close();
