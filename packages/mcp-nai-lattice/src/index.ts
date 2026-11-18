/**
 * mcp-nai-lattice — read-only semantic search over Kilo's per-project Qdrant code index.
 *
 * NAI-Lattice MCP server that federates Kilo Code's existing codebase embedding index:
 *   - embeddings via LM Studio (OpenAI-compatible `/v1/embeddings`, e.g. text-embedding-nomic-embed-text-v1.5),
 *   - vectors already stored in Qdrant by Kilo, per project, under collection `ws-<sha256(workspacePath).hex(0,16)>`,
 *   - this server adds ONLY the MCP delta: derive the collection from a workspace path,
 *     embed a query, Cosine-search the collection read-only, and return the indexed code hits.
 *
 * It never writes to Qdrant and never re-embeds — it consumes Kilo's index as-is (AAF: build only the delta).
 *
 * Team operating contract (mirrored in list_indexed_projects output):
 *   - OWNERSHIP: Every NAI project is owned by EVERYONE. Refusing a repo with "not my
 *     scope" / "pre-existing bug" / "not mine" is a violation, not an excuse.
 *   - AAF: integrate & develop ONLY the delta on existing code — never rewrite,
 *     regenerate, or re-derive a working chunk; preserve exact pins; no scope creep.
 *   - CROSS-REPO: use cross_project_semantic_query across a consumer + its dependsOn
 *     producers so call-site and handler are both recovered in one retrieval (no handoff).
 *   - EVIDENCE: proof against the real runtime on every change; retain raw failures.
 *
 * Env (all optional, sane localhost defaults):
 *   LM_STUDIO_URL      = http://127.0.0.1:1234/v1        (OpenAI-compatible embeddings endpoint)
 *   EMBEDDING_MODEL    = text-embedding-nomic-embed-text-v1.5
 *   EMBEDDING_DIM      = 768
 *   QDRANT_URL         = http://localhost:6333
 *   QDRANT_API_KEY     = (optional)
 */
import { createHash } from "node:crypto"
import { readFileSync, readdirSync, writeFileSync } from "node:fs"
import { fileURLToPath } from "node:url"
import { basename, dirname, join, isAbsolute, win32 } from "node:path"
import { McpServer, ResourceTemplate } from "@modelcontextprotocol/sdk/server/mcp.js"
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js"
import { SubscribeRequestSchema, UnsubscribeRequestSchema } from "@modelcontextprotocol/sdk/types.js"
import { z } from "zod"
import { fetchLogs, fetchStats } from "./fabric.js"
import {readCompanyPolicy} from './company-policy.js'
import {
  readTodos, updateTask, newTask, doneCheck, persistTask, todoPath, type Task,
} from "./todo.js"
import { acknowledgeFeed, followPath, followTask, makeFeed, readFeed, readFollowing, watchTodoChanges } from "./todo-feed.js"

const __dirname = dirname(fileURLToPath(import.meta.url))
const LM_BASE = process.env.LM_STUDIO_URL ?? "http://127.0.0.1:1234/v1"
const EMBED_MODEL = process.env.EMBEDDING_MODEL ?? "text-embedding-nomic-embed-text-v1.5"
const EMBED_DIM = Number(process.env.EMBEDDING_DIM ?? 768)
const QDRANT_URL = (process.env.QDRANT_URL ?? "http://localhost:6333").replace(/\/+$/, "")
const QDRANT_KEY = process.env.QDRANT_API_KEY

interface RegisteredProject {
  name: string
  description?: string
  workspacePath: string
  techStack?: string[]
  /** How to build this project (e.g. "npm run build", "cargo build --release"). */
  buildCommand?: string
  /** How to run this project locally. */
  runCommand?: string
  /** Where a finished build lands / is deployed (path, host, registry, port...). */
  deployTarget?: string
  /** Other registered projects this project imports from / is coupled to (by name). */
  dependsOn?: string[]
}

/**
 * The shared operating contract for every project in the registry. This is what a
 * cross-project agent must obey — it is the same "caretaker" canon as the AAF +
 * delta rule in the engineering contract. Every project is owned by everyone:
 * there is no "not my scope", no "pre-existing bug" exemption, no "not mine".
 * You integrate & develop by building ONLY the delta on top of what already
 * exists — never rewrite, regenerate, or reopen an existing working chunk. If a
 * sub-step returns nothing usable, retain the raw response + the exact failure
 * and surface it; partial output is a recorded, auditable state, not a success.
 */
const TEAM_CONTRACT = {
  ownership: "Everyone owns all projects. Do not refuse work on a repo with 'not my scope', 'pre-existing bug', or 'not mine' — every NAI project is shared team property.",
  preexistingBugs: "Pre-existing bugs are in scope: fix and deliver, do not defer back to 'it was already broken'. The repo is a living organism, not a set of fiefdoms.",
  aafRule: "AAF means Ambient Asset Federation: before build/buy/download/provision/copy/train/store, discover and fingerprint equivalent capabilities already within reach, assess access, compatibility, risk and economics, federate them, then implement the missing delta. It is broader than code reuse. FIRST call get_company_policy for the full canonical doctrine at <workspace>/NAI-OS/docs/AAF.md (or Foundry foundry_company_policy). Never infer the doctrine from this summary or a repository name. Preserve the complete user scope.",
  crossRepo: "To change a project, first cross_project_semantic_query across it AND its dependsOn partners so the call-site in the consumer and the handler in the producer are both recovered in ONE retrieval — no md handoff, no pasting between windows.",
  firstDoor: "For company doctrine such as AAF, call get_company_policy directly; code search is not the authority for policy. For code discovery use semantic_query / cross_project_semantic_query first, then inspect actual source. Empty, stale, unavailable or failed indexes permit local exploration; they do not establish that a capability or document is absent.",
  evidence: "Every change lands with proof against the real runtime; retain and surface raw failures. Reuse cached expensive generation (TTS/embedding) by content hash; do not regenerate unchanged work.",
} as const

/** Human-readable contract for a registry dump. */
function contractText(contract: Record<string, string>): string {
  return Object.entries(contract).map(([k, v]) => `- ${k}: ${v}`).join("\n")
}
/** The hand-maintained project registry (canonical, in NAI-Lattice). */
const REGISTRY_PATH = process.env.REGISTRY_PATH ?? join(__dirname, "..", "projects.json")
function loadRegistry(): RegisteredProject[] {
  try {
    const j = JSON.parse(readFileSync(REGISTRY_PATH, "utf8")) as { projects: RegisteredProject[] }
    return j.projects ?? []
  } catch {
    return []
  }
}

function saveRegistry(projects: RegisteredProject[]): void {
  writeFileSync(REGISTRY_PATH, JSON.stringify({ projects }, null, 2), "utf8")
}

