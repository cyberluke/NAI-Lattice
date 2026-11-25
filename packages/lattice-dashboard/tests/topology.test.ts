import { describe, expect, it } from 'vitest';
import topologyPanelSource from '../src/modules/topology/TopologyPanel.vue?raw';
import {
  TOPOLOGY_LAYOUT,
  TOPOLOGY_MAX_SYMBOL_SIZE,
  TOPOLOGY_MIN_SURFACE_PX,
  TOPOLOGY_ZOOM,
  TopologyClient,
  TopologyRequestError,
  TopologyStore,
  buildTopologyOption,
  formatTopologyTooltip,
  interpretTopologyPick,
  parseTopology,
  type TopologySnapshot,
} from '../src/topology/topology';

interface GraphNodeData {
  id: string;
  name: string;
  count: number | null;
  errors: number | null;
  symbolSize: number;
  itemStyle: { color: string };
}

interface GraphLinkData {
  source: string;
  target: string;
  count: number | null;
  lineStyle: { width: number };
}

interface GraphSeries {
  type: string;
  layout?: string;
  roam?: boolean;
  zoom?: number;
  center?: [string, string];
  draggable?: boolean;
  force?: unknown;
  circular?: { rotateLabel?: boolean };
  labelLayout?: { hideOverlap?: boolean };
  data: GraphNodeData[];
  links: GraphLinkData[];
}

function graphOf(snapshot: TopologySnapshot): GraphSeries {
  const option = buildTopologyOption(snapshot);
  return (option as unknown as { series: GraphSeries[] }).series[0];
}

