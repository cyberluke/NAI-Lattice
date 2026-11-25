import { describe, expect, it } from 'vitest';
import { TracesClient, normalizeTraceGraph } from '../src/query/traces';

function jsonResponse(payload: unknown): Response {
  return new Response(JSON.stringify(payload), {
    status: 200,
    headers: { 'Content-Type': 'application/json' },
  });
}

describe('trace graph client', () => {
  it('treats HTTP 404 as a truthful unavailable result, not a graph', async () => {
    const urls: string[] = [];
    const client = new TracesClient({
      fetchImpl: (async (input: RequestInfo | URL) => {
        urls.push(String(input));
        return new Response('{}', { status: 404 });
      }) as typeof fetch,
    });

    const result = await client.fetchGraph('abc def');
    expect(urls).toEqual(['/lattice/api/v2/traces/abc%20def/graph']);
    expect(result.status).toBe('unavailable');
    if (result.status === 'unavailable') {
      expect(result.httpStatus).toBe(404);
      expect(result.reason).toContain('HTTP 404');
      expect(result.reason).toContain('/graph');
    }
  });

  it('reads a schema-2 graph and throws for any other schema', async () => {
    const client = new TracesClient({
      fetchImpl: (async () =>
        jsonResponse({ schema: 2, trace_id: 'echoed', coverage: 'explicit', nodes: [], edges: [], events: [] })) as typeof fetch,
    });
    const result = await client.fetchGraph('echoed');
    expect(result.status).toBe('ok');
    if (result.status === 'ok') {
      expect(result.graph.traceId).toBe('echoed');
      expect(result.graph.coverage).toBe('explicit');
    }

    const legacy = new TracesClient({
      fetchImpl: (async () => jsonResponse({ schema: 1 })) as typeof fetch,
    });
    await expect(legacy.fetchGraph('legacy')).rejects.toThrow('unsupported schema 1');
  });
});

