//! Lattice observability dashboard — Bloomberg-terminal / ElasticSearch-admin style
//! live view of the canonical `observability.log.v1` ledger.
//!
//! Served by the fabric host control server on `GET /lattice` (embedded HTML),
//! `GET /lattice/events` (SSE live stream of `observability.log.v1` records + a
//! periodic `stats` line), and `GET /lattice/api` (JSON tail / search / by-trace /
//! component-breakdown for the dashboard's fetch calls and for the mcp-nai-lattice
//! log methods).
//!
//! Deliberately NON-BLOCKING for the host: it reads the ledger via a bounded
//! `event.read_range` scan on a short cadence (1s SSE push) and computes stats
//! from a bounded window — never buffers the whole ledger, never spins CPU, and
//! each SSE connection is its own thread (mirrors `notify_sse`).

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The Lattice dashboard icon (optimized 256px copy of "Neon Glass Network Hub").
/// Embedded so the installed host always serves it regardless of repo path.
pub(crate) const LATTICE_ICON_PNG: &[u8] = include_bytes!("../../../assets/icons/lattice-hub-256.png");
pub(crate) const LATTICE_ICON_MIME: &str = "image/png";

/// Simple connection counter used to decorate the dashboard.
static LIVE_PUSHES: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_SSE: AtomicUsize = AtomicUsize::new(0);
const MAX_SSE_CLIENTS: usize = 6;
struct SseLease;
impl SseLease {
    fn acquire() -> Option<Self> {
        ACTIVE_SSE.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < MAX_SSE_CLIENTS).then_some(n + 1)
        }).ok().map(|_| Self)
    }
}
impl Drop for SseLease {
    fn drop(&mut self) { ACTIVE_SSE.fetch_sub(1, Ordering::AcqRel); }
}

