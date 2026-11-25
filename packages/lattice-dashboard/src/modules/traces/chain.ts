import type {
  TraceGraph,
  TraceGraphEdge,
  TraceGraphEvent,
  TraceGraphNode,
  TraceSummary,
} from '../../query/traces';
import type { WaterfallDomain } from '../../analytics/waterfall';
import { usableTimestamp } from '../../analytics/time';
import { formatDuration } from '../../ui/format';

/** Accounting shown next to the trace tabs. */
export interface TraceGraphCounts {
  events: number;
  /** Real span nodes only; the trace root and unresolved endpoints are not spans. */
  spans: number;
  /** Explicit caller→callee observations; inferred edges never inflate this. */
  calls: number;
  inferredCalls: number;
}

/**
 * Point events are records that are neither span evidence nor call/edge
 * evidence. A producer that copies the event stream includes the span records
 * in `events`; counting those as events would double-count every span.
 * An event may still carry a `spanId` (an annotation inside a recorded span),
 * so identity is established by evidence sequence, not by the span field alone.
 */
export function graphPointEvents(graph: TraceGraph | null): TraceGraphEvent[] {
  if (graph === null) return [];
  const spanEvidence = new Set<number>();
  for (const node of graph.nodes) {
    if (node.kind !== 'span') continue;
    for (const sequence of node.evidenceSeqs) spanEvidence.add(sequence);
  }
  const edgeEvidence = new Set<number>();
  for (const edge of graph.edges) {
    for (const sequence of edge.evidenceSeqs) edgeEvidence.add(sequence);
  }
  return graph.events.filter(
    (event) =>
      // Contract §3: only a record without a span id is a point event. Span
      // records are counted as spans even when the node cap omitted the node.
      event.spanId === null &&
      (event.seq === null || !spanEvidence.has(event.seq)) &&
      (event.seq === null || !edgeEvidence.has(event.seq)),
  );
}

export interface LegacyPointRow {
  key: string;
  seq: number | null;
  ts: number | null;
  level: string;
  component: string | null;
  message: string;
}

/**
 * Legacy records for the fallback surfaces. A host that answered schema 2 with
 * `coverage: "legacy"` supplies the authoritative record list; a 404 or a
 * missing graph falls back to the legacy trace read model.
 */
export function legacyPointRows(
  trace: TraceSummary | null,
  graph: TraceGraph | null,
): LegacyPointRow[] {
  // The legacy trace read model keeps the record's kind and measured
  // duration_ms; prefer it. The graph's point-event stream is the fallback
  // when the bounded trace lookup returned nothing but the graph did.
  const spans = trace?.spans ?? [];
  if (spans.length > 0) {
    return spans.map((span, index) => ({
      key: `legacy-trace:${span.runtime_sequence ?? index}:${index}`,
      seq: span.runtime_sequence,
      ts: span.ts,
      level: span.kind,
      component: span.component,
      message: span.label,
    }));
  }
  if (graph !== null && graph.coverage === 'legacy' && graph.events.length > 0) {
    return graph.events.map((event) => ({
      key: `legacy-graph:${event.id}`,
      seq: event.seq,
      ts: event.ts,
      level: event.level,
      component: event.component,
      message: event.message,
    }));
  }
  return [];
}

export function legacyEventCount(
  trace: TraceSummary | null,
  graph: TraceGraph | null,
): number {
  return legacyPointRows(trace, graph).length;
}

export function traceGraphCounts(
  graph: TraceGraph | null,
  legacyEventCount = 0,
): TraceGraphCounts {
  if (graph === null) {
    return { events: legacyEventCount, spans: 0, calls: 0, inferredCalls: 0 };
  }
  let spans = 0;
  for (const node of graph.nodes) if (node.kind === 'span') spans += 1;
  let calls = 0;
  let inferredCalls = 0;
  for (const edge of graph.edges) {
    if (edge.kind !== 'call') continue;
    if (edge.inferred) inferredCalls += 1;
    else calls += 1;
  }
  return { events: graphPointEvents(graph).length, spans, calls, inferredCalls };
}

export type ChainTone = 'observed' | 'warn' | 'bad' | 'muted';