/** Best-effort tech-stack sniff from common manifests in a project root. */
export function sniffStack(root: string): string[] {
  const out: string[] = []
  const read = (p: string): string | undefined => {
    try { return readFileSync(p, "utf8") } catch { return undefined }
  }
  // Manifest search: root manifests first; for monorepos, also scan packages/*/*.
  const manifests: string[] = []
  const candidates = ["package.json", "Cargo.toml", "go.mod", "pyproject.toml", "requirements.txt"]
  for (const m of candidates) manifests.push(join(root, m))
  // gather packages/* manifest paths (monorepo) to catch nested package.json/Cargo.toml
  try {
    const pkgDir = join(root, "packages")
    const entries = readdirSync(pkgDir, { withFileTypes: true })
    for (const e of entries) {
      if (!e.isDirectory()) continue
      if (e.name === "node_modules") continue
      for (const m of candidates) {
        const f = join(pkgDir, e.name, m)
        if (read(f) !== undefined) manifests.push(f)
      }
    }
  } catch { /* no packages/ dir */ }

  const packageJson = (p: string): Record<string, unknown> | undefined => {
    const s = read(p); if (s === undefined) return undefined
    try { return JSON.parse(s) } catch { return undefined }
  }

  for (const man of manifests) {
    if (man.endsWith("package.json")) {
      const d = packageJson(man)
      if (!d) continue
      const all: Record<string, string> = { ...(d.dependencies as Record<string, string> ?? {}), ...(d.devDependencies as Record<string, string> ?? {}) }
      if (all.next) out.push(`Next.js ${String(all.next).replace(/^[\^~]/, "")}`)
      if (all.react) out.push("React")
      if (all.vue) out.push("Vue")
      if (all.svelte) out.push("Svelte")
      if (all.typescript ?? d.type === "module") out.push("TypeScript")
      if (all.vitest) out.push("Vitest")
      if (all.tauri) out.push("Tauri")
      if (all["better-sqlite3"] ?? all.sqlite3) out.push("SQLite")
      if (all.zod) out.push("Zod")
      if (all.express) out.push("Express")
      if (all["@qdrant/js-client-rest"] || all.qdrant) out.push("Qdrant")
    } else if (man.endsWith("Cargo.toml")) {
      const cargo = read(man) ?? ""
      if (cargo.trim() !== "") {
        if (!out.includes("Rust")) out.push("Rust")
        if (/tokio/.test(cargo)) out.push("Tokio")
        // Tauri: distinguish v2 (workspace deps + tauri.conf.json) from v1.
        const isTauri = /\btauri\b/.test(cargo) || /\btauri-build\b/.test(cargo)
        if (isTauri) {
          const conf =
            read(join(root, "tauri.conf.json")) !== undefined ||
            read(join(root, "src-tauri", "tauri.conf.json")) !== undefined
          out.push(/tauri\s*=\s*"2\./.test(cargo) || conf ? "Tauri v2" : "Tauri")
        }
        if (/qdrant/.test(cargo)) out.push("Qdrant")
        if (/surreal/.test(cargo)) out.push("SurrealDB")
        if (/opencv/.test(cargo)) out.push("OpenCV")
      }
    }
    // Stop scanning more manifests once a representative stack is found (avoids
    // every monorepo package polluting the tag set). Root manifests first.
    // Exception: if the repo root has BOTH package.json and Cargo.toml (a hybrid
    // shell), don't stop at the first so Rust + Node both register, not one.
    const isRootManifest = man.startsWith(root)
    const hybridRoot = isRootManifest &&
      read(join(root, "package.json")) !== undefined &&
      read(join(root, "Cargo.toml")) !== undefined
    if (out.length > 0 && isRootManifest && !hybridRoot) break
  }
  // Chromium / V8 / native-build fingerprint markers (bounded depth: root, src, src/src).
  // A Chromium tree has BUILD.gn + DEPS + v8/BUILD.bazel + .clangd even when all
  // manifests are nested/deep. Catch these so a browser/native fork isn't tagged "TS only".
  const chromiumDirs = [root, join(root, "src"), join(root, "src", "src")]
  const probe = (rel: string): boolean => chromiumDirs.some((d) => read(join(d, rel)) !== undefined)
  if (probe("BUILD.gn") || probe("DEPS") || probe("v8/BUILD.bazel") || probe(".clangd")) {
    out.push("Chromium")
    out.push("C++")
  }
  // vpython.toml → Chromium's Python bootstrap; pyproject.toml → Python.
  if (probe("vpython.toml")) out.push("Python")
  if (manifests.some((m) => m.endsWith("pyproject.toml") && read(m) !== undefined)) out.push("Python")

  // Dominant-language signal for native repos: count source files at a bounded depth.
  // A llama.cpp/Chromium/native tree is unmistakably C++-dominant even when a stray
  // pyproject/requirements sneaks in. Only run if we haven't already found Chromium/C++.
  if (!out.includes("C++") && !out.includes("Chromium")) {
    let cpp = 0
    let py = 0
    let js = 0
    try {
      const count = (dir: string, depth: number): void => {
        if (depth < 0) return
        const entries = readdirSync(dir, { withFileTypes: true })
        for (const e of entries) {
          if (e.isDirectory()) {
            if (e.name === "node_modules" || e.name === ".git" || e.name === "build" || e.name === "dist") continue
            count(join(dir, e.name), depth - 1)
          } else {
            if (/\.(cpp|cc|cxx|c|h|hpp|cu)$/.test(e.name)) cpp++
            else if (/\.py$/.test(e.name)) py++
            else if (/\.(ts|tsx|js|jsx)$/.test(e.name)) js++
          }
        }
      }
      count(root, 2)
      if (cpp > 0 && cpp >= Math.max(py, js)) out.push("C++")
    } catch { /* skip */ }
  }

  // fall back: if nothing found, tag Node if a package.json exists anywhere (TS monorepo default)
  if (out.length === 0 && manifests.some((m) => m.endsWith("package.json"))) out.push("TypeScript")
  return [...new Set(out)]
}

/** Upsert a project into the registry (never deletes existing entries). */
function registerProject(entry: RegisteredProject): RegisteredProject {
  const projects = loadRegistry()
  const idx = projects.findIndex((p) => p.workspacePath.toLowerCase() === entry.workspacePath.toLowerCase())
  if (idx >= 0) {
    // merge: keep existing where new is unset, prefer explicit new values
    const existing = projects[idx]!
    projects[idx] = {
      ...existing,
      ...entry,
      techStack: entry.techStack?.length ? entry.techStack : existing.techStack,
    }
  } else {
    projects.push(entry)
  }
  saveRegistry(projects)
  return entry
}

/** Collection name derived exactly the way Kilo does it. */
export function workspaceCollection(workspacePath: string): string {
  const h = createHash("sha256").update(workspacePath).digest("hex")
  return `ws-${h.substring(0, 16)}`
}

/** Embed a query via LM Studio. */
export async function embed(text: string): Promise<number[]> {
  const res = await fetch(`${LM_BASE}/embeddings`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ model: EMBED_MODEL, input: text }),
  })
  if (!res.ok) throw new Error(`embed failed ${res.status}: ${await res.text()}`)
  const j = (await res.json()) as { data: Array<{ embedding: number[] }> }
  const vec = j.data?.[0]?.embedding
  if (!vec || vec.length === 0) throw new Error("embed returned no vector")
  if (vec.length !== EMBED_DIM)
    throw new Error(`embedding dim mismatch: expected ${EMBED_DIM}, got ${vec.length}`)
  return vec
}

export interface CodeHit {
  project: string
  filePath: string
  startLine: number
  endLine: number
  codeChunk: string
  score: number
}

