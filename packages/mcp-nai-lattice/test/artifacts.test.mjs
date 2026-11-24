// artifacts.test.mjs — artifact tool delta: pure validation/response helpers plus
// an in-memory MCP handshake. No fabric host is required unless NAI_LATTICE_LIVE=1.
// Run: npm test -w @cyberluke/mcp-nai-lattice
import { test } from "node:test"
import assert from "node:assert/strict"
import { createHash } from "node:crypto"
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js"
import { Client } from "@modelcontextprotocol/sdk/client/index.js"
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js"

const a = await import(new URL("../dist/artifacts.js", import.meta.url).href)

const HASH = "ab".repeat(32)
const URI = `${a.BLAKE3_URI_PREFIX}${HASH}`
const HELLO_B64 = "aGVsbG8="
const HELLO_SHA256 = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"

// --- limits and base64 gate -------------------------------------------------
test("artifact limits are the documented 8 MiB write / 1 MiB inline caps", () => {
  assert.equal(a.MAX_ARTIFACT_BYTES, 8 * 1024 * 1024)
  assert.equal(a.MAX_INLINE_BYTES, 1024 * 1024)
})

test("decodeBase64Strict accepts canonical standard base64 and empty input", () => {
  const hello = a.decodeBase64Strict(HELLO_B64)
  assert.equal(hello.bytes.toString("utf8"), "hello")
  assert.equal(hello.base64, HELLO_B64)
  assert.equal(a.decodeBase64Strict("").bytes.length, 0)
})

test("decodeBase64Strict rejects malformed, padded and URL-safe base64", () => {
  for (const bad of ["aGVsbG8", "aGVsbG8===", "aGVs bG8=", "aGVsbG8-_=", "====", "A==="]) {
    assert.throws(() => a.decodeBase64Strict(bad), (e) => e.code === a.ARTIFACT_INVALID_REQUEST, bad)
  }
  assert.throws(() => a.decodeBase64Strict(Buffer.from("hi").toString("base64url") + "!"), (e) => e.code === a.ARTIFACT_INVALID_REQUEST)
})

test("decodeBase64Strict enforces the 8 MiB decoded limit before allocating", () => {
  // 4n base64 chars decode to 3n bytes; this is one byte over the cap.
  const over = "A".repeat((Math.floor(a.MAX_ARTIFACT_BYTES / 3) + 1) * 4)
  assert.throws(() => a.decodeBase64Strict(over), (e) =>
    e.code === a.ARTIFACT_INVALID_REQUEST && e.details?.max === a.MAX_ARTIFACT_BYTES,
  )
})

test("base64DecodedLength matches Buffer for representative strings", () => {
  for (const s of ["", "QQ==", "QUI=", "QUJD", "aGVsbG8="]) {
    assert.equal(a.base64DecodedLength(s), Buffer.from(s, "base64").length, s)
  }
})

// --- URI / hash normalization ----------------------------------------------
test("parseArtifactUri accepts canonical URIs and lowercases the digest", () => {
  const upper = `${a.BLAKE3_URI_PREFIX}${HASH.toUpperCase()}`
  assert.deepEqual(a.parseArtifactUri(upper), { uri: URI, hash: HASH })
  assert.throws(() => a.parseArtifactUri(`artifact://sha256/${HASH}`), (e) => e.code === a.ARTIFACT_INVALID_REQUEST)
  assert.throws(() => a.parseArtifactUri(`${a.BLAKE3_URI_PREFIX}${HASH}extra`), (e) => e.code === a.ARTIFACT_INVALID_REQUEST)
  assert.throws(() => a.parseArtifactUri(`${a.BLAKE3_URI_PREFIX}${"z".repeat(64)}`), (e) => e.code === a.ARTIFACT_INVALID_REQUEST)
})

test("parseExpectedHash accepts raw BLAKE3 hex and artifact URIs only", () => {
  assert.equal(a.parseExpectedHash(HASH.toUpperCase()), HASH)
  assert.equal(a.parseExpectedHash(URI), HASH)
  for (const bad of ["", "nope", "0".repeat(63), `artifact://blake3/${"z".repeat(64)}`]) {
    assert.throws(() => a.parseExpectedHash(bad), (e) => e.code === a.ARTIFACT_INVALID_REQUEST, bad)
  }
})

