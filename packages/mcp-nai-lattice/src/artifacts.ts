/**
 * artifacts.ts — generic NAF/1 artifact tools for the NAI Lattice fabric.
 *
 * The native fabric host owns the content-addressed store (BLAKE3 CAS) and
 * serves `artifact.put` / `artifact.open` / `artifact.verify`. This module adds
 * ONLY the MCP delta: strict input validation, a bounded inline read policy and
 * a short-lived client per call. It keeps no artifact store of its own.
 *
 * Host contract (crates/lattice-runtime):
 *   artifact.put    { data: base64, media_type?, logical_type? } -> { artifact_uri, deduplicated }
 *   artifact.open   { artifact_uri }                             -> { artifact_uri, data: base64, size }
 *   artifact.verify { expected_hash: blake3-hex }                -> { verified }
 *
 * `artifact.open` has no hash metadata field; the BLAKE3 digest is carried by
 * the canonical `artifact://blake3/<hex>` URI. `sha256` in the resolve result is
 * computed client-side over the returned bytes so callers get a conventional
 * checksum alongside the BLAKE3 artifact identity.
 */
import { createHash } from "node:crypto"
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js"
import { NafClient } from "@cyberluke/nai-lattice-client"
import { z } from "zod"

/** Maximum decoded payload accepted by `nai_lattice_artifact_put` (8 MiB). */
export const MAX_ARTIFACT_BYTES = 8 * 1024 * 1024
/** Maximum bytes returned inline by `nai_lattice_artifact_resolve` (1 MiB). */
export const MAX_INLINE_BYTES = 1024 * 1024
/** Canonical artifact URI prefix served by the fabric CAS. */
export const BLAKE3_URI_PREFIX = "artifact://blake3/"

export const ARTIFACT_INVALID_REQUEST = "NAI_LATTICE_ARTIFACT_INVALID_REQUEST"
export const ARTIFACT_UNAVAILABLE = "NAI_LATTICE_ARTIFACT_UNAVAILABLE"
export const ARTIFACT_HOST_REJECTED = "NAI_LATTICE_ARTIFACT_HOST_REJECTED"
export const ARTIFACT_BAD_RESPONSE = "NAI_LATTICE_ARTIFACT_BAD_RESPONSE"
export const ARTIFACT_INTERNAL_ERROR = "NAI_LATTICE_ARTIFACT_INTERNAL_ERROR"

const HASH_HEX = /^[0-9a-f]{64}$/i
const CONTROL_CHARS = /[\u0000-\u001f\u007f]/
const LOGICAL_TYPE = /^[A-Za-z0-9][A-Za-z0-9._:/-]*$/

/** Linear standard-base64 alphabet check (no backtracking on hostile input). */
function isBase64Body(value: string): boolean {
  for (let i = 0; i < value.length; i++) {
    const code = value.charCodeAt(i)
    const ok =
      (code >= 65 && code <= 90) ||
      (code >= 97 && code <= 122) ||
      (code >= 48 && code <= 57) ||
      code === 43 || // +
      code === 47 // /
    if (!ok) return false
  }
  return true
}

/**
 * Typed failure with a stable machine code. Handlers convert every thrown value
 * into JSON text, so this never escapes into the MCP stream as a raw error.
 */
export class ArtifactToolError extends Error {
  constructor(
    public readonly code: string,
    message: string,
    public readonly details?: Record<string, unknown>,
  ) {
    super(message)
    this.name = "ArtifactToolError"
  }
}

function invalid(message: string, details?: Record<string, unknown>): never {
  throw new ArtifactToolError(ARTIFACT_INVALID_REQUEST, message, details)
}

function badResponse(method: string, message: string): never {
  throw new ArtifactToolError(ARTIFACT_BAD_RESPONSE, `${method}: ${message}`, { method })
}

function asRecord(value: unknown, method: string): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    badResponse(method, "response body is not a JSON object")
  }
  return value as Record<string, unknown>
}

/** Decoded byte length of a syntactically valid standard base64 string. */
export function base64DecodedLength(value: string): number {
  const padding = value.endsWith("==") ? 2 : value.endsWith("=") ? 1 : 0
  return (value.length / 4) * 3 - padding
}