/** Search one Qdrant collection and return the indexed code hits. */
export async function searchCollection(
  workspacePath: string,
  vector: number[],
  opts: { limit?: number; minScore?: number; names?: string[] } = {},
): Promise<CodeHit[]> {
  const collection = workspaceCollection(workspacePath)
  const limit = opts.limit ?? 20
  const minScore = opts.minScore ?? 0.0

  const res = await fetch(`${QDRANT_URL}/collections/${collection}/points/query`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      ...(QDRANT_KEY ? { "api-key": QDRANT_KEY } : {}),
    },
    body: JSON.stringify({
      query: vector,
      limit,
      score_threshold: minScore,
      with_payload: { include: ["filePath", "fileHash", "codeChunk", "startLine", "endLine"] },
      with_vector: false,
      filter: { must_not: [{ key: "type", match: { value: "metadata" } }] },
    }),
  })
  if (res.status === 404) {
    // collection missing -> project not indexed yet
    return []
  }
  if (!res.ok) throw new Error(`qdrant search ${collection} failed ${res.status}: ${await res.text()}`)
  const points = parseQdrantQueryResult(await res.json())
  return points
    .filter((p) => p.payload?.filePath && typeof p.payload.codeChunk === "string")
    .map((p) => ({
      project: workspacePath,
      filePath: String(p.payload!.filePath),
      startLine: Number(p.payload!.startLine ?? 0),
      endLine: Number(p.payload!.endLine ?? 0),
      codeChunk: String(p.payload!.codeChunk),
      score: p.score,
    }))
}

/** List all Qdrant collections actually indexed by Kilo (ws-<hash>), with point counts. */
async function listIndexedCollections(): Promise<Array<{ collection: string; points: number; dimension: number }>> {
  const res = await fetch(`${QDRANT_URL}/collections`, {
    headers: QDRANT_KEY ? { "api-key": QDRANT_KEY } : {},
  })
  if (!res.ok) throw new Error(`qdrant collections failed ${res.status}: ${await res.text()}`)
  const j = (await res.json()) as { result: { collections: Array<{ name: string }> } }
  const out: Array<{ collection: string; points: number; dimension: number }> = []
  for (const c of (j.result?.collections ?? []).filter((c) => c.name.startsWith("ws-"))) {
    try {
      const info = await fetch(`${QDRANT_URL}/collections/${c.name}`, { headers: QDRANT_KEY ? { "api-key": QDRANT_KEY } : {} })
      if (!info.ok) continue
      const i = (await info.json()) as { result: { points_count?: number; config?: { params?: { vectors?: { size?: number } | number } } } }
      const v = i.result?.config?.params?.vectors
      out.push({
        collection: c.name,
        points: i.result?.points_count ?? 0,
        dimension: typeof v === "number" ? v : (v && typeof v === "object" ? v.size ?? 0 : 0),
      })
    } catch {
      /* skip unreachable */
    }
  }
  return out
}

/** Optional GPU reranker hook: POST chunk_texts to LM Studio's /rerank if present (else skip). */
export async function rerank(
  query: string,
  docs: Array<{ id: string; text: string }>,
): Promise<Array<{ id: string; score: number }>> {
  try {
    const res = await fetch(`${LM_BASE}/rerank`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ model: process.env.RERANK_MODEL ?? "", query, documents: docs.map((d) => d.text), top_n: docs.length }),
    })
    if (!res.ok) return [] // rerank not available on LM Studio — caller falls back to Qdrant score
    const j = (await res.json()) as { results?: Array<{ id?: number | string; index?: number; score: number }> }
    return (j.results ?? []).map((r) => {
      const idx = r.index ?? (typeof r.id === "number" ? r.id : Number(r.id) || NaN)
      const d = docs[idx]
      return { id: d?.id ?? String(idx), score: r.score }
    })
  } catch {
    return [] // rerank off/broken — non-fatal, Qdrant score stands
  }
}

/**
 * Reciprocal-rank fusion — the canonical NAI-Lattice fusion (mirrors
 * lattice-query `reciprocal_rank_fusion`, k=60): combine two id→rank maps.
 * A document present in both lexical (BM25) and dense (vector) recall ranks at
 * the top. Unmatched ids get only their one-side rank (and any explicit score
 * is ignored here — fused rank is the signal).
 */
function rrf(
  a: Array<{ id: string }>,
  b: Array<{ id: string }>,
  k = 60,
): Array<{ id: string; score: number }> {
  const scores = new Map<string, number>()
  for (const [i, x] of a.entries()) scores.set(x.id, (scores.get(x.id) ?? 0) + 1 / (k + i + 1))
  for (const [i, x] of b.entries()) scores.set(x.id, (scores.get(x.id) ?? 0) + 1 / (k + i + 1))
  return [...scores.entries()].map(([id, score]) => ({ id, score })).sort((x, y) => y.score - x.score)
}

/** Parses a Qdrant /points/query response into {score, payload[]}. Exported for testability. */
export function parseQdrantQueryResult(
  raw: unknown,
): Array<{ score: number; payload?: Record<string, unknown> }> {
  const j = raw as { result?: { points?: Array<{ score: number; payload?: Record<string, unknown> }> } }
  const points = j?.result?.points ?? (Array.isArray(j?.result) ? j.result : [])
  return points
}

// ── Compact BM25 (no external dep; classic Robertson/Sparck-Jones) ──────────────
const ident = /[A-Za-z0-9_]+/g
function tokenize(t: string): string[] {
  // keep camelCase/underscore identifiers whole (critical for code symbol search)
  return (t.match(ident) ?? []).map((s) => s.toLowerCase())
}

/** BM25 ranks `docs` (as {id, text}) against `query` over the given corpus. */
export function bm25Rank(
  query: string,
  docs: Array<{ id: string; text: string }>,
  k1 = 1.2,
  b = 0.75,
): Array<{ id: string; score: number }> {
  const q = tokenize(query)
  if (q.length === 0 || docs.length === 0) return []
  const corpus = docs.map((d) => { const t = tokenize(d.text); return { id: d.id, t } })
  const N = corpus.length
  const avgdl = corpus.reduce((s, c) => s + c.t.length, 0) / N
  // idf per unique query term
  const df = new Map<string, number>()
  for (const d of corpus) { for (const term of new Set(d.t)) df.set(term, (df.get(term) ?? 0) + 1) }
  const idf = (term: string): number => {
    const n = df.get(term) ?? 0
    return Math.log(1 + (N - n + 0.5) / (n + 0.5))
  }
  const out = new Map<string, number>()
  for (const d of corpus) {
    const len = d.t.length
    const freq = new Map<string, number>()
    for (const term of d.t) freq.set(term, (freq.get(term) ?? 0) + 1)
    let score = 0
    for (const term of new Set(q)) {
      const f = freq.get(term) ?? 0
      if (f === 0) continue
      score += idf(term) * ((f * (k1 + 1)) / (f + k1 * (1 - b + b * (len / avgdl))))
    }
    if (score > 0) out.set(d.id, score)
  }
  return [...out.entries()].map(([id, score]) => ({ id, score })).sort((x, y) => y.score - x.score)
}

/**
 * Fuse the dense (Qdrant cosine) ordering with a lexical BM25 ordering over the
 * SAME retrieved candidates using reciprocal-rank fusion — the NAI-Lattice hybrid
 * recall recipe for code. Exact symbol identifiers that dense embeddings miss
 * get rescued by BM25. Cheap, GPU-free, deterministic.
 */