// --- media / logical type validation ---------------------------------------
test("normalizeMediaType defaults, trims and rejects non type/subtype values", () => {
  assert.equal(a.normalizeMediaType(undefined), "application/octet-stream")
  assert.equal(a.normalizeMediaType(" text/plain; charset=utf-8 "), "text/plain; charset=utf-8")
  for (const bad of ["", "plain", "/plain", "text/", "text/\nplain"]) {
    assert.throws(() => a.normalizeMediaType(bad), (e) => e.code === a.ARTIFACT_INVALID_REQUEST, bad)
  }
})

test("normalizeLogicalType defaults and enforces a safe label", () => {
  assert.equal(a.normalizeLogicalType(undefined), "blob")
  assert.equal(a.normalizeLogicalType(" evidence/repro-1 "), "evidence/repro-1")
  for (const bad of ["", " ", "-leading", "bad label", "x".repeat(129)]) {
    assert.throws(() => a.normalizeLogicalType(bad), (e) => e.code === a.ARTIFACT_INVALID_REQUEST, bad)
  }
})

// --- host response validation ----------------------------------------------
test("buildPutResult validates the URI and deduplicated flag", () => {
  assert.deepEqual(a.buildPutResult({ artifact_uri: URI, deduplicated: false }), {
    artifact_uri: URI,
    deduplicated: false,
  })
  assert.throws(() => a.buildPutResult({ artifact_uri: URI }), (e) => e.code === a.ARTIFACT_BAD_RESPONSE)
  assert.throws(() => a.buildPutResult({ artifact_uri: "artifact://other/x", deduplicated: false }), (e) => e.code === a.ARTIFACT_BAD_RESPONSE)
})

test("buildResolveResult computes sha256/blake3 and inlines small payloads", () => {
  const result = a.buildResolveResult({ artifact_uri: URI, data: HELLO_B64, size: 5 })
  assert.deepEqual(result, {
    artifact_uri: URI,
    size: 5,
    sha256: HELLO_SHA256,
    blake3: HASH,
    inline: true,
    dataBase64: HELLO_B64,
  })
})

test("buildResolveResult returns metadata only above the inline cap", () => {
  const result = a.buildResolveResult({ artifact_uri: URI, data: HELLO_B64, size: 5 }, 4)
  assert.equal(result.inline, false)
  assert.equal(result.size, 5)
  assert.equal(result.dataBase64, undefined)
  assert.equal(result.sha256, HELLO_SHA256)
})

test("buildResolveResult rejects size mismatch and non-base64 data", () => {
  assert.throws(
    () => a.buildResolveResult({ artifact_uri: URI, data: HELLO_B64, size: 6 }),
    (e) => e.code === a.ARTIFACT_BAD_RESPONSE,
  )
  assert.throws(
    () => a.buildResolveResult({ artifact_uri: URI, data: "%%%", size: 5 }),
    (e) => e.code === a.ARTIFACT_BAD_RESPONSE,
  )
  assert.throws(() => a.buildResolveResult(null), (e) => e.code === a.ARTIFACT_BAD_RESPONSE)
})

test("buildVerifyResult requires a boolean", () => {
  assert.deepEqual(a.buildVerifyResult({ verified: true }), { verified: true })
  assert.deepEqual(a.buildVerifyResult({ verified: false }), { verified: false })
  assert.throws(() => a.buildVerifyResult({ verified: "true" }), (e) => e.code === a.ARTIFACT_BAD_RESPONSE)
})

test("artifactErrorPayload exposes stable codes for typed and unknown failures", () => {
  const typed = a.artifactErrorPayload(new a.ArtifactToolError(a.ARTIFACT_INVALID_REQUEST, "bad", { field: "x" }))
  assert.equal(typed.code, a.ARTIFACT_INVALID_REQUEST)
  assert.equal(typed.ok, false)
  assert.deepEqual(typed.details, { field: "x" })
  const unknown = a.artifactErrorPayload(new Error("boom"))
  assert.equal(unknown.code, a.ARTIFACT_INTERNAL_ERROR)
})

// --- MCP wire contract (in-memory transport, no fabric host) ----------------
async function wireHarness() {
  const server = new McpServer({ name: "artifact-test", version: "0.0.0" })
  a.registerArtifactTools(server)
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair()
  await server.connect(serverTransport)
  const client = new Client({ name: "artifact-test-client", version: "0.0.0" })
  await client.connect(clientTransport)
  return { server, client }
}

