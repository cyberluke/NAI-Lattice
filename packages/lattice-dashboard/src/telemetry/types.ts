/**
 * Wire types for the frozen Lattice dashboard API v1 (docs/DASHBOARD_API.md).
 *
 * These are deliberately tolerant on input: the legacy `/lattice/events`
 * stream wraps deltas in `{ hits: [...] }` while the initial `search` event
 * carries a bare array. Normalisation is centralised here so every consumer
 * sees one shape.
 */

export type LogLevel = 'trace' | 'debug' | 'info' | 'warn' | 'error';

export const LOG_LEVELS: readonly LogLevel[] = [
  'trace',
  'debug',
  'info',
  'warn',
  'error',
];

export type WireLevel = LogLevel | (string & { readonly __wireLevelBrand?: never });

/** One persisted observability record as delivered by SSE or `/lattice/api/query`. */
export interface LogRecord {
  runtime_sequence: number;
  timestamp_utc_ms: number;
  level: WireLevel;
  component: string;
  trace_id: string | null;
  message: string;
  caller?: string | null;
  extras?: unknown;
  /** Producer payload when the record carries one; kept verbatim for the inspector. */
  payload?: unknown;
}

export interface StatusPayload {
  activeSseClients?: number | null;
  maxSseClients?: number | null;
  pushTotal?: number | null;
  subscribers?: number | null;
  brokerScansTotal?: number | null;
  lastScanUnixMs?: number | null;
  droppedTotal?: number | null;
  /** Per-connection cursor from the broker's status tick. */
  cursor?: number | null;
  brokerScans?: number | null;
}

export interface LifecyclePayload {
  state?: string | null;
  detail?: string | null;
  etaMs?: number | null;
  sinceUnixMs?: number | null;
}

export type ConnectionState =
  | 'connecting'
  | 'live'
  | 'retrying'
  | 'error'
  | 'offline';

export interface GapInfo {
  /** First missing runtime_sequence (inclusive). */
  from: number;
  /** Last missing runtime_sequence (inclusive); null when the range is unknown. */
  to: number | null;
  reason: 'dropped' | 'catch-up-incomplete';
  /** Optional count when the producer reported a drop counter delta. */
  count?: number;
}

function asFiniteNumber(value: unknown): number | null {
  if (typeof value === 'number' && Number.isFinite(value)) return value;
  if (typeof value === 'string' && value.trim() !== '') {
    const n = Number(value);
    if (Number.isFinite(n)) return n;
  }
  return null;
}

function asString(value: unknown): string {
  return typeof value === 'string' ? value : '';
}

function asNullableString(value: unknown): string | null {
  return typeof value === 'string' && value.length > 0 ? value : null;
}

/**
 * Normalises one record. Returns null when the record cannot be keyed by a
 * monotonic `runtime_sequence`; such records are counted as malformed by the
 * feed and never rendered.
 */
export function normalizeLogRecord(input: unknown): LogRecord | null {
  if (input === null || typeof input !== 'object' || Array.isArray(input)) {
    return null;
  }
  const raw = input as Record<string, unknown>;
  const seq = asFiniteNumber(raw['runtime_sequence']);
  if (seq === null || seq < 0 || !Number.isInteger(seq)) return null;

  const ts =
    asFiniteNumber(raw['timestamp_utc_ms']) ??
    asFiniteNumber(raw['ts_ms']) ??
    0;

  const level = asString(raw['level']).toLowerCase() || 'info';
  return {
    runtime_sequence: seq,
    timestamp_utc_ms: ts,
    level,
    component: asString(raw['component']) || 'unknown',
    trace_id: asNullableString(raw['trace_id']),
    message: asString(raw['message']),
    caller: asNullableString(raw['caller']),
    extras: raw['extras'],
    payload: raw['payload'],
  };
}

/**
 * Accepts every payload shape the frozen contract and the legacy stream use:
 * a bare array, `{ hits: [...] }`, a single record, or an empty body.
 */
export function normalizeLogPayload(payload: unknown): LogRecord[] {
  const candidates: unknown[] = Array.isArray(payload)
    ? payload
    : payload !== null && typeof payload === 'object'
      ? Array.isArray((payload as { hits?: unknown }).hits)
        ? ((payload as { hits: unknown[] }).hits)
        : [payload]
      : [];

  const out: LogRecord[] = [];
  for (const candidate of candidates) {
    const record = normalizeLogRecord(candidate);
    if (record !== null) out.push(record);
  }
  out.sort((a, b) => a.runtime_sequence - b.runtime_sequence);
  return out;
}