export function fuseDenseAndLexical(query: string, dense: Array<CodeHit>): Array<CodeHit> {
  if (dense.length < 2) return dense
  const denseOrder = dense.map((h) => ({ id: hitKey(h) }))
  const lex = bm25Rank(query, dense.map((h) => ({ id: hitKey(h), text: h.codeChunk })))
  if (lex.length === 0) return dense
  const fused = rrf(denseOrder, lex)
  const byKey = new Map(dense.map((h) => [hitKey(h), h]))
  const out: CodeHit[] = []
  for (const f of fused) {
    const h = byKey.get(f.id)
    if (h) out.push({ ...h, score: f.score }) // replace cosine score with fused rank score
  }
  return out
}
const hitKey = (h: CodeHit): string => `${h.project}\0${h.filePath}\0${h.startLine}\0${h.endLine}`

/**
 * Optional GPU reranker on top of the fused order: LM Studio has no /rerank today,
 * so this normally returns [] and the fused (BM25+RRF) order stands. When a
 * /rerank endpoint appears, it re-orders the fused hits by rerank score.
 */
async function gpuRerankIfAvailable(query: string, hits: CodeHit[]): Promise<Array<{ id: string; score: number }>> {
  const rr = await rerank(
    query,
    hits.map((h) => ({ id: hitKey(h), text: h.codeChunk })),
  )
  return rr
}

const server = new McpServer({ name: "mcp-nai-lattice", version: "0.1.0" },{capabilities:{resources:{subscribe:true}}})
const READ_ONLY = { readOnlyHint: true, destructiveHint: false, idempotentHint: true, openWorldHint: false } as const

server.registerTool('get_company_policy',{
  title:'Read canonical AAF company doctrine',
  description:'Start here for AAF / Ambient Asset Federation. Read the full canonical company document and its SHA-256 directly, without Qdrant, embeddings or repo searches. AAF covers existing assets/capabilities across company, customer, partner and market environments; build-the-delta is only its shorthand. No path argument; host-enrolled document only.',
  inputSchema:{},annotations:READ_ONLY,
},async()=>{try{return {content:[{type:'text' as const,text:JSON.stringify(readCompanyPolicy())}]}}catch(error){return {isError:true,content:[{type:'text' as const,text:JSON.stringify({code:'AAF_DOCUMENT_UNAVAILABLE',error:String(error),next:'Use Foundry foundry_company_policy or inspect the enrolled canonical path; do not invent the doctrine.'})}]}}})

server.registerTool(
  "list_indexed_projects",
  {
    title: "List projects + team operating contract",
    description:
      "Discover which projects have a Kilo Qdrant code index, PLUS the shared NAI team operating contract. Returns every indexed Qdrant collection (live from Qdrant) fused with the human project registry (name, description, workspacePath, techStack, buildCommand, runCommand, deployTarget, dependsOn). Read the returned 'contract' before doing any cross-project work: it is the binding rule that EVERY NAI repo is owned by EVERYONE (no 'not my scope' / 'pre-existing bug' refusals), that you integrate & develop ONLY delta changes (AAF), and that for code questions you reach for semantic_query / cross_project_semantic_query as the FIRST door instead of a repo-wide grep / file-mask search. Use this first to learn a project's workspacePath (or its registry name) and its build/dependsOn wiring before calling semantic_query / cross_project_semantic_query.",
    inputSchema: {},
    outputSchema: {
      contract: z.string().describe("The shared operating contract (ownership + pre-existing bugs + AAF delta rule + cross-repo + evidence). Read this before any cross-project change."),
      projects: z.array(
        z.object({
          project: z.string().optional(),
          description: z.string().optional(),
          techStack: z.array(z.string()).optional(),
          workspacePath: z.string(),
          collection: z.string(),
          points: z.number(),
          dimension: z.number(),
          registered: z.boolean(),
          buildCommand: z.string().optional(),
          runCommand: z.string().optional(),
          deployTarget: z.string().optional(),
          dependsOn: z.array(z.string()).optional(),
        }),
      ),
      unregistered: z.array(z.string()).describe("Indexed collections with no registry entry (hash unknown — add a projects.json entry to name them)."),
    },
    annotations: READ_ONLY,
  },
  async () => {
    const registry = loadRegistry()
    const online = await listIndexedCollections()
    const byHash = new Map<string, RegisteredProject>()
    for (const p of registry) byHash.set(workspaceCollection(p.workspacePath), p)
    const projects = online
      .map((c) => {
        const reg = byHash.get(c.collection)
        return {
          project: reg?.name,
          description: reg?.description,
          techStack: reg?.techStack,
          workspacePath: reg?.workspacePath ?? c.collection, // unknown path → collection, caller can look it up
          collection: c.collection,
          points: c.points,
          dimension: c.dimension,
          registered: !!reg,
          buildCommand: reg?.buildCommand,
          runCommand: reg?.runCommand,
          deployTarget: reg?.deployTarget,
          dependsOn: reg?.dependsOn,
        }
      })
      .sort((a, b) => b.points - a.points)
    const unregistered = projects.filter((p) => !p.registered).map((p) => p.collection)
    const contract = contractText(TEAM_CONTRACT)
    const text = JSON.stringify({ contract, projects, unregistered })
    return {
      content: [{ type: "text" as const, text }],
      structuredContent: { contract, projects, unregistered },
    }
  },
)

/** Resolve a project selector (name or workspace path) to collection/hash. */
function resolveProject(selector: string): { workspacePath: string; collection: string; name?: string } {
  const reg = loadRegistry().find((p) => p.name.toLowerCase() === selector.toLowerCase())
  const workspacePath = reg?.workspacePath ?? selector
  return { workspacePath, collection: workspaceCollection(workspacePath), name: reg?.name }
}