const BAD_STATUS = /error|fail|fatal|panic|abort/i;
const WARN_STATUS = /cancel|timeout|deadline|pending|incomplete|partial|degrad|warn|unknown|unresolved/i;
const OK_STATUS = /^ok$|success|complete|closed|done|resolved|healthy/i;

/** Cyan (§2.2 observed), amber (degraded), red (failure), muted (unknown). */
export function nodeTone(node: TraceGraphNode): ChainTone {
  if (node.kind === 'unresolved') return 'muted';
  const status = node.status?.trim() ?? '';
  if (status.length === 0) return 'muted';
  if (BAD_STATUS.test(status)) return 'bad';
  if (WARN_STATUS.test(status)) return 'warn';
  if (OK_STATUS.test(status)) return 'observed';
  return 'muted';
}

/** Status is always written next to the tone; color is never the only signal. */
export function nodeStatusLabel(node: TraceGraphNode): string {
  if (node.status !== null && node.status.trim().length > 0) return node.status;
  if (node.kind === 'unresolved') return 'unresolved';
  if (node.kind === 'trace') return 'trace root';
  return 'status n/a';
}

export function nodeKindLabel(node: TraceGraphNode): string {
  if (node.kind === 'trace') return 'TRACE';
  if (node.kind === 'unresolved') return 'UNRESOLVED';
  return node.spanKind !== null && node.spanKind.trim().length > 0
    ? node.spanKind.toUpperCase()
    : 'SPAN';
}

export type WaterfallTone = 'request' | 'tool' | 'terminal' | 'log' | 'error';

function isErrorStatus(status: string | null): boolean {
  return status !== null && BAD_STATUS.test(status);
}

/** Bar/tick palette token: request cyan, tool amber, terminal green, log muted. */
export function waterfallTone(
  spanKind: string | null,
  operation: string,
  component: string | null,
  status: string | null,
): WaterfallTone {
  if (isErrorStatus(status)) return 'error';
  const kind = (spanKind ?? '').toLowerCase();
  const op = operation.toLowerCase();
  const comp = (component ?? '').toLowerCase();
  // A tool invocation keeps its own tone even when it is a client span.
  if (/tool|shell|command|invoke/.test(op)) return 'tool';
  if (/\b(terminal|process|exec|spawn)\b/.test(op) || /terminal|process/.test(comp)) {
    return 'terminal';
  }
  if (kind === 'server' || kind === 'client' || kind === 'producer' || kind === 'consumer') {
    return 'request';
  }
  // Producers that do not emit span_kind still expose the operation name.
  if (/^(turn|request|dispatch|rpc)\b/.test(op)) return 'request';
  return 'log';
}

export type WaterfallRowOrigin = 'span' | 'event' | 'legacy';

export interface WaterfallRow {
  key: string;
  origin: WaterfallRowOrigin;
  label: string;
  component: string | null;
  startTs: number | null;
  durationMs: number | null;
  /** A span id plus a recorded duration is required before a bar exists. */
  hasBar: boolean;
  isPointEvent: boolean;
  /** True only for a span whose duration was not measured (render N/A). */
  durationUnknown: boolean;
  status: string | null;
  tone: WaterfallTone;
  tickTone: ChainTone;
  seq: number | null;
  spanId: string | null;
  evidenceSeqs: number[];
}

function eventTickTone(level: string): ChainTone {
  const normalized = level.trim().toLowerCase();
  if (normalized === 'error' || normalized === 'fatal') return 'bad';
  if (normalized === 'warn' || normalized === 'warning') return 'warn';
  if (normalized === 'trace' || normalized === 'debug' || normalized === 'unknown') {
    return 'muted';
  }
  return 'observed';
}

function legacyTone(trace: TraceSummary, kind: string): WaterfallTone {
  const normalized = kind.trim().toLowerCase();
  if (normalized === 'terminal') {
    if (trace.state === 'open' || trace.ok === null) return 'log';
    return trace.ok ? 'terminal' : 'error';
  }
  switch (normalized) {
    case 'request':
      return 'request';
    case 'tool':
      return 'tool';
    default:
      return 'log';
  }
}

