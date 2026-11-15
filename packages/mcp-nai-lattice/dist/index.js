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
 * Env (all optional, sane localhost defaults):
 *   LM_STUDIO_URL      = http://127.0.0.1:1234/v1        (OpenAI-compatible embeddings endpoint)
 *   EMBEDDING_MODEL    = text-embedding-nomic-embed-text-v1.5
 *   EMBEDDING_DIM      = 768
 *   QDRANT_URL         = http://localhost:6333
 *   QDRANT_API_KEY     = (optional)
 */
import { createHash } from "node:crypto";
import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { basename, dirname, join } from "node:path";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
const __dirname = dirname(fileURLToPath(import.meta.url));
const LM_BASE = process.env.LM_STUDIO_URL ?? "http://127.0.0.1:1234/v1";
const EMBED_MODEL = process.env.EMBEDDING_MODEL ?? "text-embedding-nomic-embed-text-v1.5";
const EMBED_DIM = Number(process.env.EMBEDDING_DIM ?? 768);
const QDRANT_URL = (process.env.QDRANT_URL ?? "http://localhost:6333").replace(/\/+$/, "");
const QDRANT_KEY = process.env.QDRANT_API_KEY;
/** The hand-maintained project registry (canonical, in NAI-Lattice). */
const REGISTRY_PATH = process.env.REGISTRY_PATH ?? join(__dirname, "..", "projects.json");
function loadRegistry() {
    try {
        const j = JSON.parse(readFileSync(REGISTRY_PATH, "utf8"));
        return j.projects ?? [];
    }
    catch {
        return [];
    }
}
function saveRegistry(projects) {
    writeFileSync(REGISTRY_PATH, JSON.stringify({ projects }, null, 2), "utf8");
}
/** Best-effort tech-stack sniff from common manifests in a project root. */
export function sniffStack(root) {
    const out = [];
    const read = (p) => {
        try {
            return readFileSync(p, "utf8");
        }
        catch {
            return undefined;
        }
    };
    // Manifest search: root manifests first; for monorepos, also scan packages/*/*.
    const manifests = [];
    const candidates = ["package.json", "Cargo.toml", "go.mod", "pyproject.toml", "requirements.txt"];
    for (const m of candidates)
        manifests.push(join(root, m));
    // gather packages/* manifest paths (monorepo) to catch nested package.json/Cargo.toml
    try {
        const pkgDir = join(root, "packages");
        const entries = readdirSync(pkgDir, { withFileTypes: true });
        for (const e of entries) {
            if (!e.isDirectory())
                continue;
            if (e.name === "node_modules")
                continue;
            for (const m of candidates) {
                const f = join(pkgDir, e.name, m);
                if (read(f) !== undefined)
                    manifests.push(f);
            }
        }
    }
    catch { /* no packages/ dir */ }
    const packageJson = (p) => {
        const s = read(p);
        if (s === undefined)
            return undefined;
        try {
            return JSON.parse(s);
        }
        catch {
            return undefined;
        }
    };
    for (const man of manifests) {
        if (man.endsWith("package.json")) {
            const d = packageJson(man);
            if (!d)
                continue;
            const all = { ...(d.dependencies ?? {}), ...(d.devDependencies ?? {}) };
            if (all.next)
                out.push(`Next.js ${String(all.next).replace(/^[\^~]/, "")}`);
            if (all.react)
                out.push("React");
            if (all.vue)
                out.push("Vue");
            if (all.svelte)
                out.push("Svelte");
            if (all.typescript ?? d.type === "module")
                out.push("TypeScript");
            if (all.vitest)
                out.push("Vitest");
            if (all.tauri)
                out.push("Tauri");
            if (all["better-sqlite3"] ?? all.sqlite3)
                out.push("SQLite");
            if (all.zod)
                out.push("Zod");
            if (all.express)
                out.push("Express");
            if (all["@qdrant/js-client-rest"] || all.qdrant)
                out.push("Qdrant");
        }
        else if (man.endsWith("Cargo.toml")) {
            const cargo = read(man) ?? "";
            if (cargo.trim() !== "") {
                if (!out.includes("Rust"))
                    out.push("Rust");
                if (/tokio/.test(cargo))
                    out.push("Tokio");
                // Tauri: distinguish v2 (workspace deps + tauri.conf.json) from v1.
                const isTauri = /\btauri\b/.test(cargo) || /\btauri-build\b/.test(cargo);
                if (isTauri) {
                    const conf = read(join(root, "tauri.conf.json")) !== undefined ||
                        read(join(root, "src-tauri", "tauri.conf.json")) !== undefined;
                    out.push(/tauri\s*=\s*"2\./.test(cargo) || conf ? "Tauri v2" : "Tauri");
                }
                if (/qdrant/.test(cargo))
                    out.push("Qdrant");
                if (/surreal/.test(cargo))
                    out.push("SurrealDB");
                if (/opencv/.test(cargo))
                    out.push("OpenCV");
            }
        }
        // Stop scanning more manifests once a representative stack is found (avoids
        // every monorepo package polluting the tag set). Root manifests first.
        // Exception: if the repo root has BOTH package.json and Cargo.toml (a hybrid
        // shell), don't stop at the first so Rust + Node both register, not one.
        const isRootManifest = man.startsWith(root);
        const hybridRoot = isRootManifest &&
            read(join(root, "package.json")) !== undefined &&
            read(join(root, "Cargo.toml")) !== undefined;
        if (out.length > 0 && isRootManifest && !hybridRoot)
            break;
    }
    // Chromium / V8 / native-build fingerprint markers (bounded depth: root, src, src/src).
    // A Chromium tree has BUILD.gn + DEPS + v8/BUILD.bazel + .clangd even when all
    // manifests are nested/deep. Catch these so a browser/native fork isn't tagged "TS only".
    const chromiumDirs = [root, join(root, "src"), join(root, "src", "src")];
    const probe = (rel) => chromiumDirs.some((d) => read(join(d, rel)) !== undefined);
    if (probe("BUILD.gn") || probe("DEPS") || probe("v8/BUILD.bazel") || probe(".clangd")) {
        out.push("Chromium");
        out.push("C++");
    }
    // vpython.toml → Chromium's Python bootstrap; pyproject.toml → Python.
    if (probe("vpython.toml"))
        out.push("Python");
    if (manifests.some((m) => m.endsWith("pyproject.toml") && read(m) !== undefined))
        out.push("Python");
    // Dominant-language signal for native repos: count source files at a bounded depth.
    // A llama.cpp/Chromium/native tree is unmistakably C++-dominant even when a stray
    // pyproject/requirements sneaks in. Only run if we haven't already found Chromium/C++.
    if (!out.includes("C++") && !out.includes("Chromium")) {
        let cpp = 0;
        let py = 0;
        let js = 0;
        try {
            const count = (dir, depth) => {
                if (depth < 0)
                    return;
                const entries = readdirSync(dir, { withFileTypes: true });
                for (const e of entries) {
                    if (e.isDirectory()) {
                        if (e.name === "node_modules" || e.name === ".git" || e.name === "build" || e.name === "dist")
                            continue;
                        count(join(dir, e.name), depth - 1);
                    }
                    else {
                        if (/\.(cpp|cc|cxx|c|h|hpp|cu)$/.test(e.name))
                            cpp++;
                        else if (/\.py$/.test(e.name))
                            py++;
                        else if (/\.(ts|tsx|js|jsx)$/.test(e.name))
                            js++;
                    }
                }
            };
            count(root, 2);
            if (cpp > 0 && cpp >= Math.max(py, js))
                out.push("C++");
        }
        catch { /* skip */ }
    }
    // fall back: if nothing found, tag Node if a package.json exists anywhere (TS monorepo default)
    if (out.length === 0 && manifests.some((m) => m.endsWith("package.json")))
        out.push("TypeScript");
    return [...new Set(out)];
}
/** Upsert a project into the registry (never deletes existing entries). */
function registerProject(entry) {
    const projects = loadRegistry();
    const idx = projects.findIndex((p) => p.workspacePath.toLowerCase() === entry.workspacePath.toLowerCase());
    if (idx >= 0) {
        // merge: keep existing where new is unset, prefer explicit new values
        const existing = projects[idx];
        projects[idx] = {
            ...existing,
            ...entry,
            techStack: entry.techStack?.length ? entry.techStack : existing.techStack,
        };
    }
    else {
        projects.push(entry);
    }
    saveRegistry(projects);
    return entry;
}
/** Collection name derived exactly the way Kilo does it. */
export function workspaceCollection(workspacePath) {
    const h = createHash("sha256").update(workspacePath).digest("hex");
    return `ws-${h.substring(0, 16)}`;
}
/** Embed a query via LM Studio. */
export async function embed(text) {
    const res = await fetch(`${LM_BASE}/embeddings`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ model: EMBED_MODEL, input: text }),
    });
    if (!res.ok)
        throw new Error(`embed failed ${res.status}: ${await res.text()}`);
    const j = (await res.json());
    const vec = j.data?.[0]?.embedding;
    if (!vec || vec.length === 0)
        throw new Error("embed returned no vector");
    if (vec.length !== EMBED_DIM)
        throw new Error(`embedding dim mismatch: expected ${EMBED_DIM}, got ${vec.length}`);
    return vec;
}
/** Search one Qdrant collection and return the indexed code hits. */
export async function searchCollection(workspacePath, vector, opts = {}) {
    const collection = workspaceCollection(workspacePath);
    const limit = opts.limit ?? 20;
    const minScore = opts.minScore ?? 0.0;
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
    });
    if (res.status === 404) {
        // collection missing -> project not indexed yet
        return [];
    }
    if (!res.ok)
        throw new Error(`qdrant search ${collection} failed ${res.status}: ${await res.text()}`);
    const points = parseQdrantQueryResult(await res.json());
    return points
        .filter((p) => p.payload?.filePath && typeof p.payload.codeChunk === "string")
        .map((p) => ({
        project: workspacePath,
        filePath: String(p.payload.filePath),
        startLine: Number(p.payload.startLine ?? 0),
        endLine: Number(p.payload.endLine ?? 0),
        codeChunk: String(p.payload.codeChunk),
        score: p.score,
    }));
}
/** List all Qdrant collections actually indexed by Kilo (ws-<hash>), with point counts. */
async function listIndexedCollections() {
    const res = await fetch(`${QDRANT_URL}/collections`, {
        headers: QDRANT_KEY ? { "api-key": QDRANT_KEY } : {},
    });
    if (!res.ok)
        throw new Error(`qdrant collections failed ${res.status}: ${await res.text()}`);
    const j = (await res.json());
    const out = [];
    for (const c of (j.result?.collections ?? []).filter((c) => c.name.startsWith("ws-"))) {
        try {
            const info = await fetch(`${QDRANT_URL}/collections/${c.name}`, { headers: QDRANT_KEY ? { "api-key": QDRANT_KEY } : {} });
            if (!info.ok)
                continue;
            const i = (await info.json());
            const v = i.result?.config?.params?.vectors;
            out.push({
                collection: c.name,
                points: i.result?.points_count ?? 0,
                dimension: typeof v === "number" ? v : (v && typeof v === "object" ? v.size ?? 0 : 0),
            });
        }
        catch {
            /* skip unreachable */
        }
    }
    return out;
}
/** Optional GPU reranker hook: POST chunk_texts to LM Studio's /rerank if present (else skip). */
export async function rerank(query, docs) {
    try {
        const res = await fetch(`${LM_BASE}/rerank`, {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ model: process.env.RERANK_MODEL ?? "", query, documents: docs.map((d) => d.text), top_n: docs.length }),
        });
        if (!res.ok)
            return []; // rerank not available on LM Studio — caller falls back to Qdrant score
        const j = (await res.json());
        return (j.results ?? []).map((r) => {
            const idx = r.index ?? (typeof r.id === "number" ? r.id : Number(r.id) || NaN);
            const d = docs[idx];
            return { id: d?.id ?? String(idx), score: r.score };
        });
    }
    catch {
        return []; // rerank off/broken — non-fatal, Qdrant score stands
    }
}
/**
 * Reciprocal-rank fusion — the canonical NAI-Lattice fusion (mirrors
 * lattice-query `reciprocal_rank_fusion`, k=60): combine two id→rank maps.
 * A document present in both lexical (BM25) and dense (vector) recall ranks at
 * the top. Unmatched ids get only their one-side rank (and any explicit score
 * is ignored here — fused rank is the signal).
 */