server.registerTool(
  "register_current_project",
  {
    title: "Register the current workspace in the project registry",
    description:
      "Add (or update) the given workspace in mcp-nai-lattice's projects.json so it becomes discoverable via list_indexed_projects and queryable by name. Observes the repo to fill missing deltas: project name (folder basename), techStack (sniffed from package.json/Cargo.toml/go.mod/pyproject), and it logs which Qdrant collection the workspace would map to. Pass name/description/techStack/buildCommand/runCommand/deployTarget/dependsOn to override or enrich; an existing entry is merged, never deleted. Idempotent. The more build/deploy context you record, the better cross_project_semantic_query can drive a 'change in A -> fix producer B -> rebuild -> deploy' flow without a handoff.",
    inputSchema: {
      workspacePath: z.string().describe("Absolute workspace root to register."),
      name: z.string().optional().describe("Short project name; defaults to the folder basename."),
      description: z.string().optional().describe("One-line project description (recommended for discovery)."),
      techStack: z.array(z.string()).optional().describe("Tech-stack tags; defaults to sniffing package.json/Cargo.toml/etc."),
      buildCommand: z.string().optional().describe("How to build this project (e.g. 'npm run build', 'cargo build --release', 'meson ninja')."),
      runCommand: z.string().optional().describe("How to run this project locally."),
      deployTarget: z.string().optional().describe("Where a finished build lands / is deployed."),
      dependsOn: z.array(z.string()).optional().describe("Other registered project names this project imports from / is coupled to (e.g. nai-browser dependsOn beellama)."),
    },
    outputSchema: {
      registered: z.boolean(),
      entry: z.object({ name: z.string(), description: z.string().optional(), techStack: z.array(z.string()).optional(), workspacePath: z.string(), buildCommand: z.string().optional(), runCommand: z.string().optional(), deployTarget: z.string().optional(), dependsOn: z.array(z.string()).optional() }),
      collection: z.string(),
      derived: z.object({ name: z.string(), techStack: z.array(z.string()) }).describe("What was auto-derived (could not find source: what the agent must supply)."),
    },
    annotations: { readOnlyHint: false, destructiveHint: false, idempotentHint: true, openWorldHint: false },
  },
  async ({ workspacePath, name, description, techStack, buildCommand, runCommand, deployTarget, dependsOn }) => {
    // Preserve curated fields on re-register: only override name/description/
    // techStack + build metadata when explicitly passed, else keep the existing.
    const existing = loadRegistry().find(
      (p) => p.workspacePath.toLowerCase() === workspacePath.toLowerCase(),
    )
    const derivedName = name ?? existing?.name ?? basename(workspacePath.replace(/[\\/]+$/, ""))
    const derivedStack = techStack?.length ? techStack : existing?.techStack ?? sniffStack(workspacePath)
    const desc = description ?? existing?.description
    const entry: RegisteredProject = {
      name: derivedName,
      workspacePath,
      ...(desc ? { description: desc } : {}),
      ...(derivedStack.length ? { techStack: derivedStack } : {}),
      ...(buildCommand ? { buildCommand } : existing?.buildCommand ? { buildCommand: existing.buildCommand } : {}),
      ...(runCommand ? { runCommand } : existing?.runCommand ? { runCommand: existing.runCommand } : {}),
      ...(deployTarget ? { deployTarget } : existing?.deployTarget ? { deployTarget: existing.deployTarget } : {}),
      ...(dependsOn?.length ? { dependsOn } : existing?.dependsOn ? { dependsOn: existing.dependsOn } : {}),
    }
    const saved = registerProject(entry)
    return {
      content: [{ type: "text" as const, text: JSON.stringify({ registered: true, entry: saved, collection: workspaceCollection(workspacePath) }) }],
      structuredContent: {
        registered: true,
        entry: saved,
        collection: workspaceCollection(workspacePath),
        derived: { name: derivedName, techStack: derivedStack },
      },
    }
  },
)

server.registerTool(
  "semantic_query",
  {
    title: "Semantic search the current project's code index",
    description:
      "Search a single workspace's Qdrant code index (collection ws-<sha256(workspace)>) and return matched code chunks with filePath, line range, score and code. EMBED the query with LM Studio; dense + BM25 fusion. USE THIS AS THE FIRST DOOR for ANY question about NAI code BEFORE grepping a repo, running a file-mask search, or reading through READMEs/docs: if the answer is 'where does X live / how is X implemented / what touches X in this project', a semantic query resolves it in ONE call instead of a repo-wide hunt. It handles 'what is a subsystem' by locating the actual implementing/consuming files, not just symbol names. IDs inside any code chunk you change are YOURS to integrate: every project is owned by everyone (see list_indexed_projects contract), pre-existing bugs are in scope, and you build only the AAF delta on that chunk. For NAI Lattice protocol/enhancement questions, run this on the nai-lattice project first; for questions spanning multiple repos use cross_project_semantic_query.",
    inputSchema: {
      query: z.string().describe("Natural-language / fuzzy code search query."),
      limit: z.number().int().min(1).max(100).optional().describe("Max results (default 20)."),
      minScore: z.number().min(0).max(1).optional().describe("Qdrant score threshold (default 0)."),
      workspacePath: z.string().optional().describe(
        "Explicit workspace root to search; defaults to the server's cwd. IMPORTANT: pass the path exactly as it is on disk (drive letter + backslashes, e.g. D:\\_SATIN_AI_2\\Tutty\\tutty-studio) — the Qdrant collection is derived from the sha256 of this exact string. A lowercased / forward-slashed UI-normalized form yields a different (empty) collection; if a search returns no hits, retry the same path in the alternate casing/slashes.",
      ),
      applyRerank: z.boolean().optional().describe("Rerank hits on top if a /rerank endpoint is configured (default true)."),
    },
    outputSchema: {
      collection: z.string(),
      hits: z.array(
        z.object({ project: z.string(), filePath: z.string(), startLine: z.number(), endLine: z.number(), codeChunk: z.string(), score: z.number() }),
      ),
    },
    annotations: READ_ONLY,
  },
  async ({ query, limit, minScore, workspacePath, applyRerank }) => {
    const ws = workspacePath ?? process.cwd()
    const collection = workspaceCollection(ws)
    const vector = await embed(query)
    const hits = await searchCollection(ws, vector, { limit, minScore })
    // BM25+RRF fusion first (rescues exact identifiers); then optional GPU reranker.
    let ordered = applyRerank !== false ? fuseDenseAndLexical(query, hits) : hits
    if (applyRerank !== false && ordered.length > 1) {
      const byId = new Map<string, number>()
      const rr = await gpuRerankIfAvailable(query, ordered)
      for (const r of rr) byId.set(r.id, r.score)
      if (rr.length) {
        ordered = [...ordered].sort(
          (a, b) => (byId.get(hitKey(b)) ?? b.score) - (byId.get(hitKey(a)) ?? a.score),
        )
      }
    }
    return {
      content: [{ type: "text" as const, text: JSON.stringify({ collection, hits: ordered }) }],
      structuredContent: { collection, hits: ordered },
    }
  },
)

