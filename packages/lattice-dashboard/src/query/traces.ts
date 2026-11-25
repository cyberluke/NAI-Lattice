export interface TraceSpan {
  ts: number | null;
  kind: string;
  label: string;
  runtime_sequence: number | null;
  durationMs: number | null;
  /** The log record's component, or null when the span has none (§3). */
  component: string | null;
}

export interface TraceSummary {
  traceId: string;
  firstTs: number | null;
  lastTs: number | null;
  durationMs: number | null;
  state: string;
  terminal: string | null;
  /** null when the trace has no terminal span (the host emits `"ok": null`). */
  ok: boolean | null;
  spans: TraceSpan[];
}

/** `GET /lattice/api/traces` (frozen contract §3). */
export interface TraceResponse {
  schema: number;
  window: number | null;
  traces: TraceSummary[];
  generatedAt: number | null;
  /** Echoed when a `?trace=` filter was applied. */
  trace?: string;
  /** True when the `?trace=` scan hit its examined-record bound. */
  truncated?: boolean;
}

/** Back-compat alias for the window response name. */
export type TracesResponse = TraceResponse;

/**
 * `GET /lattice/api/v2/traces/{id}/graph` (schema 2).
 *
 * Field names are normalized from the host contract, which may use snake_case
 * (audit §2.3) or camelCase. Nothing is inferred here: a missing node kind is a
 * span only when the record carries a span id, and an inaccessible endpoint is
 * reported as unavailable rather than synthesized.
 */
export type TraceGraphCoverage = 'explicit' | 'legacy' | 'partial' | 'unknown';
export type TraceGraphNodeKind = 'trace' | 'span' | 'unresolved';
export type TraceGraphEdgeKind = 'parent' | 'call' | 'async' | 'response' | 'unknown';

export interface TraceGraphNode {
  id: string;
  kind: TraceGraphNodeKind;
  traceId: string | null;
  spanId: string | null;
  parentSpanId: string | null;
  component: string | null;
  instanceId: string | null;
  operation: string;
  spanKind: string | null;
  status: string | null;
  startTs: number | null;
  /** Producer offset from the trace's first observed span; absolute ts preferred. */
  startOffsetMs: number | null;
  durationMs: number | null;
  /** Host-provided explanation for unresolved endpoints and missing links. */
  reason: string | null;
  evidenceSeqs: number[];
}

export interface TraceGraphEdge {
  id: string;
  kind: TraceGraphEdgeKind;
  from: string;
  to: string;
  operation: string | null;
  transport: string | null;
  latencyMs: number | null;
  inferred: boolean;
  evidenceSeqs: number[];
}

export interface TraceGraphEvent {
  id: string;
  seq: number | null;
  ts: number | null;
  level: string;
  component: string | null;
  message: string;
  spanId: string | null;
}

export interface TraceGraphDiagnostic {
  code: string;
  severity: string;
  message: string;
}

export interface TraceGraph {
  schema: number;
  traceId: string | null;
  coverage: TraceGraphCoverage;
  coverageDetail: string | null;
  generatedAt: number | null;
  nodes: TraceGraphNode[];
  edges: TraceGraphEdge[];
  events: TraceGraphEvent[];
  diagnostics: TraceGraphDiagnostic[];
}

/**
 * A missing graph endpoint is a truthful legacy state, not an error: the host
 * may predate schema 2 or hold only request-correlated records.
 */
export type TraceGraphResult =
  | { status: 'ok'; graph: TraceGraph }
  | { status: 'unavailable'; httpStatus: number; reason: string };

export interface TracesClientOptions {
  url?: string;
  /** Base for `…/{trace_id}/graph`; defaults to the versioned host route. */
  graphUrl?: string;
  fetchImpl?: typeof fetch;
}

