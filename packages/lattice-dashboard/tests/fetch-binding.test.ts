import { afterEach, describe, expect, it } from 'vitest';
import { QueryClient } from '../src/query/client';
import { TracesClient } from '../src/query/traces';
import { SummaryStore } from '../src/telemetry/summary';

/**
 * Regression: extracting `globalThis.fetch` into a field and calling it as
 * `this.fetchImpl(...)` loses the Window receiver. Chrome/WebView2 throw
 * "Illegal invocation"; jsdom/Node do not, so the unit suite missed it until
 * the first real-browser run. The clients must bind the receiver at capture.
 */
const originalFetch = globalThis.fetch;

function strictFetch(payload: unknown): typeof fetch {
  return function strict(this: unknown): Promise<Response> {
    if (this !== globalThis) {
      throw new TypeError("Failed to execute 'fetch' on 'Window': Illegal invocation");
    }
    return Promise.resolve(
      new Response(JSON.stringify(payload), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      }),
    );
  } as unknown as typeof fetch;
}

const summaryPayload = {
  schema: 1,
  generatedAt: 1_764_000_000_000,
  window: 0,
  levels: {},
  errors: 0,
  components: {},
  distinctTraceIds: 0,
  stream: {
    activeSseClients: 0,
    maxSseClients: 24,
    pushTotal: 0,
    subscribers: 0,
    brokerScansTotal: 0,
    lastScanUnixMs: null,
    droppedTotal: 0,
  },
  health: { state: 'healthy', detail: null, lastError: null, sinceUnixMs: 0, uptimeMs: 0 },
  version: {
    host: '0.1.3',
    dashboard: '0.1.0',
    protocolMin: 1,
    protocolMax: 1,
    storageSchema: 1,
  },
  features: ['storage_api'],
  storage: {
    schema: 1,
    root: 'C:\\qa\\v1',
    base: 'C:\\qa',
    budgetBytes: 1,
    usedBytes: 0,
    freeBytes: 2,
    reserveBytes: 1,
    pressure: 'normal',
    classes: {},
    retention: { logsDays: 30, auditDays: 90, backupDays: 14, runtimeKeep: 2 },
    lastPrune: null,
  },
};

afterEach(() => {
  globalThis.fetch = originalFetch;
});

describe('global fetch binding', () => {
  it('the query client keeps the Window receiver', async () => {
    globalThis.fetch = strictFetch({
      schema: 1,
      hits: [],
      count: 0,
      nextAfter: null,
      hasMore: false,
      exhausted: true,
      scanned: 0,
      budgetHit: false,
      order: 'asc',
      generatedAt: 0,
    });
    const page = await new QueryClient().fetchPage({});
    expect(page.count).toBe(0);
  });

  it('the traces client keeps the Window receiver', async () => {
    globalThis.fetch = strictFetch({ schema: 1, window: 0, traces: [], generatedAt: 0 });
    const response = await new TracesClient().fetch();
    expect(response.traces).toEqual([]);
  });

  it('the summary store keeps the Window receiver', async () => {
    globalThis.fetch = strictFetch(summaryPayload);
    const store = new SummaryStore({ intervalMs: 999_999 });
    await store.refresh(true);
    expect(store.error.value).toBeNull();
    expect(store.summary.value?.version?.host ?? null).toBe('0.1.3');
    store.stop();
  });
});
