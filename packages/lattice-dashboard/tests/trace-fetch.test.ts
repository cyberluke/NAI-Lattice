import { flushPromises, mount } from '@vue/test-utils';
import { nextTick } from 'vue';
import { describe, expect, it, vi } from 'vitest';
import { APP_CONTEXT_KEY, type LatticeAppContext } from '../src/app-context';
import TracesPanel from '../src/modules/traces/TracesPanel.vue';
import { TracesClient } from '../src/query/traces';
import { InvestigationStore } from '../src/state/investigation';

const WINDOW_TRACE = {
  traceId: 'known-trace',
  firstTs: 1_764_000_000_000,
  lastTs: 1_764_000_000_100,
  durationMs: 100,
  state: 'closed',
  terminal: 'completed',
  ok: true,
  spans: [],
};

const OLDER_TRACE = {
  traceId: 'older-trace-1',
  firstTs: 1_764_000_000_000,
  lastTs: 1_764_000_000_500,
  durationMs: 500,
  state: 'closed',
  terminal: 'completed',
  ok: true,
  spans: [
    {
      ts: 1_764_000_000_010,
      kind: 'request',
      label: 'out-of-window span',
      runtime_sequence: 7,
      durationMs: null,
    },
  ],
};

function jsonResponse(payload: unknown): Response {
  return new Response(JSON.stringify(payload), {
    status: 200,
    headers: { 'Content-Type': 'application/json' },
  });
}

function windowPayload() {
  return {
    schema: 1,
    window: 20,
    traces: [WINDOW_TRACE],
    generatedAt: 1_764_000_000_000,
  };
}

function makeContext(fetchImpl: typeof fetch): {
  context: LatticeAppContext;
  investigation: InvestigationStore;
  urls: string[];
} {
  const urls: string[] = [];
  const recording = (async (input: RequestInfo | URL, init?: RequestInit) => {
    urls.push(String(input));
    return fetchImpl(input, init);
  }) as typeof fetch;
  const investigation = new InvestigationStore();
  return {
    investigation,
    urls,
    context: {
      traces: new TracesClient({ fetchImpl: recording }),
      investigation,
    } as unknown as LatticeAppContext,
  };
}

function mountPanel(context: LatticeAppContext) {
  return mount(TracesPanel, {
    global: { provide: { [APP_CONTEXT_KEY]: context } },
  });
}

describe('TracesClient.fetchTrace', () => {
  it('requests ?trace= with encoding and exposes the echo and truncated flag', async () => {
    const urls: string[] = [];
    const client = new TracesClient({
      fetchImpl: (async (input: RequestInfo | URL) => {
        urls.push(String(input));
        return jsonResponse({
          schema: 1,
          window: null,
          traces: [{ ...OLDER_TRACE, traceId: 'abc def' }],
          generatedAt: 1_764_000_000_000,
          trace: 'abc def',
          truncated: true,
        });
      }) as typeof fetch,
    });

    const response = await client.fetchTrace('abc def');
    expect(urls).toEqual(['/lattice/api/traces?trace=abc%20def']);
    expect(response.trace).toBe('abc def');
    expect(response.truncated).toBe(true);

    await client.fetch();
    expect(urls[1]).toBe('/lattice/api/traces');
  });
});

describe('TracesPanel focused trace resolution', () => {
  it('fetches and renders a trace that is older than the current window', async () => {
    const fetchImpl = (async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.endsWith('/graph')) {
        // This fixture host predates the schema-2 graph endpoint.
        return new Response('{}', { status: 404 });
      }
      if (url.includes('?trace=')) {
        return jsonResponse({
          schema: 1,
          window: null,
          traces: [OLDER_TRACE],
          generatedAt: 1_764_000_000_000,
          trace: 'older-trace-1',
          truncated: true,
        });
      }
      return jsonResponse(windowPayload());
    }) as typeof fetch;
    const { context, investigation, urls } = makeContext(fetchImpl);
    // A deep link or an FDC3 InspectTrace arrives before the panel mounts.
    investigation.setTrace('older-trace-1');

    const wrapper = mountPanel(context);
    await flushPromises();
    await nextTick();

    await vi.waitFor(() => {
      expect(wrapper.text()).toContain('out-of-window span');
    });
    expect(urls).toContain('/lattice/api/traces?trace=older-trace-1');
    expect(wrapper.findAll('.trace-row')).toHaveLength(1);
    expect(wrapper.text()).toContain('resolved outside the current window');
    expect(wrapper.text()).toContain('search was bounded');
    wrapper.unmount();
  });

  it('shows an explicit not-found state for an empty bounded lookup', async () => {
    const fetchImpl = (async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.endsWith('/graph')) {
        return new Response('{}', { status: 404 });
      }
      if (url.includes('?trace=')) {
        return jsonResponse({
          schema: 1,
          window: null,
          traces: [],
          generatedAt: 1_764_000_000_000,
          trace: 'ghost-trace',
          truncated: true,
        });
      }
      return jsonResponse(windowPayload());
    }) as typeof fetch;
    const { context, investigation } = makeContext(fetchImpl);
    investigation.setTrace('ghost-trace');

    const wrapper = mountPanel(context);
    await flushPromises();
    await nextTick();

    await vi.waitFor(() => {
      expect(wrapper.text()).toContain('trace not found in the ledger');
    });
    expect(wrapper.findAll('.trace-row')).toHaveLength(0);
    expect(wrapper.text()).toContain('search was bounded');
    wrapper.unmount();
  });
});