function legacyTickTone(trace: TraceSummary, kind: string): ChainTone {
  const tone = legacyTone(trace, kind);
  if (tone === 'error') return 'bad';
  if (tone === 'tool') return 'warn';
  if (tone === 'log') return 'muted';
  return 'observed';
}

/**
 * Rows fed to the waterfall.
 *
 * - Schema-2 graph: real span nodes (bars only when span id + duration exist)
 *   and point events (ticks), never a full-length bar for a point event.
 * - Legacy correlation: lifecycle records become point markers only; their
 *   recorded `duration_ms` is shown as a value but is never drawn as a span.
 */
export function buildWaterfallRows(
  trace: TraceSummary,
  graph: TraceGraph | null,
): WaterfallRow[] {
  const rows: WaterfallRow[] = [];
  const usesGraph = graph !== null && graph.coverage !== 'legacy';
  if (usesGraph && graph !== null) {
    const knownStarts: number[] = [];
    for (const node of graph.nodes) {
      if (node.kind === 'span' && usableTimestamp(node.startTs)) knownStarts.push(node.startTs);
    }
    const offsetBaseline =
      knownStarts.length > 0 ? Math.min(...knownStarts) : trace.firstTs;
    for (const node of graph.nodes) {
      if (node.kind !== 'span') continue;
      const startTs =
        node.startTs ??
        (node.startOffsetMs !== null && usableTimestamp(offsetBaseline)
          ? offsetBaseline + node.startOffsetMs
          : null);
      const hasDuration = node.durationMs !== null;
      const hasBar =
        node.spanId !== null && hasDuration && usableTimestamp(startTs);
      rows.push({
        key: `span:${node.id}`,
        origin: 'span',
        label: node.operation || '(operation not recorded)',
        component: node.component,
        startTs,
        durationMs: node.durationMs,
        hasBar,
        isPointEvent: false,
        durationUnknown: !hasDuration,
        status: node.status,
        tone: waterfallTone(node.spanKind, node.operation, node.component, node.status),
        tickTone: nodeTone(node),
        seq: node.evidenceSeqs.length > 0 ? node.evidenceSeqs[0] : null,
        spanId: node.spanId,
        evidenceSeqs: node.evidenceSeqs,
      });
    }
    for (const event of graphPointEvents(graph)) {
      rows.push({
        key: `event:${event.id}`,
        origin: 'event',
        label: event.message || '(event message not recorded)',
        component: event.component,
        startTs: event.ts,
        durationMs: null,
        hasBar: false,
        isPointEvent: true,
        durationUnknown: false,
        status: event.level,
        tone: 'log',
        tickTone: eventTickTone(event.level),
        seq: event.seq,
        spanId: event.spanId,
        evidenceSeqs: event.seq !== null ? [event.seq] : [],
      });
    }
  } else if (trace.spans.length > 0) {
    trace.spans.forEach((span, index) => {
      rows.push({
        key: `legacy:${span.runtime_sequence ?? index}:${index}`,
        origin: 'legacy',
        label: span.label || '(label not recorded)',
        component: span.component,
        startTs: span.ts,
        durationMs: span.durationMs,
        hasBar: false,
        isPointEvent: true,
        durationUnknown: span.durationMs === null,
        status: span.kind,
        tone: legacyTone(trace, span.kind),
        tickTone: legacyTickTone(trace, span.kind),
        seq: span.runtime_sequence,
        spanId: null,
        evidenceSeqs:
          span.runtime_sequence !== null ? [span.runtime_sequence] : [],
      });
    });
  } else if (graph !== null && graph.coverage === 'legacy' && graph.events.length > 0) {
    // The bounded legacy trace lookup found nothing; the graph's point-event
    // stream is still real evidence, so keep its markers and N/A durations.
    for (const event of graph.events) {
      rows.push({
        key: `legacy-graph:${event.id}`,
        origin: 'legacy',
        label: event.message || '(label not recorded)',
        component: event.component,
        startTs: event.ts,
        durationMs: null,
        hasBar: false,
        isPointEvent: true,
        durationUnknown: true,
        status: event.level,
        tone: 'log',
        tickTone: eventTickTone(event.level),
        seq: event.seq,
        spanId: null,
        evidenceSeqs: event.seq !== null ? [event.seq] : [],
      });
    }
  }
  rows.sort((a, b) => {
    const aTs = usableTimestamp(a.startTs) ? a.startTs : Number.POSITIVE_INFINITY;
    const bTs = usableTimestamp(b.startTs) ? b.startTs : Number.POSITIVE_INFINITY;
    if (aTs !== bTs) return aTs - bTs;
    return (a.seq ?? Number.POSITIVE_INFINITY) - (b.seq ?? Number.POSITIVE_INFINITY);
  });
  return rows;
}