/**
 * Validate and decode standard base64. Rejects whitespace, URL-safe characters,
 * malformed padding and non-canonical pad bits. The size gate runs on the
 * decoded length before any buffer is allocated.
 */
export function decodeBase64Strict(
  value: unknown,
  maxBytes: number = MAX_ARTIFACT_BYTES,
): { bytes: Buffer; base64: string } {
  if (typeof value !== "string") invalid("data_base64 must be a base64 string")
  if (value.length % 4 !== 0) {
    invalid("data_base64 is not valid standard base64 (no whitespace or URL-safe alphabet)")
  }
  // Bound the work before scanning: 4 base64 chars carry at most 3 bytes.
  const maxChars = Math.ceil(maxBytes / 3) * 4
  if (value.length > maxChars) {
    invalid(`artifact payload is too large for the ${maxBytes}-byte limit`, { max: maxBytes })
  }
  const padding = value.endsWith("==") ? 2 : value.endsWith("=") ? 1 : 0
  const body = padding === 0 ? value : value.slice(0, value.length - padding)
  if (body.includes("=") || !isBase64Body(body)) {
    invalid("data_base64 is not valid standard base64 (no whitespace or URL-safe alphabet)")
  }
  const decodedLength = base64DecodedLength(value)
  if (decodedLength > maxBytes) {
    invalid(`artifact payload is too large: ${decodedLength} bytes (max ${maxBytes})`, {
      size: decodedLength,
      max: maxBytes,
    })
  }
  const bytes = Buffer.from(value, "base64")
  if (bytes.length !== decodedLength) {
    invalid("data_base64 decoded to an unexpected length")
  }
  if (bytes.toString("base64") !== value) {
    invalid("data_base64 is not canonical base64")
  }
  return { bytes, base64: value }
}

/** Parse a canonical `artifact://blake3/<64-hex>` URI and normalize the digest. */
export function parseArtifactUri(value: unknown): { uri: string; hash: string } {
  if (typeof value !== "string") invalid("artifact_uri must be a string")
  const raw = value.trim()
  if (!raw.startsWith(BLAKE3_URI_PREFIX)) {
    invalid(`artifact_uri must start with ${BLAKE3_URI_PREFIX}`)
  }
  const hash = raw.slice(BLAKE3_URI_PREFIX.length)
  if (!HASH_HEX.test(hash)) {
    invalid("artifact_uri must carry a 64-character BLAKE3 hex digest")
  }
  const normalized = hash.toLowerCase()
  return { uri: `${BLAKE3_URI_PREFIX}${normalized}`, hash: normalized }
}

/** Accept a raw 64-hex BLAKE3 digest or the canonical artifact URI. */
export function parseExpectedHash(value: unknown): string {
  if (typeof value !== "string") invalid("expected_hash must be a string")
  const raw = value.trim()
  if (raw.startsWith(BLAKE3_URI_PREFIX)) return parseArtifactUri(raw).hash
  if (!HASH_HEX.test(raw)) {
    invalid("expected_hash must be a 64-character BLAKE3 hex digest or an artifact://blake3 URI")
  }
  return raw.toLowerCase()
}

/** Default and validate the artifact media type. */
export function normalizeMediaType(value: unknown): string {
  if (value === undefined) return "application/octet-stream"
  if (typeof value !== "string") invalid("media_type must be a string")
  const trimmed = value.trim()
  if (!trimmed || trimmed.length > 255) invalid("media_type must be 1..255 characters")
  if (CONTROL_CHARS.test(trimmed)) invalid("media_type must not contain control characters")
  const slash = trimmed.indexOf("/")
  if (slash <= 0 || slash === trimmed.length - 1) invalid("media_type must look like type/subtype")
  return trimmed
}

/** Default and validate the logical artifact type label. */
export function normalizeLogicalType(value: unknown): string {
  if (value === undefined) return "blob"
  if (typeof value !== "string") invalid("logical_type must be a string")
  const trimmed = value.trim()
  if (!trimmed || trimmed.length > 128 || !LOGICAL_TYPE.test(trimmed)) {
    invalid("logical_type must be 1..128 characters of [A-Za-z0-9._:/-] starting alphanumeric")
  }
  return trimmed
}

