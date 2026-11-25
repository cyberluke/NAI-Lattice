import { flushPromises, mount } from '@vue/test-utils';
import { describe, expect, it, vi } from 'vitest';
import { APP_CONTEXT_KEY, type LatticeAppContext } from '../src/app-context';
import ExecutionChain from '../src/modules/traces/ExecutionChain.vue';
import type { TraceGraph, TraceSummary } from '../src/query/traces';
import { MODULE_COMPONENTS } from '../src/shell/registry';
import { InvestigationStore } from '../src/state/investigation';
import { MODULE_TITLES } from '../src/workspace/schema';

const SUMMARY: TraceSummary = {
  traceId: 'trace-1',
  firstTs: 1_000,
  lastTs: 1_100,
  durationMs: 100,
  state: 'closed',
  terminal: 'completed',
  ok: true,
  spans: [],
};

const GRAPH: TraceGraph = {
  schema: 2,
  traceId: 'trace-1',
  coverage: 'explicit',
  coverageDetail: null,
  generatedAt: 1_100,
  nodes: [
    {
      id: 'trace:trace-1',
      kind: 'trace',
      traceId: 'trace-1',
      spanId: null,
      parentSpanId: null,
      component: null,
      instanceId: null,
      operation: 'trace-1',
      spanKind: null,
      status: 'ok',
      startTs: null,
      startOffsetMs: null,
      durationMs: null,
      reason: null,
      evidenceSeqs: [],
    },
    {
      id: 'span:s1',
      kind: 'span',
      traceId: 'trace-1',
      spanId: 's1',
      parentSpanId: null,
      component: 'nai-os.shell',
      instanceId: null,
      operation: 'turn.execute',
      spanKind: 'server',
      status: 'ok',
      startTs: 1_000,
      startOffsetMs: 0,
      durationMs: 100,
      reason: null,
      evidenceSeqs: [1],
    },
  ],
  edges: [
    {
      id: 'call:trace->s1',
      kind: 'call',
      from: 'trace:trace-1',
      to: 'span:s1',
      operation: 'turn.dispatch',
      transport: 'naf/rpc',
      latencyMs: 2,
      inferred: false,
      evidenceSeqs: [2],
    },
  ],
  events: [
    {
      id: 'ev-1',
      seq: 3,
      ts: 1_050,
      level: 'warn',
      component: 'nai-os.shell',
      message: 'point event',
      spanId: null,
    },
  ],
  diagnostics: [],
};

function makeContext(options: {
  graph?: TraceGraph;
  graphUnavailable?: boolean;
  withSummary?: boolean;
} = {}): { context: LatticeAppContext; investigation: InvestigationStore; fetchGraph: ReturnType<typeof vi.fn> } {
  const investigation = new InvestigationStore();
  const fetchGraph = vi.fn(async () =>
    options.graphUnavailable === true
      ? {
          status: 'unavailable' as const,
          httpStatus: 404,
          reason: 'HTTP 404 for this graph.',
        }
      : { status: 'ok' as const, graph: options.graph ?? GRAPH },
  );
  const fetchTrace = vi.fn(async () => ({
    schema: 1,
    window: null,
    traces: options.withSummary === false ? [] : [SUMMARY],
    generatedAt: 1_100,
  }));
  return {
    investigation,
    fetchGraph,
    context: {
      investigation,
      traces: { fetchGraph, fetchTrace },
    } as unknown as LatticeAppContext,
  };
}

describe('Execution Chain module', () => {
  it('is registered as a stable module type and title', () => {
    expect(MODULE_TITLES['execution-chain']).toBe('Execution Chain');
    expect(MODULE_COMPONENTS['execution-chain']).toBeDefined();
  });

  it('shows the exact empty state and does not query without a selected trace', async () => {
    const { context, fetchGraph } = makeContext();
    const wrapper = mount(ExecutionChain, {
      global: { provide: { [APP_CONTEXT_KEY]: context } },
    });
    await flushPromises();
    expect(wrapper.text()).toContain(
      'Select a trace in Request → Response to inspect its execution chain.',
    );
    expect(fetchGraph).not.toHaveBeenCalled();
    wrapper.unmount();
  });

  it('renders counts, the chain and the legacy fallback for the selected trace', async () => {
    const { context, investigation } = makeContext();
    investigation.setTrace('trace-1');
    const wrapper = mount(ExecutionChain, {
      global: { provide: { [APP_CONTEXT_KEY]: context } },
    });
    await flushPromises();

    expect(wrapper.text()).toContain('execution chain');
    const counts = wrapper.find('.trace-counts').text().replace(/\s+/g, ' ').trim();
    expect(counts).toBe('EVENTS 1 · SPANS 1 · CALLS 1');
    expect(wrapper.findAll('.chain-node')).toHaveLength(2);
    expect(wrapper.findAll('.chain-edge')).toHaveLength(1);
    wrapper.unmount();

    const unavailable = makeContext({ graphUnavailable: true });
    unavailable.investigation.setTrace('trace-1');
    const fallback = mount(ExecutionChain, {
      global: { provide: { [APP_CONTEXT_KEY]: unavailable.context } },
    });
    await flushPromises();
    expect(fallback.text()).toContain('CAUSAL CHAIN UNAVAILABLE — legacy correlation only');
    expect(fallback.findAll('.chain-node')).toHaveLength(0);
    fallback.unmount();
  });
});