describe('trace graph normalization', () => {
  it('accepts snake_case producer fields and keeps evidence/edge semantics', () => {
    const graph = normalizeTraceGraph(
      {
        schema: 2,
        trace_id: 'trace-1',
        coverage: { level: 'explicit', detail: 'two producers agreed' },
        nodes: [
          {
            node_id: 'n-root',
            node_kind: 'trace',
            operation: 'turn.execute',
            component: 'nai-os.shell',
            status: 'ok',
            start_ts: 1_000,
            duration_ms: 500,
            evidence_refs: [1],
          },
          {
            span_id: 's-tool',
            node_kind: 'span',
            parent_span_id: 'n-root',
            operation: 'tool.exec read_file',
            component: 'nai-os.worker',
            span_kind: 'client',
            status: 'error',
            durationMs: 200,
            evidence_seqs: [{ seq: 3 }, 4, 4],
          },
        ],
        edges: [
          {
            kind: 'call',
            source_span_id: 'n-root',
            target_span_id: 's-tool',
            operation: 'tool.call',
            transport: 'naf/rpc',
            latency_ms: 7,
            inferred: false,
            evidence_refs: [2],
          },
          {
            kind: 'async',
            from: 's-tool',
            to: 'n-root',
            inferred: true,
            evidence_seqs: [5],
          },
        ],
        events: [
          {
            runtime_sequence: 9,
            timestamp_utc_ms: 1_010,
            severity: 'warn',
            component: 'nai-os.worker',
            event_name: 'queue.wait',
            span_id: 's-tool',
          },
        ],
        diagnostics: ['edge duplicate suppressed', { code: 'missing-parent', severity: 'warn', message: 'parent not retained' }],
      },
      'fallback',
    );

    expect(graph.traceId).toBe('trace-1');
    expect(graph.coverage).toBe('explicit');
    expect(graph.coverageDetail).toBe('two producers agreed');
    expect(graph.nodes).toHaveLength(2);
    expect(graph.nodes[0]).toMatchObject({
      id: 'n-root',
      kind: 'trace',
      operation: 'turn.execute',
      component: 'nai-os.shell',
      status: 'ok',
      startTs: 1_000,
      durationMs: 500,
      evidenceSeqs: [1],
    });
    expect(graph.nodes[1]).toMatchObject({
      id: 's-tool',
      kind: 'span',
      spanId: 's-tool',
      parentSpanId: 'n-root',
      spanKind: 'client',
      status: 'error',
      durationMs: 200,
      evidenceSeqs: [3, 4],
    });
    expect(graph.edges[0]).toMatchObject({
      kind: 'call',
      from: 'n-root',
      to: 's-tool',
      operation: 'tool.call',
      transport: 'naf/rpc',
      latencyMs: 7,
      inferred: false,
      evidenceSeqs: [2],
    });
    expect(graph.edges[1]).toMatchObject({ kind: 'async', inferred: true, evidenceSeqs: [5] });
    expect(graph.events[0]).toMatchObject({
      seq: 9,
      ts: 1_010,
      level: 'warn',
      component: 'nai-os.worker',
      message: 'queue.wait',
      spanId: 's-tool',
    });
    expect(graph.diagnostics).toHaveLength(2);
    expect(graph.diagnostics[0].message).toBe('edge duplicate suppressed');
    expect(graph.diagnostics[1]).toMatchObject({ code: 'missing-parent', severity: 'warn' });
  });

  it('reads the live host schema: span:<id> nodes, startOffsetMs and evidenceSequences', () => {
    const graph = normalizeTraceGraph({
      schema: 2,
      traceId: 'host-trace',
      coverage: 'spans',
      generatedAt: 1_764_700_000_020,
      truncated: false,
      nodes: [
        {
          id: 'trace:host-trace',
          kind: 'trace',
          label: 'host-trace',
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
          instanceId: 'inst-1',
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
          id: 'parent:trace:host-trace->span:s-root',
          source: 'trace:host-trace',
          target: 'span:s-root',
          kind: 'parent',
          operation: null,
          transport: null,
          durationMs: null,
          evidenceSequences: [7],
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
          evidenceSequences: [9],
          inferred: false,
        },
      ],
      events: [
        {
          sequence: 7,
          ts: 1_764_700_000_000,
          level: 'info',
          component: 'alpha',
          message: 'turn.submitted',
          spanId: 's-root',
        },
        {
          sequence: 8,
          ts: 1_764_700_000_010,
          level: 'error',
          component: 'beta',
          message: 'tool.exec',
          spanId: 's-child',
        },
        {
          sequence: 9,
          ts: 1_764_700_000_011,
          level: 'info',
          component: 'alpha',
          message: 'call observation',
          spanId: null,
        },
        {
          sequence: 10,
          ts: 1_764_700_000_012,
          level: 'warn',
          component: 'alpha',
          message: 'standalone point event',
          spanId: null,
        },
      ],
      diagnostics: [],
    });

    expect(graph.coverage).toBe('explicit');
    expect(graph.nodes[1]).toMatchObject({
      id: 'span:s-root',
      kind: 'span',
      spanId: 's-root',
      startOffsetMs: 0,
      startTs: 1_764_700_000_000,
      durationMs: 10,
      evidenceSeqs: [7],
    });
    expect(graph.nodes[2]).toMatchObject({
      id: 'span:s-child',
      spanId: 's-child',
      startOffsetMs: 10,
      startTs: 1_764_700_000_010,
      parentSpanId: 's-root',
    });
    expect(graph.edges[1]).toMatchObject({
      kind: 'call',
      from: 'span:s-root',
      to: 'span:s-child',
      operation: 'tool.call',
      transport: 'naf/rpc',
      latencyMs: 4,
      evidenceSeqs: [9],
    });
    expect(graph.events.map((event) => event.seq)).toEqual([7, 8, 9, 10]);
    expect(graph.events[2].level).toBe('info');

    expect(normalizeTraceGraph({ schema: 2, coverage: 'mixed', nodes: [], edges: [], events: [] }).coverage).toBe('partial');
  });

  it('collapses duplicate span nodes but keeps every evidence sequence', () => {
    const graph = normalizeTraceGraph({
      schema: 2,
      traceId: 'dupe',
      coverage: 'spans',
      nodes: [
        { id: 'span:s1', span_id: 's1', operation: 'tool.exec', durationMs: 5, evidenceSequences: [1, 2] },
        { id: 'span:s1', span_id: 's1', operation: 'tool.exec', durationMs: 9, evidenceSequences: [2, 3] },
      ],
      edges: [],
      events: [],
    });
    expect(graph.nodes).toHaveLength(1);
    expect(graph.nodes[0].durationMs).toBe(5);
    expect(graph.nodes[0].evidenceSeqs).toEqual([1, 2, 3]);
  });

  it('drops edge records without endpoints and nodes without identities', () => {
    const graph = normalizeTraceGraph({
      schema: 2,
      coverage: 'legacy',
      nodes: [{ node_kind: 'span', operation: 'anonymous' }],
      edges: [{ kind: 'call', from: 'a' }],
      events: [],
    });
    expect(graph.coverage).toBe('legacy');
    expect(graph.nodes).toHaveLength(0);
    expect(graph.edges).toHaveLength(0);
  });
});
