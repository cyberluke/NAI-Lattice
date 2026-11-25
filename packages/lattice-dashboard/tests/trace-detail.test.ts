import { flushPromises, mount, type VueWrapper } from '@vue/test-utils';
import { nextTick } from 'vue';
import { describe, expect, it, vi } from 'vitest';
import { APP_CONTEXT_KEY, type LatticeAppContext } from '../src/app-context';
import {
  normalizeTraceGraph,
  type TraceGraph,
  type TraceGraphEdge,
  type TraceGraphEvent,
  type TraceGraphNode,
  type TraceSummary,
} from '../src/query/traces';
import TraceDetail from '../src/modules/traces/TraceDetail.vue';

const BASE_TRACE: TraceSummary = {
  traceId: 'trace-1',
  firstTs: 1_000,
  lastTs: 1_500,
  durationMs: 500,
  state: 'closed',
  terminal: 'completed',
  ok: true,
  spans: [],
};

function node(overrides: Partial<TraceGraphNode> & { id: string }): TraceGraphNode {
  return {
    kind: 'span',
    traceId: 'trace-1',
    spanId: overrides.id,
    parentSpanId: null,
    component: 'nai-os.agent',
    instanceId: null,
    operation: `op.${overrides.id}`,
    spanKind: 'internal',
    status: 'ok',
    startTs: 1_000,
    startOffsetMs: null,
    durationMs: 100,
    reason: null,
    evidenceSeqs: [1],
    ...overrides,
  };
}

function edge(
  id: string,
  kind: TraceGraphEdge['kind'],
  from: string,
  to: string,
  evidenceSeqs: number[],
): TraceGraphEdge {
  return {
    id,
    kind,
    from,
    to,
    operation: kind === 'call' ? `call.${id}` : null,
    transport: kind === 'call' ? 'naf/rpc' : null,
    latencyMs: kind === 'call' ? 2 : null,
    inferred: false,
    evidenceSeqs,
  };
}

const EVENTS: TraceGraphEvent[] = [
  {
    id: 'ev-1',
    seq: 42,
    ts: 1_150,
    level: 'warn',
    component: 'nai-os.agent',
    message: 'queue wait threshold',
    spanId: null,
  },
  {
    id: 'ev-2',
    seq: 43,
    ts: 1_250,
    level: 'info',
    component: 'nai-os.worker',
    message: 'cache miss sampled',
    spanId: null,
  },
  {
    id: 'ev-span-record',
    seq: 3,
    ts: 1_100,
    level: 'info',
    component: 'nai-os.agent',
    message: 'agent.turn span record',
    spanId: 's-agent',
  },
];

