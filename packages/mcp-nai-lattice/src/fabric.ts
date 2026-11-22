/**
 * fabric.ts — read-only query helper against the NAI Lattice canonical
 * observability ledger (`observability.log.v1`), reused by the mcp-nai-lattice
 * log tools. It opens one ephemeral `@cyberluke/nai-lattice-client` connection to the
 * fabric host (auto-discovered canonical pipe), runs `event.read_range` for the
 * observability namespace, and returns the records as plain objects. Best-effort:
 * a down host degrades to an empty result + error string (the tool reports it),
 * never throws into the MCP stream.
 */
import { NafClient } from "@cyberluke/nai-lattice-client";

const NAMESPACE = "observability.log.v1";
const EVENT_TYPE = "log.record";

/** Filter a record (payload flat shape). */
interface Rec {
  runtime_sequence?: number
  timestamp_utc_ms?: number
  ts_ms?: number
  level?: string
  component?: string
  trace_id?: string
  caller?: string
  message?: string
  extras?: Record<string, unknown>
}

export interface QueryOpts {
  level?: string
  component?: string
  trace?: string
  contains?: string
  limit: number
}

const LEVEL_ORDER = ["trace", "debug", "info", "warn", "error"]

function levelAtLeast(level: string | undefined, min: string | undefined): boolean | undefined {
  if (!min) return undefined
  const a = LEVEL_ORDER.indexOf(level ?? "info")
  const b = LEVEL_ORDER.indexOf(min)
  return a >= b
}

/** Open a short-lived fabric client and fetch the bounded tail of log records. */
export async function fetchLogs(opts: QueryOpts): Promise<{ records: Rec[]; error?: string }> {
  let client: NafClient
  try {
    client = new NafClient({
      appId: "mcp-nai-lattice/log",
      clientVersion: "0.2.0",
      requestedCapabilities: ["event.read_range"],
      packageIdentity: "@cyberluke/nai-lattice-client@0.2.0",
      buildSha: "mcp-nai-lattice",
      producerId: `mcp-nai-lattice-${process.pid}`,
    })
  } catch (e) {
    return { records: [], error: `fabric client init: ${String((e as Error)?.message ?? e)}` }
  }

  try {
    await client.connect()
  } catch (e) {
    return { records: [], error: `fabric connect (is nai-fabric-host running?): ${String((e as Error)?.message ?? e)}` }
  }

  try {
    const res = await client.dispatch("event.read_range", {
      namespace: NAMESPACE,
      event_type: EVENT_TYPE,
      limit: 6000,
    })
    if (res.status !== 0) {
      return { records: [], error: `read_range status=${res.status} ${res.error ?? ""}` }
    }
    const body = JSON.parse(new TextDecoder().decode(res.body)) as { events?: Array<Record<string, unknown>> }
    const events = body?.events ?? []

    // Parse each event payload into a flat record + apply filters + newest-first.
    const records: Rec[] = events
      .map((ev) => {
        const payload = (ev["payload"] as Rec) ?? {}
        const rec: Rec = { ...payload, runtime_sequence: Number(ev["runtime_sequence"] ?? 0) }
        return rec
      })
      .filter((r) => {
        if (opts.level && r.level !== opts.level) return false
        if (levelAtLeast(r.level, opts.level) === false) return false
        if (opts.component && r.component !== opts.component) return false
        if (opts.trace && r.trace_id !== opts.trace) return false
        if (opts.contains) {
          const hay = `${r.level ?? ""} ${r.message ?? ""}`.toLowerCase()
          if (!hay.includes(opts.contains.toLowerCase())) return false
        }
        return true
      })
      .sort((a, b) =>
        (b.timestamp_utc_ms ?? b.ts_ms ?? 0) - (a.timestamp_utc_ms ?? a.ts_ms ?? 0),
      )
      .slice(0, opts.limit)

    return { records }
  } catch (e) {
    return { records: [], error: `read_range: ${String((e as Error)?.message ?? e)}` }
  }
}

/** Aggregate traffic/health stats from a bounded recent scan. */
export async function fetchStats(): Promise<{
  window: number
  levels: Record<string, number>
  errors: number
  components: Record<string, number>
  distinctTraceIds: number
  error?: string
}> {
  const { records, error } = await fetchLogs({ limit: 2000 })
  const levels: Record<string, number> = {}
  const components: Record<string, number> = {}
  const traces = new Set<string>()
  for (const r of records) {
    const lv = r.level ?? "info"
    levels[lv] = (levels[lv] ?? 0) + 1
    const comp = r.component ?? "?"
    components[comp] = (components[comp] ?? 0) + 1
    if (r.trace_id) traces.add(r.trace_id)
  }
  return {
    window: records.length,
    levels,
    errors: (levels["error"] ?? 0) + (levels["warn"] ?? 0),
    components,
    distinctTraceIds: traces.size,
    ...(error ? { error } : {}),
  }
}