/** Union of the trace envelope and the graph observations; never a guessed end. */
export function resolveWaterfallDomain(
  trace: TraceSummary,
  rows: readonly WaterfallRow[],
): WaterfallDomain | null {
  let start = Number.POSITIVE_INFINITY;
  let end = Number.NEGATIVE_INFINITY;
  const consider = (value: number | null): void => {
    if (!usableTimestamp(value)) return;
    if (value < start) start = value;
    if (value > end) end = value;
  };
  consider(trace.firstTs);
  consider(trace.lastTs);
  for (const row of rows) {
    consider(row.startTs);
    if (row.hasBar && row.durationMs !== null && usableTimestamp(row.startTs)) {
      consider(row.startTs + Math.max(0, row.durationMs));
    }
  }
  if (!Number.isFinite(start) || !Number.isFinite(end)) return null;
  const endMs = end > start ? end : start + 1;
  return { startMs: start, endMs, spanMs: endMs - start };
}

export interface WaterfallGeometry {
  leftPx: number;
  widthPx: number;
  minApplied: boolean;
}

/** Fixed logical track geometry; CSS enforces the 1 px minimum. */
export function buildWaterfallGeometry(
  rows: readonly WaterfallRow[],
  domain: WaterfallDomain | null,
  trackWidth: number,
  minWidth = 1,
): Map<string, WaterfallGeometry> {
  const out = new Map<string, WaterfallGeometry>();
  if (domain === null) return out;
  const width = Number.isFinite(trackWidth) && trackWidth > 0 ? trackWidth : 1;
  const floor = Number.isFinite(minWidth) && minWidth > 0 ? Math.min(minWidth, width) : 1;
  for (const row of rows) {
    if (!row.hasBar || !usableTimestamp(row.startTs)) continue;
    const ratio = (row.startTs - domain.startMs) / domain.spanMs;
    let leftPx = ratio * width;
    if (!Number.isFinite(leftPx)) leftPx = 0;
    leftPx = Math.min(width - floor, Math.max(0, leftPx));
    const duration = row.durationMs ?? 0;
    const rawWidth = (Math.max(0, duration) / domain.spanMs) * width;
    const widthPx = Math.min(
      width - leftPx,
      Math.max(floor, Number.isFinite(rawWidth) ? rawWidth : floor),
    );
    out.set(row.key, {
      leftPx,
      widthPx,
      minApplied: duration <= 0 || rawWidth < floor,
    });
  }
  return out;
}

export function offsetText(offsetMs: number | null): string {
  if (offsetMs === null || !Number.isFinite(offsetMs)) return '—';
  return `+${formatDuration(Math.max(0, offsetMs))}`;
}

export function rowDurationText(row: WaterfallRow): string {
  if (row.isPointEvent && row.origin !== 'legacy') return '—';
  if (row.durationMs === null) return 'N/A';
  return formatDuration(row.durationMs);
}

export function rowDurationTitle(row: WaterfallRow): string {
  if (row.isPointEvent) {
    return row.origin === 'legacy'
      ? 'legacy correlation record: duration_ms is a record field, not a span interval'
      : 'point event: no span interval exists';
  }
  if (row.durationMs === null) return 'span duration was not recorded';
  return 'recorded span duration';
}

// ---------------------------------------------------------------------------
// Execution chain layout
// ---------------------------------------------------------------------------

export type ChainLayoutNote = 'calls' | 'parent' | 'none';

export interface ChainNodeView {
  node: TraceGraphNode;
  depth: number;
  incoming: number;
  outgoing: number;
  cycle: boolean;
}

export interface ChainLinkView {
  edge: TraceGraphEdge;
  from: ChainNodeView;
  to: ChainNodeView;
}