function fixtureGraph(branched = false): TraceGraph {
  const nodes: TraceGraphNode[] = [
    node({
      id: 'n-trace',
      kind: 'trace',
      spanId: null,
      component: 'nai-os.shell',
      operation: 'turn.execute',
      status: 'ok',
      startTs: 1_000,
      durationMs: 500,
      evidenceSeqs: [1],
    }),
    node({
      id: 's-root',
      spanId: 's-root',
      component: 'nai-os.shell',
      operation: 'turn.execute',
      spanKind: 'server',
      status: 'ok',
      startTs: 1_000,
      durationMs: 500,
      evidenceSeqs: [2],
    }),
    node({
      id: 's-agent',
      spanId: 's-agent',
      parentSpanId: 's-root',
      component: 'nai-os.agent',
      operation: 'agent.turn',
      spanKind: 'internal',
      status: 'ok',
      startTs: 1_100,
      durationMs: 300,
      evidenceSeqs: [3],
    }),
    node({
      id: 's-tool',
      spanId: 's-tool',
      parentSpanId: 's-agent',
      component: 'nai-os.worker',
      operation: 'tool.exec read_file',
      spanKind: 'client',
      status: 'ok',
      startTs: 1_200,
      durationMs: 100,
      evidenceSeqs: [4],
    }),
  ];
  const edges: TraceGraphEdge[] = [
    edge('c1', 'call', 'n-trace', 's-root', [11]),
    edge('c2', 'call', 's-root', 's-agent', [12]),
    edge('c3', 'call', 's-agent', 's-tool', [13]),
    edge('p1', 'parent', 'n-trace', 's-root', []),
    edge('p2', 'parent', 's-root', 's-agent', []),
    edge('p3', 'parent', 's-agent', 's-tool', []),
  ];
  if (branched) {
    nodes.push(
      node({
        id: 's-unknown',
        spanId: 's-unknown',
        parentSpanId: 's-agent',
        component: 'nai-os.agent',
        operation: 'model.invoke',
        spanKind: 'internal',
        status: 'pending',
        startTs: 1_300,
        durationMs: null,
        evidenceSeqs: [5],
      }),
    );
    edges.push(
      edge('c4', 'call', 's-agent', 's-unknown', [14]),
      edge('p4', 'parent', 's-agent', 's-unknown', []),
    );
  }
  return {
    schema: 2,
    traceId: 'trace-1',
    coverage: 'explicit',
    coverageDetail: null,
    generatedAt: 1_500,
    nodes,
    edges,
    events: EVENTS,
    diagnostics: [],
  };
}

function contextWithGraph(graph: TraceGraph | null, legacy = false): LatticeAppContext {
  const fetchGraph = vi.fn(async () =>
    legacy || graph === null
      ? {
          status: 'unavailable' as const,
          httpStatus: 404,
          reason:
            'This host returned HTTP 404 for /lattice/api/v2/traces/trace-1/graph. Only legacy request correlation is available for this trace.',
        }
      : { status: 'ok' as const, graph },
  );
  return {
    traces: { fetchGraph },
  } as unknown as LatticeAppContext;
}

async function mountDetail(
  graph: TraceGraph | null,
  trace: TraceSummary = BASE_TRACE,
  legacy = false,
): Promise<VueWrapper> {
  const wrapper = mount(TraceDetail, {
    props: { trace },
    global: { provide: { [APP_CONTEXT_KEY]: contextWithGraph(graph, legacy) } },
  });
  await flushPromises();
  await nextTick();
  return wrapper;
}

function countsText(wrapper: VueWrapper): string {
  return wrapper.find('.trace-counts').text().replace(/\s+/g, ' ').trim();
}