export type ArtifactPutResult = { artifact_uri: string; deduplicated: boolean }
export type ArtifactResolveResult = {
  artifact_uri: string
  size: number
  sha256: string
  blake3: string
  inline: boolean
  dataBase64?: string
}
export type ArtifactVerifyResult = { verified: boolean }

/** Validate a host-returned artifact URI; host drift is a bad response, not a bad request. */
function parseHostUri(value: unknown, method: string): { uri: string; hash: string } {
  try {
    return parseArtifactUri(value)
  } catch (error) {
    if (error instanceof ArtifactToolError) badResponse(method, error.message)
    throw error
  }
}

/** Decode host-returned base64; host drift is a bad response, not a bad request. */
function decodeHostBase64(value: unknown, method: string): Buffer {
  try {
    return decodeBase64Strict(value, Number.MAX_SAFE_INTEGER).bytes
  } catch (error) {
    if (error instanceof ArtifactToolError) badResponse(method, error.message)
    throw error
  }
}

/** Validate the host's `artifact.put` response. */
export function buildPutResult(raw: unknown): ArtifactPutResult {
  const body = asRecord(raw, "artifact.put")
  const { uri } = parseHostUri(body.artifact_uri, "artifact.put")
  if (typeof body.deduplicated !== "boolean") {
    badResponse("artifact.put", "response is missing a boolean 'deduplicated'")
  }
  return { artifact_uri: uri, deduplicated: body.deduplicated }
}

/**
 * Validate the host's `artifact.open` response and apply the inline policy.
 * Above `inlineLimit` the bytes are omitted (metadata only). The full payload
 * still crosses the local pipe because the host response has no ranged mode.
 */
export function buildResolveResult(
  raw: unknown,
  inlineLimit: number = MAX_INLINE_BYTES,
): ArtifactResolveResult {
  const body = asRecord(raw, "artifact.open")
  const { uri, hash } = parseHostUri(body.artifact_uri, "artifact.open")
  if (typeof body.data !== "string") badResponse("artifact.open", "response is missing base64 data")
  const bytes = decodeHostBase64(body.data, "artifact.open")
  if (typeof body.size !== "number" || !Number.isInteger(body.size) || body.size < 0) {
    badResponse("artifact.open", "response is missing a non-negative integer 'size'")
  }
  if (body.size !== bytes.length) {
    badResponse("artifact.open", `size mismatch: declared ${body.size}, decoded ${bytes.length}`)
  }
  const sha256 = createHash("sha256").update(bytes).digest("hex")
  const inline = bytes.length <= inlineLimit
  const result: ArtifactResolveResult = {
    artifact_uri: uri,
    size: bytes.length,
    sha256,
    blake3: hash,
    inline,
  }
  if (inline) result.dataBase64 = body.data
  return result
}

/** Validate the host's `artifact.verify` response. */
export function buildVerifyResult(raw: unknown): ArtifactVerifyResult {
  const body = asRecord(raw, "artifact.verify")
  if (typeof body.verified !== "boolean") {
    badResponse("artifact.verify", "response is missing a boolean 'verified'")
  }
  return { verified: body.verified }
}

/** JSON payload attached to every failed tool result. */
export function artifactErrorPayload(error: unknown): Record<string, unknown> {
  if (error instanceof ArtifactToolError) {
    return {
      ok: false,
      code: error.code,
      error: error.message,
      ...(error.details ? { details: error.details } : {}),
    }
  }
  return {
    ok: false,
    code: ARTIFACT_INTERNAL_ERROR,
    error: String((error as Error)?.message ?? error),
  }
}

/**
 * Dispatch one artifact method over a short-lived NAF/1 connection. The client
 * is always closed, including on init/connect/dispatch failure.
 */