server.registerTool(
  "cross_project_semantic_query",
  {
    title: "Semantic search across multiple projects (cross-repo change)",
    description:
      "Embed the query once and search each listed workspace's Qdrant code index. Returns merged, de-duplicated code hits that may span different projects. USE THIS AS THE FIRST DOOR for ANY cross-repo / integration / protocol-enhancement question, BEFORE grepping multiple repos or stitching grep results together: it recovers the call-site in the consumer AND the handler in the producer in ONE retrieval. Examples: 'enhance the NAI inference API in nai-browser and make matching changes in beellama' -> pass [nai-browser, beellama]; 'enhance the NAI Lattice protocol and its integration in consuming apps' -> pass [nai-lattice, kilocode, nai-browser, beellama] (the protocol repo PLUS the repos that consume/couple to it, using each entry's dependsOn from list_indexed_projects). No md handoff file, no pasting between windows. Every project is owned by everyone (see the contract from list_indexed_projects): do not refuse a repo with 'not my scope' / 'pre-existing bug'; integrate & develop ONLY the AAF delta, then rebuild/run/deploy using each project's buildCommand/runCommand/deployTarget.",
    inputSchema: {
      query: z.string().describe("Natural-language / fuzzy code search query."),
workspacePaths: z.array(z.string()).min(1).describe(
        "Workspace roots (or registry project names) to search across. Each path is hashed exactly as given (sha256 → ws-<16hex>) to find its Qdrant collection; registry names from list_indexed_projects are resolved to their registered path. Use on-disk paths (not the UI-lowercased form); zero hits usually means a casing/slash mismatch.",
      ),
      limitPerProject: z.number().int().min(1).max(100).optional().describe("Max results per project (default 10)."),
      applyRerank: z.boolean().optional().describe("Rerank merged hits if a /rerank endpoint is configured (default true)."),
    },
    outputSchema: {
      hits: z.array(
        z.object({ project: z.string(), filePath: z.string(), startLine: z.number(), endLine: z.number(), codeChunk: z.string(), score: z.number() }),
      ),
    },
    annotations: READ_ONLY,
  },
  async ({ query, workspacePaths, limitPerProject, applyRerank }) => {
    const vector = await embed(query)
    const per = limitPerProject ?? 10
    const collected: CodeHit[] = []
    for (const sel of workspacePaths) {
      const { workspacePath } = resolveProject(sel)
      const found = await searchCollection(workspacePath, vector, { limit: per })
      collected.push(...found)
    }
    // de-dup by filePath+line range
    const seen = new Set<string>()
    const merged: CodeHit[] = []
    for (const h of [...collected].sort((a, b) => b.score - a.score)) {
      const k = `${h.project}\0${h.filePath}\0${h.startLine}\0${h.endLine}`
      if (seen.has(k)) continue
      seen.add(k)
      merged.push(h)
    }
    let ordered = merged
    if (applyRerank !== false) {
      ordered = fuseDenseAndLexical(query, merged)
      if (ordered.length > 1) {
        const rr = await gpuRerankIfAvailable(query, ordered)
        if (rr.length) {
          const byId = new Map(rr.map((r) => [r.id, r.score]))
          ordered = [...ordered].sort(
            (a, b) => (byId.get(hitKey(b)) ?? b.score) - (byId.get(hitKey(a)) ?? a.score),
          )
        }
      }
    }
    return {
      content: [{ type: "text" as const, text: JSON.stringify({ hits: ordered }) }],
      structuredContent: { hits: ordered },
    }
  },
)

server.registerTool(
  "log_tail",
  {
    title: "Tail the NAI Lattice observability ledger (latest log records)",
    description:
      "Read the latest structured log records from the canonical observability.log.v1 ledger on the fabric host (NAI OS backend, webview consoles, sidecar output, and the Lattice dashboard/CLI all write here). Kibana/ElasticSearch-style tail. Filter by level, component (a Rust module, a webview label, or sidecar[pid]), traceId, or message text. Use this when an agent needs to inspect real runtime logs instead of asking a human to open DevTools or NAI OS.log. Returns newest-first records with ts_ms, level, component, trace_id, caller, message.",
    inputSchema: {
      limit: z.number().int().min(1).max(500).optional().describe("Max records (default 50)."),
      level: z.enum(["trace", "debug", "info", "warn", "error"]).optional().describe("Only records at least this severity (e.g. 'error' shows error+warn)."),
      component: z.string().optional().describe("Exact component to filter (Rust module, webview label, or sidecar[pid])."),
      trace: z.string().optional().describe("Filter by an exact traceId to follow one request end-to-end."),
      contains: z.string().optional().describe("Case-insensitive substring matched against level+message."),
    },
  },
  async ({ limit, level, component, trace, contains }) => {
    const { records, error } = await fetchLogs({
      limit: limit ?? 50,
      level,
      component,
      trace,
      contains,
    })
    if (error) {
      return {
        content: [{ type: "text" as const, text: JSON.stringify({ ok: false, error }) }],
        isError: true,
      }
    }
    return {
      content: [{ type: "text" as const, text: JSON.stringify({ ok: true, count: records.length, records }) }],
      structuredContent: { ok: true, records },
    }
  },
)

server.registerTool(
  "log_search",
  {
    title: "Search the NAI Lattice observability ledger for a substring",
    description:
      "Search the structured observability.log.v1 ledger for records whose level or message contains a text substring (case-insensitive). Kibana/ES-style free-text search over the live log stream on the fabric host. Use to find a specific error, a request id, a component's messages, or a string that appeared in a recent failure — without touching DevTools or NAI OS.log.",
    inputSchema: {
      contains: z.string().describe("Substring to search in level + message (case-insensitive)."),
      limit: z.number().int().min(1).max(500).optional().describe("Max hit records (default 50)."),
      level: z.enum(["trace", "debug", "info", "warn", "error"]).optional().describe("Only records at least this severity."),
      component: z.string().optional().describe("Exact component to narrow to."),
    },
  },
  async ({ contains, limit, level, component }) => {
    if (!contains) {
      return { content: [{ type: "text" as const, text: "log_search requires a non-empty `contains`." }], isError: true }
    }
    const { records, error } = await fetchLogs({ limit: limit ?? 50, level, component, contains })
    if (error) {
      return { content: [{ type: "text" as const, text: JSON.stringify({ ok: false, error }) }], isError: true }
    }
    return {
      content: [{ type: "text" as const, text: JSON.stringify({ ok: true, count: records.length, records }) }],
      structuredContent: { ok: true, records },
    }
  },
)

server.registerTool(
  "log_traffic",
  {
    title: "NAI Lattice observability traffic/health summary",
    description:
      "Aggregate traffic and health over the observability.log.v1 ledger: how many records, per-level counts, error+warn total, per-component breakdown, and distinct traceIds in a bounded recent window. Use to answer 'is the runtime healthy, how deep is the traffic, which component is noisy, how many distinct concurrent requests' — a live health pulse without opening the dashboard.",
    inputSchema: {},
  },
  async () => {
    const s = await fetchStats()
    if (s.error) {
      return { content: [{ type: "text" as const, text: JSON.stringify({ ok: false, error: s.error }) }], isError: true }
    }
    const text = `${JSON.stringify(s)}`
    return {
      content: [{ type: "text" as const, text }],
      structuredContent: { ok: true, ...s },
    }
  },
)

// ── Shared mind-map TODO tools (authoritative in the lattice, cross-agent) ────
// Agents DO NOT hunt for a per-repo TODO file. The authoritative list is the
// `todo.jsonl` ledger in this package's `state/` dir, read/written via these
// tools. `todo_done_check` is the quality gate: a list is milestone-DONE only
// when every item is done OR the still-open gated items have their build/deploy
// gate recorded with evidence. Commit the returned `commitChecklist` when the
// current list reaches a milestone.

const taskStatus = z.enum(["open", "in_progress", "blocked", "done"])
const nodeKind = z.enum(["task", "decision", "fact", "lamp", "state", "blocker"])
const actorInput = z.string().trim().min(1).max(120).optional()
const revisionInput = z.number().int().nonnegative().optional()

function todoWorkspace(selector: string) {
  const normalized = (value: string) => process.platform === "win32"
    ? value.replaceAll("/", "\\").replace(/\\+$/, "").toLowerCase()
    : value.replace(/\/+$/, "")
  const registered = loadRegistry().find(project => project.name.toLowerCase() === selector.toLowerCase() ||
    normalized(project.workspacePath) === normalized(selector))
  if (!registered && !isAbsolute(selector) && !win32.isAbsolute(selector)) {
    throw Error("Use an exact absolute workspace path or a registered Lattice product")
  }
  const workspace = registered?.workspacePath ?? selector
  return { workspace, product: registered?.name, workspaceId: workspaceCollection(workspace) }
}