function numberOrNull(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function stringOrNull(value: unknown): string | null {
  return typeof value === 'string' ? value : null;
}

function normalizeSpan(value: unknown): TraceSpan | null {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return null;
  const raw = value as Record<string, unknown>;
  return {
    ts: numberOrNull(raw['ts']),
    kind: stringOrNull(raw['kind']) ?? 'log',
    label: stringOrNull(raw['label']) ?? '',
    runtime_sequence: numberOrNull(raw['runtime_sequence']),
    durationMs: numberOrNull(raw['durationMs']),
    component: stringOrNull(raw['component']),
  };
}

function normalizeTrace(value: unknown): TraceSummary | null {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return null;
  const raw = value as Record<string, unknown>;
  const traceId = stringOrNull(raw['traceId']);
  if (traceId === null || traceId.length === 0) return null;
  const spansRaw = Array.isArray(raw['spans']) ? raw['spans'] : [];
  const spans: TraceSpan[] = [];
  for (const span of spansRaw) {
    const normalized = normalizeSpan(span);
    if (normalized !== null) spans.push(normalized);
  }
  const ok = raw['ok'];
  return {
    traceId,
    firstTs: numberOrNull(raw['firstTs']),
    lastTs: numberOrNull(raw['lastTs']),
    durationMs: numberOrNull(raw['durationMs']),
    state: stringOrNull(raw['state']) ?? 'unknown',
    terminal: stringOrNull(raw['terminal']),
    ok: typeof ok === 'boolean' ? ok : null,
    spans,
  };
}

function asRecord(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function firstPresent(record: Record<string, unknown>, keys: readonly string[]): unknown {
  for (const key of keys) {
    if (key in record && record[key] !== undefined && record[key] !== null) return record[key];
  }
  return null;
}

function graphString(record: Record<string, unknown>, keys: readonly string[]): string | null {
  const value = firstPresent(record, keys);
  return typeof value === 'string' && value.trim().length > 0 ? value : null;
}

function graphNumber(record: Record<string, unknown>, keys: readonly string[]): number | null {
  const value = firstPresent(record, keys);
  if (typeof value === 'number' && Number.isFinite(value)) return value;
  if (typeof value === 'string' && value.trim().length > 0) {
    const parsed = Number(value);
    if (Number.isFinite(parsed)) return parsed;
  }
  return null;
}

function graphTimestamp(record: Record<string, unknown>, keys: readonly string[]): number | null {
  const value = firstPresent(record, keys);
  if (typeof value === 'number' && Number.isFinite(value) && value > 0) return value;
  if (typeof value === 'string' && value.trim().length > 0) {
    const numeric = Number(value);
    if (Number.isFinite(numeric) && numeric > 0) return numeric;
    const parsed = Date.parse(value);
    if (Number.isFinite(parsed) && parsed > 0) return parsed;
  }
  return null;
}

function graphBoolean(record: Record<string, unknown>, keys: readonly string[]): boolean | null {
  const value = firstPresent(record, keys);
  return typeof value === 'boolean' ? value : null;
}

/** Evidence sequences from either numeric refs or `{seq}`/`{runtime_sequence}` objects. */
function graphEvidence(record: Record<string, unknown>): number[] {
  const value = firstPresent(record, [
    'evidenceSequences',
    'evidence_sequences',
    'evidenceSeqs',
    'evidence_seqs',
    'evidence',
    'evidenceRefs',
    'evidence_refs',
  ]);
  if (!Array.isArray(value)) return [];
  const out: number[] = [];
  const seen = new Set<number>();
  for (const entry of value) {
    let sequence: number | null = null;
    if (typeof entry === 'number' && Number.isFinite(entry)) sequence = entry;
    else if (typeof entry === 'string' && entry.trim().length > 0) {
      const parsed = Number(entry);
      if (Number.isFinite(parsed)) sequence = parsed;
    } else {
      const nested = asRecord(entry);
      if (nested !== null) {
        sequence = graphNumber(nested, ['runtime_sequence', 'runtimeSequence', 'seq', 'sequence']);
      }
    }
    if (sequence !== null && !seen.has(sequence)) {
      seen.add(sequence);
      out.push(sequence);
    }
  }
  return out;
}

function normalizeCoverage(value: unknown): {
  coverage: TraceGraphCoverage;
  detail: string | null;
} {
  const record = asRecord(value);
  const raw = typeof value === 'string' ? value : record === null ? null : graphString(record, [
    'level',
    'mode',
    'coverage',
    'kind',
    'value',
  ]);
  const detail =
    record === null ? null : graphString(record, ['detail', 'reason', 'note', 'message']);
  const normalized = (raw ?? '').toLowerCase();
  switch (normalized) {
    case 'explicit':
    case 'full':
    case 'complete':
    // The host names a fully spanned trace `spans` (schema-2 contract).
    case 'spans':
      return { coverage: 'explicit', detail };
    case 'legacy':
    case 'legacy_correlation':
    case 'legacy-correlation':
      return { coverage: 'legacy', detail };
    case 'partial':
    case 'sampled':
    // A mix of span-bearing and point-only records is still partial evidence.
    case 'mixed':
      return { coverage: 'partial', detail };
    default:
      return { coverage: 'unknown', detail };
  }
}

function normalizeNodeKind(record: Record<string, unknown>): TraceGraphNodeKind {
  const raw = (
    graphString(record, ['kind', 'node_kind', 'nodeKind', 'entity_kind', 'entityKind']) ?? ''
  ).toLowerCase();
  if (raw === 'trace' || raw === 'root' || raw === 'trace_root') return 'trace';
  if (raw === 'unresolved' || raw === 'unknown' || raw === 'external') return 'unresolved';
  return 'span';
}

function normalizeGraphNode(value: unknown): TraceGraphNode | null {
  const record = asRecord(value);
  if (record === null) return null;
  const kind = normalizeNodeKind(record);
  const rawId = graphString(record, ['id', 'nodeId', 'node_id']);
  const explicitSpanId = graphString(record, ['spanId', 'span_id']);
  const id =
    rawId ??
    (kind === 'trace'
      ? graphString(record, ['traceId', 'trace_id'])
      : explicitSpanId);
  if (id === null || id.length === 0) return null;
  // Host nodes identify spans as `span:<id>` without a separate spanId field.
  const derivedSpanId = id.startsWith('span:') ? id.slice('span:'.length) : '';
  const spanId =
    explicitSpanId ?? (kind === 'span' && derivedSpanId.length > 0 ? derivedSpanId : null);
  return {
    id,
    kind,
    traceId: graphString(record, ['traceId', 'trace_id']),
    spanId,
    parentSpanId: graphString(record, ['parentSpanId', 'parent_span_id']),
    component: graphString(record, ['component', 'componentId', 'component_id']),
    instanceId: graphString(record, ['instanceId', 'instance_id']),
    operation:
      graphString(record, ['operation', 'name', 'label', 'event_name']) ?? '',
    spanKind: graphString(record, ['spanKind', 'span_kind', 'kind_detail']),
    status: graphString(record, ['status', 'state', 'outcome']),
    startTs: graphTimestamp(record, [
      'startTs',
      'start_ts',
      'startWallTime',
      'start_wall_time',
      'ts',
      'timestamp',
    ]),
    startOffsetMs: graphNumber(record, ['startOffsetMs', 'start_offset_ms']),
    durationMs: graphNumber(record, ['durationMs', 'duration_ms']),
    reason: graphString(record, ['reason', 'detail', 'unresolved_reason']),
    evidenceSeqs: graphEvidence(record),
  };
}

function normalizeEdgeKind(record: Record<string, unknown>): TraceGraphEdgeKind {
  const raw = (
    graphString(record, ['kind', 'edge_kind', 'edgeKind']) ?? ''
  ).toLowerCase();
  switch (raw) {
    case 'parent':
      return 'parent';
    case 'call':
    case 'invoke':
    case 'invocation':
      return 'call';
    case 'async':
      return 'async';
    case 'response':
      return 'response';
    default:
      break;
  }
  const observation = (
    graphString(record, ['observationKind', 'observation_kind']) ?? ''
  ).toLowerCase();
  if (observation === 'call' || observation === 'client' || observation === 'server') {
    return 'call';
  }
  return 'unknown';
}

function normalizeGraphEdge(value: unknown, index: number): TraceGraphEdge | null {
  const record = asRecord(value);
  if (record === null) return null;
  const from =
    graphString(record, [
      'from',
      'fromId',
      'from_id',
      'source',
      'sourceId',
      'source_id',
      'sourceNodeId',
      'source_node_id',
      'sourceSpanId',
      'source_span_id',
    ]) ?? null;
  const to =
    graphString(record, [
      'to',
      'toId',
      'to_id',
      'target',
      'targetId',
      'target_id',
      'targetNodeId',
      'target_node_id',
      'targetSpanId',
      'target_span_id',
    ]) ?? null;
  if (from === null || to === null) return null;
  const kind = normalizeEdgeKind(record);
  const explicitId = graphString(record, ['id', 'edgeId', 'edge_id']);
  return {
    id:
      explicitId ??
      `${kind}:${from}->${to}:${index}`,
    kind,
    from,
    to,
    operation: graphString(record, ['operation', 'name', 'label']),
    transport: graphString(record, ['transport', 'protocol', 'channel']),
    latencyMs: graphNumber(record, ['latencyMs', 'latency_ms', 'durationMs', 'duration_ms']),
    inferred: graphBoolean(record, ['inferred']) === true,
    evidenceSeqs: graphEvidence(record),
  };
}

function normalizeGraphEvent(value: unknown, index: number): TraceGraphEvent | null {
  const record = asRecord(value);
  if (record === null) return null;
  const seq = graphNumber(record, ['seq', 'runtime_sequence', 'runtimeSequence', 'sequence']);
  const id =
    graphString(record, ['id', 'stable_record_id', 'stableRecordId']) ??
    (seq !== null ? `event-${seq}` : `event-${index}`);
  return {
    id,
    seq,
    ts: graphTimestamp(record, ['ts', 'timestamp', 'ts_ms', 'tsMs', 'timestamp_utc_ms']),
    level: graphString(record, ['level', 'severity']) ?? 'unknown',
    component: graphString(record, ['component']),
    message: graphString(record, ['message', 'event_name', 'eventName', 'name', 'label']) ?? '',
    spanId: graphString(record, ['spanId', 'span_id']),
  };
}

function normalizeDiagnostic(value: unknown, index: number): TraceGraphDiagnostic | null {
  if (typeof value === 'string' && value.trim().length > 0) {
    return { code: `diagnostic-${index}`, severity: 'info', message: value };
  }
  const record = asRecord(value);
  if (record === null) return null;
  const message = graphString(record, ['message', 'detail', 'reason']);
  if (message === null) return null;
  return {
    code: graphString(record, ['code', 'kind']) ?? `diagnostic-${index}`,
    severity: graphString(record, ['severity', 'level']) ?? 'info',
    message,
  };
}

/**
 * Normalizes a schema-2 graph payload. Throws on an unsupported schema instead
 * of guessing a graph from an unknown shape.
 */
export function normalizeTraceGraph(payload: unknown, fallbackTraceId: string | null = null): TraceGraph {
  const raw = asRecord(payload);
  if (raw === null) throw new Error('trace graph returned an unexpected payload');
  const schema = graphNumber(raw, ['schema']);
  if (schema !== 2) {
    throw new Error(`trace graph returned unsupported schema ${String(raw['schema'] ?? 'missing')}`);
  }
  const { coverage, detail } = normalizeCoverage(firstPresent(raw, ['coverage']));
  const nodes: TraceGraphNode[] = [];
  const nodeById = new Map<string, TraceGraphNode>();
  for (const value of Array.isArray(raw['nodes']) ? raw['nodes'] : []) {
    const node = normalizeGraphNode(value);
    if (node === null) continue;
    const existing = nodeById.get(node.id);
    if (existing !== undefined) {
      // A retry/queue may duplicate a span record: collapse the node but keep
      // every evidence sequence (contract §3).
      for (const sequence of node.evidenceSeqs) {
        if (!existing.evidenceSeqs.includes(sequence)) existing.evidenceSeqs.push(sequence);
      }
      continue;
    }
    nodeById.set(node.id, node);
    nodes.push(node);
  }
  const edges: TraceGraphEdge[] = [];
  const edgeIds = new Set<string>();
  let edgeIndex = 0;
  for (const value of Array.isArray(raw['edges']) ? raw['edges'] : []) {
    const edge = normalizeGraphEdge(value, edgeIndex);
    edgeIndex += 1;
    if (edge === null || edgeIds.has(edge.id)) continue;
    edgeIds.add(edge.id);
    edges.push(edge);
  }
  const events: TraceGraphEvent[] = [];
  let eventIndex = 0;
  for (const value of Array.isArray(raw['events']) ? raw['events'] : []) {
    const event = normalizeGraphEvent(value, eventIndex);
    eventIndex += 1;
    if (event !== null) events.push(event);
  }
  events.sort((a, b) => {
    const aTs = a.ts ?? Number.POSITIVE_INFINITY;
    const bTs = b.ts ?? Number.POSITIVE_INFINITY;
    if (aTs !== bTs) return aTs - bTs;
    return (a.seq ?? Number.POSITIVE_INFINITY) - (b.seq ?? Number.POSITIVE_INFINITY);
  });

  // The host emits span nodes with `startOffsetMs` only. The same records also
  // appear as events carrying absolute `ts` and the span id, so recover the
  // absolute start from that evidence before falling back to the span baseline.
  const eventBySeq = new Map<number, TraceGraphEvent>();
  const earliestBySpan = new Map<string, number>();
  for (const event of events) {
    if (event.seq !== null) eventBySeq.set(event.seq, event);
    if (event.spanId !== null && event.ts !== null) {
      const current = earliestBySpan.get(event.spanId);
      if (current === undefined || event.ts < current) earliestBySpan.set(event.spanId, event.ts);
    }
  }
  let spanBaseline: number | null = null;
  for (const node of nodes) {
    if (node.kind !== 'span') continue;
    if (node.startTs === null) {
      let recovered: number | null = null;
      for (const sequence of node.evidenceSeqs) {
        const event = eventBySeq.get(sequence);
        if (event !== undefined && event.ts !== null) {
          recovered = event.ts;
          break;
        }
      }
      if (recovered === null && node.spanId !== null) {
        recovered = earliestBySpan.get(node.spanId) ?? null;
      }
      node.startTs = recovered;
    }
    if (node.startTs !== null) {
      spanBaseline =
        spanBaseline === null ? node.startTs : Math.min(spanBaseline, node.startTs);
    }
  }
  if (spanBaseline !== null) {
    for (const node of nodes) {
      if (node.kind === 'span' && node.startTs === null && node.startOffsetMs !== null) {
        node.startTs = spanBaseline + node.startOffsetMs;
      }
    }
  }

  const diagnostics: TraceGraphDiagnostic[] = [];
  let diagnosticIndex = 0;
  for (const value of Array.isArray(raw['diagnostics']) ? raw['diagnostics'] : []) {
    const diagnostic = normalizeDiagnostic(value, diagnosticIndex);
    diagnosticIndex += 1;
    if (diagnostic !== null) diagnostics.push(diagnostic);
  }
  // Attach the host's own unresolved explanation to the muted endpoint node.
  for (const node of nodes) {
    if (node.kind !== 'unresolved' || node.reason !== null) continue;
    const missing = node.id.startsWith('unresolved:')
      ? node.id.slice('unresolved:'.length)
      : node.operation;
    const diagnostic = diagnostics.find(
      (candidate) => missing.length > 0 && candidate.message.includes(missing),
    );
    if (diagnostic !== undefined) node.reason = diagnostic.message;
  }
  return {
    schema,
    traceId: graphString(raw, ['traceId', 'trace_id']) ?? fallbackTraceId,
    coverage,
    coverageDetail: detail,
    generatedAt: graphNumber(raw, ['generatedAt', 'generated_at']),
    nodes,
    edges,
    events,
    diagnostics,
  };
}

/** Client for `GET /lattice/api/traces` (frozen contract §3). */
export class TracesClient {
  private readonly url: string;
  private readonly graphUrl: string;
  private readonly fetchImpl: typeof fetch | null;

  constructor(options: TracesClientOptions = {}) {
    this.url = options.url ?? '/lattice/api/traces';
    this.graphUrl = options.graphUrl ?? '/lattice/api/v2/traces';
    this.fetchImpl =
      options.fetchImpl ??
      // `fetch` must keep its Window receiver. Extracting it unbound throws
      // "Illegal invocation" in a real browser (jsdom does not catch this).
      (typeof globalThis.fetch === 'function' ? globalThis.fetch.bind(globalThis) : null);
  }

  /** Bounded recent-window grouping. */
  async fetch(signal?: AbortSignal): Promise<TraceResponse> {
    return this.request(this.url, signal);
  }

  /**
   * Resolves one trace id even when it is older than the recent window
   * (contract §3). The host bounds the scan and reports `truncated`.
   */
  async fetchTrace(traceId: string, signal?: AbortSignal): Promise<TraceResponse> {
    const id = traceId.trim();
    if (id.length === 0) throw new Error('trace id is empty');
    return this.request(`${this.url}?trace=${encodeURIComponent(id)}`, signal);
  }

  /**
   * Reads the schema-2 execution graph. HTTP 404 is a truthful "this host has
   * no causal graph for that trace" result, never a reason to synthesize one.
   */
  async fetchGraph(traceId: string, signal?: AbortSignal): Promise<TraceGraphResult> {
    const id = traceId.trim();
    if (id.length === 0) throw new Error('trace id is empty');
    if (this.fetchImpl === null) throw new Error('fetch is unavailable in this runtime');
    const url = `${this.graphUrl}/${encodeURIComponent(id)}/graph`;
    const response = await this.fetchImpl(url, {
      method: 'GET',
      headers: { Accept: 'application/json' },
      cache: 'no-store',
      credentials: 'same-origin',
      signal,
    });
    if (response.status === 404) {
      return {
        status: 'unavailable',
        httpStatus: 404,
        reason: `This host returned HTTP 404 for ${url}. Only legacy request correlation is available for this trace.`,
      };
    }
    if (!response.ok) throw new Error(`trace graph failed: HTTP ${response.status}`);
    const payload: unknown = await response.json();
    return { status: 'ok', graph: normalizeTraceGraph(payload, id) };
  }

  private async request(url: string, signal?: AbortSignal): Promise<TraceResponse> {
    if (this.fetchImpl === null) throw new Error('fetch is unavailable in this runtime');
    const response = await this.fetchImpl(url, {
      method: 'GET',
      headers: { Accept: 'application/json' },
      cache: 'no-store',
      credentials: 'same-origin',
      signal,
    });
    if (!response.ok) throw new Error(`traces failed: HTTP ${response.status}`);
    const payload: unknown = await response.json();
    if (payload === null || typeof payload !== 'object' || Array.isArray(payload)) {
      throw new Error('traces returned an unexpected payload');
    }
    const raw = payload as Record<string, unknown>;
    const tracesRaw = Array.isArray(raw['traces']) ? raw['traces'] : [];
    const traces: TraceSummary[] = [];
    for (const trace of tracesRaw) {
      const normalized = normalizeTrace(trace);
      if (normalized !== null) traces.push(normalized);
    }
    traces.sort((a, b) => (b.firstTs ?? 0) - (a.firstTs ?? 0));
    const echo = stringOrNull(raw['trace']);
    const truncated = raw['truncated'];
    return {
      schema: numberOrNull(raw['schema']) ?? 1,
      window: numberOrNull(raw['window']),
      traces,
      generatedAt: numberOrNull(raw['generatedAt']),
      ...(echo !== null ? { trace: echo } : {}),
      ...(typeof truncated === 'boolean' ? { truncated } : {}),
    };
  }
}