export async function dispatchArtifact(method: string, args: unknown): Promise<unknown> {
  let client: NafClient | null = null
  try {
    try {
      client = new NafClient({
        appId: "mcp-nai-lattice/artifact",
        clientVersion: "0.2.0",
        requestedCapabilities: [method],
        packageIdentity: "@cyberluke/mcp-nai-lattice@0.1.1",
        buildSha: "mcp-nai-lattice",
        producerId: `mcp-nai-lattice-artifact-${process.pid}`,
      })
    } catch (error) {
      throw new ArtifactToolError(
        ARTIFACT_UNAVAILABLE,
        `fabric client init failed: ${String((error as Error)?.message ?? error)}`,
        { method },
      )
    }

    try {
      await client.connect()
    } catch (error) {
      throw new ArtifactToolError(
        ARTIFACT_UNAVAILABLE,
        `fabric connect failed (is the NAI Lattice fabric host running?): ${String((error as Error)?.message ?? error)}`,
        { method },
      )
    }

    let result
    try {
      result = await client.dispatch(method, args)
    } catch (error) {
      throw new ArtifactToolError(
        ARTIFACT_UNAVAILABLE,
        `${method} dispatch failed: ${String((error as Error)?.message ?? error)}`,
        { method },
      )
    }
    if (result.status !== 0) {
      throw new ArtifactToolError(
        ARTIFACT_HOST_REJECTED,
        result.error || `${method} rejected with status ${result.status}`,
        { method, status: result.status },
      )
    }

    let text: string
    try {
      text = new TextDecoder().decode(result.body)
    } catch {
      throw new ArtifactToolError(ARTIFACT_BAD_RESPONSE, `${method} returned an unreadable body`, { method })
    }
    try {
      return JSON.parse(text) as unknown
    } catch {
      throw new ArtifactToolError(ARTIFACT_BAD_RESPONSE, `${method} returned non-JSON`, { method })
    }
  } finally {
    if (client) {
      try {
        await client.close()
      } catch {
        /* best effort: the pipe is short-lived either way */
      }
    }
  }
}

/** Store bytes and return the canonical BLAKE3 artifact URI. */
export async function artifactPut(input: {
  data_base64: string
  media_type?: string
  logical_type?: string
}): Promise<ArtifactPutResult> {
  const { base64 } = decodeBase64Strict(input.data_base64)
  const mediaType = normalizeMediaType(input.media_type)
  const logicalType = normalizeLogicalType(input.logical_type)
  const raw = await dispatchArtifact("artifact.put", {
    data: base64,
    media_type: mediaType,
    logical_type: logicalType,
  })
  return buildPutResult(raw)
}

/** Open an artifact by URI, enforcing the inline output cap. */
export async function artifactResolve(input: { uri: string }): Promise<ArtifactResolveResult> {
  const { uri } = parseArtifactUri(input.uri)
  const raw = await dispatchArtifact("artifact.open", { artifact_uri: uri })
  return buildResolveResult(raw)
}

/** Re-hash a stored artifact against an expected BLAKE3 digest. */
export async function artifactVerify(input: { expected_hash: string }): Promise<ArtifactVerifyResult> {
  const expectedHash = parseExpectedHash(input.expected_hash)
  const raw = await dispatchArtifact("artifact.verify", { expected_hash: expectedHash })
  return buildVerifyResult(raw)
}

const PUT_ANNOTATIONS = {
  readOnlyHint: false,
  destructiveHint: false,
  idempotentHint: true,
  openWorldHint: false,
} as const
const READ_ANNOTATIONS = {
  readOnlyHint: true,
  destructiveHint: false,
  idempotentHint: true,
  openWorldHint: false,
} as const

type ToolReply = {
  content: Array<{ type: "text"; text: string }>
  structuredContent?: Record<string, unknown>
  isError?: boolean
}

async function toolReply(work: () => Promise<unknown>): Promise<ToolReply> {
  try {
    const data = (await work()) as Record<string, unknown>
    return {
      content: [{ type: "text" as const, text: JSON.stringify(data) }],
      structuredContent: data,
    }
  } catch (error) {
    return {
      content: [{ type: "text" as const, text: JSON.stringify(artifactErrorPayload(error)) }],
      isError: true,
    }
  }
}

/**
 * Register the generic artifact tools on an MCP server. Names are product
 * agnostic: any NAI consumer can store, resolve and verify fabric artifacts.
 */