function taskPreview(t: Task, full = false) {
  const {drops:_drops,dropsPrunedThrough:_pruned,...task} = t
  return {
    ...task, revision: t.revision ?? 0, title: full ? t.title : t.title.slice(0, 300),
    parent: t.parent ?? null, doneAt: t.doneAt ?? null,
    ...(full ? {} : {gates: t.gates && Object.fromEntries(Object.entries(t.gates).map(([key, gate]) =>
      [key, {ok:gate.ok, doneAt:gate.doneAt}]))}),
  }
}

function todoReply(result: Awaited<ReturnType<typeof updateTask>>) {
  if (!result.ok) {
    const data = {ok:false, error:result.reason, current:result.current && taskPreview(result.current)}
    return {content:[{type:"text" as const,text:JSON.stringify(data)}], isError:true}
  }
  const task = result.task
  return {
    content:[{type:"text" as const,text:JSON.stringify({ok:true, id:task.id, workspaceId:task.workspaceId,
      product:task.product, status:task.status, revision:task.revision, assignee:task.assignee})}],
    structuredContent:{ok:true,task:taskPreview(task,true)},
  }
}

server.registerTool("todo_add", {
  title:"Add a shared workspace task or mind-map node",
  description:"Record a task, decision, fact, lamp, state or blocker in the shared Lattice mind map. Pass the owning product or workspace and an actor label. A child without a workspace inherits its parent’s scope.",
  inputSchema:{
    title:z.string().trim().min(1).max(1500), kind:nodeKind.optional(), parent:z.string().optional(),
    workspace:z.string().max(1024).optional().describe("Owning Lattice product name or exact absolute workspace path."),
    actor:actorInput, needsBuild:z.boolean().optional(), needsDeploy:z.boolean().optional(),
  },
  outputSchema:{ok:z.boolean(), task:z.any().optional(), error:z.string().optional()},
  annotations:{readOnlyHint:false,destructiveHint:false,idempotentHint:false,openWorldHint:false},
}, async ({title,kind,parent,workspace,actor,needsBuild,needsDeploy}) => {
  const scope = workspace ? todoWorkspace(workspace) : {}
  return todoReply(await persistTask(newTask(title,{
    ...scope, parent, kind, createdBy:actor,
    gated:needsBuild || needsDeploy ? {needsBuild, needsDeploy} : undefined,
  })))
})

server.registerTool("todo_list", {
  title:"Page the shared mind map by workspace, kind, or status",
  description:"Read tasks, decisions, facts and lamps by product/path/ws hash. Use the task revision when claiming or changing work. Want a ping when a peer task lands? Follow its ID with todo_follow.",
  inputSchema:{
    workspace:z.string().max(1024).optional(), id:z.string().optional(), status:taskStatus.optional(),
    kind:nodeKind.optional(), offset:z.number().int().nonnegative().optional(),
    limit:z.number().int().min(1).max(30).optional(),
  },
  outputSchema:{tasks:z.array(z.any()),doneCheck:z.any(),total:z.number(),nextOffset:z.number().nullable()},
  annotations:READ_ONLY,
}, async ({workspace,id,status,kind,offset=0,limit=15}) => {
  const workspaceId = workspace && (/^ws-[0-9a-f]{16}$/.test(workspace) ? workspace : todoWorkspace(workspace).workspaceId)
  const scoped = readTodos().filter(task => !workspaceId || task.workspaceId === workspaceId)
  const filtered = scoped.filter(task => (!id || task.id === id) &&
    (!status || status === task.status) && (!kind || kind === (task.kind ?? "task")))
  const check = doneCheck(undefined,scoped)
  const outcome = check.done ? {done:true,milestone:check.milestone,summary:check.summary}
    : {done:false,reason:check.reason,summary:check.summary}
  const tasks = []
  let bytes = 0
  for (const task of filtered.slice(offset,offset + limit)) {
    const preview = taskPreview(task,!!id)
    const size = Buffer.byteLength(JSON.stringify(preview))
    if (tasks.length && bytes + size > 6000) break
    tasks.push(preview)
    bytes += size
  }
  const nextOffset = offset + tasks.length < filtered.length ? offset + tasks.length : null
  const data = {tasks,doneCheck:outcome,total:filtered.length,nextOffset}
  return {content:[],structuredContent:data}
})

server.registerTool("todo_update", {
  title:"Claim, update or complete a shared task",
  description:"Transition open → in_progress → done, or mark blocked/reopen. Pass your actor label and the revision returned by todo_list to avoid overwriting a peer. Build/deploy evidence remains required for gated tasks.",
  inputSchema:{
    id:z.string(), status:taskStatus.optional(), parent:z.string().nullable().optional(),
    title:z.string().trim().min(1).max(1500).optional(), workspace:z.string().max(1024).optional(),
    actor:actorInput, expectedRevision:revisionInput,
  },
  outputSchema:{ok:z.boolean(),task:z.any().optional(),error:z.string().optional()},
  annotations:{readOnlyHint:false,destructiveHint:false,idempotentHint:false,openWorldHint:false},
}, async ({id,status,parent,title,workspace,actor,expectedRevision}) => {
  const scope = workspace ? todoWorkspace(workspace) : {}
  return todoReply(await updateTask(id,expectedRevision,current => {
    const next:Task = {...current,...scope,updatedBy:actor ?? current.updatedBy ?? "agent"}
    if (title !== undefined) next.title = title
    if (parent !== undefined) next.parent = parent
    if (status === undefined) return next
    if (status === "done") {
      if ((next.gated?.needsBuild && next.gates?.build?.ok !== true) ||
          (next.gated?.needsDeploy && next.gates?.deploy?.ok !== true)) {
        return "task is quality-gated; record the required build/deploy evidence via todo_gate"
      }
      next.doneAt = current.doneAt ?? new Date().toISOString()
    } else if (current.status === "done") {
      delete next.doneAt
      delete next.gates
    }
    if (status === "in_progress" && actor && current.status === "in_progress" &&
        current.assignee && current.assignee !== actor && expectedRevision === undefined) {
      return "task is already assigned; refresh its revision before explicitly reassigning"
    }
    next.status = status
    if (status === "in_progress" && actor) next.assignee = actor
    if (status === "open") delete next.assignee
    return next
  }))
})

server.registerTool("todo_gate", {
  title:"Record build or deploy evidence",
  description:"Record an observed build/deploy result for a task that requires it. Pass the command, evidence, actor and current revision when available.",
  inputSchema:{
    id:z.string(), kind:z.enum(["build","deploy"]), ok:z.boolean(), cmd:z.string().max(1500).optional(),
    evidence:z.string().max(1500).optional(), actor:actorInput, expectedRevision:revisionInput,
  },
  outputSchema:{ok:z.boolean(),task:z.any().optional(),error:z.string().optional()},
  annotations:{readOnlyHint:false,destructiveHint:false,idempotentHint:false,openWorldHint:false},
}, async ({id,kind,ok,cmd,evidence,actor,expectedRevision}) =>
  todoReply(await updateTask(id,expectedRevision,current => {
    if (!(kind === "build" ? current.gated?.needsBuild : current.gated?.needsDeploy)) {
      return "task does not require the " + kind + " gate"
    }
    const next:Task = {...current, updatedBy:actor ?? current.updatedBy ?? "agent",
      gates:{...current.gates,[kind]:{kind,ok,cmd,evidence,doneAt:new Date().toISOString()}}}
    if (!ok && next.status === "done") {
      next.status = "blocked"
      delete next.doneAt
    }
    return next
  })),
)