describe('trace detail truthful accounting', () => {
  it('shows legacy fallback text and no fake chain nodes on HTTP 404', async () => {
    const legacyTrace: TraceSummary = {
      ...BASE_TRACE,
      spans: [
        {
          ts: 1_000,
          kind: 'request',
          label: 'legacy alpha',
          runtime_sequence: 3,
          durationMs: 600,
          component: 'nai-os.shell',
        },
      ],
    };
    const wrapper = await mountDetail(null, legacyTrace, true);

    expect(wrapper.find('.trace-tab--active').text()).toBe('CHAIN');
    expect(wrapper.text()).toContain('CAUSAL CHAIN UNAVAILABLE — legacy correlation only');
    expect(wrapper.text()).toContain('HTTP 404');
    expect(wrapper.text()).toContain('legacy alpha');
    expect(countsText(wrapper)).toBe('EVENTS 1 · SPANS 0 · CALLS 0');
    expect(wrapper.findAll('.chain-node')).toHaveLength(0);
    expect(wrapper.findAll('.chain-edge')).toHaveLength(0);
    wrapper.unmount();
  });

  it('treats a 200 response with coverage legacy as the same truthful fallback', async () => {
    const legacyTrace: TraceSummary = {
      ...BASE_TRACE,
      spans: [
        {
          ts: 1_000,
          kind: 'request',
          label: 'legacy gamma',
          runtime_sequence: 5,
          durationMs: null,
          component: 'nai-os.shell',
        },
      ],
    };
    const legacyGraph: TraceGraph = {
      schema: 2,
      traceId: 'trace-1',
      coverage: 'legacy',
      coverageDetail: 'No record carries extras.span_id.',
      generatedAt: 1_500,
      nodes: [],
      edges: [],
      events: [
        {
          id: 'ev-legacy',
          seq: 5,
          ts: 1_000,
          level: 'info',
          component: 'nai-os.shell',
          message: 'legacy gamma',
          spanId: null,
        },
      ],
      diagnostics: [],
    };
    const wrapper = await mountDetail(legacyGraph, legacyTrace);

    expect(wrapper.text()).toContain('CAUSAL CHAIN UNAVAILABLE — legacy correlation only');
    expect(wrapper.text()).toContain('legacy gamma');
    expect(countsText(wrapper)).toBe('EVENTS 1 · SPANS 0 · CALLS 0');
    expect(wrapper.findAll('.chain-node')).toHaveLength(0);
    wrapper.unmount();
  });

  it('renders counts, a linear breadcrumb and selectable node/edge evidence', async () => {
    const wrapper = await mountDetail(fixtureGraph(false));

    expect(countsText(wrapper)).toBe('EVENTS 2 · SPANS 3 · CALLS 3');
    expect(wrapper.find('.chain-breadcrumb').exists()).toBe(true);
    expect(wrapper.findAll('.chain-node')).toHaveLength(4);
    expect(wrapper.findAll('.chain-edge')).toHaveLength(3);

    const toolNode = wrapper
      .findAll('.chain-node')
      .find((candidate) => candidate.text().includes('tool.exec read_file'));
    expect(toolNode).toBeDefined();
    await toolNode?.trigger('click');

    const inspector = wrapper.find('.chain-inspector');
    expect(inspector.exists()).toBe(true);
    expect(inspector.text()).toContain('span inspector');
    expect(inspector.text()).toContain('nai-os.worker');
    expect(inspector.text()).toContain('tool.exec read_file');
    expect(inspector.text()).toContain('seq 4');

    const firstEdge = wrapper.findAll('.chain-edge')[0];
    await firstEdge.trigger('click');
    const edgeInspector = wrapper.find('.chain-inspector');
    expect(edgeInspector.text()).toContain('edge evidence');
    expect(edgeInspector.text()).toContain('seq 11');
    expect(edgeInspector.text()).toContain('naf/rpc');
    wrapper.unmount();
  });

  it('renders a branched layered layout for fan-out', async () => {
    const wrapper = await mountDetail(fixtureGraph(true));

    expect(wrapper.find('.chain-layers').exists()).toBe(true);
    expect(wrapper.find('.chain-breadcrumb').exists()).toBe(false);
    const layers = wrapper.findAll('.chain-layer');
    expect(layers).toHaveLength(4);
    expect(layers[3].findAll('.chain-node')).toHaveLength(2);
    expect(countsText(wrapper)).toBe('EVENTS 2 · SPANS 4 · CALLS 4');
    wrapper.unmount();
  });

  it('lists the call tree from parent edges with indentation', async () => {
    const wrapper = await mountDetail(fixtureGraph(false));
    const treeTab = wrapper.findAll('.trace-tab').find((tab) => tab.text() === 'CALL TREE');
    await treeTab?.trigger('click');

    const rows = wrapper.findAll('.call-tree-row');
    expect(rows).toHaveLength(4);
    expect(rows[0].attributes('data-depth')).toBe('0');
    expect(rows[3].attributes('data-depth')).toBe('3');
    expect(rows[3].text()).toContain('tool.exec read_file');
    expect(rows[3].text()).toContain('nai-os.worker');
    expect(rows[3].text()).toContain('100ms');
    wrapper.unmount();
  });

  it('lists only point events and never turns a span record into an event', async () => {
    const wrapper = await mountDetail(fixtureGraph(false));
    const eventsTab = wrapper.findAll('.trace-tab').find((tab) => tab.text() === 'EVENTS');
    await eventsTab?.trigger('click');

    const eventRows = wrapper.findAll('.trace-event-row');
    expect(eventRows).toHaveLength(2);
    expect(wrapper.text()).toContain('queue wait threshold');
    expect(wrapper.text()).toContain('cache miss sampled');
    expect(wrapper.text()).not.toContain('agent.turn span record');
    // No synthetic span link exists for a contract point event.
    expect(wrapper.findAll('.trace-event-message__link')).toHaveLength(0);
    wrapper.unmount();
  });
});

