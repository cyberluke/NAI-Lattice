// unit.test.mjs — node:test for mcp-nai-lattice pure logic, run against the built dist.
// Run: node --test test/
import { test } from "node:test"
import assert from "node:assert/strict"

const mod = await import(new URL("../dist/index.js", import.meta.url).href)
const { workspaceCollection, bm25Rank, parseQdrantQueryResult, fuseDenseAndLexical, embed, sniffStack } = mod

// --- workspaceCollection (the Kilo collection-hash contract) ---
test("workspaceCollection derives ws-<sha256(0,16)> matching Kilo", () => {
  // Synthetic fixture (no machine-specific paths): uppercase/backslash form.
  assert.equal(workspaceCollection("D:\\work\\example"), "ws-f03043509a1a2950")
})

test("collection hash is case/slash-sensitive (verified different for lowercased)", () => {
  const norm = workspaceCollection("d:/work/example")
  assert.equal(norm, "ws-e21095ac842fc8bb") // NOT the uppercase/backslash hash
  assert.notEqual(norm, "ws-f03043509a1a2950")
})

// --- parseQdrantQueryResult (the Qdrant /points/query shape contract) ---
test("parseQdrantQueryResult handles result.points (Qdrant query response)", () => {
  const parsed = parseQdrantQueryResult({ result: { points: [{ score: 0.9, payload: { filePath: "a.ts" } }] } })
  assert.equal(parsed.length, 1)
  assert.equal(parsed[0].score, 0.9)
})
test("parseQdrantQueryResult is empty on missing/empty result", () => {
  assert.equal(parseQdrantQueryResult({}).length, 0)
  assert.equal(parseQdrantQueryResult({ result: {} }).length, 0)
})

// --- bm25Rank ---
test("bm25Rank ranks exact-symbol matches above token-dither", () => {
  const docs = [
    { id: "generic", text: "export function process() { handleEverything() }" },
    { id: "exact", text: "retry(jobKey) { this.requeue(SqlitePublicationQueue.RETRY) }" },
  ]
  const q = "SqlitePublicationQueue RETRY requeue"
  const ranked = bm25Rank(q, docs)
  assert.ok(ranked.length > 0)
  assert.equal(ranked[0].id, "exact")
})

// --- fuseDenseAndLexical (BM25+RRF rescues exact identifiers from a worst dense order) ---
test("fuseDenseAndLexical promotes the exact-symbol chunk to the top", () => {
  const dense = [
    { project: "p", filePath: "generic.ts", startLine: 1, endLine: 2, codeChunk: "process() { handleEverything() }", score: 0.8 },
    { project: "p", filePath: "queue.ts", startLine: 90, endLine: 92, codeChunk: "retry() { this.requeue(SqlitePublicationQueue.RETRY) }", score: 0.72 },
  ]
  const fused = fuseDenseAndLexical("SqlitePublicationQueue retry requeue", dense)
  assert.equal(fused[0].filePath, "queue.ts")
})

// --- sniffStack (tech-stack detection; guards the Chromium-rooted monorepo case) ---
test("sniffStack detects Chromium/C++ via BUILD.gn + vpython markers", async () => {
  const { mkdtempSync, mkdirSync, writeFileSync, rmSync } = await import("node:fs")
  const { join } = await import("node:path")
  const { tmpdir } = await import("node:os")
  const root = mkdtempSync(join(tmpdir(), "sniff-"))
  try {
    const srcDir = join(root, "src", "src")
    mkdirSync(srcDir, { recursive: true })
    writeFileSync(join(srcDir, "BUILD.gn"), "// chromium\n")
    writeFileSync(join(srcDir, "vpython.toml"), "[python_version]\n")
    const stack = sniffStack(root)
    assert.ok(stack.includes("Chromium"), `expected Chromium in ${stack}`)
    assert.ok(stack.includes("C++"), `expected C++ in ${stack}`)
    assert.ok(stack.includes("Python"), `expected Python in ${stack}`)
  } finally {
    rmSync(root, { recursive: true, force: true })
  }
})