export interface ChainLayout {
  mode: 'linear' | 'branched' | 'flat';
  layers: ChainNodeView[][];
  links: ChainLinkView[];
  roots: number;
  inferredHidden: number;
  droppedEdges: number;
  /** Parent edges used only to fill a gap where no call/async link exists. */
  augmentedParents: number;
  cycleCount: number;
  note: ChainLayoutNote;
}

export interface ChainLayoutOptions {
  includeInferred?: boolean;
}

function nodeIndexMap(graph: TraceGraph): Map<string, string> {
  const byId = new Map<string, string>();
  for (const node of graph.nodes) {
    byId.set(node.id, node.id);
    if (node.spanId !== null && !byId.has(node.spanId)) byId.set(node.spanId, node.id);
  }
  return byId;
}

/**
 * Explicit chain: call/async observations, else the explicit parent hierarchy.
 * Inferred edges are hidden unless requested and never enter the default graph.
 */
export function buildChainLayout(
  graph: TraceGraph,
  options: ChainLayoutOptions = {},
): ChainLayout {
  const includeInferred = options.includeInferred === true;
  const resolver = nodeIndexMap(graph);

  const callEdges = graph.edges.filter((edge) => edge.kind === 'call' || edge.kind === 'async');
  const parentEdges = graph.edges.filter((edge) => edge.kind === 'parent');
  const primaryEdges = callEdges.length > 0 ? callEdges : parentEdges;
  const note: ChainLayoutNote =
    callEdges.length > 0 ? 'calls' : parentEdges.length > 0 ? 'parent' : 'none';

  let inferredHidden = 0;
  let droppedEdges = 0;
  let augmentedParents = 0;
  const effective: Array<{ edge: TraceGraphEdge; fromId: string; toId: string }> = [];

  const consider = (edge: TraceGraphEdge): { fromId: string; toId: string } | null => {
    if (edge.inferred && !includeInferred) {
      inferredHidden += 1;
      return null;
    }
    const fromId = resolver.get(edge.from) ?? null;
    const toId = resolver.get(edge.to) ?? null;
    if (fromId === null || toId === null) {
      droppedEdges += 1;
      return null;
    }
    return { fromId, toId };
  };

  for (const edge of primaryEdges) {
    const endpoints = consider(edge);
    if (endpoints === null) continue;
    effective.push({ edge, ...endpoints });
  }

  // Calls are not necessarily instrumented on every hop. Use the explicit
  // parent hierarchy only to attach a node that no call/async link reaches;
  // never duplicate a pair or override a recorded call direction.
  if (callEdges.length > 0) {
    const incoming = new Set(effective.map((entry) => entry.toId));
    const pairs = new Set<string>();
    for (const entry of effective) {
      pairs.add(`${entry.fromId}\u0000${entry.toId}`);
      pairs.add(`${entry.toId}\u0000${entry.fromId}`);
    }
    for (const edge of parentEdges) {
      const endpoints = consider(edge);
      if (endpoints === null) continue;
      if (endpoints.fromId === endpoints.toId) continue;
      if (
        incoming.has(endpoints.toId) ||
        pairs.has(`${endpoints.fromId}\u0000${endpoints.toId}`)
      ) {
        continue;
      }
      effective.push({ edge, ...endpoints });
      incoming.add(endpoints.toId);
      pairs.add(`${endpoints.fromId}\u0000${endpoints.toId}`);
      pairs.add(`${endpoints.toId}\u0000${endpoints.fromId}`);
      augmentedParents += 1;
    }
  }

  const views = new Map<string, ChainNodeView>();
  for (const node of graph.nodes) {
    views.set(node.id, { node, depth: 0, incoming: 0, outgoing: 0, cycle: false });
  }
  for (const entry of effective) {
    const from = views.get(entry.fromId);
    const to = views.get(entry.toId);
    if (from === undefined || to === undefined) continue;
    from.outgoing += 1;
    to.incoming += 1;
  }

  // Longest-path layering with an explicit cycle guard (no recursion).
  const adjacency = new Map<string, string[]>();
  const indegree = new Map<string, number>();
  for (const node of graph.nodes) {
    adjacency.set(node.id, []);
    indegree.set(node.id, 0);
  }
  for (const entry of effective) {
    adjacency.get(entry.fromId)?.push(entry.toId);
    indegree.set(entry.toId, (indegree.get(entry.toId) ?? 0) + 1);
  }
  const depth = new Map<string, number>();
  for (const node of graph.nodes) depth.set(node.id, 0);
  const processed = new Set<string>();
  const queue: string[] = [];
  for (const node of graph.nodes) {
    if ((indegree.get(node.id) ?? 0) === 0) queue.push(node.id);
  }
  let cursor = 0;
  while (cursor < queue.length) {
    const id = queue[cursor];
    cursor += 1;
    if (processed.has(id)) continue;
    processed.add(id);
    const currentDepth = depth.get(id) ?? 0;
    for (const child of adjacency.get(id) ?? []) {
      if (!processed.has(child)) {
        depth.set(child, Math.max(depth.get(child) ?? 0, currentDepth + 1));
      }
      const next = (indegree.get(child) ?? 0) - 1;
      indegree.set(child, next);
      if (next === 0 && !processed.has(child)) queue.push(child);
    }
  }
  // DFS coloring finds true back edges (unlike a depth comparison, which would
  // also flag a legitimate same-depth cross link). Both cycle endpoints are
  // marked once and the traversal never runs away.
  const cycleNodes = new Set<string>();
  for (const node of graph.nodes) {
    if (!processed.has(node.id)) cycleNodes.add(node.id);
  }
  const color = new Map<string, 0 | 1 | 2>();
  for (const node of graph.nodes) color.set(node.id, 0);
  const visit = (id: string): void => {
    color.set(id, 1);
    for (const child of adjacency.get(id) ?? []) {
      const childColor = color.get(child) ?? 0;
      if (childColor === 1) {
        cycleNodes.add(id);
        cycleNodes.add(child);
      } else if (childColor === 0) {
        visit(child);
      }
    }
    color.set(id, 2);
  };
  for (const node of graph.nodes) {
    if ((color.get(node.id) ?? 0) === 0) visit(node.id);
  }
  for (const id of cycleNodes) {
    const view = views.get(id);
    if (view !== undefined) view.cycle = true;
  }
  const cycleCount = cycleNodes.size;

  const layers: ChainNodeView[][] = [];
  for (const node of graph.nodes) {
    const view = views.get(node.id);
    if (view === undefined) continue;
    view.depth = depth.get(node.id) ?? 0;
    while (layers.length <= view.depth) layers.push([]);
    layers[view.depth].push(view);
  }
  const sortNodes = (a: ChainNodeView, b: ChainNodeView): number => {
    const aTs = usableTimestamp(a.node.startTs) ? a.node.startTs : Number.POSITIVE_INFINITY;
    const bTs = usableTimestamp(b.node.startTs) ? b.node.startTs : Number.POSITIVE_INFINITY;
    if (aTs !== bTs) return aTs - bTs;
    const aName = `${a.node.component ?? ''}\u0000${a.node.operation}\u0000${a.node.id}`;
    const bName = `${b.node.component ?? ''}\u0000${b.node.operation}\u0000${b.node.id}`;
    return aName < bName ? -1 : aName > bName ? 1 : 0;
  };
  for (const layer of layers) layer.sort(sortNodes);

  const links: ChainLinkView[] = [];
  for (const entry of effective) {
    const from = views.get(entry.fromId);
    const to = views.get(entry.toId);
    if (from === undefined || to === undefined) continue;
    links.push({ edge: entry.edge, from, to });
  }

  let roots = 0;
  let linear = effective.length > 0 && cycleCount === 0 && graph.nodes.length > 0;
  for (const node of graph.nodes) {
    const view = views.get(node.id);
    if (view === undefined) continue;
    if (view.incoming === 0) roots += 1;
    if (view.incoming > 1 || view.outgoing > 1) linear = false;
  }
  if (roots !== 1) linear = false;
  if (effective.length !== Math.max(0, graph.nodes.length - 1)) linear = false;
  const mode: ChainLayout['mode'] =
    graph.nodes.length === 0 ? 'flat' : effective.length === 0 ? 'flat' : linear ? 'linear' : 'branched';

  return {
    mode,
    layers,
    links,
    roots,
    inferredHidden,
    droppedEdges,
    augmentedParents,
    cycleCount,
    note,
  };
}