describe('live host schema (span:<id>, startOffsetMs, evidenceSequences)', () => {
  const hostGraph = normalizeTraceGraph({
    schema: 2,
    traceId: 'trace-1',
    coverage: 'spans',
    generatedAt: 1_764_700_000_020,
    truncated: false,
    nodes: [
      {
        id: 'trace:trace-1',
        kind: 'trace',
        label: 'trace-1',
        component: null,
        operation: null,
        status: 'ok',
        durationMs: null,
        startOffsetMs: null,
        parentSpanId: null,
        instanceId: null,
        evidenceSequences: [],
      },
      {
        id: 'span:s-root',
        kind: 'span',
        label: 'turn.submitted',
        component: 'alpha',
        operation: 'turn.submitted',
        status: 'ok',
        durationMs: 10,
        startOffsetMs: 0,
        parentSpanId: null,
        instanceId: null,
        evidenceSequences: [7],
      },
      {
        id: 'span:s-child',
        kind: 'span',
        label: 'tool.exec',
        component: 'beta',
        operation: 'tool.exec',
        status: 'error',
        durationMs: 5,
        startOffsetMs: 10,
        parentSpanId: 's-root',
        instanceId: null,
        evidenceSequences: [8],
      },
    ],
    edges: [
      {
        id: 'parent:trace:trace-1->span:s-root',
        source: 'trace:trace-1',
        target: 'span:s-root',
        kind: 'parent',
        operation: null,
        transport: null,
        durationMs: null,
        evidenceSequences: [7],
        inferred: false,
      },
      {
        id: 'parent:span:s-root->span:s-child',
        source: 'span:s-root',
        target: 'span:s-child',
        kind: 'parent',
        operation: 'tool.exec',
        transport: null,
        durationMs: null,
        evidenceSequences: [8],
        inferred: false,
      },
      {
        id: 'call:span:s-root->span:s-child:tool.call:naf',
        source: 'span:s-root',
        target: 'span:s-child',
        kind: 'call',
        operation: 'tool.call',
        transport: 'naf/rpc',
        durationMs: 4,
        evidenceSequences: [7],
        inferred: false,
      },
    ],
    events: [
      {
        sequence: 7,
        ts: 1_000,
        level: 'info',
        component: 'alpha',
        message: 'turn.submitted',
        spanId: 's-root',
      },
      {
        sequence: 8,
        ts: 1_010,
        level: 'error',
        component: 'beta',
        message: 'tool.exec',
        spanId: 's-child',
      },
      {
        sequence: 9,
        ts: 1_020,
        level: 'warn',
        component: 'alpha',
        message: 'queue watermark crossed',
        spanId: null,
      },
    ],
    diagnostics: [],
  });

  it('renders the host graph with point-only counts and real span bars', async () => {
    const wrapper = await mountDetail(hostGraph);
    expect(countsText(wrapper)).toBe('EVENTS 1 · SPANS 2 · CALLS 1');
    expect(wrapper.find('.chain-breadcrumb').exists()).toBe(true);
    expect(wrapper.findAll('.chain-node')).toHaveLength(3);

    const tab = wrapper.findAll('.trace-tab').find((candidate) => candidate.text() === 'WATERFALL');
    await tab?.trigger('click');
    const rows = wrapper.findAll('.waterfall-row');
    const childRow = rows.find((row) => row.text().includes('tool.exec'));
    expect(childRow).toBeDefined();
    expect(childRow?.find('.wf-bar').classes()).toContain('wf-bar--error');
    expect(childRow?.text()).toContain('beta');
    expect(childRow?.text()).toContain('+10ms');

    const pointRow = rows.find((row) => row.text().includes('queue watermark crossed'));
    expect(pointRow?.find('.wf-tick').exists()).toBe(true);
    expect(pointRow?.find('.wf-bar').exists()).toBe(false);
    wrapper.unmount();
  });
});

