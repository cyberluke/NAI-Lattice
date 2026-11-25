import { describe, expect, it } from 'vitest';
import type {
  TraceGraph,
  TraceGraphEvent,
  TraceGraphNode,
  TraceSummary,
} from '../src/query/traces';
import {
  buildCallTreeRows,
  buildChainLayout,
  buildWaterfallGeometry,
  buildWaterfallRows,
  graphPointEvents,
  nodeStatusLabel,
  nodeTone,
  offsetText,
  resolveWaterfallDomain,
  rowDurationText,
  traceGraphCounts,
  waterfallTone,
} from '../src/modules/traces/chain';

function makeNode(overrides: Partial<TraceGraphNode> & { id: string }): TraceGraphNode {
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

function makeGraph(
  nodes: TraceGraphNode[],
  edges: TraceGraph['edges'],
  events: TraceGraphEvent[] = [],
): TraceGraph {
  return {
    schema: 2,
    traceId: 'trace-1',
    coverage: 'explicit',
    coverageDetail: null,
    generatedAt: 1_000,
    nodes,
    edges,
    events,
    diagnostics: [],
  };
}

const TRACE: TraceSummary = {
  traceId: 'trace-1',
  firstTs: 1_000,
  lastTs: 2_000,
  durationMs: 1_000,
  state: 'closed',
  terminal: 'completed',
  ok: true,
  spans: [],
};

describe('trace graph accounting', () => {
  it('counts events, real spans and explicit calls separately', () => {
    const graph = makeGraph(
      [
        makeNode({ id: 'root', kind: 'trace', spanId: null, durationMs: 1_000 }),
        makeNode({ id: 'a' }),
        makeNode({ id: 'b', status: 'error' }),
        makeNode({ id: 'external', kind: 'unresolved', spanId: null, status: null }),
      ],
      [
        { id: 'e1', kind: 'call', from: 'root', to: 'a', operation: null, transport: null, latencyMs: null, inferred: false, evidenceSeqs: [1] },
        { id: 'e2', kind: 'call', from: 'a', to: 'b', operation: null, transport: null, latencyMs: null, inferred: false, evidenceSeqs: [2] },
        { id: 'e3', kind: 'call', from: 'a', to: 'b', operation: null, transport: null, latencyMs: null, inferred: true, evidenceSeqs: [3] },
        { id: 'e4', kind: 'parent', from: 'root', to: 'a', operation: null, transport: null, latencyMs: null, inferred: false, evidenceSeqs: [] },
        { id: 'e5', kind: 'response', from: 'a', to: 'root', operation: null, transport: null, latencyMs: null, inferred: false, evidenceSeqs: [] },
      ],
      [
        { id: 'ev1', seq: 10, ts: 1_100, level: 'info', component: 'x', message: 'one', spanId: null },
        { id: 'ev2', seq: 11, ts: 1_200, level: 'warn', component: 'x', message: 'two', spanId: null },
      ],
    );

    expect(traceGraphCounts(graph)).toEqual({
      events: 2,
      spans: 2,
      calls: 2,
      inferredCalls: 1,
    });
    expect(traceGraphCounts(null, 4)).toEqual({
      events: 4,
      spans: 0,
      calls: 0,
      inferredCalls: 0,
    });
  });

  it('does not count span records or call observations as point events', () => {
    const graph = makeGraph(
      [
        makeNode({ id: 's1', evidenceSeqs: [1] }),
        makeNode({ id: 's2', evidenceSeqs: [2] }),
      ],
      [
        {
          id: 'e1',
          kind: 'call',
          from: 's1',
          to: 's2',
          operation: null,
          transport: null,
          latencyMs: null,
          inferred: false,
          evidenceSeqs: [3],
        },
      ],
      [
        { id: 'ev-span', seq: 1, ts: 1_000, level: 'info', component: 'x', message: 'span record', spanId: 's1' },
        { id: 'ev-orphan', seq: 5, ts: 1_005, level: 'info', component: 'x', message: 'span record beyond a cap', spanId: 's-capped' },
        { id: 'ev-call', seq: 3, ts: 1_001, level: 'info', component: 'x', message: 'call record', spanId: null },
        { id: 'ev-point', seq: 4, ts: 1_002, level: 'warn', component: 'x', message: 'point', spanId: null },
      ],
    );

    expect(traceGraphCounts(graph).events).toBe(1);
    expect(graphPointEvents(graph).map((event) => event.seq)).toEqual([4]);
  });

  it('maps status tones to labels and never treats unresolved as observed', () => {
    expect(nodeTone(makeNode({ id: 'ok', status: 'completed' }))).toBe('observed');
    expect(nodeTone(makeNode({ id: 'bad', status: 'error' }))).toBe('bad');
    expect(nodeTone(makeNode({ id: 'warn', status: 'cancelled' }))).toBe('warn');
    expect(nodeTone(makeNode({ id: 'unknown', status: null }))).toBe('muted');
    expect(nodeTone(makeNode({ id: 'unres', kind: 'unresolved', status: 'ok' }))).toBe('muted');
    expect(nodeStatusLabel(makeNode({ id: 'u', kind: 'unresolved', status: null }))).toBe(
      'unresolved',
    );
    expect(nodeStatusLabel(makeNode({ id: 'n', status: null }))).toBe('status n/a');
  });
});

describe('execution chain layout', () => {
  const root = makeNode({ id: 'root', kind: 'trace', spanId: null });
  const a = makeNode({ id: 'a', parentSpanId: 'root', startTs: 1_100 });
  const b = makeNode({ id: 'b', parentSpanId: 'a', startTs: 1_200 });
  const c = makeNode({ id: 'c', parentSpanId: 'a', startTs: 1_300 });

  function call(id: string, from: string, to: string, inferred = false): TraceGraph['edges'][number] {
    return {
      id,
      kind: 'call',
      from,
      to,
      operation: `call.${id}`,
      transport: 'naf/rpc',
      latencyMs: 3,
      inferred,
      evidenceSeqs: [1],
    };
  }

  it('renders a linear path as a breadcrumb with one root', () => {
    const graph = makeGraph(
      [root, a, b],
      [call('e1', 'root', 'a'), call('e2', 'a', 'b')],
    );
    const layout = buildChainLayout(graph);
    expect(layout.mode).toBe('linear');
    expect(layout.layers).toHaveLength(3);
    expect(layout.links).toHaveLength(2);
    expect(layout.roots).toBe(1);
    expect(layout.note).toBe('calls');
  });

  it('marks fan-out as branched with multiple nodes in the second layer', () => {
    const graph = makeGraph(
      [root, a, b, c],
      [call('e1', 'root', 'a'), call('e2', 'root', 'b'), call('e3', 'a', 'c')],
    );
    const layout = buildChainLayout(graph);
    expect(layout.mode).toBe('branched');
    expect(layout.layers[1].map((view) => view.node.id).sort()).toEqual(['a', 'b']);
    expect(layout.layers[2].map((view) => view.node.id)).toEqual(['c']);
  });

  it('fills call gaps with explicit parent edges without duplicating a call pair', () => {
    const graph = makeGraph(
      [root, a, b, c],
      [
        call('e1', 'root', 'a'),
        {
          id: 'p1',
          kind: 'parent',
          from: 'root',
          to: 'a',
          operation: null,
          transport: null,
          latencyMs: null,
          inferred: false,
          evidenceSeqs: [],
        },
        {
          id: 'p2',
          kind: 'parent',
          from: 'a',
          to: 'b',
          operation: null,
          transport: null,
          latencyMs: null,
          inferred: false,
          evidenceSeqs: [],
        },
        {
          id: 'p3',
          kind: 'parent',
          from: 'root',
          to: 'c',
          operation: null,
          transport: null,
          latencyMs: null,
          inferred: false,
          evidenceSeqs: [],
        },
      ],
    );
    const layout = buildChainLayout(graph);
    expect(layout.note).toBe('calls');
    expect(layout.augmentedParents).toBe(2);
    expect(layout.links.map((link) => link.edge.id).sort()).toEqual(['e1', 'p2', 'p3']);
  });

  it('hides inferred edges by default and exposes them on request', () => {
    const graph = makeGraph(
      [root, a],
      [call('e1', 'root', 'a'), call('e2', 'a', 'root', true)],
    );
    const hidden = buildChainLayout(graph);
    expect(hidden.links).toHaveLength(1);
    expect(hidden.inferredHidden).toBe(1);

    const shown = buildChainLayout(graph, { includeInferred: true });
    expect(shown.links).toHaveLength(2);
    expect(shown.inferredHidden).toBe(0);
    expect(shown.links.some((link) => link.edge.inferred)).toBe(true);
  });

  it('falls back to explicit parent edges and reports missing endpoints', () => {
    const parentOnly = makeGraph(
      [root, a],
      [
        {
          id: 'p1',
          kind: 'parent',
          from: 'root',
          to: 'a',
          operation: null,
          transport: null,
          latencyMs: null,
          inferred: false,
          evidenceSeqs: [],
        },
        {
          id: 'p2',
          kind: 'parent',
          from: 'ghost',
          to: 'a',
          operation: null,
          transport: null,
          latencyMs: null,
          inferred: false,
          evidenceSeqs: [],
        },
      ],
    );
    const layout = buildChainLayout(parentOnly);
    expect(layout.note).toBe('parent');
    expect(layout.mode).toBe('linear');
    expect(layout.droppedEdges).toBe(1);
    expect(layout.links).toHaveLength(1);
  });
});

describe('call tree', () => {
  const root = makeNode({ id: 'root', kind: 'trace', spanId: null });
  const a = makeNode({ id: 'a', startTs: 1_100 });
  const b = makeNode({ id: 'b', startTs: 1_300 });
  const c = makeNode({ id: 'c', startTs: 1_200 });

  function parent(id: string, from: string, to: string): TraceGraph['edges'][number] {
    return {
      id,
      kind: 'parent',
      from,
      to,
      operation: null,
      transport: null,
      latencyMs: null,
      inferred: false,
      evidenceSeqs: [],
    };
  }

  it('builds indentation depth from parent edges only', () => {
    const graph = makeGraph(
      [root, a, b, c],
      [parent('p1', 'root', 'a'), parent('p2', 'a', 'c'), parent('p3', 'root', 'b')],
    );
    const result = buildCallTreeRows(graph);
    const depth = new Map(result.rows.map((row) => [row.node.id, row.depth]));
    expect(depth.get('root')).toBe(0);
    expect(depth.get('a')).toBe(1);
    expect(depth.get('c')).toBe(2);
    expect(depth.get('b')).toBe(1);
    expect(result.roots).toBe(1);
    expect(result.parentEdges).toBe(3);
  });

  it('breaks cycles once and reports a missing parent edge', () => {
    const graph = makeGraph(
      [a, b],
      [parent('p1', 'a', 'b'), parent('p2', 'b', 'a'), parent('p3', 'ghost', 'a')],
    );
    const result = buildCallTreeRows(graph);
    expect(result.rows).toHaveLength(2);
    expect(result.cyclic).toBeGreaterThanOrEqual(1);
    expect(result.missingParents).toBe(1);
  });
});

describe('waterfall rows and geometry', () => {
  const span = makeNode({
    id: 's1',
    spanId: 's1',
    component: 'nai-os.worker',
    operation: 'tool.exec read_file',
    spanKind: 'client',
    status: 'ok',
    startTs: 1_000,
    durationMs: 200,
    evidenceSeqs: [7],
  });
  const unknown = makeNode({
    id: 's2',
    spanId: 's2',
    component: 'nai-os.agent',
    operation: 'model.invoke',
    spanKind: 'internal',
    status: 'pending',
    startTs: 1_400,
    durationMs: null,
    evidenceSeqs: [8],
  });
  const event: TraceGraphEvent = {
    id: 'ev1',
    seq: 9,
    ts: 1_100,
    level: 'warn',
    component: 'nai-os.agent',
    message: 'queue wait',
    spanId: null,
  };

  it('uses bars only for span id + duration and ticks for point events', () => {
    const graph = makeGraph(
      [makeNode({ id: 'root', kind: 'trace', spanId: null }), span, unknown],
      [],
      [event],
    );
    const rows = buildWaterfallRows(TRACE, graph);
    expect(rows.map((row) => row.origin)).toEqual(['span', 'event', 'span']);

    const timed = rows.find((row) => row.key === 'span:s1');
    expect(timed).toMatchObject({
      hasBar: true,
      isPointEvent: false,
      durationMs: 200,
      component: 'nai-os.worker',
      tone: 'tool',
      seq: 7,
    });

    const point = rows.find((row) => row.origin === 'event');
    expect(point).toMatchObject({ hasBar: false, isPointEvent: true, durationMs: null });
    expect(rowDurationText(point!)).toBe('—');

    const noDuration = rows.find((row) => row.key === 'span:s2');
    expect(noDuration).toMatchObject({ hasBar: false, durationUnknown: true, durationMs: null });
    expect(rowDurationText(noDuration!)).toBe('N/A');
  });

  it('turns legacy lifecycle records into point markers without bars', () => {
    const legacyTrace: TraceSummary = {
      ...TRACE,
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
    const rows = buildWaterfallRows(legacyTrace, null);
    expect(rows).toHaveLength(1);
    expect(rows[0]).toMatchObject({
      origin: 'legacy',
      isPointEvent: true,
      hasBar: false,
      durationMs: 600,
      status: 'request',
    });
    expect(rows[0].key).toContain('legacy');
  });

  it('computes offsets, bar geometry and a truthful domain', () => {
    const graph = makeGraph([span, unknown], [], [event]);
    const rows = buildWaterfallRows(TRACE, graph);
    const domain = resolveWaterfallDomain(TRACE, rows);
    expect(domain).toEqual({ startMs: 1_000, endMs: 2_000, spanMs: 1_000 });

    const geometry = buildWaterfallGeometry(rows, domain, 1_000);
    expect(geometry.get('span:s1')).toEqual({ leftPx: 0, widthPx: 200, minApplied: false });
    expect(geometry.has('event:ev1')).toBe(false);
    expect(geometry.has('span:s2')).toBe(false);
    expect(offsetText(0)).toBe('+0ms');
    expect(offsetText(200)).toBe('+200ms');
    expect(offsetText(null)).toBe('—');
  });

  it('maps span kind, operation and failure to the approved tones', () => {
    expect(waterfallTone('server', 'turn.execute', 'nai-os.shell', 'ok')).toBe('request');
    expect(waterfallTone(null, 'turn.execute', 'nai-os.shell', 'ok')).toBe('request');
    expect(waterfallTone(null, 'agent.turn', 'nai-os.agent', 'ok')).toBe('log');
    expect(waterfallTone('internal', 'tool.exec', 'nai-os.worker', 'ok')).toBe('tool');
    expect(waterfallTone('internal', 'process.exec', 'nai-os.terminal', 'ok')).toBe('terminal');
    expect(waterfallTone('internal', 'log.write', 'nai-os.observability', 'ok')).toBe('log');
    expect(waterfallTone('internal', 'log.write', 'nai-os.observability', 'error')).toBe('error');
  });
});