/** Layer gap `index` (1-based target depth) receives every link entering it. */
export function linksIntoLayer(layout: ChainLayout, index: number): ChainLinkView[] {
  return layout.links.filter((link) => link.to.depth === index);
}

// ---------------------------------------------------------------------------
// Call tree (parent edges only)
// ---------------------------------------------------------------------------

export type CallTreeRelation = 'root' | 'child' | 'cycle' | 'missing-parent';

export interface CallTreeRow {
  node: TraceGraphNode;
  depth: number;
  relation: CallTreeRelation;
  edge: TraceGraphEdge | null;
  note: string | null;
}

export interface CallTreeResult {
  rows: CallTreeRow[];
  roots: number;
  cyclic: number;
  missingParents: number;
  parentEdges: number;
}

export function buildCallTreeRows(graph: TraceGraph): CallTreeResult {
  const byNode = new Map(graph.nodes.map((node) => [node.id, node]));
  const resolver = nodeIndexMap(graph);
  const parentOf = new Map<string, TraceGraphEdge>();
  let missingParents = 0;
  let parentEdges = 0;
  for (const edge of graph.edges) {
    if (edge.kind !== 'parent') continue;
    parentEdges += 1;
    const fromId = resolver.get(edge.from) ?? null;
    const toId = resolver.get(edge.to) ?? null;
    if (toId === null) continue;
    if (fromId === null || !byNode.has(fromId)) {
      missingParents += 1;
      continue;
    }
    if (!parentOf.has(toId)) parentOf.set(toId, edge);
  }
  const children = new Map<string, string[]>();
  for (const [childId, edge] of parentOf) {
    const parentId = resolver.get(edge.from) ?? null;
    if (parentId === null) continue;
    const list = children.get(parentId) ?? [];
    list.push(childId);
    children.set(parentId, list);
  }
  for (const list of children.values()) {
    list.sort((a, b) => {
      const aNode = byNode.get(a);
      const bNode = byNode.get(b);
      const aTs = aNode !== undefined && usableTimestamp(aNode.startTs)
        ? aNode.startTs
        : Number.POSITIVE_INFINITY;
      const bTs = bNode !== undefined && usableTimestamp(bNode.startTs)
        ? bNode.startTs
        : Number.POSITIVE_INFINITY;
      if (aTs !== bTs) return aTs - bTs;
      return a < b ? -1 : a > b ? 1 : 0;
    });
  }

  const rows: CallTreeRow[] = [];
  const visited = new Set<string>();
  let roots = 0;
  let cyclic = 0;

  const walk = (
    nodeId: string,
    depth: number,
    edge: TraceGraphEdge | null,
    relation: CallTreeRelation,
    stack: Set<string>,
  ): void => {
    const node = byNode.get(nodeId);
    if (node === undefined) return;
    if (stack.has(nodeId)) {
      cyclic += 1;
      return;
    }
    if (visited.has(nodeId)) return;
    visited.add(nodeId);
    rows.push({
      node,
      depth,
      relation,
      edge,
      note:
        relation === 'cycle'
          ? 'parent hierarchy forms a cycle; the repeated node is not expanded twice'
          : null,
    });
    stack.add(nodeId);
    for (const child of children.get(nodeId) ?? []) {
      walk(child, depth + 1, parentOf.get(child) ?? null, 'child', stack);
    }
    stack.delete(nodeId);
  };

  for (const node of graph.nodes) {
    if (parentOf.has(node.id)) continue;
    roots += 1;
    walk(node.id, 0, null, 'root', new Set<string>());
  }
  // Nodes that only appear inside a parent cycle still need one visible row.
  for (const node of graph.nodes) {
    if (visited.has(node.id)) continue;
    walk(node.id, 0, null, 'cycle', new Set<string>());
  }

  return { rows, roots, cyclic, missingParents, parentEdges };
}

/** Response observations are retained but are not separate chain hops. */
export function countEdgeKind(graph: TraceGraph | null, kind: TraceGraphEdge['kind']): number {
  if (graph === null) return 0;
  return graph.edges.filter((edge) => edge.kind === kind).length;
}