/// Esimated per-minute push cadence for the "traffic" illusion isn't fabricated:
/// these are REAL counters from the ledger scan window, not fake "traffic".
pub(crate) fn push_count() -> usize {
    LIVE_PUSHES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Read the latest `observability.log.v1` records from the shared runtime,
/// newest-first, bounded. Returns the raw list of `LogRecord`-shaped payloads.
pub(crate) fn latest_logs(limit: usize) -> Vec<serde_json::Value> {
    let Some(rt) = crate::gastown_orto::RUNTIME.get() else {
        return Vec::new();
    };
    let args = serde_json::json!({
        "namespace": crate::gastown_orto::lattice_obs_namespace(),
        "event_type": "log.record",
        "limit": limit,
    });
    let raw = rt.dispatch(
        "event.read_range",
        &serde_json::to_vec(&args).unwrap_or_default(),
        "lattice-dashboard",
        None,
    );
    let Ok(raw) = raw else { return Vec::new() };
    let Ok(v): Result<serde_json::Value, _> = serde_json::from_slice(&raw) else {
        return Vec::new();
    };
    let mut out: Vec<serde_json::Value> = Vec::new();
    if let Some(events) = v.get("events").and_then(|e| e.as_array()) {
        for ev in events {
            // The payload already carries {ts_ms, level, component, trace_id,
            // caller, message, extras}; add cursor + correlation for the UI.
            let mut rec = ev.get("payload").cloned().unwrap_or(serde_json::json!({}));
            if let Some(seq) = ev.get("runtime_sequence").and_then(|s| s.as_u64()) {
                rec["runtime_sequence"] = serde_json::json!(seq);
            }
            rec["timestamp_utc_ms"] = ev
                .get("timestamp_utc_ms")
                .cloned()
                .unwrap_or(serde_json::json!(0));
            out.push(rec);
        }
    }
    out
}

/// Aggregate counters for the traffic/health panel, from a bounded recent scan.
pub(crate) fn stats() -> serde_json::Value {
    let logs = latest_logs(2000);
    let mut by_level: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut by_component: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut trace_ids: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // Measured from the latest in-process NAI OS emitter health record.
    // Absent means UNKNOWN, never synthesize zero backpressure.
    let mut newest_emitter: Option<(u64, serde_json::Value)> = None;
    for l in &logs {
        if l.get("component").and_then(|x| x.as_str()) == Some("nai-os.observability") {
            if let Some(metrics) = l.pointer("/extras/emitter") {
                let ts = l.get("ts_ms").and_then(|x| x.as_u64()).unwrap_or(0);
                if newest_emitter.as_ref().is_none_or(|(prior, _)| ts > *prior) {
                    newest_emitter = Some((ts, metrics.clone()));
                }
            }
        }
        if let Some(lv) = l.get("level").and_then(|x| x.as_str()) {
            *by_level.entry(lv.to_string()).or_insert(0) += 1;
        }
        if let Some(c) = l.get("component").and_then(|x| x.as_str()) {
            *by_component.entry(c.to_string()).or_insert(0) += 1;
        }
        if let Some(t) = l.get("trace_id").and_then(|x| x.as_str()) {
            if !t.is_empty() {
                trace_ids.insert(t.to_string());
            }
        }
    }
    let errors = by_level.get("error").copied().unwrap_or(0)
        + by_level.get("warn").copied().unwrap_or(0);
    serde_json::json!({
        "schema": 1,
        "window": logs.len(),
        "levels": by_level,
        "errors": errors,
        "components": by_component,
        "distinctTraceIds": trace_ids.len(),
        "livePushes": push_count(),
        "activeSseClients": ACTIVE_SSE.load(Ordering::Relaxed),
        "maxSseClients": MAX_SSE_CLIENTS,
        "emitter": newest_emitter.map(|(ts, metrics)| serde_json::json!({
            "observedAt": ts,
            "metrics": metrics,
        })),
        "generatedAt": crate::now_unix_ms(),
    })
}

/// Search / tail JSON for the dashboard fetch API and the MCP log methods.
/// Filters are applied in-process over the bounded recent window (newest-first).
pub(crate) fn search(opts: &SearchOpts) -> serde_json::Value {
    let mut logs = latest_logs(6000);
    logs.retain(|l| {
        let level = l.get("level").and_then(|x| x.as_str()).unwrap_or("");
        if let Some(min) = &opts.level_min {
            if !level_at_least(level, min) {
                return false;
            }
        }
        if let Some(c) = &opts.component {
            let comp = l.get("component").and_then(|x| x.as_str()).unwrap_or("");
            if comp != c {
                return false;
            }
        }
        if let Some(t) = &opts.trace {
            let ct = l.get("trace_id").and_then(|x| x.as_str()).unwrap_or("");
            if ct != t {
                return false;
            }
        }
        if let Some(needle) = &opts.contains {
            let msg = l.get("message").and_then(|x| x.as_str()).unwrap_or("");
            let hay = format!("{level} {msg}").to_lowercase();
            if !hay.contains(&needle.to_lowercase()) {
                return false;
            }
        }
        true
    });
    logs.truncate(opts.limit.unwrap_or(100).clamp(1, 500));
    serde_json::json!({
        "schema": 1,
        "hits": logs,
        "count": logs.len(),
        "generatedAt": crate::now_unix_ms(),
    })
}

fn level_at_least(level: &str, min: &str) -> bool {
    const ORDER: &[&str] = &["trace", "debug", "info", "warn", "error"];
    let a = ORDER.iter().position(|l| *l == level).unwrap_or(2);
    let b = ORDER.iter().position(|l| *l == min).unwrap_or(2);
    a >= b
}

/// Search options mirroring the MCP log methods.
#[derive(Default)]
pub(crate) struct SearchOpts {
    pub level_min: Option<String>,
    pub component: Option<String>,
    pub trace: Option<String>,
    pub contains: Option<String>,
    pub limit: Option<usize>,
}

/// Parse the `/lattice/api` query string into `SearchOpts`. The full path
/// (including `?...`) is passed in; `%20`-style escaping is decoded minimally.
pub(crate) fn parse_lattice_search_opts(path: &str) -> SearchOpts {
    let mut o = SearchOpts::default();
    let Some((_, qs)) = path.split_once('?') else { return o };
    for pair in qs.split('&') {
        let Some((k, v)) = pair.split_once('=') else { continue };
        let val = percent_decode(v).unwrap_or_default();
        match k {
            "level" => o.level_min = (!val.is_empty()).then(|| val.clone()),
            "component" => o.component = (!val.is_empty()).then(|| val.clone()),
            "trace" => o.trace = (!val.is_empty()).then(|| val.clone()),
            "contains" => o.contains = (!val.is_empty()).then(|| val.clone()),
            "limit" => o.limit = val.parse::<usize>().ok().map(|n| n.clamp(1,500)),
            _ => {}
        }
    }
    o
}

/// Minimal percent-decode (`%XX` → byte; `+` → space).
fn percent_decode(v: &str) -> Option<String> {
    // Percent decoding is BYTE-based: building chars from %C5%99 separately
    // corrupts Czech/Vietnamese UTF-8 text and silently breaks log searches.
    let raw = v.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'%' if i + 2 < raw.len() => {
                let h = std::str::from_utf8(&raw[i + 1..i + 3]).ok()?;
                let b = u8::from_str_radix(h, 16).ok()?;
                out.push(b);
                i += 3;
            }
            b'+' => { out.push(b' '); i += 1; }
            b => { out.push(b); i += 1; }
        }
    }
    String::from_utf8(out).ok()
}