describe('trace waterfall accounting', () => {
  async function mountWaterfall(graph: TraceGraph | null, trace = BASE_TRACE, legacy = false) {
    const wrapper = await mountDetail(graph, trace, legacy);
    const tab = wrapper.findAll('.trace-tab').find((candidate) => candidate.text() === 'WATERFALL');
    await tab?.trigger('click');
    return wrapper;
  }

  it('renders point events as ticks and timed spans as colored bars', async () => {
    const wrapper = await mountWaterfall(fixtureGraph(false));
    const rows = wrapper.findAll('.waterfall-row');
    const rowWith = (text: string) =>
      rows.find((row) => row.text().includes(text));

    const eventRow = rowWith('queue wait threshold');
    expect(eventRow).toBeDefined();
    expect(eventRow?.find('.wf-tick').exists()).toBe(true);
    expect(eventRow?.find('.wf-bar').exists()).toBe(false);

    const toolRow = rowWith('tool.exec read_file');
    expect(toolRow).toBeDefined();
    const toolBar = toolRow?.find('.wf-bar');
    expect(toolBar?.exists()).toBe(true);
    expect(toolBar?.classes()).toContain('wf-bar--tool');
    expect(toolRow?.text()).toContain('nai-os.worker');
    expect(toolRow?.text()).toContain('+200ms');
    expect(toolRow?.text()).toContain('100ms');

    const rootRow = rowWith('turn.execute');
    expect(rootRow?.find('.wf-bar').classes()).toContain('wf-bar--request');

    expect(wrapper.find('.waterfall-legend').text()).toContain('REQUEST');
    expect(wrapper.find('.waterfall-legend').text()).toContain('ERROR');
    wrapper.unmount();
  });

  it('shows N/A for a span without a recorded duration and never fakes a bar', async () => {
    const wrapper = await mountWaterfall(fixtureGraph(true));
    const unknownRow = wrapper
      .findAll('.waterfall-row')
      .find((row) => row.text().includes('model.invoke'));
    expect(unknownRow).toBeDefined();
    expect(unknownRow?.find('.wf-bar').exists()).toBe(false);
    expect(unknownRow?.find('.wf-unknown').exists()).toBe(true);
    expect(unknownRow?.find('.wf-duration').text()).toBe('N/A');
    wrapper.unmount();
  });

  it('renders legacy correlation records as point markers only', async () => {
    const legacyTrace: TraceSummary = {
      ...BASE_TRACE,
      spans: [
        {
          ts: 1_000,
          kind: 'tool',
          label: 'legacy beta',
          runtime_sequence: 8,
          durationMs: 600,
          component: 'nai-os.worker',
        },
      ],
    };
    const wrapper = await mountWaterfall(null, legacyTrace, true);

    expect(wrapper.text()).toContain('CAUSAL CHAIN UNAVAILABLE — legacy correlation only');
    const rows = wrapper.findAll('.waterfall-row');
    expect(rows).toHaveLength(1);
    expect(rows[0].find('.wf-tick').exists()).toBe(true);
    expect(rows[0].find('.wf-bar').exists()).toBe(false);
    expect(rows[0].text()).toContain('legacy beta');
    expect(rows[0].text()).toContain('600ms');
    wrapper.unmount();
  });
});
