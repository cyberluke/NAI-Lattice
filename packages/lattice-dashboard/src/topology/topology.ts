import { shallowRef, type ShallowRef } from 'vue';
import type { LatticeChartOption } from '../charts/echarts';
import { CHART_COLORS, CHART_TEXT_STYLE } from '../charts/palette';
import { isAbortError } from '../query/client';

/**
 * `GET /lattice/api/topology?window=4000` (frozen contract §9).
 *
 * Nodes and edges are derived by the host from real component-bearing trace
 * spans. The UI never adds a node of its own: an empty window renders an empty
 * graph, and an unknown count stays `null` rather than becoming a fake zero.
 */
export interface TopologyNode {
  id: string;
  label: string;
  count: number | null;
  errors: number | null;
}

export interface TopologyEdge {
  source: string;
  target: string;
  count: number | null;
}

export interface TopologySnapshot {
  schema: number;
  generatedAt: number | null;
  window: number | null;
  truncated: boolean;
  nodes: TopologyNode[];
  edges: TopologyEdge[];
}

export const TOPOLOGY_DEFAULT_WINDOW = 4000;
export const TOPOLOGY_INTERVAL_MS = 5000;

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function numberOrNull(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function countOrNull(value: unknown): number | null {
  const number = numberOrNull(value);
  return number !== null && number >= 0 ? number : null;
}

function nonEmptyStringOrNull(value: unknown): string | null {
  return typeof value === 'string' && value.length > 0 ? value : null;
}

function parseNode(value: unknown): TopologyNode | null {
  if (!isRecord(value)) return null;
  const id = nonEmptyStringOrNull(value['id']);
  if (id === null) return null;
  return {
    id,
    // The id is real host data; using it as a display label is presentation,
    // not an invented component.
    label: nonEmptyStringOrNull(value['label']) ?? id,
    count: countOrNull(value['count']),
    errors: countOrNull(value['errors']),
  };
}

/**
 * Edges whose endpoints are absent from the real node set are dropped: a graph
 * renderer would otherwise synthesize phantom endpoints.
 */
function parseEdges(value: unknown, nodeIds: ReadonlySet<string>): TopologyEdge[] {
  if (!Array.isArray(value)) return [];
  const edges: TopologyEdge[] = [];
  const seen = new Set<string>();
  for (const raw of value) {
    if (!isRecord(raw)) continue;
    const source = nonEmptyStringOrNull(raw['source']);
    const target = nonEmptyStringOrNull(raw['target']);
    if (source === null || target === null) continue;
    if (!nodeIds.has(source) || !nodeIds.has(target)) continue;
    const key = `${source}\u0000${target}`;
    if (seen.has(key)) continue;
    seen.add(key);
    edges.push({ source, target, count: countOrNull(raw['count']) });
  }
  return edges;
}

export function parseTopology(payload: unknown): TopologySnapshot {
  if (!isRecord(payload)) throw new Error('topology returned an unexpected payload');
  if (payload['schema'] !== 1) {
    throw new Error(`unsupported topology schema: ${String(payload['schema'])}`);
  }
  const nodes: TopologyNode[] = [];
  const nodeIds = new Set<string>();
  const rawNodes = Array.isArray(payload['nodes']) ? payload['nodes'] : [];
  for (const raw of rawNodes) {
    const node = parseNode(raw);
    if (node === null || nodeIds.has(node.id)) continue;
    nodeIds.add(node.id);
    nodes.push(node);
  }
  return {
    schema: 1,
    generatedAt: numberOrNull(payload['generatedAt']),
    window: numberOrNull(payload['window']),
    truncated: payload['truncated'] === true,
    nodes,
    edges: parseEdges(payload['edges'], nodeIds),
  };
}

function maxCount(values: readonly (number | null)[]): number {
  let max = 0;
  for (const value of values) if (value !== null) max = Math.max(max, value);
  return max;
}

function scaledSize(count: number | null, max: number, base: number, span: number): number {
  if (count === null || max <= 0) return base;
  return base + span * Math.sqrt(Math.max(0, Math.min(1, count / max)));
}

interface GraphNodeData {
  id: string;
  name: string;
  value: number | null;
  count: number | null;
  errors: number | null;
  symbolSize: number;
  itemStyle: { color: string; borderColor: string; borderWidth: number };
  label: { show: boolean };
}

interface GraphLinkData {
  source: string;
  target: string;
  value: number | null;
  count: number | null;
  lineStyle: { width: number };
}

const MAX_LABELLED_NODES = 24;

/**
 * Deterministic circular layout constants.
 *
 * A force layout re-simulates on every poll: nodes jump, and the simulation can
 * push them outside the panel. Circular placement with an id-ordered data array
 * is stable across refreshes, and the initial zoom leaves room for the largest
 * symbol inside the panel's 200 px minimum surface.
 */
export const TOPOLOGY_LAYOUT = 'circular' as const;
export const TOPOLOGY_ZOOM = 0.78;
export const TOPOLOGY_SYMBOL_BASE = 12;
export const TOPOLOGY_SYMBOL_SPAN = 22;
export const TOPOLOGY_MAX_SYMBOL_SIZE = TOPOLOGY_SYMBOL_BASE + TOPOLOGY_SYMBOL_SPAN;
export const TOPOLOGY_MIN_SURFACE_PX = 160;

function compareNodes(a: TopologyNode, b: TopologyNode): number {
  return a.id.localeCompare(b.id);
}

function compareEdges(a: TopologyEdge, b: TopologyEdge): number {
  const bySource = a.source.localeCompare(b.source);
  return bySource !== 0 ? bySource : a.target.localeCompare(b.target);
}

export function buildTopologyOption(snapshot: TopologySnapshot): LatticeChartOption {
  const nodeMax = maxCount(snapshot.nodes.map((node) => node.count));
  const edgeMax = maxCount(snapshot.edges.map((edge) => edge.count));
  // Host order is encounter order and counts move every poll. The id is the
  // only stable identity, so it alone determines ring order and label slots.
  const ordered = [...snapshot.nodes].sort(compareNodes);
  const data: GraphNodeData[] = ordered.map((node, index) => {
    const errored = node.errors !== null && node.errors > 0;
    return {
      id: node.id,
      name: node.label,
      value: node.count,
      count: node.count,
      errors: node.errors,
      symbolSize: scaledSize(
        node.count,
        nodeMax,
        TOPOLOGY_SYMBOL_BASE,
        TOPOLOGY_SYMBOL_SPAN,
      ),
      itemStyle: {
        color: errored ? CHART_COLORS.bad : CHART_COLORS.accent,
        borderColor: errored ? 'rgba(255,107,107,0.55)' : 'rgba(25,194,216,0.45)',
        borderWidth: 1,
      },
      label: { show: index < MAX_LABELLED_NODES },
    };
  });
  const links: GraphLinkData[] = [...snapshot.edges].sort(compareEdges).map((edge) => ({
    source: edge.source,
    target: edge.target,
    value: edge.count,
    count: edge.count,
    lineStyle: { width: scaledSize(edge.count, edgeMax, 0.8, 4) },
  }));

  return {
    animation: false,
    animationDuration: 0,
    animationDurationUpdate: 0,
    backgroundColor: 'transparent',
    textStyle: CHART_TEXT_STYLE,
    tooltip: {
      trigger: 'item',
      backgroundColor: '#11151c',
      borderColor: CHART_COLORS.grid,
      textStyle: { color: CHART_COLORS.fg, fontSize: 10 },
      formatter: formatTopologyTooltip,
    },
    series: [
      {
        type: 'graph',
        layout: TOPOLOGY_LAYOUT,
        roam: true,
        zoom: TOPOLOGY_ZOOM,
        center: ['50%', '50%'],
        draggable: false,
        symbol: 'circle',
        circular: { rotateLabel: true },
        labelLayout: { hideOverlap: true },
        label: {
          position: 'right',
          color: CHART_COLORS.muted,
          fontSize: 9,
          overflow: 'truncate',
          width: 120,
        },
        lineStyle: { color: 'rgba(139,152,165,0.42)', curveness: 0.08 },
        emphasis: { focus: 'adjacency', lineStyle: { width: 3 } },
        data,
        links,
      },
    ],
  };
}

function countLabel(count: number | null): string {
  return count === null ? 'count not reported' : `${count} transition${count === 1 ? '' : 's'}`;
}

export function formatTopologyTooltip(params: unknown): string {
  if (Array.isArray(params)) return params.map(formatTopologyTooltip).filter(Boolean).join('\n');
  if (!isRecord(params)) return '';
  const data = isRecord(params['data']) ? params['data'] : null;
  if (data === null) return '';
  if (params['dataType'] === 'edge' || ('source' in data && 'target' in data)) {
    const source = typeof data['source'] === 'string' ? data['source'] : '?';
    const target = typeof data['target'] === 'string' ? data['target'] : '?';
    return `${source} → ${target}\n${countLabel(numberOrNull(data['count']))}`;
  }
  const name = typeof data['name'] === 'string' ? data['name'] : 'component';
  const count = numberOrNull(data['count']);
  const errors = numberOrNull(data['errors']);
  const countText = count === null ? 'count not reported' : `${count} span${count === 1 ? '' : 's'}`;
  const errorText = errors === null ? 'errors not reported' : `${errors} error${errors === 1 ? '' : 's'}`;
  return `${name}\n${countText} · ${errorText}`;
}

export type TopologyPick =
  | { kind: 'node'; component: string; label: string }
  | { kind: 'edge'; source: string; target: string; count: number | null }
  | { kind: 'none' };

/**
 * Converts an ECharts pick into a real action. A node pick can filter the Log
 * Tail by the host-reported component; an edge pick only has real counts, so
 * the panel shows those and never fabricates an edge detail page.
 */
export function interpretTopologyPick(params: unknown): TopologyPick {
  if (!isRecord(params)) return { kind: 'none' };
  const data = isRecord(params['data']) ? params['data'] : null;
  if (data === null) return { kind: 'none' };
  if (params['dataType'] === 'edge' || ('source' in data && 'target' in data)) {
    const source = nonEmptyStringOrNull(data['source']);
    const target = nonEmptyStringOrNull(data['target']);
    if (source === null || target === null) return { kind: 'none' };
    return { kind: 'edge', source, target, count: numberOrNull(data['count']) };
  }
  if (params['dataType'] === 'node' || 'id' in data) {
    const component = nonEmptyStringOrNull(data['id']);
    if (component === null) return { kind: 'none' };
    return {
      kind: 'node',
      component,
      label: nonEmptyStringOrNull(data['name']) ?? component,
    };
  }
  return { kind: 'none' };
}

export class TopologyRequestError extends Error {
  readonly status: number | null;

  constructor(message: string, status: number | null = null) {
    super(message);
    this.name = 'TopologyRequestError';
    this.status = status;
  }
}

export interface TopologyClientOptions {
  url?: string;
  fetchImpl?: typeof fetch;
  windowSize?: number;
}

export class TopologyClient {
  private readonly url: string;
  private readonly fetchImpl: typeof fetch | null;
  private readonly windowSize: number;

  constructor(options: TopologyClientOptions = {}) {
    this.url = options.url ?? '/lattice/api/topology';
    this.fetchImpl =
      options.fetchImpl ??
      // `fetch` must keep its Window receiver in a real browser.
      (typeof globalThis.fetch === 'function' ? globalThis.fetch.bind(globalThis) : null);
    const windowSize = options.windowSize ?? TOPOLOGY_DEFAULT_WINDOW;
    this.windowSize = Math.min(8000, Math.max(1, Math.trunc(windowSize)));
  }

  buildUrl(): string {
    const separator = this.url.includes('?') ? '&' : '?';
    return `${this.url}${separator}window=${this.windowSize}`;
  }

  async fetch(signal?: AbortSignal): Promise<TopologySnapshot> {
    if (this.fetchImpl === null) throw new TopologyRequestError('fetch is unavailable in this runtime');
    let response: Response;
    try {
      response = await this.fetchImpl(this.buildUrl(), {
        method: 'GET',
        headers: { Accept: 'application/json' },
        cache: 'no-store',
        credentials: 'same-origin',
        signal,
      });
    } catch (error) {
      if (isAbortError(error)) throw error;
      throw new TopologyRequestError(error instanceof Error ? error.message : 'topology request failed');
    }
    if (!response.ok) {
      throw new TopologyRequestError(`topology failed: HTTP ${response.status}`, response.status);
    }
    let payload: unknown;
    try {
      payload = await response.json();
    } catch {
      throw new TopologyRequestError('topology returned malformed JSON');
    }
    return parseTopology(payload);
  }
}

export interface TopologyStoreOptions {
  url?: string;
  fetchImpl?: typeof fetch;
  intervalMs?: number;
  windowSize?: number;
  now?: () => number;
}

/** Panel-scoped poller for the bounded topology window. */
export class TopologyStore {
  readonly snapshot: ShallowRef<TopologySnapshot | null> = shallowRef(null);
  readonly error: ShallowRef<string | null> = shallowRef(null);
  readonly stale: ShallowRef<boolean> = shallowRef(false);
  readonly loading: ShallowRef<boolean> = shallowRef(false);
  readonly unsupported: ShallowRef<boolean> = shallowRef(false);
  readonly lastFetchedAt: ShallowRef<number | null> = shallowRef(null);

  private readonly client: TopologyClient;
  private readonly intervalMs: number;
  private readonly now: () => number;
  private timer: ReturnType<typeof setInterval> | null = null;
  private controller: AbortController | null = null;
  private visibilityHandler: (() => void) | null = null;
  private inFlight = false;
  private disposed = false;

  constructor(options: TopologyStoreOptions = {}) {
    this.client = new TopologyClient(options);
    this.intervalMs = options.intervalMs ?? TOPOLOGY_INTERVAL_MS;
    this.now = options.now ?? (() => Date.now());
  }

  start(): void {
    if (this.disposed || this.timer !== null) return;
    void this.refresh();
    this.timer = setInterval(() => {
      if (typeof document !== 'undefined' && document.visibilityState === 'hidden') return;
      void this.refresh();
    }, this.intervalMs);
    if (typeof document !== 'undefined' && typeof document.addEventListener === 'function') {
      this.visibilityHandler = () => {
        if (document.visibilityState === 'visible') void this.refresh();
      };
      document.addEventListener('visibilitychange', this.visibilityHandler);
    }
  }

  async refresh(): Promise<void> {
    if (this.disposed || this.inFlight) return;
    this.inFlight = true;
    this.loading.value = true;
    const controller = new AbortController();
    this.controller = controller;
    try {
      const snapshot = await this.client.fetch(controller.signal);
      if (this.disposed || controller.signal.aborted) return;
      this.snapshot.value = snapshot;
      this.unsupported.value = false;
      this.lastFetchedAt.value = this.now();
      this.error.value = null;
      this.stale.value = false;
    } catch (error) {
      if (this.disposed || controller.signal.aborted || isAbortError(error)) return;
      if (error instanceof TopologyRequestError && error.status === 404) {
        this.unsupported.value = true;
        this.snapshot.value = null;
        this.error.value = null;
        this.stale.value = false;
        this.lastFetchedAt.value = null;
        return;
      }
      this.error.value = error instanceof Error ? error.message : 'topology failed';
      if (this.snapshot.value !== null) this.stale.value = true;
    } finally {
      if (this.controller === controller) this.controller = null;
      this.inFlight = false;
      this.loading.value = false;
    }
  }

  stop(): void {
    if (this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
    if (this.visibilityHandler !== null && typeof document !== 'undefined') {
      document.removeEventListener('visibilitychange', this.visibilityHandler);
      this.visibilityHandler = null;
    }
    if (this.controller !== null) {
      this.controller.abort();
      this.controller = null;
    }
    this.inFlight = false;
    this.loading.value = false;
  }

  dispose(): void {
    this.disposed = true;
    this.stop();
  }
}