export function registerArtifactTools(server: McpServer): void {
  server.registerTool(
    "nai_lattice_artifact_put",
    {
      title: "Store an artifact in the NAI Lattice fabric CAS",
      description:
        "Store bytes in the NAI Lattice fabric content-addressed store (BLAKE3 CAS) over NAF/1 artifact.put. Input is standard base64 (whitespace/URL-safe alphabet rejected) with a decoded limit of 8 MiB. Returns the canonical artifact://blake3/<hex> URI and the host deduplicated flag. Content addressing makes this idempotent: identical bytes converge on the same URI. Use it for cross-agent evidence, repro artifacts and other durable handoffs; it is not a replacement for a product's own artifact or telemetry store. A missing fabric host returns typed JSON with code NAI_LATTICE_ARTIFACT_UNAVAILABLE.",
      inputSchema: {
        data_base64: z.string().describe("Artifact bytes as standard base64 (max 8 MiB decoded)."),
        media_type: z
          .string()
          .optional()
          .describe("Media type stored with the artifact (default application/octet-stream)."),
        logical_type: z
          .string()
          .optional()
          .describe("Logical type label for discovery, e.g. blob, log, image, transcript (default blob)."),
      },
      outputSchema: {
        artifact_uri: z.string().describe("Canonical artifact://blake3/<64-hex> URI."),
        deduplicated: z.boolean().describe("Host deduplication flag from artifact.put."),
      },
      annotations: PUT_ANNOTATIONS,
    },
    async ({ data_base64, media_type, logical_type }) =>
      toolReply(() => artifactPut({ data_base64, media_type, logical_type })),
  )

  server.registerTool(
    "nai_lattice_artifact_resolve",
    {
      title: "Resolve an artifact from the NAI Lattice fabric CAS",
      description:
        "Open an artifact by its canonical artifact://blake3/<64-hex> URI over NAF/1 artifact.open. Returns artifact_uri, size, sha256 (SHA-256 computed over the bytes), blake3 (the CAS identity from the URI) and dataBase64 when the payload is at most 1 MiB. Above 1 MiB the result is metadata only (inline:false) to keep MCP payloads bounded; the local fabric host still transfers the full object. A missing artifact or unreachable host returns typed JSON (NAI_LATTICE_ARTIFACT_HOST_REJECTED or NAI_LATTICE_ARTIFACT_UNAVAILABLE), never a raw exception.",
      inputSchema: {
        uri: z.string().describe("Canonical artifact://blake3/<64-hex> URI returned by artifact.put."),
      },
      outputSchema: {
        artifact_uri: z.string().describe("Canonical artifact URI."),
        size: z.number().int().nonnegative().describe("Artifact size in bytes."),
        sha256: z.string().describe("SHA-256 hex digest computed over the returned bytes."),
        blake3: z.string().describe("BLAKE3 hex digest that identifies the CAS object."),
        inline: z.boolean().describe("True when dataBase64 is included (payload <= 1 MiB)."),
        dataBase64: z.string().optional().describe("Artifact bytes as base64 when inline is true."),
      },
      annotations: READ_ANNOTATIONS,
    },
    async ({ uri }) => toolReply(() => artifactResolve({ uri })),
  )

  server.registerTool(
    "nai_lattice_artifact_verify",
    {
      title: "Verify an artifact in the NAI Lattice fabric CAS",
      description:
        "Re-hash a stored artifact and compare it with an expected BLAKE3 digest over NAF/1 artifact.verify. Accepts either a raw 64-character BLAKE3 hex digest or the canonical artifact://blake3/<hex> URI. Returns { verified: boolean }; a digest that is absent from the local CAS verifies as false rather than failing. Invalid digests return typed JSON with code NAI_LATTICE_ARTIFACT_INVALID_REQUEST, and an unreachable host returns NAI_LATTICE_ARTIFACT_UNAVAILABLE.",
      inputSchema: {
        expected_hash: z
          .string()
          .describe("Raw 64-character BLAKE3 hex digest or artifact://blake3/<hex> URI."),
      },
      outputSchema: {
        verified: z.boolean().describe("True when the stored bytes hash to expected_hash."),
      },
      annotations: READ_ANNOTATIONS,
    },
    async ({ expected_hash }) => toolReply(() => artifactVerify({ expected_hash })),
  )
}