/// Bounded, cursor-resumable SSE. Exactly one connection consumes one lease.
/// A dead browser releases its worker on the first failed write; a healthy
/// client reconnects after 55s using the last actual runtime_sequence.
pub(crate) fn lattice_sse<W: Write>(w: &mut W, after: Option<u64>) {
    let Some(_lease) = SseLease::acquire() else {
        let _ = w.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nRetry-After: 3\r\nConnection: close\r\n\r\n");
        return;
    };
    if w.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache, no-transform\r\nConnection: keep-alive\r\nX-Accel-Buffering: no\r\nX-Content-Type-Options: nosniff\r\n\r\n").is_err() {
        return;
    }
    let mut cursor = after.unwrap_or(0);
    // Initial tail, or missing records on reconnect. Never reset the browser's
    // Last-Event-ID to zero (that used to trigger duplicate replay storms).
    let mut initial = latest_logs(200);
    initial.retain(|r| r.get("runtime_sequence").and_then(|s| s.as_u64()).unwrap_or(0) > cursor || after.is_none());
    initial.sort_by_key(|r| r.get("runtime_sequence").and_then(|s| s.as_u64()).unwrap_or(0));
    cursor = initial.iter()
        .filter_map(|r| r.get("runtime_sequence").and_then(|s| s.as_u64()))
        .max().unwrap_or(cursor).max(cursor);
    let payload = serde_json::to_string(&initial).unwrap_or_else(|_| "[]".into());
    let id = if cursor > 0 { format!("id: {cursor}\r\n") } else { String::new() };
    if w.write_all(format!("{id}event: search\r\ndata: {payload}\r\n\r\n").as_bytes()).is_err()
        || w.flush().is_err() {
        return;
    }
    LIVE_PUSHES.fetch_add(1, Ordering::Relaxed);
    let started = std::time::Instant::now();
    let mut tick = 0usize;
    while started.elapsed() < std::time::Duration::from_secs(55) {
        std::thread::sleep(std::time::Duration::from_millis(1250));
        let mut recent = latest_logs(240);
        recent.retain(|r| r.get("runtime_sequence").and_then(|s| s.as_u64()).unwrap_or(0) > cursor);
        recent.sort_by_key(|r| r.get("runtime_sequence").and_then(|s| s.as_u64()).unwrap_or(0));
        if !recent.is_empty() {
            cursor = recent.iter()
                .filter_map(|r| r.get("runtime_sequence").and_then(|s| s.as_u64()))
                .max().unwrap_or(cursor).max(cursor);
            let payload = serde_json::to_string(&serde_json::json!({"hits":recent}))
                .unwrap_or_else(|_| "{\"hits\":[]}".into());
            if w.write_all(format!("id: {cursor}\r\nevent: log\r\ndata: {payload}\r\n\r\n").as_bytes()).is_err() {
                break;
            }
        }
        tick += 1;
        if tick % 4 == 0 {
            let payload = serde_json::to_string(&stats()).unwrap_or_default();
            if w.write_all(format!("event: stats\r\ndata: {payload}\r\n\r\n").as_bytes()).is_err() {
                break;
            }
        } else if recent_is_empty_tick(tick) {
            // SSE comment heartbeat detects dead peers without ledger scans.
            if w.write_all(b": heartbeat\r\n\r\n").is_err() { break; }
        }
        if w.flush().is_err() { break; }
        LIVE_PUSHES.fetch_add(1, Ordering::Relaxed);
    }
}

/// Heartbeats are cheap and ensure idle clients get disconnect detection.
fn recent_is_empty_tick(tick: usize) -> bool { tick % 2 == 0 }