server.registerTool("todo_done_check", {
  title:"Check the shared or workspace-specific done milestone",
  description:"Evaluate completion and build/deploy evidence for the selected workspace or the whole ledger. Pages evidence items for larger ledgers.",
  inputSchema:{
    workspace:z.string().max(1024).optional(), offset:z.number().int().nonnegative().optional(),
    limit:z.number().int().min(1).max(30).optional(),
  },
  outputSchema:{
    done:z.boolean(),milestone:z.string().optional(),reason:z.string().optional(),summary:z.any(),
    commitChecklist:z.array(z.string()).optional(),incomplete:z.array(z.any()).optional(),nextOffset:z.number().nullable(),
  },
  annotations:READ_ONLY,
}, async ({workspace,offset=0,limit=12}) => {
  const workspaceId = workspace && (/^ws-[0-9a-f]{16}$/.test(workspace) ? workspace : todoWorkspace(workspace).workspaceId)
  const tasks = readTodos().filter(task => !workspaceId || task.workspaceId === workspaceId)
  const check = doneCheck(undefined,tasks)
  const all = check.done ? check.commitChecklist : check.incomplete.map(task => ({
    id:task.id,title:task.title.slice(0,300),status:task.status,workspaceId:task.workspaceId,product:task.product,
  }))
  const page: Array<(typeof all)[number]> = []
  let bytes = 0
  for (const item of all.slice(offset,offset+limit)) {
    const size = Buffer.byteLength(JSON.stringify(item))
    if (page.length && bytes + size > 6000) break
    page.push(item)
    bytes += size
  }
  const result = {
    done:check.done, milestone:check.done ? check.milestone : undefined,
    reason:check.done ? undefined : check.reason, summary:check.summary,
    commitChecklist:check.done ? page : undefined,
    incomplete:check.done ? undefined : page,
    nextOffset:offset+page.length < all.length ? offset+page.length : null,
  }
  return {content:[],structuredContent:result}
})

const fanSchema = z.string().regex(/^[A-Za-z0-9._:-]{1,140}$/).describe("The fan ID shown by Satin for your chat or a stable ID for your client.")
const feedURI = (fan: string) => "nai-lattice://task-feed/" + encodeURIComponent(fan)

server.registerTool("todo_follow", {
  title:"Follow this arc ✨",
  description:"Follow or unfollow a task by ID, or list your follows. Done is the default drop. Set on to catch other statuses. Use the fan ID in your Satin prompt.",
  inputSchema:{fan:fanSchema,id:z.string().min(1).max(120).optional(),action:z.enum(["follow","unfollow","list"]).default("follow"),
    on:z.array(taskStatus).min(1).max(4).optional()},
  annotations:{readOnlyHint:false,destructiveHint:false,idempotentHint:true,openWorldHint:false},
}, async ({fan,id,action,on}) => {
  if (action === "list") {
    return {content:[{type:"text" as const,text:JSON.stringify({following:readFollowing(fan),feed:feedURI(fan)})}]}
  }
  if (!id) throw Error("Pick a task ID to follow or unfollow.")
  const result = await followTask(fan,id,action==="follow" ? on ?? ["done"] : undefined)
  const data = {message:action==="follow" ? "Following this arc ✨" : "Unfollowed",id,following:result.following.length,
    feed:feedURI(fan),next:"Use todo_feed for new drops or subscribe to the resource in an MCP client."}
  return {content:[{type:"text" as const,text:JSON.stringify(data)}]}
})

server.registerTool("todo_feed", {
  title:"Your drop feed",
  description:"Read recent state changes on tasks you follow. Acknowledge consumed drops by task ID and revision. Unread drops survive reconnects; more means catch up another page after acknowledging.",
  inputSchema:{fan:fanSchema,ack:z.array(z.object({id:z.string().max(120),revision:z.number().int().positive()})).max(12).optional()},
  annotations:{readOnlyHint:false,destructiveHint:false,idempotentHint:true,openWorldHint:false},
}, async ({fan,ack}) => {
  if (ack?.length) await acknowledgeFeed(fan,ack)
  const {latest:_latest,...feed} = readFeed(fan)
  return {content:[{type:"text" as const,text:JSON.stringify({...feed,following:readFollowing(fan).length})}]}
})

server.registerResource("Task drop feed",new ResourceTemplate("nai-lattice://task-feed/{fan}",{list:undefined}),
  {mimeType:"application/json",description:"Unread state changes for the subscribed fan ID."}, (uri,variables) => {
    const fan = fanSchema.parse(decodeURIComponent(variables.fan as string))
    const {latest:_latest,...data} = readFeed(fan)
    return {contents:[{uri:uri.href,mimeType:"application/json",text:JSON.stringify(data)}]}
  })

const resourceFollowers = new Map<string,{fan:string; latest:Record<string,number>}>()
let stopResourceWatch: (() => void) | undefined
function subscribeFan(uri: string) {
  const matched = /^nai-lattice:\/\/task-feed\/([^/?#]+)$/.exec(uri)
  if (!matched) throw Error("Unknown task feed resource")
  return fanSchema.parse(decodeURIComponent(matched[1]))
}

server.server.setRequestHandler(SubscribeRequestSchema,({params}) => {
  const fan = subscribeFan(params.uri)
  if (resourceFollowers.has(params.uri) && stopResourceWatch) return {}
  if (!resourceFollowers.has(params.uri) && resourceFollowers.size >= 32) throw Error("Resource subscription limit reached")
  const ledger = todoPath()
  stopResourceWatch ??= watchTodoChanges(ledger, file => {
    let snapshot: Task[] | undefined
    for (const [uri,sub] of resourceFollowers) {
      if (file && file !== basename(ledger) && file !== basename(followPath(sub.fan,ledger))) continue
      try {
        const following = readFollowing(sub.fan,ledger)
        if (!following.length) continue
        const current = makeFeed(following,snapshot ??= readTodos(false,ledger)).latest
        const newer = Object.entries(current).some(([id,rev]) => rev > (sub.latest[id] ?? 0))
        Object.assign(sub.latest,current)
        if (newer) void server.server.sendResourceUpdated({uri}).catch(() => {})
      } catch { /* durable feed remains available on the next read */ }
    }
  },() => {stopResourceWatch = undefined})
  resourceFollowers.set(params.uri,{fan,latest:readFeed(fan).latest})
  return {}
})
server.server.setRequestHandler(UnsubscribeRequestSchema,({params}) => {
  resourceFollowers.delete(params.uri)
  if (!resourceFollowers.size) {
    stopResourceWatch?.()
    stopResourceWatch = undefined
  }
  return {}
})
server.server.onclose = () => {
  stopResourceWatch?.()
  stopResourceWatch = undefined
  resourceFollowers.clear()
}

async function main(): Promise<void> {
  const transport = new StdioServerTransport()
  await server.connect(transport)
  console.error("[mcp-nai-lattice] ready on stdio")
}
// Windows-safe entry guard: match the last slash-separated segment of process.argv[1]
const entry = process.argv[1]?.split(/[\\/]/).pop()
if (entry && import.meta.url.split(/[\\/]/).pop() === entry) {
  main().catch((e) => {
    console.error("[mcp-nai-lattice] fatal:", e)
    process.exit(1)
  })
}
