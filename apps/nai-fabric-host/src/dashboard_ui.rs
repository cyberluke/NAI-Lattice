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
/* Kanban board */
#kanban .col{background:#0d111710;border:1px solid var(--line);border-radius:8px;padding:6px}
#kanban .kcol{display:flex;justify-content:space-between;align-items:center;padding:4px 6px;border-bottom:1px solid var(--line);margin-bottom:6px}
.card{background:#161b22;border:1px solid var(--line);border-radius:8px;padding:8px;margin-bottom:8px;position:relative}
.card .ctitle{font-weight:600;font-size:12px;line-height:1.3;margin-bottom:4px}
.card .cmeta{display:flex;gap:6px;align-items:center;flex-wrap:wrap;font-size:11px;color:var(--muted);margin-top:4px}
.card.stale{border-color:#f85149;box-shadow:0 0 0 1px #f8514933}
.card.active{border-color:var(--ok);box-shadow:0 0 0 1px var(--ok)}
.card .cbtn{background:#21262d;border:1px solid var(--line);border-radius:6px;padding:2px 6px;font-size:11px;cursor:pointer;margin-left:auto}
.card .cbtn:hover{filter:brightness(1.2)}
</style></head><body>
<header>
  <h1>🐈‍⬛ NAI Inquisitor</h1>
  <div class="topnav">
    <span class="badge" id="binver">binary…</span>
    <label style="margin:0">Project</label>
    <select id="proj_pick" onchange="openProject(this.value)"><option value="">— choose project —</option></select>
    <button class="alt" onclick="newProjectMode()">+ New Project</button>
    <span class="badge">SSE <span id="sse_state" class="conn conn-warn">idle</span></span>
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

<div class="panel" id="universal">
  <h2>Universal — all projects <span class="tick">every project + its active primary + gate progress (not just the selected one)</span></h2>
  <div id="uni_reload" style="display:flex;gap:8px;align-items:center;margin-bottom:6px">
    <button class="alt" onclick="renderUniversal()">↻ refresh</button>
    <span class="tick" id="uni_meta"></span>
  </div>
  <table id="uni_projects" style="margin-bottom:10px"><tr><th>Project</th><th>Active primary</th><th>Status</th><th>Gates (active)</th><th>Done count</th></tr></table>
  <div class="tick" style="margin:4px 0 2px">ALL primaries (across every convoy/project):</div>
  <table id="uni_primaries"><tr><th>Project</th><th>Primary</th><th>Status</th><th>Dispatch</th><th>Gates</th><th>Awaits (blocks)</th></tr></table>
</div>

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
      <div id="kanban" style="display:grid;grid-template-columns:repeat(3,1fr);gap:8px">
        <div class="col"><div class="kcol"><b>Queued</b> <span class="tick" id="kc_q"></span></div><div id="kq"></div></div>
        <div class="col"><div class="kcol"><b>Active</b> <span class="tick" id="kc_a"></span></div><div id="ka"></div></div>
        <div class="col"><div class="kcol"><b>Verified</b> <span class="tick" id="kc_v"></span></div><div id="kv"></div></div>
      </div>
    </div>
    <div class="panel">
      <h2>Live Engineering Stream <span class="tick">Mayor / Polecat / Inquisitor / Refinery</span></h2>
      <div id="livelihood" class="callout" style="margin-bottom:6px;display:flex;gap:10px;align-items:center;flex-wrap:wrap">
        <span class="conn conn-warn" id="liveness_state">seeking…</span>
        <span id="liveness_text" class="tick">(waiting for liveness signal)</span>
      </div>
      <div id="stream" data-placeholder="1"><span class="dim">Live engineering stream idle — transitions appear here (Polecat / Inquisitor / Mayor).</span></div>
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

<!-- Human resolver: human actions for a blocked/failed/active primary -->
<div id="resolver" style="display:none;position:fixed;inset:0;background:#000a;z-index:1100;align-items:center;justify-content:center">
  <div style="background:var(--panel);border:1px solid var(--line);border-radius:10px;max-width:820px;width:94%;max-height:88vh;display:flex;flex-direction:column;padding:14px">
    <div style="display:flex;justify-content:space-between;align-items:center;border-bottom:1px solid var(--line);padding-bottom:8px;margin-bottom:8px"><b>Resolve task <span id="rs_title" class="tick"></span></b><button class="alt" onclick="document.getElementById('resolver').style.display='none'">✕</button></div>
    <div id="rs_body" style="overflow:auto;flex:1"></div>
    <div id="rs_log" class="tick" style="margin-top:8px;border-top:1px solid var(--line);padding-top:6px"></div>
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
    `<div class="tick mono">roots: ${(p.resource_roots&&p.resource_roots.length)?p.resource_roots.join(' ; '):'(none — add resource_roots)'}</div>`+
    `convoy: ${convoy?convoy.bead_id.slice(0,12):'—'} · active primary: ${p.active_bead_id?p.active_bead_id.slice(0,12):'—'}<br/>`+
    `gpu quarantine: ${(p.gpu_quarantine||[]).join(', ')||'none'}`;
  $('task_log').textContent='';
}

async function refreshProjectKanban(){
  const kb=$('kanban');
  if(!activeProject){kb.innerHTML='<span class="tick">open a project (Create & Open or choose one above)</span>';$('projbeadcount').textContent='0';return;}
  const r=await jpost('/project',{method:'gt.project.view',args:{project_id:activeProject}});
  let beads=[];const blocks=[];let active='';
  if(r&&r.ok){active=r.active_bead_id||'';(r.convoys||[]).forEach(c=>{(c.beads||[]).forEach(b=>beads.push(b));(c.blocks_edges||[]).forEach(e=>blocks.push(e));});}
  $('projbeadcount').textContent=beads.length;
  const awaitLabel={};blocks.forEach(e=>awaitLabel[e.bead]=e.awaits);
  const done=(r&&r.ok)?(r.completed_primary_beads||[]):[];
  const card=(b,flavor)=>{
    const da=b.dispatch_attempts||0;
    const status=b.status||'';
    const isActive=(b.bead_id===active);
    // active+failed confusion → show "⚠ blocked" not a green ⚡
    const stateTag = isActive
      ? (status==='in_progress'||status==='in_review' ? ' <span class="ok">● active</span>'
         : status==='failed' ? ' <span class="bad">● active/blocked</span>'
         : ' <span class="ok">● active</span>')
      : (done.includes(b.bead_id) ? ' <span class="ok">✓10/10</span>':'');
    const aw=awaitLabel[b.bead_id];
    const stale = (status==='in_review'||status==='in_progress') && da>=5 ? 'stale' : (isActive&&status==='failed'?'stale':'');
    const body=(b.body||'').replace(/\\n/g,' ').slice(0,90);
    return `<div class="card ${isActive&&(status==='in_progress'||status==='in_review')?'active':''} ${stale}">
      <div class="ctitle">${(b.title||'(untitled)')}${stateTag}</div>
      <div class="cmeta"><span>${badge(status)}</span><span>${da}x</span>${aw?`<span class="mono">blocks→${aw.slice(0,8)}</span>`:''}</div>
      ${body?`<div class="tick" style="margin-top:4px;color:var(--muted)">${body}</div>`:''}
      <div class="cmeta"><span class="mono">${b.bead_id.slice(0,8)}</span><button class="cbtn" onclick="viewBeadMessages('${b.bead_id}','${(b.title||'').replace(/'/g,'')}')">view messages</button><button class="cbtn" style="background:#3fb95033;border-color:#3fb95066" onclick="openResolver('${b.bead_id}','${(b.title||'').replace(/'/g,'')}')">resolve</button></div>
    </div>`;
  };
  const q=[],a=[],v=[];
  beads.forEach(b=>{
    if(b.status==='closed'){v.push(b);}
    else if(b.status==='in_progress'||b.status==='in_review'){a.push(b);}
    else if(b.status==='open'||b.status==='failed'){a.push(b);} // active primary or failed lands in Active (so you see it)
    else q.push(b);
  });
  const fill=(el,arr,hint)=>{el.innerHTML=arr.length?arr.map(x=>card(x)).join(''):`<span class="tick">${hint}</span>`;return arr.length;};
  fill(document.getElementById('kq'),q,'nothing queued');
  const nA=fill(document.getElementById('ka'),a,'nothing in active flight');
  fill(document.getElementById('kv'),v,'none verified yet');
  document.getElementById('kc_q').textContent=q.length;
  document.getElementById('kc_a').textContent=nA;
  document.getElementById('kc_v').textContent=v.length;
  // Live tick: ages/staleness are re-rendered on each refresh.
  refreshKanbanAges();
}
// Age each card's state via a lightweight 2s timer (liveness observability).
function refreshKanbanAges(){/* ages are driven by the liveness envelope / stream; keep hook */}

// View the messages tied to a bead: its objective body + the project's linked
// Kilo session transcript(s). Surface as a modal so you can read what a task is
// about and what discussion/evidence exists behind it.
async function viewBeadMessages(beadId,title){
  let modal=document.getElementById('bead_modal');
  if(!modal){modal=document.createElement('div');modal.id='bead_modal';
    modal.style.cssText='position:fixed;inset:0;background:#000a;z-index:1000;display:flex;align-items:center;justify-content:center;';
    modal.addEventListener('click',e=>{if(e.target===modal){modal.style.display='none';}});
    modal.innerHTML='<div style="background:var(--panel);border:1px solid var(--line);border-radius:10px;max-width:860px;width:92%;max-height:86vh;display:flex;flex-direction:column;padding:14px">'
      +'<div style="display:flex;justify-content:space-between;align-items:center;border-bottom:1px solid var(--line);padding-bottom:8px;margin-bottom:8px"><b id="bm_title"></b><button class="alt" onclick="document.getElementById(\'bead_modal\').style.display=\'none\'">✕</button></div>'
      +'<div id="bm_body" style="overflow:auto;flex:1;font-size:12px"></div></div>';
    document.body.appendChild(modal);
  }
  document.getElementById('bm_title').textContent=title||beadId.slice(0,8);
  document.getElementById('bm_body').innerHTML='<span class="tick">loading messages…</span>';
  modal.style.display='flex';
  // The bead's own objective + any metadata
  const r=await jpost('/project',{method:'gt.project.view',args:{project_id:activeProject}});
  let bead=null;if(r&&r.ok){r.convoys.forEach(c=>(c.beads||[]).forEach(b=>{if(b.bead_id===beadId)bead=b;}));}
  let h='';
  if(bead){
    h+=`<div class="callout" style="margin-bottom:8px"><b>Objective:</b> ${bead.body||'(none)'}</div>`;
    const gates=(bead.metadata&&bead.metadata.bt_gates)||[];
    if(gates.length){h+='<div class="tick">Gates:'+gates.map(g=>` <span class="${g.status==='PASS'?'ok':'warn'}">${g.id}:${g.status==='PASS'?'✓':'~'}</span>`).join('')+'</div>';}
    h+='<div style="margin-top:6px" class="tick">status '+badge(bead.status)+' · dispatch '+bead.dispatch_attempts+'x</div>';
  }
  // The project's ON-TOPIC transcript: durable imported evidence (from the
  // project's resource roots), NOT the raw /recall session list — so off-topic
  // workspaces (e.g. the Gas Town dev repo) never leak under a project bead.
  try{
    if(activeProject){
      const bc=await jpost('/project',{method:'gt.project.bootstrap_context',args:{project_id:activeProject}});
      const ctx=(bc&&bc.bootstrap&&bc.bootstrap.imported_context_messages)||[];
      const proj=(bc&&bc.bootstrap&&bc.bootstrap.project)||{};
      if(ctx.length){
        h+='<div style="margin-top:10px"><b>Project evidence context</b> <span class="tick">(from resource roots only — no off-topic workspaces)</span></div>';
        // Show most recent first; label each by source + hash.
        ctx.slice().reverse().forEach(m=>{
          const rootOk=(m.source||'').startsWith('file:')||(m.source||'').startsWith('import:');
          const head=(m.text||'').replace(/\\n/g,' ').slice(0,160);
          h+=`<div style="margin-top:6px;padding-left:6px;border-left:3px solid ${m.role==='user'?'#58a6ff':'#3fb950'}">
            <div class="tick">${m.source||m.role} · seq ${m.seq} · <span class="mono">${(m.hash||'').slice(0,12)}</span> ${rootOk?'<span class="ok">in-project</span>':'<span class="bad">unverified source</span>'}</div>
            <div class="tick" style="color:var(--fg)">${m.role==='user'?'🧑':'🤖'} ${head}${((m.text||0).length>160)?'…':''}</div>
          </div>`;
        });
      } else {
        h+='<div class="tick" style="margin-top:8px">no on-topic project evidence yet</div>';
      }
    } else {
      h+='<div class="tick" style="margin-top:8px">open a project first</div>';
    }
  }catch(e){ h+='<div class="bad">evidence context failed: '+String(e)+'</div>'; }
  document.getElementById('bm_body').innerHTML=h||'<span class="tick">no details</span>';
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
let SSE_LAST_ID=0; // durable event-log cursor: replay missed events on reconnect
async function startSSE(){
  if(SSE_CTRL){SSE_CTRL.abort();SSE_CTRL=null;}
  const st=$('sse_state');
  st.textContent='connecting…'; st.className='conn conn-warn';
  const label=(t)=>{st.textContent=t;};
  try{
    const headers={'Authorization':'Bearer '+currentToken};
    if(SSE_LAST_ID>0)headers['Last-Event-ID']=String(SSE_LAST_ID);
    const resp=await fetch('/events',{headers});
    if(resp.status===401){label('auth required');st.className='conn conn-err';scheduleSSE(3000);return;}
    if(!resp.ok||!resp.body){label('sse '+resp.status);st.className='conn conn-err';scheduleSSE(3000);return;}
    // Connected: the transport is live — the placeholder is cleared, heartbeat
    // is only liveness, and any `transition`s update the stream below.
    label('live'); st.className='conn conn-ok';
    const streamEl=$('stream');
    if(streamEl && streamEl.dataset.placeholder && streamEl.children.length===0){streamEl.innerHTML='';delete streamEl.dataset.placeholder;}
    const reader=resp.body.getReader();
    const dec=new TextDecoder();
    SSE_CTRL=new AbortController();
    let buf='';
    let evType=''; let evId=''; let evData=null;
    const flush=()=>{
      if(evType==='transition'||evType==='events'||evType==='sync'||evType==='liveness'){
        if(evId){const n=parseInt(evId,10); if(!isNaN(n)&&n>SSE_LAST_ID)SSE_LAST_ID=n;}
        if(evData!=null){onSSEMessage(evType,evData);}
        if(evType==='sync'){ // heartbeat: liveness only, no stream entry needed
        }
      }
      evType=''; evId=''; evData=null;
    };
    while(true){
      const {done,value}=await reader.read();
      if(done)break;
      buf+=dec.decode(value,{stream:true});
      let i;
      while((i=buf.indexOf('\n'))>=0){const line=buf.slice(0,i).replace(/\r$/,'');buf=buf.slice(i+1);
        if(!line){flush();continue;}
        if(line.startsWith('event:')){evType=line.slice(6).trim();}
        else if(line.startsWith('id:')){evId=line.slice(3).trim();}
        else if(line.startsWith('data:')){evData=line.slice(5).trim();}
      }
    }
    flush();
    // stream ended → reconnect (replays from SSE_LAST_ID)
    label('reconnecting…'); st.className='conn conn-err'; scheduleSSE(2000);
  }catch(e){
    label('reconnecting…'); st.className='conn conn-err'; scheduleSSE(2000);
  }
}
let sseTimer=null;
function scheduleSSE(ms){if(sseTimer)clearTimeout(sseTimer);sseTimer=setTimeout(startSSE,ms);}
// Idempotent render: dedupe by event_id so replayed transitions never duplicate.
let SSE_SEEN=new Set();
// Liveness observability: last liveness payload + a live timer that ages items.
let LSSE=null; const AGED=new Date().getTime();
function fmtAgo(ms){ if(!ms&&ms!==0)return ''; const s=Math.max(0,Math.floor(ms/1000)); if(s<2)return 'just now'; if(s<120)return s+'s ago'; const m=Math.floor(s/60); if(m<120)return m+'m ago'; const h=Math.floor(m/60); return h+'h ago'; }
function setLivenessState(cls,txt){const el=$('liveness_state');if(el){el.textContent=txt;el.className='conn '+cls;}}
function renderLiveness(obj){
  const txt=$('liveness_text'); if(!txt)return;
  const inF=(obj&&obj.beads)||[]; const now=obj&&obj.now_epoch_ms||Date.now();
  if(!inF.length){ setLivenessState('conn-ok','idle'); txt.innerHTML='<span class="ok">● all work settled</span> — no bead in progress/in_review right now.'; LSSE={state:'idle'}; return; }
  // Age the in-flight items; flag any that have been stuck in-state too long.
  const ordered=inF.slice().sort((a,b)=>(b.in_state_ms||0)-(a.in_state_ms||0));
  const oldest=ordered[0]; const oldestAge=(oldest.in_state_ms||0);
  const blue=oldest.in_state_ms<60000;
  const newestSeen = now - (obj.lastSeenEpoch||now);
  const actives=ordered.map(b=>`<span title="${b.bead_id} (${b.rig})" class="${b.status==='in_review'?'ok':'warn'}">${b.title?b.title.split(' ')[0]:''}#${(b.bead_id||'').slice(0,8)} ${fmtAgo(b.in_state_ms)}</span>`).join(' · ');
  setLivenessState(blue?'conn-ok':'conn-warn', `▲ ${inF.length} in flight`);
  const warn= inF.some(b=>b.in_state_ms>90000) ? '<span class="bad">↻ stalled? 90s+ no transition</span>' : '<span class="ok">✔ advancing</span>';
  txt.innerHTML=`oldest ${oldest.title?('“'+oldest.title.split('').slice(0,24).join('')+'”'):''} ${fmtAgo(oldestAge)} ${blue?'':'(stale)'} · ${actives} · ${warn}`;
  LSSE={state: inF.length?'running':(obj.any_advancing?'running':'idle')};
}
function onSSEMessage(evType,data){
  let obj=null; try{obj=JSON.parse(data);}catch(e){obj=null;}
  if(!obj)return;
  // Liveness frames update the observability status line (not the stream box).
  if(evType==='liveness'||obj.type==='liveness'){
    renderLiveness(obj);
    // Refresh ages every 2s while a frame is present (client-side ticking).
    if(!window.__liveTimer){window.__liveTimer=setInterval(()=>{ if(LSSE&&LSSE.state==='running'){renderLiveness(LSSE._last||{beads:[]});} },2000);}
    LSSE._last=obj;
    return;
  }
  const box=$('stream'); if(!box)return;
  // `sync` and `events` are SSE bookkeeping/backlog frames — they advance the
  // reconnect cursor but must NEVER appear as stream rows. Only `transition`
  // (derived engineering events) belong in the visible stream.
  if(evType==='sync'||evType==='events') return;
  if(obj.event_id){
    if(SSE_SEEN.has(obj.event_id))return; // idempotent on reconnect
    SSE_SEEN.add(obj.event_id); if(SSE_SEEN.size>200)SSE_SEEN=new Set([...SSE_SEEN].slice(-100));
  }
  if(box.childElementCount>80){box.innerHTML='';box.dataset.placeholder='1';}
  const et=obj.event_type||evType||'sync';
  const kindClass=['POLECAT_STARTED','POLECAT_TOOL_EXECUTED','POLECAT_COMPLETED','EXECUTION_FAILED','EXECUTION_BLOCKED','EXECUTION_STARTED'].includes(et)?'tw-polecat'
    :(et==='INQUISITOR_REVIEWED'?'tw-inq':(et==='CORRECTION_CREATED'||et==='MAYOR_DISPATCHED'?'tw-corr':(et==='QUALITY_GATE_UPDATED'||et==='EVIDENCE_INGESTED'?'tw-ev':'tw-sync')));
  const d=document.createElement('div');
  d.className='tw '+kindClass;
  const primary=(obj.primary_bead_id||'').slice(0,8);
  const exec=(obj.execution_bead_id||'').slice(0,8);
  const ago=obj.ts_epoch_ms?fmtAgo(Date.now()-obj.ts_epoch_ms):'';
  d.innerHTML=`<b>${et}</b> ${primary?'<span class="mono">#'+primary+'</span>':''}${exec?'<span class="mono">→'+exec+'</span>':''} <span class="mono">a${obj.attempt_id??''}</span> <span class="tw-sum">${(obj.summary||'').slice(0,120)}</span> ${ago?`<span class="tick" style="color:var(--muted)">${ago}</span>`:''}`;
  d.dataset.ts=obj.ts_epoch_ms||'';
  box.prepend(d);
  // Refresh the age labels every 2s (no full re-render, just the visible stream).
  if(!window.__streamTimer){window.__streamTimer=setInterval(()=>{Array.prototype.forEach.call(box.children,(el)=>{if(!el.dataset||!el.dataset.ts)return;const t=el.querySelector('.tick');if(t)t.textContent=fmtAgo(Date.now()-parseInt(el.dataset.ts,10));});},2000);}
}

// ---- Universal dashboard: ALL projects + ALL primaries + gate progress ----
async function renderUniversal(){
  const pj=$('uni_projects'), pr=$('uni_primaries'), meta=$('uni_meta');
  if(!pj||!pr)return;
  pj.innerHTML='<tr><th>Project</th><th>Active primary</th><th>Status</th><th>Gates (active)</th><th>Done count</th></tr>';
  pr.innerHTML='<tr><th>Project</th><th>Primary</th><th>Status</th><th>Dispatch</th><th>Gates</th><th>Awaits (blocks)</th></tr>';
  if(meta)meta.textContent='…';
  const r=await jpost('/project',{method:'gt.dashboard.universal',args:{}});
  if(!(r&&r.ok&&Array.isArray(r.projects))){pj.innerHTML+='<tr><td colspan="5" class="bad">'+(r&&r.error||'unavailable')+'</td></tr>';if(meta)meta.textContent='error';return;}
  if(meta)meta.textContent=`${r.project_count} projects · ${(r.all_primaries||[]).length} primaries · last verdict ${(r.inquisitor&&r.inquisitor.verdict)||'…'}`;
  // Project rows
  for(const p of r.projects){
    const activeId=(p.active_bead_id||'');
    const act = activeId? (p.primaries||[]).find(b=>b.bead_id===activeId):null;
    const gatesN=(p.active_gate_total||0), gatesP=(p.active_gate_passed||0);
    const done=p.completed_primary_beads?p.completed_primary_beads.length:0;
    const s = act?(act.status||''):(r.inquisitor&&r.inquisitor.verdict)||'';
    const sg = gatesN? (gatesP===gatesN?'<span class="ok">'+gatesP+'/'+gatesN+' MET</span>':'<span class="warn">'+gatesP+'/'+gatesN+'</span>') : '<span class="tick">—</span>';
    pj.innerHTML+=`<tr><td><b>${p.name}</b><div class="tick mono">${(p.project_id||'').slice(0,20)}</div>${p.resource_roots&&p.resource_roots.length?`<div class="tick" style="color:var(--muted)">roots: ${p.resource_roots.map(r=>{const n=r.split(/[\\/]/).filter(Boolean).pop()||r;return n;}).join(', ')}</div>`:''}<div class="tick mono">${(p.repo_path||'').slice(0,46)}</div></td>`
      +`<td>${act?act.title:'<span class="tick">(none)</span>'}</td>`
      +`<td>${badge(s)}</td><td>${sg}</td><td>${done}</td></tr>`;
    // Primaries of this project
    for(const b of (p.primaries||[])){
      const gates=(b.metadata&&b.metadata.bt_gates)||[];
      const pass=gates.filter(g=>g.status==='PASS').length;
      const chips=gates.map(g=>`<span title="${g.evidence||''}" style="margin-right:3px" class="${g.status==='PASS'?'ok':'warn'}">${(g.id||'').slice(0,16)}:${g.status==='PASS'?'✓':'~'}</span>`).join('');
      const blocks=(p.convoys||[]).flatMap(c=>(c.blocks_edges||[])).filter(e=>e.bead===b.bead_id).map(e=>e.awaits||'').join(',')||'';
      pr.innerHTML+=`<tr><td class="tick">${p.name}</td><td>${b.title}${b.bead_id===activeId?' <span class="ok">(active)</span>':''}</td>`
        +`<td>${badge(b.status)}</td><td>${b.dispatch_attempts}</td>`
        +`<td>${gates.length? (pass===gates.length?'<span class="ok">'+pass+'/'+gates.length+' ✓</span>':'<span class="warn">'+pass+'/'+gates.length+'</span>')+' '+chips :'<span class="tick">no gates</span>'}</td>`
        +`<td class="mono">${blocks||''}</td></tr>`;
    }
  }
}

// ---- Human resolver: re-issue corrective / mark gate / advance serial / roots ----
let RS={beadId:'',title:''};
async function openResolver(beadId,title){
  RS.beadId=beadId; RS.title=title||'';
  document.getElementById('rs_title').textContent=(title||beadId.slice(0,8));
  const m=document.getElementById('resolver'); m.style.display='flex';
  // Load the bead's current gates (from project view) so the human can edit them.
  let gates=[]; 
  if(activeProject){ const r=await jpost('/project',{method:'gt.project.view',args:{project_id:activeProject}});
    if(r&&r.ok) r.convoys.forEach(c=>(c.beads||[]).forEach(b=>{if(b.bead_id===beadId) gates=(b.metadata&&b.metadata.bt_gates)||[];})); }
  const gateCk=gates.length?gates.map((g,i)=>`<div style="margin:2px 0;display:flex;gap:6px;align-items:center"><input type="checkbox" id="rs_gate_${i}" data-id="${g.id.replace(/"/g,'')}" ${g.status==='PASS'?'checked':''}/><b>${g.id}</b> <span class="tick">${g.status}</span><input id="rs_ev_${i}" placeholder="evidence (executed)" style="flex:1" value="${(g.evidence||'').replace(/"/g,'')}"/></div>`).join(''):'<span class="tick">no gates attached to this bead yet</span>';
  document.getElementById('rs_body').innerHTML=`
    <div class="callout" style="margin-bottom:10px"><b>${beadId.slice(0,8)}</b> — blocked: ${RS.title||''}</div>
    <div class="tick" style="margin:6px 0 3px">1) Re-issue a corrective (new hypothesis) → sends a fresh engineer to redo work with your instructions</div>
    <div style="display:flex;gap:6px;align-items:center;margin-bottom:8px">
      <input id="rs_instr" placeholder="new hypothesis / instructions for the engineer (default uses the task objective)" style="flex:1"/>
      <button onclick="rsCorrective()">▸ dispatch corrective</button>
    </div>
    <div class="tick" style="margin:6px 0 3px">2) Manually mark gates PASS (only when you have real executed evidence; the loop's oracle also checks)</div>
    <div id="rs_gates" style="margin-bottom:8px">${gateCk}</div>
    <button onclick="rsMarkGates()">✓ save gate status</button>
    <div class="tick" style="margin:10px 0 3px">3) Advance the serial gate — make THIS the active primary / the next to run</div>
    <button onclick="rsSetActive()">▶ set as active primary</button>
    <div class="tick" style="margin:10px 0 3px">4) Edit the project's allowed resource roots (multi-repo)</div>
    <div style="display:flex;gap:6px;align-items:center">
      <input id="rs_roots" placeholder="comma-separated roots, e.g. D:\_SATIN_AI\MNN, D:\_SATIN_AI\TaoAvatar-NNR-MNN" style="flex:1"/>
      <button onclick="rsSetRoots()">save roots</button>
    </div>
    `;
}
async function rsCorrective(){
  document.getElementById('rs_log').textContent='dispatching corrective…';
  const r=await jpost('/project',{method:'gt.project.dispatch_corrective',args:{project_id:activeProject, instructions:document.getElementById('rs_instr').value.trim()}});
  document.getElementById('rs_log').innerHTML=r&&r.ok?`<span class="ok">corrective ${r.corrective_bead_id?r.corrective_bead_id.slice(0,8):''} dispatched for #${RS.title||RS.beadId.slice(0,8)}</span>`:`<span class="bad">${r&&r.error||'dispatch failed'}</span>`;
}
async function rsMarkGates(){
  const gates=[]; let i=0;
  while(document.getElementById('rs_gate_'+i)){
    const cb=document.getElementById('rs_gate_'+i);
    const pass=cb.checked;
    const id=(cb.dataset&&cb.dataset.id)||('gate_'+i);
    const ev=document.getElementById('rs_ev_'+i).value.trim();
    gates.push({id:id, status:pass?'PASS':'UNVERIFIED', evidence:ev||'human-confirmed'});
    i++;
  }
  document.getElementById('rs_log').textContent='saving gate status: '+gates.map(g=>g.id).join(', ');
  const r=await jpost('/project',{method:'gt.project.set_bead_gates',args:{bead_id:RS.beadId, gates}});
  document.getElementById('rs_log').innerHTML=r&&r.ok?`<span class="ok">gates saved (${gates.length})</span>`:`<span class="bad">${r&&r.error||'save failed'}</span>`;
  refreshProjectKanban();
}
async function rsSetActive(){
  document.getElementById('rs_log').textContent='setting active primary…';
  const r=await jpost('/project',{method:'gt.project.set_active_bead',args:{project_id:activeProject, bead_id:RS.beadId}});
  document.getElementById('rs_log').innerHTML=r&&r.ok?`<span class="ok">${RS.title||RS.beadId.slice(0,8)} is now the active primary (serial gate ${r.serial_gate||''})</span>`:`<span class="bad">${r&&r.error||'set failed'}</span>`;
  refreshProjectKanban();
}
async function rsSetRoots(){
  const raw=document.getElementById('rs_roots').value.split(',').map(s=>s.trim()).filter(Boolean);
  if(!raw.length){document.getElementById('rs_log').innerHTML='<span class="bad">enter at least one root</span>';return;}
  document.getElementById('rs_log').textContent='saving resource roots…';
  const r=await jpost('/project',{method:'gt.project.set_resource_roots',args:{project_id:activeProject, resource_roots:raw}});
  document.getElementById('rs_log').innerHTML=r&&r.ok?`<span class="ok">roots updated: ${(r.resource_roots||[]).join(' ; ')}</span>`:`<span class="bad">${r&&r.error||'save failed'}</span>`;
  renderUniversal(); refreshProjectHeader();
}

async function init(){
  startSSE();
  renderUniversal();
  await refreshProjectPicker();
  await loadRecallSessionsForProject();
  if(activeProject && $('proj_pick')&&[...$('proj_pick').options].some(o=>o.value===activeProject)){
    $('proj_pick').value=activeProject; await openProject(activeProject);
  } else {
    // no active project yet: keep SSE + stream running regardless
  }
  // Periodic refresh: keep the Kanban board + universal view + Inquisitor live.
  setInterval(()=>{
    renderUniversal();
    if(activeProject){ refreshProjectKanban(); renderInquisitor(); renderOracle(); renderMemory(); }
  }, 4000);
}
document.querySelector('#tokenbar button')?.addEventListener('click',saveToken);
init();
</script>
</body></html>
"#;