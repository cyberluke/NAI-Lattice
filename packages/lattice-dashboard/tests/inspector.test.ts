import { flushPromises, mount } from '@vue/test-utils';
import { nextTick } from 'vue';
import { afterEach, describe, expect, it } from 'vitest';
import LogTail from '../src/modules/log-tail/LogTail.vue';
import { APP_CONTEXT_KEY, type LatticeAppContext } from '../src/app-context';
import { Fdc3Adapter } from '../src/interop/adapter';
import { QueryClient } from '../src/query/client';
import { TracesClient } from '../src/query/traces';
import { InvestigationStore } from '../src/state/investigation';
import { TelemetryFeed, type EventSourceLike } from '../src/telemetry/feed';
import type { LogRecord } from '../src/telemetry/types';

class FakeEventSource implements EventSourceLike {
  onopen: ((event: Event) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  private readonly listeners = new Map<string, (event: MessageEvent) => void>();

  constructor() {
    FakeEventSource.last = this;
  }

  static last: FakeEventSource | null = null;

  addEventListener(type: string, listener: (event: MessageEvent) => void): void {
    this.listeners.set(type, listener);
  }

  close(): void {
    /* no-op */
  }

  open(): void {
    this.onopen?.(new Event('open'));
  }

  emit(type: string, payload: unknown): void {
    this.listeners.get(type)?.({ data: JSON.stringify(payload), lastEventId: '' } as MessageEvent);
  }
}

function jsonResponse(payload: unknown): Response {
  return new Response(JSON.stringify(payload), {
    status: 200,
    headers: { 'Content-Type': 'application/json' },
  });
}

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

function makeContext(): LatticeAppContext {
  FakeEventSource.last = null;
  const fetchImpl = (async (input: RequestInfo | URL): Promise<Response> => {
    if (String(input).includes('/lattice/api/traces')) {
      return jsonResponse({ schema: 1, window: 0, traces: [], generatedAt: 0 });
    }
    return jsonResponse(emptyQuery);
  }) as typeof fetch;
  return {
    feed: new TelemetryFeed({
      eventSourceFactory: () => new FakeEventSource(),
      flushScheduler: (callback) => callback(),
      now: () => 1_764_000_000_000,
    }),
    query: new QueryClient({ fetchImpl }),
    traces: new TracesClient({ fetchImpl }),
    interop: new Fdc3Adapter({ agent: null }),
    investigation: new InvestigationStore(),
  } as unknown as LatticeAppContext;
}

function record(sequence: number): LogRecord {
  return {
    runtime_sequence: sequence,
    timestamp_utc_ms: 1_764_000_000_000 + sequence,
    level: 'error',
    component: 'alpha.component',
    trace_id: 'trace-abcdef0123456789',
    caller: 'alpha.rs:42',
    message: 'request failed with code 500',
    extras: { event_type: 'log', nested: { ok: false } },
    payload: { ts_ms: 1_764_000_000_000 + sequence, detail: 'raw payload' },
  };
}

let contexts: LatticeAppContext[] = [];

afterEach(() => {
  for (const context of contexts) {
    context.feed.dispose();
    context.interop.dispose();
  }
  contexts = [];
  document.body.innerHTML = '';
});

describe('record inspector', () => {
  it('opens from a Log Tail row with every raw field and Escape closes it', async () => {
    const context = makeContext();
    contexts.push(context);
    context.feed.start();
    const wrapper = mount(LogTail, {
      global: { provide: { [APP_CONTEXT_KEY]: context } },
      attachTo: document.body,
    });
    await nextTick();

    const source = FakeEventSource.last;
    expect(source).not.toBeNull();
    source!.open();
    source!.emit('search', [record(7)]);
    await nextTick();
    await flushPromises();

    expect(wrapper.find('.record-inspector').exists()).toBe(false);
    await wrapper.find('.log-row').trigger('click');
    await nextTick();

    const inspector = wrapper.find('.record-inspector');
    expect(inspector.exists()).toBe(true);
    const text = inspector.text();
    expect(text).toContain('runtime_sequence');
    expect(text).toContain('7');
    expect(text).toContain('alpha.component');
    expect(text).toContain('trace-abcdef0123456789');
    expect(text).toContain('alpha.rs:42');
    expect(text).toContain('request failed with code 500');
    expect(text).toContain('event_type');
    expect(text).toContain('raw payload');
    expect(text).toContain('FOCUS TRACE');
    expect(inspector.html()).not.toContain('v-html');

    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
    await nextTick();
    expect(wrapper.find('.record-inspector').exists()).toBe(false);
    wrapper.unmount();
  });

  it('focuses the traces panel from the inspector action', async () => {
    const context = makeContext();
    contexts.push(context);
    context.feed.start();
    const requestSpy: string[] = [];
    const originalRequest = context.investigation.requestPanel.bind(context.investigation);
    context.investigation.requestPanel = (panel) => {
      requestSpy.push(panel);
      originalRequest(panel);
    };

    const wrapper = mount(LogTail, {
      global: { provide: { [APP_CONTEXT_KEY]: context } },
      attachTo: document.body,
    });
    await nextTick();
    FakeEventSource.last!.open();
    FakeEventSource.last!.emit('search', [record(9)]);
    await nextTick();
    await flushPromises();

    await wrapper.find('.log-row').trigger('click');
    await nextTick();
    const focus = wrapper
      .findAll('button')
      .find((candidate) => candidate.text() === 'FOCUS TRACE');
    expect(focus).toBeDefined();
    await focus!.trigger('click');
    expect(context.investigation.trace.value).toBe('trace-abcdef0123456789');
    expect(requestSpy).toContain('traces');
    expect(wrapper.find('.record-inspector').exists()).toBe(false);
    wrapper.unmount();
  });

  it('shows no payload section when the record has none', async () => {
    const context = makeContext();
    contexts.push(context);
    context.feed.start();
    const wrapper = mount(LogTail, {
      global: { provide: { [APP_CONTEXT_KEY]: context } },
      attachTo: document.body,
    });
    await nextTick();

    const plain: LogRecord = { ...record(11) };
    delete plain.payload;
    FakeEventSource.last!.open();
    FakeEventSource.last!.emit('search', [plain]);
    await nextTick();
    await flushPromises();

    await wrapper.find('.log-row').trigger('click');
    await nextTick();
    expect(wrapper.find('.record-inspector').text()).not.toContain('payload');
    wrapper.unmount();
  });
});