function jsonResponse(payload: unknown, status = 200): Response {
  return new Response(JSON.stringify(payload), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

const PAYLOAD = {
  schema: 1,
  generatedAt: 1_764_000_000_000,
  window: 4000,
  truncated: false,
  nodes: [
    { id: 'nai-os.agent', label: 'nai-os.agent', count: 65, errors: 11 },
    { id: 'nai-os.terminal', label: 'nai-os.terminal', count: 22, errors: 0 },
    { id: 'nai-os.quiet', label: 'quiet', count: null, errors: null },
  ],
  edges: [
    { source: 'nai-os.agent', target: 'nai-os.terminal', count: 22 },
    { source: 'nai-os.agent', target: 'ghost', count: 3 },
    { source: 'nai-os.agent', target: 'nai-os.terminal', count: 99 },
    { source: 'nai-os.terminal', target: 'nai-os.quiet', count: null },
  ],
};

describe('topology shaping', () => {
  it('keeps real nodes/edges and drops phantom endpoints and duplicates', () => {
    const snapshot = parseTopology(PAYLOAD);
    expect(snapshot.window).toBe(4000);
    expect(snapshot.truncated).toBe(false);
    expect(snapshot.nodes.map((node) => node.id)).toEqual([
      'nai-os.agent',
      'nai-os.terminal',
      'nai-os.quiet',
    ]);
    expect(snapshot.nodes[2].count).toBeNull();
    expect(snapshot.nodes[2].label).toBe('quiet');
    // The ghost edge would make ECharts synthesize a node; the duplicate keeps
    // the first real host count.
    expect(snapshot.edges).toEqual([
      { source: 'nai-os.agent', target: 'nai-os.terminal', count: 22 },
      { source: 'nai-os.terminal', target: 'nai-os.quiet', count: null },
    ]);
  });

  it('renders a real empty graph when the window has no components', () => {
    const snapshot = parseTopology({
      schema: 1,
      generatedAt: 1_764_000_000_000,
      window: 4000,
      truncated: true,
      nodes: [],
      edges: [],
    });
    const graph = graphOf(snapshot);
    expect(graph.type).toBe('graph');
    expect(graph.data).toEqual([]);
    expect(graph.links).toEqual([]);
    expect(snapshot.truncated).toBe(true);
  });

  it('sizes nodes/edges by host counts and tones error nodes red', () => {
    const graph = graphOf(parseTopology(PAYLOAD));
    const agent = graph.data.find((node) => node.id === 'nai-os.agent');
    const quiet = graph.data.find((node) => node.id === 'nai-os.quiet');
    expect(agent).toBeDefined();
    expect(quiet).toBeDefined();
    expect(agent?.symbolSize).toBeGreaterThan(quiet?.symbolSize ?? 0);
    expect(agent?.itemStyle.color).toBe('#ff6b6b');
    expect(quiet?.itemStyle.color).toBe('#19c2d8');
    const high = graph.links.find((link) => link.target === 'nai-os.terminal');
    const unreported = graph.links.find((link) => link.target === 'nai-os.quiet');
    expect(high?.lineStyle.width).toBeGreaterThan(unreported?.lineStyle.width ?? 0);
    expect(unreported?.count).toBeNull();
  });

  it('turns a node pick into a real component filter and an edge pick into counts only', () => {
    expect(
      interpretTopologyPick({
        dataType: 'node',
        data: { id: 'nai-os.agent', name: 'nai-os.agent', count: 65, errors: 11 },
      }),
    ).toEqual({ kind: 'node', component: 'nai-os.agent', label: 'nai-os.agent' });
    expect(
      interpretTopologyPick({
        dataType: 'edge',
        data: { source: 'nai-os.agent', target: 'nai-os.terminal', count: 22 },
      }),
    ).toEqual({ kind: 'edge', source: 'nai-os.agent', target: 'nai-os.terminal', count: 22 });
    expect(interpretTopologyPick({ dataType: 'edge', data: { count: 1 } })).toEqual({
      kind: 'none',
    });
    expect(interpretTopologyPick(null)).toEqual({ kind: 'none' });
  });

  it('formats real counts in tooltips and never invents an unknown count', () => {
    expect(
      formatTopologyTooltip({
        dataType: 'node',
        data: { id: 'a', name: 'a', count: 3, errors: 1 },
      }),
    ).toBe('a\n3 spans · 1 error');
    expect(
      formatTopologyTooltip({
        dataType: 'edge',
        data: { source: 'a', target: 'b', count: 7 },
      }),
    ).toBe('a → b\n7 transitions');
    expect(
      formatTopologyTooltip({ dataType: 'node', data: { id: 'a', name: 'a', count: null, errors: null } }),
    ).toBe('a\ncount not reported · errors not reported');
  });

  it('rejects a foreign schema instead of interpreting it', () => {
    expect(() => parseTopology({ schema: 2, nodes: [], edges: [] })).toThrow(
      /unsupported topology schema/,
    );
  });
});

describe('topology layout stability', () => {
  it('uses a deterministic circular layout instead of a force simulation', () => {
    const option = buildTopologyOption(parseTopology(PAYLOAD)) as unknown as {
      animation?: boolean;
      animationDurationUpdate?: number;
      series: GraphSeries[];
    };
    const graph = option.series[0];

    expect(option.animation).toBe(false);
    expect(option.animationDurationUpdate).toBe(0);
    expect(graph.layout).toBe(TOPOLOGY_LAYOUT);
    expect(graph.layout).toBe('circular');
    expect(graph.force).toBeUndefined();
    expect(graph.roam).toBe(true);
    expect(graph.zoom).toBe(TOPOLOGY_ZOOM);
    expect(graph.center).toEqual(['50%', '50%']);
    expect(graph.draggable).toBe(false);
    expect(graph.circular?.rotateLabel).toBe(true);
    expect(graph.labelLayout?.hideOverlap).toBe(true);
  });

  it('orders nodes by id, never by a count that changes every poll', () => {
    const snapshot = parseTopology(PAYLOAD);
    const first = graphOf(snapshot);
    expect(first.data.map((node) => node.id)).toEqual([
      'nai-os.agent',
      'nai-os.quiet',
      'nai-os.terminal',
    ]);

    // A poll where every count changes must not reorder the ring.
    const changed = parseTopology({
      ...PAYLOAD,
      nodes: [
        { id: 'nai-os.agent', label: 'nai-os.agent', count: 1, errors: 0 },
        { id: 'nai-os.terminal', label: 'nai-os.terminal', count: 900, errors: 4 },
        { id: 'nai-os.quiet', label: 'quiet', count: 400, errors: null },
      ],
    });
    expect(graphOf(changed).data.map((node) => node.id)).toEqual(
      first.data.map((node) => node.id),
    );
    expect(graphOf(changed).links.map((link) => `${link.source}>${link.target}`)).toEqual([
      'nai-os.agent>nai-os.terminal',
      'nai-os.terminal>nai-os.quiet',
    ]);
  });

  it('scales the largest node inside the panel minimum surface', () => {
    // ECharts scales the whole circular view, including symbols, by `zoom`.
    const halfSurface = TOPOLOGY_MIN_SURFACE_PX / 2;
    const worstNodeHalf = (TOPOLOGY_ZOOM * TOPOLOGY_MAX_SYMBOL_SIZE) / 2;
    const worstRadius = (TOPOLOGY_ZOOM * TOPOLOGY_MIN_SURFACE_PX) / 2;
    expect(worstRadius + worstNodeHalf).toBeLessThanOrEqual(halfSurface);
    expect(
      graphOf(parseTopology(PAYLOAD)).data.every(
        (node) => node.symbolSize <= TOPOLOGY_MAX_SYMBOL_SIZE,
      ),
    ).toBe(true);

    expect(topologyPanelSource).toContain(`min-height: ${TOPOLOGY_MIN_SURFACE_PX}px`);
  });
});

describe('topology client/store', () => {
  it('requests the frozen 4000-record window and surfaces a 404 as unsupported', async () => {
    const urls: string[] = [];
    const fetchImpl = (async (input: RequestInfo | URL) => {
      urls.push(String(input));
      return jsonResponse({}, 404);
    }) as unknown as typeof fetch;
    const store = new TopologyStore({ fetchImpl, now: () => 1000 });
    await store.refresh();
    expect(urls).toEqual(['/lattice/api/topology?window=4000']);
    expect(store.unsupported.value).toBe(true);
    expect(store.snapshot.value).toBeNull();
    expect(store.error.value).toBeNull();

    const client = new TopologyClient({ fetchImpl });
    await expect(client.fetch()).rejects.toBeInstanceOf(TopologyRequestError);
    store.dispose();
  });
});