test("artifact tools are listed with write/read annotations", async () => {
  const { server, client } = await wireHarness()
  try {
    const { tools } = await client.listTools()
    const byName = new Map(tools.map((t) => [t.name, t]))
    const put = byName.get("nai_lattice_artifact_put")
    const resolve = byName.get("nai_lattice_artifact_resolve")
    const verify = byName.get("nai_lattice_artifact_verify")
    assert.ok(put && resolve && verify)
    assert.equal(put.annotations?.readOnlyHint, false)
    assert.equal(put.annotations?.destructiveHint, false)
    assert.equal(put.annotations?.idempotentHint, true)
    assert.equal(resolve.annotations?.readOnlyHint, true)
    assert.equal(verify.annotations?.readOnlyHint, true)
    assert.equal(resolve.annotations?.destructiveHint, false)
  } finally {
    await client.close()
    await server.close()
  }
})

test("invalid artifact requests return typed JSON errors, never throw", async () => {
  const { server, client } = await wireHarness()
  try {
    const cases = [
      ["nai_lattice_artifact_put", { data_base64: "aGVsbG8" }],
      ["nai_lattice_artifact_put", { data_base64: "%%%not-base64%%%" }],
      ["nai_lattice_artifact_resolve", { uri: "artifact://sha256/deadbeef" }],
      ["nai_lattice_artifact_verify", { expected_hash: "nope" }],
    ]
    for (const [name, args] of cases) {
      const result = await client.callTool({ name, arguments: args })
      assert.equal(result.isError, true, name)
      const payload = JSON.parse(result.content[0].text)
      assert.equal(payload.ok, false, name)
      assert.equal(payload.code, a.ARTIFACT_INVALID_REQUEST, name)
      assert.equal(typeof payload.error, "string", name)
    }
  } finally {
    await client.close()
    await server.close()
  }
})

// --- optional live fabric check --------------------------------------------
test("live: artifact put/resolve/verify round trip", { skip: process.env.NAI_LATTICE_LIVE !== "1" }, async () => {
  const bytes = Buffer.from(`mcp-nai-lattice live ${Date.now()}`)
  const data_base64 = bytes.toString("base64")
  const put = await a.artifactPut({ data_base64, media_type: "text/plain", logical_type: "evidence" })
  assert.match(put.artifact_uri, /^artifact:\/\/blake3\/[0-9a-f]{64}$/)
  assert.equal(typeof put.deduplicated, "boolean")

  const resolved = await a.artifactResolve({ uri: put.artifact_uri })
  assert.equal(resolved.artifact_uri, put.artifact_uri)
  assert.equal(resolved.size, bytes.length)
  assert.equal(resolved.sha256, createHash("sha256").update(bytes).digest("hex"))
  assert.equal(resolved.dataBase64, data_base64)

  const verified = await a.artifactVerify({ expected_hash: put.artifact_uri })
  assert.equal(verified.verified, true)
})

test("live: MCP wire round trip validates output schemas", { skip: process.env.NAI_LATTICE_LIVE !== "1" }, async () => {
  const { server, client } = await wireHarness()
  try {
    const putResult = await client.callTool({
      name: "nai_lattice_artifact_put",
      arguments: { data_base64: HELLO_B64, media_type: "text/plain" },
    })
    assert.ok(!putResult.isError)
    const put = JSON.parse(putResult.content[0].text)
    assert.match(put.artifact_uri, /^artifact:\/\/blake3\/[0-9a-f]{64}$/)
    assert.equal(putResult.structuredContent.artifact_uri, put.artifact_uri)

    const resolveResult = await client.callTool({
      name: "nai_lattice_artifact_resolve",
      arguments: { uri: put.artifact_uri },
    })
    assert.ok(!resolveResult.isError)
    const resolved = JSON.parse(resolveResult.content[0].text)
    assert.equal(resolved.dataBase64, HELLO_B64)
    assert.equal(resolved.sha256, HELLO_SHA256)
    assert.equal(resolveResult.structuredContent.size, 5)

    const verifyResult = await client.callTool({
      name: "nai_lattice_artifact_verify",
      arguments: { expected_hash: put.artifact_uri },
    })
    assert.ok(!verifyResult.isError)
    assert.equal(JSON.parse(verifyResult.content[0].text).verified, true)
    assert.equal(verifyResult.structuredContent.verified, true)
  } finally {
    await client.close()
    await server.close()
  }
})