/// Embedded Bloomberg/ES-admin style HTML dashboard. Dark, dense, monospace;
/// real data only. Panels: header (live stats ticker), a traffic/health strip,
/// a per-component breakdown, and a live scrolling log tail with level coloring +
/// a traceId search box.
pub(crate) const LATTICE_UI_HTML: &str = r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8">
<title>NAI Lattice · Observability</title>
<link rel="icon" href="/lattice/icon" type="image/png">
<style>
:root{--bg:#0a0d12;--panel:#11151c;--panel2:#0d1117;--line:#1f2630;--fg:#e6edf3;--muted:#8b98a5;--accent:#19c2d8;--ok:#26de81;--warn:#ffce54;--bad:#ff6b6b;--search:#f7b731}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:13px ui-monospace,Consolas,Menlo,monospace}
.hd{display:flex;align-items:center;gap:14px;padding:9px 16px;border-bottom:1px solid var(--line);background:var(--panel);position:sticky;top:0;z-index:20;flex-wrap:wrap}
.hd h1{margin:0;font-size:14px;letter-spacing:1px;color:var(--accent);display:flex;align-items:center;gap:9px;text-shadow:0 0 13px #19c2d855}.hd .tick{color:var(--muted);font-size:11px}
 .tw{display:grid;grid-template-columns:repeat(6,1fr);gap:8px;padding:12px 16px;border-bottom:1px solid var(--line)}
.kpi{background:linear-gradient(125deg,var(--panel),#0a1720);border:1px solid #23505a;border-radius:6px;padding:8px 10px;box-shadow:inset 0 1px #ffffff0d}
.kpi .lab{color:var(--muted);font-size:10px;text-transform:uppercase;letter-spacing:.5px}.kpi .val{font-size:20px;font-weight:700;margin-top:2px}
.grid{display:grid;grid-template-columns:minmax(0,1.6fr) minmax(0,1fr);gap:10px;padding:12px 16px;align-items:start}
@media(max-width:1000px){.grid{grid-template-columns:1fr}}
.panel{background:var(--panel);border:1px solid var(--line);border-radius:6px;overflow:hidden}
.panel h2{margin:0;font-size:11px;letter-spacing:.5px;color:var(--accent);border-bottom:1px solid var(--line);padding:7px 10px;display:flex;justify-content:space-between;align-items:center}
#tail{height:60vh;overflow:auto;font-size:12px;line-height:1.5;padding:6px 8px;background:var(--panel2)}
.ln{white-space:pre-wrap;word-break:break-word;padding:0 4px;border-left:3px solid transparent}
.ln .ts{color:var(--muted);padding-right:8px}.ln .lv{font-weight:700;padding-right:8px}
.ln.l-info{border-left-color:var(--accent)}.ln.l-warn{border-left-color:var(--warn);color:var(--warn)}
.ln.l-error{border-left-color:var(--bad);color:var(--bad);background:#ff6b6b0d}
.ln .comp{color:var(--muted);padding-right:8px}.ln .trace{color:var(--search);font-size:11px}
#components{font-size:12px}.row{display:flex;justify-content:space-between;padding:5px 10px;border-bottom:1px solid var(--line);align-items:center}
.bar{height:6px;border-radius:3px;background:var(--line);flex:1;margin:0 8px;overflow:hidden}
.bar i{display:block;height:100%;background:var(--accent)}
 .searchbar{display:flex;gap:6px;align-items:center;flex-wrap:wrap;padding:8px 10px;border-bottom:1px solid var(--line)}
input{background:var(--panel2);border:1px solid var(--line);color:var(--fg);border-radius:4px;padding:5px 8px;font-size:12px;width:220px}
button{background:var(--accent);border:0;color:#041;border-radius:4px;padding:5px 10px;font-weight:700;cursor:pointer}
.s-ok{color:var(--ok)}.s-warn{color:var(--warn)}.s-bad{color:var(--bad)}
</style></head><body>
<div class="hd"><h1><img src="/lattice/icon" alt="" width="26" height="26">NAI LATTICE</h1><span class="tick">observability.log.v1</span>
  <span class="tick" id="clock"></span><span class="tick" id="conn" style="color:var(--accent)">connecting…</span>
  <span class="s-ok" id="event-cursor">cursor</span>
</div>
<div class="tw">
  <div class="kpi"><div class="lab">Live log lines</div><div class="val" id="k-window">–</div></div>
  <div class="kpi"><div class="lab">Warn+Error</div><div class="val" id="k-errors" style="color:var(--warn)">–</div></div>
  <div class="kpi"><div class="lab">Distinct traceId</div><div class="val" id="k-traces">–</div></div>
  <div class="kpi"><div class="lab">Components</div><div class="val" id="k-comp">–</div></div>
  <div class="kpi"><div class="lab">SSE pushes</div><div class="val" id="k-push">–</div></div>
  <div class="kpi"><div class="lab">Active streams</div><div class="val" id="k-active">–</div></div>
</div>
<div class="searchbar"><button onclick="refresh()">↻</button><button id="pause" onclick="togglePause()">Ⅱ PAUSE</button><button onclick="clearTail()">CLEAR</button>
  <input id="q" placeholder="grep message / error code" onkeydown="if(event.key==='Enter')refresh()"/>
  <input id="component" placeholder="component (exact)" onkeydown="if(event.key==='Enter')refresh()" />
  <input id="trace" placeholder="traceId (exact)" onkeydown="if(event.key==='Enter')refresh()" />
  <select id="lvl" onchange="refresh()"><option value="">all levels</option><option>error</option><option>warn</option><option>info</option><option>debug</option><option>trace</option></select>
</div>
<div class="grid">
  <div class="panel"><h2>LOG TAIL <span class="tick" id="tail-meta"></span></h2><div id="tail"></div></div>
  <div><div class="panel"><h2>INGESTION / BACKPRESSURE</h2><div class="row" id="ingest" style="display:block;line-height:1.8">Waiting for measured producer health…</div></div><div class="panel" style="margin-top:10px"><h2>COMPONENTS</h2><div id="components"></div></div></div>
</div>
<script>
const $=id=>document.getElementById(id);
const fmtTs=ms=>{const d=new Date(Number(ms));return Number.isFinite(d.getTime())?d.toLocaleTimeString('en-GB',{hour12:false})+'.'+String(d.getMilliseconds()).padStart(3,'0'):'--:--:--'};
const seen=new Set();
const MAX_ROWS=1400;
let paused=false;
let stream=null;
function clearTail(){seen.clear();$('tail').replaceChildren();$('tail-meta').textContent='cleared';}
function togglePause(){paused=!paused;$('pause').textContent=paused?'▶ RESUME':'Ⅱ PAUSE';$('pause').style.background=paused?'var(--warn)':'var(--accent)';}
function filters(){
 return {q:($('q').value||'').toLowerCase(),lv:$('lvl').value,
  component:$('component').value.trim(),trace:$('trace').value.trim()};
}
function filtered(ln,f){
 if(f.lv&&ln.level!==f.lv)return false;
 if(f.component&&ln.component!==f.component)return false;
 if(f.trace&&ln.trace_id!==f.trace)return false;
 if(f.q&&!String(ln.message||'').toLowerCase().includes(f.q))return false;
 return true;
}
function addSpan(parent,cls,value){
 const span=document.createElement('span');span.className=cls;span.textContent=String(value??'');parent.append(span);
}
function renderTail(hits,force=false){
 if(paused&&!force)return;
 const tail=$('tail'),f=filters();
 const stick=tail.scrollHeight-tail.scrollTop-tail.clientHeight<72;
 let added=0;
 for(const ln of hits){
  if(!filtered(ln,f))continue;
  const seq=String(ln.runtime_sequence??('legacy:'+ln.ts_ms+':'+ln.component+':'+ln.message));
  if(seen.has(seq))continue;
  const lv=['trace','debug','info','warn','error'].includes(ln.level)?ln.level:'info';
  const div=document.createElement('div');div.className='ln l-'+lv;div.dataset.key=seq;
  addSpan(div,'ts',fmtTs(ln.timestamp_utc_ms||ln.ts_ms));
  addSpan(div,'lv',lv.toUpperCase());
  addSpan(div,'comp',ln.component);
  if(ln.trace_id)addSpan(div,'trace','['+ln.trace_id+'] ');
  div.append(document.createTextNode(String(ln.message||'')));
  tail.append(div);seen.add(seq);added++;
  while(tail.childElementCount>MAX_ROWS){
   const oldest=tail.firstElementChild;
   if(!oldest)break;
   seen.delete(oldest.dataset.key);
   oldest.remove();
  }
 }
 if(added&&stick)tail.scrollTop=tail.scrollHeight;
 $('tail-meta').textContent=tail.childElementCount+' rows · '+(paused?'paused':'live');
}
function renderStats(s){
 $('k-window').textContent=s.window??'—';
 $('k-errors').textContent=s.errors??'—';
 $('k-traces').textContent=s.distinctTraceIds??'—';
 $('k-comp').textContent=Object.keys(s.components||{}).length;
 $('k-push').textContent=s.livePushes??'—';
 $('k-active').textContent=(s.activeSseClients??'—')+' / '+(s.maxSseClients??'—');
 const h=s.emitter,el=$('ingest');el.replaceChildren();
 if(!h||!h.metrics){el.textContent='NO PRODUCER TELEMETRY · runtime unknown';el.style.color='var(--warn)';}
 else{
  const age=Math.max(0,Date.now()-Number(h.observedAt||0));
  const m=h.metrics;
  el.style.color=age>35000?'var(--warn)':'var(--fg)';
  el.textContent='Queued '+(m.queued??'?')+'  |  Dropped '+(m.dropped??'?')+
   '  |  Flushed '+(m.flushed??'?')+'  |  Age '+Math.round(age/1000)+'s';
  if(Number(m.dropped)>0)el.style.color='var(--bad)';
 }
 const comps=$('components');comps.replaceChildren();
 const top=Object.entries(s.components||{}).sort((a,b)=>Number(b[1])-Number(a[1])).slice(0,25);
 const max=Math.max(1,...top.map(([,n])=>Number(n)||0));
 for(const [name,n] of top){
  const row=document.createElement('div');row.className='row';
  addSpan(row,'',name);
  const bar=document.createElement('div');bar.className='bar';
  const fill=document.createElement('i');fill.style.width=Math.max(0,Math.min(100,Number(n)/max*100))+'%';
  bar.append(fill);row.append(bar);addSpan(row,'',n);comps.append(row);
 }
 if(!top.length)addSpan(comps,'row','no records yet');
}
async function refresh(){
 const f=filters();
 const qp=new URLSearchParams({limit:'350'});
 if(f.q)qp.set('contains',f.q);
 if(f.lv)qp.set('level',f.lv);
 if(f.component)qp.set('component',f.component);
 if(f.trace)qp.set('trace',f.trace);
 try{
  const response=await fetch('/lattice/api?'+qp.toString(),{cache:'no-store'});
  if(!response.ok)throw new Error('HTTP '+response.status);
  const json=await response.json();
  clearTail();renderTail((json.hits||[]).slice().reverse(),true);
  renderStats(json.stats||{});
 }catch(error){$('tail-meta').textContent='QUERY ERROR '+String(error);}
}
setInterval(()=>$('clock').textContent=new Date().toLocaleTimeString('en-GB',{hour12:false}),1000);
function connect(){
 if(stream){stream.close();stream=null;}
 stream=new EventSource('/lattice/events');
 stream.onopen=()=>{$('conn').textContent='● live';$('conn').style.color='var(--ok)';};
 stream.onerror=()=>{$('conn').textContent='↻ reconnecting';$('conn').style.color='var(--warn)';};
 for(const kind of ['log','search','stats']){
  stream.addEventListener(kind,event=>{
   try{
    const data=JSON.parse(event.data);
    if(kind==='stats')renderStats(data);
    else renderTail(kind==='search'?data:(data.hits||[]));
   }catch(e){console.warn('[lattice] malformed SSE',kind,e);}
  });
 }
 // EventSource automatically reconnects with Last-Event-ID. Never allocate
 // an extra untracked stream on a timer: each costs a server worker.
}
window.addEventListener('pagehide',()=>{if(stream)stream.close();});
connect();
refresh();
</script></body></html>"#;
#[cfg(test)]
mod dashboard_parser_tests {
    use super::*;
    #[test]
    fn percent_decoder_preserves_utf8_and_plus() {
        assert_eq!(percent_decode("Praha+%C5%99%C3%ADjen"), Some("Praha říjen".into()));
        assert_eq!(percent_decode("%E1%BB%87"), Some("ệ".into()));
        assert_eq!(percent_decode("%FF"), None);
    }
    #[test]
    fn search_limits_are_hard_bounded() {
        let opts = parse_lattice_search_opts("/lattice/api?limit=9999999&contains=GPU%20hot");
        assert_eq!(opts.limit, Some(500));
        assert_eq!(opts.contains.as_deref(), Some("GPU hot"));
    }
}