function rrf(a, b, k = 60) {
    const scores = new Map();
    for (const [i, x] of a.entries())
        scores.set(x.id, (scores.get(x.id) ?? 0) + 1 / (k + i + 1));
    for (const [i, x] of b.entries())
        scores.set(x.id, (scores.get(x.id) ?? 0) + 1 / (k + i + 1));
    return [...scores.entries()].map(([id, score]) => ({ id, score })).sort((x, y) => y.score - x.score);
}
/** Parses a Qdrant /points/query response into {score, payload[]}. Exported for testability. */
export function parseQdrantQueryResult(raw) {
    const j = raw;
    const points = j?.result?.points ?? (Array.isArray(j?.result) ? j.result : []);
    return points;
}
// ── Compact BM25 (no external dep; classic Robertson/Sparck-Jones) ──────────────
const ident = /[A-Za-z0-9_]+/g;
function tokenize(t) {
    // keep camelCase/underscore identifiers whole (critical for code symbol search)
    return (t.match(ident) ?? []).map((s) => s.toLowerCase());
}
/** BM25 ranks `docs` (as {id, text}) against `query` over the given corpus. */
export function bm25Rank(query, docs, k1 = 1.2, b = 0.75) {
    const q = tokenize(query);
    if (q.length === 0 || docs.length === 0)
        return [];
    const corpus = docs.map((d) => { const t = tokenize(d.text); return { id: d.id, t }; });
    const N = corpus.length;
    const avgdl = corpus.reduce((s, c) => s + c.t.length, 0) / N;
    // idf per unique query term
    const df = new Map();
    for (const d of corpus) {
        for (const term of new Set(d.t))
            df.set(term, (df.get(term) ?? 0) + 1);
    }
    const idf = (term) => {
        const n = df.get(term) ?? 0;
        return Math.log(1 + (N - n + 0.5) / (n + 0.5));
    };
    const out = new Map();
    for (const d of corpus) {
        const len = d.t.length;
        const freq = new Map();
        for (const term of d.t)
            freq.set(term, (freq.get(term) ?? 0) + 1);
        let score = 0;
        for (const term of new Set(q)) {
            const f = freq.get(term) ?? 0;
            if (f === 0)
                continue;
            score += idf(term) * ((f * (k1 + 1)) / (f + k1 * (1 - b + b * (len / avgdl))));
        }
        if (score > 0)
            out.set(d.id, score);
    }
    return [...out.entries()].map(([id, score]) => ({ id, score })).sort((x, y) => y.score - x.score);
}
/**
 * Fuse the dense (Qdrant cosine) ordering with a lexical BM25 ordering over the
 * SAME retrieved candidates using reciprocal-rank fusion — the NAI-Lattice hybrid
 * recall recipe for code. Exact symbol identifiers that dense embeddings miss
 * get rescued by BM25. Cheap, GPU-free, deterministic.
 */
