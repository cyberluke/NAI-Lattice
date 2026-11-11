//! Self-contained local NAI Inquisitor workspace (no VS Code, no external deps).
//!
//! Project-centric governor UI. Reads REAL persisted state via the loopback
//! server (`GET /dashboard`, `POST /project`, `/inquisitor`, `/recall`), streams
//! the durable audit event log live via `GET /events` (SSE), and drives the
//! Mayor / convoy / quaran / vector mutation endpoints with the control token.
//! No mock data. Single-source Kilo session picker. Create-&-Open project UX.

pub(crate) const UI_HTML: &str = r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8">
<title>NAI Inquisitor — Project Workspace</title>
<style>
:root{--bg:#0d1117;--panel:#161b22;--line:#30363d;--fg:#c9d1d9;--muted:#8b949e;--accent:#2f81f7;--ok:#3fb950;--warn:#d29922;--bad:#f85149;--purple:#a371f7}
*{box-sizing:border-box}body{margin:0;font:14px/1.4 -apple-system,Segoe UI,Roboto,sans-serif;background:var(--bg);color:var(--fg)}
header{align-items:center;display:flex;gap:12px;padding:10px 18px;border-bottom:1px solid var(--line);background:var(--panel);flex-wrap:wrap;position:sticky;top:0;z-index:10}
header h1{margin:0;font-size:16px}header .sub{color:var(--muted);font-size:12px}
@media(max-width:1100px){header{flex-direction:column;align-items:flex-start}}
.mono{font-family:ui-monospace,Consolas,monospace}
.topnav{display:flex;gap:8px;align-items:center;flex:1;flex-wrap:wrap}
.topnav select{max-width:260px}
.topnav .badge{background:#21262d;border:1px solid var(--line);border-radius:10px;padding:2px 8px;font-size:11px;color:var(--muted)}
.grid{display:grid;grid-template-columns:minmax(260px,340px) minmax(0,1fr) minmax(280px,360px);gap:12px;padding:12px;align-items:start}
@media(max-width:1200px){.grid{grid-template-columns:1fr}}
.panel{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:12px;margin-bottom:12px}
.panel h2{margin:0 0 8px;font-size:13px;color:var(--accent);border-bottom:1px solid var(--line);padding-bottom:6px;display:flex;justify-content:space-between;align-items:center}
label{display:block;margin:8px 0 3px;font-size:12px;color:var(--muted)}
input,textarea,select{width:100%;background:#0d1117;border:1px solid var(--line);color:var(--fg);border-radius:6px;padding:7px;font-size:13px}
button{background:var(--accent);color:#fff;border:0;border-radius:6px;padding:8px 12px;cursor:pointer;font-size:13px}
button:hover{filter:brightness(1.15)}button:disabled{opacity:.5;cursor:default}
button.alt{background:#21262d;border:1px solid var(--line)}
th{text-align:left}table{width:100%;border-collapse:collapse;font-size:12px}
td,th{padding:5px 6px;border-bottom:1px solid var(--line);vertical-align:top}
.status{display:inline-block;padding:2px 8px;border-radius:10px;font-size:11px;font-weight:600}
.s-open{background:#1f6feb33;color:#58a6ff}.s-inprogress{background:#d2992233;color:#e3b341}.s-inreview{background:#a371f733;color:#d2a8ff}.s-closed{background:#3fb95033;color:#3fb950}.s-failed{background:#f8514933;color:#f85149}.s-blocked{background:#f8514933;color:#f85149}
.tick{color:var(--muted);font-size:11px}.ok{color:var(--ok)}.warn{color:var(--warn)}.bad{color:var(--bad)}
#projctx{max-height:170px;overflow:auto;font-size:11px}.callout{background:#1f6feb14;border:1px solid var(--line);border-radius:6px;padding:6px 8px;font-size:11px}
#stream{height:240px;overflow:auto;border:1px solid var(--line);border-radius:6px;padding:8px;font-family:ui-monospace,Consolas,monospace;font-size:11px;background:#0d1117}
#history{max-height:150px;overflow:auto;font-size:11px}
.conn{font-size:11px;padding:2px 8px;border-radius:10px}
.conn-ok{background:#3fb95022;color:#3fb950}.conn-err{background:#f8514922;color:#f85149}.conn-warn{background:#d2992222;color:#e3b341}
hr{border:none;border-top:1px solid var(--line);margin:10px 0}
</style></head><body>
<header>
  <h1>🐈‍⬛ NAI Inquisitor</h1>
  <div class="topnav">
    <span class="badge" id="binver">binary…</span>
    <label style="margin:0">Project</label>
    <select id="proj_pick" onchange="openProject(this.value)"><option value="">— choose project —</option></select>
    <button class="alt" onclick="newProjectMode()">+ New Project</button>
    <span class="badge">SSE <span id="sse_state" class="conn conn-warn">connecting…</span></span>
  </div>
  <div class="sub">
    <span id="townsel">town ?</span> ·
    <span class="conn conn-ok" id="ctx_conn">control: ok</span>
  </div>
  <div id="tokenbar" style="display:flex;gap:6px;align-items:center">
    <input id="token" type="password" value="" placeholder="control token" autocomplete="off" style="width:180px"/>
    <button onclick="saveToken()">Save</button>
  </div>
</header>

<div class="grid">
  <!-- LEFT: New Project / active project header + context -->
  <div>
    <div class="panel" id="newproj_panel">
      <h2>New Project <span class="tick">import history then create & open</span></h2>
      <label>Project name</label>
      <input id="proj_name" placeholder="TaoAvatar Windows Native"/>
      <label>Engineering objective</label>
      <textarea id="proj_obj" rows="3" placeholder="e.g. Recover authentic TaoAvatar NNR execution semantics…"></textarea>
      <label>Windows filesystem repository path</label>
      <input id="proj_repo" placeholder="D:\path\to\repo  (git or folder)"/>
      <label>Import existing history (Markdown / JSON — raw, not summarised)</label>
      <textarea id="proj_ctx" rows="5" placeholder="Paste prior decisions, falsified hypotheses, constraints… (optional but recommended)"></textarea>
      <label>Link a Kilo session (optional)</label>
      <select id="proj_sel"><option value="">— load sessions —</option></select>
      <div style="display:flex;gap:6px;margin-top:6px">
        <button class="alt" onclick="loadRecallSessionsForProject()">↻ sessions</button>
        <button id="createopen" onclick="createAndOpenProject()">Create & Open Project</button>
      </div>
      <div id="proj_log" class="tick" style="margin-top:6px"></div>
    </div>

    <div class="panel" id="projheader_panel" style="display:none">
      <h2>Project <span id="projstatus" class="status s-open"></span></h2>
      <div id="proj_header" class="tick">(no project yet)</div>
      <hr/>
      <div>
        <label>Add primary Kanban task</label>
        <input id="task_title" placeholder="task objective"/>
        <div style="display:flex;gap:6px;margin-top:6px">
          <button onclick="addTask()">+ Add task</button>
          <button onclick="startAutonomous()">▶ Start autonomous loop</button>
        </div>
        <div id="task_log" class="tick" style="margin-top:6px"></div>
      </div>
    </div>
  </div>

  <!-- CENTER: Project Kanban + live stream -->
  <div>
    <div class="panel">
      <h2>Project Kanban <span id="projbeadcount" class="badge">0</span><span class="tick">Queued → Active → Verified (10/10)</span></h2>
      <table id="projkanban"><tr><th>Title</th><th>Status</th><th>Dispatch</th><th>Blocks→</th></tr></table>
    </div>
    <div class="panel">
      <h2>Live Engineering Stream <span class="tick">Mayor / Polecat / Inquisitor / Refinery</span></h2>
      <div id="stream">(connecting to /events…)</div>
    </div>
    <div class="panel">
      <h2>History <span class="tick">recent events</span></h2>
      <div id="history"></div>
    </div>
  </div>

  <!-- RIGHT: Inquisitor + Quality Oracle + Scientific Memory -->
  <div>
    <div class="panel">
      <h2>Inquisitor <span id="inq_model" class="badge">model…</span></h2>
      <div id="inq"></div>
      <div style="display:flex;gap:6px;margin-top:8px">
        <button onclick="inquisitorRunRun()">Run Inquisitor</button>
        <span class="tick" id="inqlog" style="align-self:center"></span>
      </div>
    </div>
    <div class="panel">
      <h2>Quality Oracle <span class="tick">acceptance gates</span></h2>
      <div id="oracle">(no active project)</div>
    </div>
    <div class="panel">
      <h2>Scientific Memory <span id="memcount" class="badge">0</span></h2>
      <div id="memory"></div>
    </div>
    <div class="panel">
      <h2>Kilo Recall <span class="tick">single source · optional session link</span></h2>
      <select id="recallsel"><option value="">— load sessions —</option></select>
      <button class="alt" onclick="recallSession(document.getElementById('recallsel').value)">Show context</button>
      <div id="recallctx" class="tick" style="max-height:120px;overflow:auto;margin-top:6px"></div>
    </div>
  </div>
</div>

<script>window.GT_TOKEN="__CTRL_TOKEN__";</script>
<script>
const $=id=>document.getElementById(id);
let currentToken=(typeof window.GT_TOKEN!=='undefined'&&window.GT_TOKEN)||localStorage.getItem('gt_token')||'';
$('token').value=currentToken;
function saveToken(){currentToken=$('token').value.trim();localStorage.setItem('gt_token',currentToken);$('tokenbar').style.border='1px solid var(--ok)';setTimeout(()=>{/* volatile */},600);}
function auth(){return currentToken?{'Authorization':'Bearer '+currentToken}:{};}
async function jget(u){const r=await fetch(u,{headers:auth()});return r.json();}
async function jpost(u,b){try{const r=await fetch(u,{method:'POST',headers:{'Content-Type':'application/json',...auth()},body:b?JSON.stringify(b):'{}'});const t=await r.text();try{return JSON.parse(t)}catch{return {raw:t,status:r.status}}}catch(e){return {error:String(e)}}}
const STATUS={'open':'s-open','in_progress':'s-inprogress','in_review':'s-inreview','closed':'s-closed','failed':'s-failed','blocked':'s-blocked'};
const badge=s=>`<span class="status ${STATUS[s]||'s-open'}">${s||''}</span>`;

let activeProject=localStorage.getItem('gt_active_project')||'';
let SSE=null;

async function refreshProjectPicker(){
  const r=await jpost('/project',{method:'gt.project.list',args:{}});
  const sel=$('proj_pick'); if(!sel)return;
  sel.innerHTML='<option value="">— choose project —</option>';
  (r&&r.projects||[]).forEach(p=>{const multi=document.querySelectorAll(`#proj_pick option[value="${p.project_id}"]`).length; if(multi===0){const o=document.createElement('option');o.value=p.project_id;o.textContent=p.name+' ('+p.project_id.slice(0,8)+')';sel.appendChild(o);}});
  if(activeProject && [...sel.options].some(o=>o.value===activeProject)) sel.value=activeProject;
  else if(activeProject){} 
}

async function openProject(id){
  if(!id)return;
  activeProject=id; localStorage.setItem('gt_active_project',id);
  $('newproj_panel').style.display='none'; $('projheader_panel').style.display='block';
  await Promise.all([refreshProjectHeader(),refreshProjectKanban(),renderInquisitor(),renderOracle(),renderMemory()]);
  startSSE();
}

function newProjectMode(){
  activeProject=''; localStorage.removeItem('gt_active_project');
  $('newproj_panel').style.display='block'; $('projheader_panel').style.display='none';
  $('proj_pick').value='';
}

async function createAndOpenProject(){
  const name=$('proj_name').value.trim();
  if(!name){$('proj_log').innerHTML='<span class="bad">project name required</span>';return;}
  const arms={
    method:'gt.project.create',
    args:{
      name,
      objective:$('proj_obj').value.trim(),
      repo_path:$('proj_repo').value.trim(),
      session_links:($('proj_sel').value?[$('proj_sel').value]:[]),
      imported_messages:parseImportedCtx($('proj_ctx').value)
    }
  };
  const r=await jpost('/project',arms);
  if(r&&r.ok){
    $('proj_log').innerHTML=`<span class="ok">created ${r.project_id} · ${r.imported_messages} raw msgs — opening…</span>`;
    localStorage.setItem('gt_active_project',r.project_id);
    await refreshProjectPicker();
    const sel=$('proj_pick'); if(sel){sel.value=r.project_id;}
    activeProject=r.project_id;
    await openProject(r.project_id);
  } else {
    $('proj_log').innerHTML=`<span class="bad">${r&&r.error||'create failed'}</span>`;
  }
}
function parseImportedCtx(text){
  const t=(text||'').trim(); if(!t)return [];
  try{const v=JSON.parse(t); if(Array.isArray(v))return v.filter(x=>x&&x.text).map(x=>({role:x.role||'user',text:x.text}));}catch(e){}
  return t.split(/\n+/).filter(x=>x.trim()).map((x,i)=>({role:i%2===0?'user':'assistant',text:x.trim()}));
}

async function refreshProjectHeader(){
  const el=$('proj_header'); if(!activeProject){el.textContent='(no active project)';return;}
  const r=await jpost('/project',{method:'gt.project.get',args:{project_id:activeProject}});
  if(!(r&&r.ok)){el.innerHTML=`<span class="bad">${r&&r.error||'load failed'}</span>`;return;}
  const p=r.project;
  $('projstatus').textContent=p.active_bead_id?'active':'idle';
  $('projstatus').className='status '+(p.active_bead_id?'s-inprogress':'s-open');
  const convoy=(r.convoys||[])[0];
  el.innerHTML=`<b>${p.name}</b><br/>`+
    `<div class="tick">obj: ${p.objective||'(none)'}</div>`+
    `<div class="tick mono">repo: ${p.repo_path||''}</div>`+
    `convoy: ${convoy?convoy.bead_id.slice(0,12):'—'} · active primary: ${p.active_bead_id?p.active_bead_id.slice(0,12):'—'}<br/>`+
    `gpu quarantine: ${(p.gpu_quarantine||[]).join(', ')||'none'}`;
  $('task_log').textContent='';
}

async function refreshProjectKanban(){
  const tb=$('projkanban');
  if(!activeProject){tb.innerHTML='<tr><th>Title</th><th>Status</th><th>Dispatch</th><th>Blocks→</th></tr><tr><td colspan=4 class="tick">open a project (Create & Open or choose one above)</td></tr>';$('projbeadcount').textContent='0';return;}
  const r=await jpost('/project',{method:'gt.project.view',args:{project_id:activeProject}});
  let beads=[];const blocks=[];let active='';
  if(r&&r.ok){active=r.active_bead_id||'';(r.convoys||[]).forEach(c=>{(c.beads||[]).forEach(b=>beads.push(b));(c.blocks_edges||[]).forEach(e=>blocks.push(e));});}
  $('projbeadcount').textContent=beads.length;
  const awaitLabel={};blocks.forEach(e=>awaitLabel[e.bead]=e.awaits);
  const done=(r&&r.ok)?(r.completed_primary_beads||[]):[];
  tb.innerHTML='<tr><th>Title</th><th>Status</th><th>Dispatch</th><th>Blocks→</th></tr>'+
    (beads.length?beads.slice().reverse().map(b=>{
      const da=b.dispatch_attempts||0;
      const mark=(b.bead_id===active)?' ⚡active':(done.includes(b.bead_id)?' ✓10/10':'');
      const aw=awaitLabel[b.bead_id];
      return `<tr><td>${b.title||'(untitled)'}${mark}<span class="tick">${b.bead_id.slice(0,8)}</span></td><td>${badge(b.status)}</td><td>${da}x</td><td class="mono">${aw?aw.slice(0,8):'-'}</td></tr>`;
    }).join(''):'<tr><td colspan=4 class="tick">no beads yet — add tasks below or run Mayor</td></tr>');
}

async function addTask(){
  const title=$('task_title').value.trim();
  if(!activeProject){$('task_log').innerHTML='<span class="bad">open a project first</span>';return;}
  if(!title){$('task_log').innerHTML='<span class="bad">task objective required</span>';return;}
  const r=await jpost('/project',{method:'gt.project.add_primary',args:{project_id:activeProject,title:title}});
  $('task_log').innerHTML = r&&r.ok?`<span class="ok">task added ${r.bead_id?r.bead_id.slice(0,8):''}</span>`:`<span class="bad">${r&&r.error||'add failed'}</span>`;
  refreshProjectHeader(); refreshProjectKanban();
}
async function startAutonomous(){
  if(!activeProject){$('task_log').innerHTML='<span class="bad">open a project first</span>';return;}
  // start existing convoy(s) for the project + tick to dispatch the first primary
  const v=await jpost('/project',{method:'gt.project.view',args:{project_id:activeProject}});
  let started=0;
  for(const c of (v&&v.convoys||[])){
    const s=await jpost('/start_convoy',{convoy_id:c.convoy_id});
    if(s&&s.started)started++;
  }
  const tick=await jpost('/tick',{});
  $('task_log').innerHTML=`<span class="ok">started ${started} convoy(s); tick applied ${(tick&&tick.applied)||0}</span>`;
  refreshProjectKanban();
}
async function findEffectiveRig(){
  const d=await jget('/dashboard');
  if(d&&d.rigs&&d.rigs.length)return d.rigs[0].rig_id;
  // fallback: project rig_ids
  return '';
}

// ---- Inquisitor + Oracle + Memory ----
async function renderInquisitor(){
  const el=$('inq'); if(!el)return;
  el.innerHTML='(loading…)';
  const r=await jpost('/inquisitor',{});
  if(!(r&&r.verdict)){el.innerHTML=`<span class="bad">${r&&r.error||'state unavailable'}</span>`;return;}
  $('inq_model').textContent='model: gpt-6-luna / deepseek';
  const gates=r.quality_gates||[];
  const passed=gates.filter(g=>g.status==='PASS').length;
  let h=`<div>verdict <b>${r.verdict}</b> · quality <b>${passed}/${gates.length}</b> · 10/10 ${passed===gates.length?'<span class="ok">MET</span>':'<span class="warn">not yet</span>'}</div>`;
  if(r.request_provenance&&r.request_provenance[0]){const pv=r.request_provenance[0];h+=`<div class="tick">last review ${new Date(pv.ts_unix_ms).toLocaleTimeString()} · ctx=${pv.project_context_present?'✓':'–'} · req ${(pv.request_hash||'').slice(0,10)}…</div>`;}
  h+=`<div class="tick">findings: ${(r.scientific_findings||[]).length} · rejected: ${(r.rejected_claims||[]).length} · facts: ${(r.verified_facts||[]).length}</div>`;
  if((r.scientific_findings||[]).length){h+='<div class="tick" style="margin-top:4px">'+r.scientific_findings.slice(-3).map(f=>'• '+(f.claim||'').slice(0,70)).join('<br/>')+'</div>';}
  el.innerHTML=h;
}
async function renderOracle(){
  const el=$('oracle'); if(!activeProject){el.textContent='(open a project to see its gates)';return;}
  const r=await jpost('/inquisitor',{});
  const gates=(r&&r.quality_gates)||[];
  if(!gates.length){el.innerHTML='<span class="tick">no acceptance gates reported yet — run the Inquisitor</span>';return;}
  el.innerHTML='<table><tr><th>Gate</th><th>Status</th><th>Evidence</th></tr>'+gates.map(g=>`<tr><td>${g.id}</td><td>${badge(g.status)}</td><td class="mono">${(g.evidence||'').slice(0,26)}</td></tr>`).join('')+'</table>';
}
async function renderMemory(){
  const el=$('memory'); if(!el)return;
  const r=await jpost('/inquisitor',{});
  const facts=(r&&r.verified_facts)||[]; const rej=(r&&r.rejected_claims)||[]; const find=(r&&r.scientific_findings)||[];
  $('memcount').textContent=find.length;
  let h='';
  if(facts.length){h+='<div class="ok">verified facts</div>'+facts.slice(-5).map(f=>`<div class="tick">• ${f.slice(0,80)}</div>`).join('');}
  if(rej.length){h+='<div class="warn" style="margin-top:6px">falsified / rejected hypotheses</div>'+rej.slice(-5).map(x=>`<div class="tick">• ${x.slice(0,80)}</div>`).join('');}
  if(find.length){h+='<div style="margin-top:6px">permanent findings</div>'+find.slice(-6).map(f=>`<div class="tick">• [${f.kind}] ${(f.claim||'').slice(0,72)}</div>`).join('');}
  el.innerHTML=h||'<span class="tick">no scientific memory yet</span>';
}
async function inquisitorRunRun(){
  if(!activeProject){$('inqlog').textContent='open a project first';return;}
  $('inqlog').textContent='running…';
  const r=await jpost('/inquisitor/run',{objective:'verify current primary bead per project context',hook:'onQualityGate',task:{phase:'acceptance'},project_id:activeProject,provider:'stub'});
  $('inqlog').innerHTML = r&&r.ok?`<span class="ok">${r.verdict} · ${r.action} · ${r.quality_passed}/${r.quality_total}</span>`:`<span class="bad">${r&&r.error||'run failed'}</span>`;
  renderInquisitor();renderOracle();renderMemory();
}

// ---- Single-source Kilo session picker ----
async function loadRecallSessionsForProject(){
  const sel=$('proj_sel'); if(!sel)return;
  sel.innerHTML='<option value="">loading…</option>';
  const r=await jget('/recall');
  if(r&&r.ok&&Array.isArray(r.sessions)){
    sel.innerHTML='<option value="">— link a session (optional) —</option>'+r.sessions.map(s=>`<option value="${s.id}">${s.title||s.id.slice(0,8)} (${s.message_count} msgs)</option>`).join('');
    // sync the single Recall selector too
    const rs=$('recallsel'); if(rs)rs.innerHTML='<option value="">— pick a session —</option>'+r.sessions.map(s=>`<option value="${s.id}">${s.title||s.id.slice(0,8)}</option>`).join('');
  } else {
    sel.innerHTML='<option value="">— session provider unavailable —</option>';
    const rs=$('recallsel'); if(rs)rs.innerHTML='<option value="">— session provider unavailable —</option>';
  }
}
async function recallSession(id){
  const el=$('recallctx'); if(!id){el.innerHTML='<span class="tick">pick a session to see its context</span>';return;}
  el.textContent='loading…';
  const r=await jget('/recall/'+encodeURIComponent(id));
  el.innerHTML = (r&&r.ok&&Array.isArray(r.messages))? r.messages.slice().reverse().slice(0,10).map(m=>`<div><b>${m.role}</b> ${(m.text||'').slice(0,120)}</div>`).join('') : `<span class="bad">${(r&&r.error)||'unavailable'}</span>`;
}

// ---- SSE via fetch stream (EventSource cannot send Authorization header) ----
let SSE_CTRL=null;
async function startSSE(){
  if(SSE_CTRL){SSE_CTRL.abort();SSE_CTRL=null;}
  const st=$('sse_state');
  st.textContent='connecting…'; st.className='conn conn-warn';
  const label=(t)=>{st.textContent=t;};
  try{
    const resp=await fetch('/events',{headers:{'Authorization':'Bearer '+currentToken}});
    if(resp.status===401){label('auth required');st.className='conn conn-err';scheduleSSE(3000);return;}
    if(!resp.ok||!resp.body){label('sse '+resp.status);st.className='conn conn-err';scheduleSSE(3000);return;}
    label('connected'); st.className='conn conn-ok';
    const reader=resp.body.getReader();
    const dec=new TextDecoder();
    SSE_CTRL=new AbortController();
    let buf='';
    while(true){
      const {done,value}=await reader.read();
      if(done)break;
      buf+=dec.decode(value,{stream:true});
      let i;
      while((i=buf.indexOf('\n'))>=0){const line=buf.slice(0,i);buf=buf.slice(i+1);if(line.startsWith('data:')){onSSEMessage(line.slice(5).trim());}}
    }
    // stream ended → reconnect
    label('reconnecting…'); st.className='conn conn-err'; scheduleSSE(2000);
  }catch(e){
    label('reconnecting…'); st.className='conn conn-err'; scheduleSSE(2000);
  }
}
let sseTimer=null;
function scheduleSSE(ms){if(sseTimer)clearTimeout(sseTimer);sseTimer=setTimeout(startSSE,ms);}
function onSSEMessage(data){
  const box=$('stream'); if(!box)return;
  if(box.childElementCount>80){box.innerHTML='';}
  const d=document.createElement('div'); d.textContent=(data||'').slice(0,170); box.prepend(d);
}

async function init(){
  startSSE();
  await refreshProjectPicker();
  await loadRecallSessionsForProject();
  if(activeProject && $('proj_pick')&&[...$('proj_pick').options].some(o=>o.value===activeProject)){
    $('proj_pick').value=activeProject; await openProject(activeProject);
  } else {
    // no active project yet: keep SSE + stream running regardless
  }
}
document.querySelector('#tokenbar button')?.addEventListener('click',saveToken);
init();
</script>
</body></html>
"#;