import { mount, flushPromises } from '@vue/test-utils';
import { nextTick } from 'vue';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import App from '../src/App.vue';
import { APP_CONTEXT_KEY, type LatticeAppContext } from '../src/app-context';
import LogTail from '../src/modules/log-tail/LogTail.vue';
import { Fdc3Adapter } from '../src/interop/adapter';
import { QueryClient } from '../src/query/client';
import { TracesClient } from '../src/query/traces';
import { InvestigationStore } from '../src/state/investigation';
import { TelemetryFeed, type EventSourceLike } from '../src/telemetry/feed';
import { SummaryStore } from '../src/telemetry/summary';
import type { LogRecord } from '../src/telemetry/types';
import TracesPanel from '../src/modules/traces/TracesPanel.vue';
import WorkspaceDock from '../src/shell/WorkspaceDock.vue';
import { WORKSPACE_STORAGE_KEY, WORKSPACE_VERSION } from '../src/workspace/schema';

class FakeEventSource implements EventSourceLike {
  static instances: FakeEventSource[] = [];
  onopen: ((event: Event) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  closed = false;
  private readonly listeners = new Map<string, (event: MessageEvent) => void>();

  constructor(readonly url: string) {
    FakeEventSource.instances.push(this);
  }

  addEventListener(type: string, listener: (event: MessageEvent) => void): void {
    this.listeners.set(type, listener);
  }

  close(): void {
    this.closed = true;
  }

  open(): void {
    this.onopen?.(new Event('open'));
  }

  emit(type: string, payload: unknown): void {
    this.listeners.get(type)?.({ data: JSON.stringify(payload), lastEventId: '' } as MessageEvent);
  }
}

function record(sequence: number, message: string): LogRecord {
  return {
    runtime_sequence: sequence,
    timestamp_utc_ms: 1_764_000_000_000 + sequence,
    level: 'info',
    component: 'nai-os.observability',
    trace_id: 'trace-abcdef012345',
    message,
  };
}

const summaryPayload = {
  schema: 1,
  generatedAt: 1_764_000_000_000,
  window: 120,
  levels: { info: 118, warn: 1, error: 1 },
  errors: 2,
  components: { 'nai-os.observability': 118, 'nai-pilot': 2 },
  distinctTraceIds: 3,
  stream: {
    activeSseClients: 1,
    maxSseClients: 6,
    pushTotal: 40,
    subscribers: 1,
    brokerScansTotal: 40,
    lastScanUnixMs: 1_764_000_000_000,
    droppedTotal: 0,
  },
  health: {
    state: 'healthy',
    detail: null,
    lastError: null,
    sinceUnixMs: 1_764_000_000_000,
    uptimeMs: 90_000,
  },
  version: {
    host: '0.1.3',
    dashboard: '0.1.0',
    protocolMin: 1,
    protocolMax: 1,
    storageSchema: 1,
  },
  features: ['search_api', 'sse_resume'],
};

const emptyQuery = {
  schema: 1,
  hits: [],
  count: 0,
  nextAfter: null,
  hasMore: false,
  exhausted: true,
  scanned: 0,
  budgetHit: false,
  order: 'asc',
  generatedAt: 1_764_000_000_000,
};

const emptyTraces = { schema: 1, window: 0, traces: [], generatedAt: 1_764_000_000_000 };

/**
 * Passes the workspace schema validator but not Dockview: its deserializer
 * requires the grid root to be a `branch` node.
 */
const unrestorableWorkspace = {
  version: WORKSPACE_VERSION,
  savedAt: '2026-11-24T20:00:00.000Z',
  layout: {
    grid: {
      root: { type: 'leaf', data: [] },
      width: 1400,
      height: 800,
      orientation: 'HORIZONTAL',
    },
    panels: {
      'log-tail@1': {
        id: 'log-tail@1',
        contentComponent: 'module',
        title: 'Log Tail',
        params: { moduleType: 'log-tail', instanceId: 'log-tail@1' },
      },
    },
  },
};

function jsonResponse(payload: unknown, status = 200): Response {
  return new Response(JSON.stringify(payload), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

function makeContext(tracesPayload: unknown = emptyTraces): LatticeAppContext {
  FakeEventSource.instances = [];
  const fetchImpl = async (input: RequestInfo | URL): Promise<Response> => {
    const url = String(input);
    if (url.includes('/lattice/api/summary')) return jsonResponse(summaryPayload);
    if (url.includes('/lattice/api/traces')) return jsonResponse(tracesPayload);
    if (url.includes('/lattice/api/query')) return jsonResponse(emptyQuery);
    return jsonResponse({}, 404);
  };
  const query = new QueryClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
  return {
    feed: new TelemetryFeed({
      eventSourceFactory: (url) => new FakeEventSource(url),
      flushScheduler: (callback) => callback(),
      queryClient: query,
      now: () => 1_764_000_000_000,
    }),
    summary: new SummaryStore({ fetchImpl: fetchImpl as unknown as typeof fetch, intervalMs: 60_000 }),
    query,
    traces: new TracesClient({ fetchImpl: fetchImpl as unknown as typeof fetch }),
    interop: new Fdc3Adapter({ agent: null }),
    investigation: new InvestigationStore(),
  };
}

let contexts: LatticeAppContext[] = [];

function track(context: LatticeAppContext): LatticeAppContext {
  contexts.push(context);
  return context;
}

beforeEach(() => {
  contexts = [];
  document.body.innerHTML = '';
  window.localStorage.clear();
  window.history.replaceState({}, '', '/');
});

afterEach(() => {
  for (const context of contexts) {
    context.feed.dispose();
    context.summary.dispose();
    context.interop.dispose();
  }
  document.body.innerHTML = '';
  window.localStorage.clear();
  window.history.replaceState({}, '', '/');
});

describe('shell mount (jsdom)', () => {
  it('mounts the header, KPI ribbon and Dockview workspace', async () => {
    const context = track(makeContext());
    const wrapper = mount(App, { props: { context }, attachTo: document.body });
    await flushPromises();
    await nextTick();

    expect(wrapper.text()).toContain('NAI LATTICE');
    expect(wrapper.text()).toContain('OBSERVABILITY TERMINAL');
    expect(wrapper.text()).toContain('Live Log Lines');
    expect(wrapper.text()).toContain('Warn + Error');
    expect(wrapper.find('.lattice-workspace').exists()).toBe(true);
    expect(wrapper.find('.dockview-host').exists()).toBe(true);

    await flushPromises();
    expect(wrapper.text()).toContain('healthy');
    expect(wrapper.text()).toContain('FDC3: no desktop agent');

    const source = FakeEventSource.instances[0];
    expect(source).toBeDefined();
    source.open();
    source.emit('search', [record(1, 'shell-panel-row')]);
    await nextTick();
    await flushPromises();
    expect(wrapper.text()).toContain('shell-panel-row');
    expect(wrapper.text()).toContain('PAUSE');

    wrapper.unmount();
  });

  it('persists a real Dockview layout and restores it without resetting', async () => {
    const first = track(makeContext());
    const wrapper = mount(App, { props: { context: first }, attachTo: document.body });
    await flushPromises();
    await nextTick();

    const dock = wrapper.findComponent(WorkspaceDock);
    expect(dock.exists()).toBe(true);
    const exposed = dock.vm as unknown as { perform(action: string): void };
    exposed.perform('duplicate');
    await flushPromises();

    await vi.waitFor(
      () => {
        expect(window.localStorage.getItem(WORKSPACE_STORAGE_KEY)).not.toBeNull();
      },
      { timeout: 3000, interval: 50 },
    );
    const raw = window.localStorage.getItem(WORKSPACE_STORAGE_KEY) ?? '';
    expect(raw).toContain(`"version":${WORKSPACE_VERSION}`);
    expect(raw).toContain('"contentComponent":"module"');
    expect(raw).toContain('"params":{"moduleType"');
    wrapper.unmount();

    const second = track(makeContext());
    const restored = mount(App, { props: { context: second }, attachTo: document.body });
    await flushPromises();
    await nextTick();
    const text = restored.text();
    expect(text).not.toContain('canonical layout');
    expect(text).not.toContain('could not be restored');
    expect(restored.findComponent(WorkspaceDock).exists()).toBe(true);
    expect(text).toContain('Log Tail');
    restored.unmount();
  });

  it('migrates a v1 workspace with a visible notice and persists the canonical layout', async () => {
    window.localStorage.setItem(
      WORKSPACE_STORAGE_KEY,
      JSON.stringify({
        version: 1,
        savedAt: '2026-11-24T20:00:00.000Z',
        layout: {
          grid: {
            root: { type: 'branch', data: [] },
            width: 1400,
            height: 800,
            orientation: 'HORIZONTAL',
          },
          panels: {
            'log-tail@1': {
              id: 'log-tail@1',
              contentComponent: 'module',
              title: 'Log Tail',
              params: { moduleType: 'log-tail', instanceId: 'log-tail@1' },
            },
          },
        },
      }),
    );
    const context = track(makeContext());
    const wrapper = mount(App, { props: { context }, attachTo: document.body });
    await flushPromises();
    await nextTick();

    expect(wrapper.text()).toContain('migrated');
    expect(wrapper.text()).toContain(`version ${WORKSPACE_VERSION}`);
    const raw = window.localStorage.getItem(WORKSPACE_STORAGE_KEY);
    expect(raw).not.toBeNull();
    const persisted = JSON.parse(raw ?? '{}') as {
      version: number;
      layout: { panels: Record<string, unknown> };
    };
    expect(persisted.version).toBe(WORKSPACE_VERSION);
    expect(Object.keys(persisted.layout.panels).sort()).toEqual([
      'components@1',
      'errors@1',
      'execution-chain@1',
      'health@1',
      'host@1',
      'log-tail@1',
      'overview@1',
      'storage@1',
      'topology@1',
      'traces@1',
    ]);
    wrapper.unmount();
  });

  it('clears a stored layout Dockview cannot restore and persists the canonical one', async () => {
    window.localStorage.setItem(WORKSPACE_STORAGE_KEY, JSON.stringify(unrestorableWorkspace));
    const context = track(makeContext());
    const wrapper = mount(App, { props: { context }, attachTo: document.body });
    await flushPromises();
    await nextTick();

    expect(wrapper.text()).toContain('could not be restored');

    const raw = window.localStorage.getItem(WORKSPACE_STORAGE_KEY);
    expect(raw).not.toBeNull();
    expect(raw).not.toContain('"root":{"type":"leaf"');
    const persisted = JSON.parse(raw ?? '{}') as {
      version: number;
      layout: { grid: { root: { type: string } }; panels: Record<string, unknown> };
    };
    expect(persisted.version).toBe(WORKSPACE_VERSION);
    expect(persisted.layout.grid.root.type).toBe('branch');
    expect(Object.keys(persisted.layout.panels).sort()).toEqual([
      'components@1',
      'errors@1',
      'execution-chain@1',
      'health@1',
      'host@1',
      'log-tail@1',
      'overview@1',
      'storage@1',
      'topology@1',
      'traces@1',
    ]);
    wrapper.unmount();

    // The next reload must restore the canonical layout instead of resetting again.
    const second = track(makeContext());
    const restored = mount(App, { props: { context: second }, attachTo: document.body });
    await flushPromises();
    await nextTick();
    expect(restored.text()).not.toContain('could not be restored');
    expect(restored.text()).not.toContain('canonical layout');
    expect(restored.findComponent(WorkspaceDock).exists()).toBe(true);
    restored.unmount();
  });

  it('removes shell chrome in embed mode', async () => {
    window.history.replaceState({}, '', '/lattice/?embed=1');
    const context = track(makeContext());
    const wrapper = mount(App, { props: { context }, attachTo: document.body });
    await flushPromises();
    await nextTick();
    expect(wrapper.find('.shell-header').exists()).toBe(false);
    expect(wrapper.find('.kpi-ribbon').exists()).toBe(false);
    expect(wrapper.find('.lattice-workspace').exists()).toBe(true);
    expect(wrapper.find('.dockview-host').exists()).toBe(true);
    wrapper.unmount();
  });

  it('renders live log rows and freezes them for pause/resume/clear', async () => {
    const context = track(makeContext());
    context.feed.start();
    const source = FakeEventSource.instances[0];
    source.open();

    const wrapper = mount(LogTail, {
      props: { instanceId: 'log-tail@test' },
      global: { provide: { [APP_CONTEXT_KEY]: context } },
      attachTo: document.body,
    });
    await nextTick();

    source.emit('search', [record(1, 'first-live-row')]);
    await nextTick();
    expect(wrapper.text()).toContain('first-live-row');

    const button = (label: string) =>
      wrapper.findAll('button').find((candidate) => candidate.text() === label);
    await button('PAUSE')?.trigger('click');
    expect(wrapper.text()).toContain('VIEW FROZEN');

    source.emit('log', { hits: [record(2, 'while-paused-row')] });
    await nextTick();
    expect(wrapper.text()).not.toContain('while-paused-row');

    await button('RESUME')?.trigger('click');
    await flushPromises();
    await nextTick();
    expect(wrapper.text()).toContain('while-paused-row');

    await button('CLEAR')?.trigger('click');
    await nextTick();
    expect(wrapper.text()).not.toContain('while-paused-row');

    source.emit('log', { hits: [record(3, 'after-clear-row')] });
    await nextTick();
    expect(wrapper.text()).toContain('after-clear-row');

    wrapper.unmount();
  });

  it('never paints an open trace green and treats a null verdict as unknown', async () => {
    const context = track(
      makeContext({
        schema: 1,
        window: 60,
        generatedAt: 1_764_000_000_000,
        traces: [
          {
            traceId: 'open-ok',
            state: 'open',
            ok: true,
            terminal: null,
            spans: [],
            firstTs: 4000,
            lastTs: null,
            durationMs: null,
          },
          {
            traceId: 'open-null',
            state: 'open',
            ok: null,
            terminal: null,
            spans: [],
            firstTs: 3000,
            lastTs: null,
            durationMs: null,
          },
          {
            traceId: 'failed',
            state: 'closed',
            ok: false,
            terminal: 'error',
            spans: [],
            firstTs: 2000,
            lastTs: null,
            durationMs: null,
          },
          {
            traceId: 'unknown',
            state: 'closed',
            ok: null,
            terminal: null,
            spans: [],
            firstTs: 1000,
            lastTs: null,
            durationMs: null,
          },
        ],
      }),
    );
    const wrapper = mount(TracesPanel, {
      global: { provide: { [APP_CONTEXT_KEY]: context } },
      attachTo: document.body,
    });
    await flushPromises();
    await nextTick();

    const toneOf = (traceId: string): string => {
      const row = wrapper.findAll('.trace-row').find((candidate) => candidate.text().includes(traceId));
      expect(row).toBeDefined();
      const state = row?.find('.trace-state');
      expect(state).toBeDefined();
      return state?.classes().find((name) => name.startsWith('tone--')) ?? '';
    };

    expect(toneOf('open-ok')).toBe('tone--warn');
    expect(toneOf('open-null')).toBe('tone--warn');
    expect(toneOf('failed')).toBe('tone--bad');
    expect(toneOf('unknown')).toBe('tone--muted');
    wrapper.unmount();
  });
});