export function fuseDenseAndLexical(query, dense) {
    if (dense.length < 2)
        return dense;
    const denseOrder = dense.map((h) => ({ id: hitKey(h) }));
    const lex = bm25Rank(query, dense.map((h) => ({ id: hitKey(h), text: h.codeChunk })));
    if (lex.length === 0)
        return dense;
    const fused = rrf(denseOrder, lex);
    const byKey = new Map(dense.map((h) => [hitKey(h), h]));
    const out = [];
    for (const f of fused) {
        const h = byKey.get(f.id);
        if (h)
            out.push({ ...h, score: f.score }); // replace cosine score with fused rank score
    }
    return out;
}
const hitKey = (h) => `${h.project}\0${h.filePath}\0${h.startLine}\0${h.endLine}`;
/**
 * Optional GPU reranker on top of the fused order: LM Studio has no /rerank today,
 * so this normally returns [] and the fused (BM25+RRF) order stands. When a
 * /rerank endpoint appears, it re-orders the fused hits by rerank score.
 */
async function gpuRerankIfAvailable(query, hits) {
    const rr = await rerank(query, hits.map((h) => ({ id: hitKey(h), text: h.codeChunk })));
    return rr;
}
const server = new McpServer({ name: "mcp-nai-lattice", version: "0.1.0" });
const READ_ONLY = { readOnlyHint: true, destructiveHint: false, idempotentHint: true, openWorldHint: false };
server.registerTool("list_indexed_projects", {
    title: "List projects available for semantic search",
    description: "Discover which projects have a Kilo Qdrant code index. Returns every indexed Qdrant collection (live from Qdrant) fused with the human project registry (name + description + workspacePath). Use this first to learn a project's workspacePath (or its registry name) before calling semantic_query / cross_project_semantic_query.",
    inputSchema: {},
    outputSchema: {
        projects: z.array(z.object({
            project: z.string().optional(),
            description: z.string().optional(),
            techStack: z.array(z.string()).optional(),
            workspacePath: z.string(),
            collection: z.string(),
            points: z.number(),
            dimension: z.number(),
            registered: z.boolean(),
        })),
        unregistered: z.array(z.string()).describe("Indexed collections with no registry entry (hash unknown — add a projects.json entry to name them)."),
    },
    annotations: READ_ONLY,
}, async () => {
    const registry = loadRegistry();
    const online = await listIndexedCollections();
    const byHash = new Map();
    for (const p of registry)
        byHash.set(workspaceCollection(p.workspacePath), p);
    const projects = online
        .map((c) => {
        const reg = byHash.get(c.collection);
        return {
            project: reg?.name,
            description: reg?.description,
            techStack: reg?.techStack,
            workspacePath: reg?.workspacePath ?? c.collection, // unknown path → collection, caller can look it up
            collection: c.collection,
            points: c.points,
            dimension: c.dimension,
            registered: !!reg,
        };
    })
        .sort((a, b) => b.points - a.points);
    const unregistered = projects.filter((p) => !p.registered).map((p) => p.collection);
    return {
        content: [{ type: "text", text: JSON.stringify({ projects, unregistered }) }],
        structuredContent: { projects, unregistered },
    };
});
/** Resolve a project selector (name or workspace path) to collection/hash. */
function resolveProject(selector) {
    const reg = loadRegistry().find((p) => p.name.toLowerCase() === selector.toLowerCase());
    const workspacePath = reg?.workspacePath ?? selector;
    return { workspacePath, collection: workspaceCollection(workspacePath), name: reg?.name };
}
server.registerTool("register_current_project", {
    title: "Register the current workspace in the project registry",
    description: "Add (or update) the given workspace in mcp-nai-lattice's projects.json so it becomes discoverable via list_indexed_projects and queryable by name. Observes the repo to fill missing deltas: project name (folder basename), techStack (sniffed from package.json/Cargo.toml/go.mod/pyproject), and it logs which Qdrant collection the workspace would map to. Pass name/description/techStack to override or enrich; an existing entry is merged, never deleted. Idempotent.",
    inputSchema: {
        workspacePath: z.string().describe("Absolute workspace root to register."),
        name: z.string().optional().describe("Short project name; defaults to the folder basename."),
        description: z.string().optional().describe("One-line project description (recommended for discovery)."),
        techStack: z.array(z.string()).optional().describe("Tech-stack tags; defaults to sniffing package.json/Cargo.toml/etc."),
    },
    outputSchema: {
        registered: z.boolean(),
        entry: z.object({ name: z.string(), description: z.string().optional(), techStack: z.array(z.string()).optional(), workspacePath: z.string() }),
        collection: z.string(),
        derived: z.object({ name: z.string(), techStack: z.array(z.string()) }).describe("What was auto-derived (could not find source: what the agent must supply)."),
    },
    annotations: { readOnlyHint: false, destructiveHint: false, idempotentHint: true, openWorldHint: false },
}, async ({ workspacePath, name, description, techStack }) => {
    // Preserve curated fields on re-register: only override name/description/
    // techStack when explicitly passed, else keep the existing registration.
    const existing = loadRegistry().find((p) => p.workspacePath.toLowerCase() === workspacePath.toLowerCase());
    const derivedName = name ?? existing?.name ?? basename(workspacePath.replace(/[\\/]+$/, ""));
    const derivedStack = techStack?.length ? techStack : existing?.techStack ?? sniffStack(workspacePath);
    const desc = description ?? existing?.description;
    const entry = {
        name: derivedName,
        workspacePath,
        ...(desc ? { description: desc } : {}),
        ...(derivedStack.length ? { techStack: derivedStack } : {}),
    };
    const saved = registerProject(entry);
    return {
        content: [{ type: "text", text: JSON.stringify({ registered: true, entry: saved, collection: workspaceCollection(workspacePath) }) }],
        structuredContent: {
            registered: true,
            entry: saved,
            collection: workspaceCollection(workspacePath),
            derived: { name: derivedName, techStack: derivedStack },
        },
    };
});
server.registerTool("semantic_query", {
    title: "Semantic search the current project's code index",
    description: "Embed the query with LM Studio and Cosine-search the current workspace's Qdrant code index (collection ws-<sha256(workspace)>). Returns matched code chunks with filePath, line range, and score. Use when you want code roughly like the query regardless of exact symbol names.",
    inputSchema: {
        query: z.string().describe("Natural-language / fuzzy code search query."),
        limit: z.number().int().min(1).max(100).optional().describe("Max results (default 20)."),
        minScore: z.number().min(0).max(1).optional().describe("Qdrant score threshold (default 0)."),
        workspacePath: z.string().optional().describe("Explicit workspace root to search; defaults to the server's cwd. IMPORTANT: pass the path exactly as it is on disk (drive letter + backslashes, e.g. D:\\_SATIN_AI_2\\Tutty\\tutty-studio) — the Qdrant collection is derived from the sha256 of this exact string. A lowercased / forward-slashed UI-normalized form yields a different (empty) collection; if a search returns no hits, retry the same path in the alternate casing/slashes."),
        applyRerank: z.boolean().optional().describe("Rerank hits on top if a /rerank endpoint is configured (default true)."),
    },
    outputSchema: {
        collection: z.string(),
        hits: z.array(z.object({ project: z.string(), filePath: z.string(), startLine: z.number(), endLine: z.number(), codeChunk: z.string(), score: z.number() })),
    },
    annotations: READ_ONLY,
}, async ({ query, limit, minScore, workspacePath, applyRerank }) => {
    const ws = workspacePath ?? process.cwd();
    const collection = workspaceCollection(ws);
    const vector = await embed(query);
    const hits = await searchCollection(ws, vector, { limit, minScore });
    // BM25+RRF fusion first (rescues exact identifiers); then optional GPU reranker.
    let ordered = applyRerank !== false ? fuseDenseAndLexical(query, hits) : hits;
    if (applyRerank !== false && ordered.length > 1) {
        const byId = new Map();
        const rr = await gpuRerankIfAvailable(query, ordered);
        for (const r of rr)
            byId.set(r.id, r.score);
        if (rr.length) {
            ordered = [...ordered].sort((a, b) => (byId.get(hitKey(b)) ?? b.score) - (byId.get(hitKey(a)) ?? a.score));
        }
    }
    return {
        content: [{ type: "text", text: JSON.stringify({ collection, hits: ordered }) }],
        structuredContent: { collection, hits: ordered },
    };
});
server.registerTool("cross_project_semantic_query", {
    title: "Semantic search across multiple projects' code indexes",
    description: "Embed the query once and search each listed workspace's Qdrant code index (each ws-<sha256(path)>). Returns merged, de-duplicated code hits that may span different projects. Use for cross-project / cross-app semantic code search.",
    inputSchema: {
        query: z.string().describe("Natural-language / fuzzy code search query."),
        workspacePaths: z.array(z.string()).min(1).describe("Workspace roots (or registry project names) to search across. Each path is hashed exactly as given (sha256 → ws-<16hex>) to find its Qdrant collection; registry names from list_indexed_projects are resolved to their registered path. Use on-disk paths (not the UI-lowercased form); zero hits usually means a casing/slash mismatch."),
        limitPerProject: z.number().int().min(1).max(100).optional().describe("Max results per project (default 10)."),
        applyRerank: z.boolean().optional().describe("Rerank merged hits if a /rerank endpoint is configured (default true)."),
    },
    outputSchema: {
        hits: z.array(z.object({ project: z.string(), filePath: z.string(), startLine: z.number(), endLine: z.number(), codeChunk: z.string(), score: z.number() })),
    },
    annotations: READ_ONLY,
}, async ({ query, workspacePaths, limitPerProject, applyRerank }) => {
    const vector = await embed(query);
    const per = limitPerProject ?? 10;
    const collected = [];
    for (const sel of workspacePaths) {
        const { workspacePath } = resolveProject(sel);
        const found = await searchCollection(workspacePath, vector, { limit: per });
        collected.push(...found);
    }
    // de-dup by filePath+line range
    const seen = new Set();
    const merged = [];
    for (const h of [...collected].sort((a, b) => b.score - a.score)) {
        const k = `${h.project}\0${h.filePath}\0${h.startLine}\0${h.endLine}`;
        if (seen.has(k))
            continue;
        seen.add(k);
        merged.push(h);
    }
    let ordered = merged;
    if (applyRerank !== false) {
        ordered = fuseDenseAndLexical(query, merged);
        if (ordered.length > 1) {
            const rr = await gpuRerankIfAvailable(query, ordered);
            if (rr.length) {
                const byId = new Map(rr.map((r) => [r.id, r.score]));
                ordered = [...ordered].sort((a, b) => (byId.get(hitKey(b)) ?? b.score) - (byId.get(hitKey(a)) ?? a.score));
            }
        }
    }
    return {
        content: [{ type: "text", text: JSON.stringify({ hits: ordered }) }],
        structuredContent: { hits: ordered },
    };
});
async function main() {
    const transport = new StdioServerTransport();
    await server.connect(transport);
    console.error("[mcp-nai-lattice] ready on stdio");
}
// Windows-safe entry guard: match the last slash-separated segment of process.argv[1]
const entry = process.argv[1]?.split(/[\\/]/).pop();
if (entry && import.meta.url.split(/[\\/]/).pop() === entry) {
    main().catch((e) => {
        console.error("[mcp-nai-lattice] fatal:", e);
        process.exit(1);
    });
}
