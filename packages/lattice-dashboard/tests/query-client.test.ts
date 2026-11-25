import { afterEach, describe, expect, it, vi } from 'vitest';
import { QueryClient, QuerySession, type QuerySessionState } from '../src/query/client';
import type { LogRecord } from '../src/telemetry/types';

function record(sequence: number): LogRecord {
  return {
    runtime_sequence: sequence,
    timestamp_utc_ms: 1_764_000_000_000 + sequence,
    level: 'info',
    component: 'test',
    trace_id: null,
    message: `row-${sequence}`,
  };
}

function page(hits: LogRecord[], nextAfter: number | null, hasMore: boolean) {
  return {
    schema: 1,
    hits,
    count: hits.length,
    nextAfter,
    hasMore,
    exhausted: !hasMore,
    scanned: 10,
    budgetHit: false,
    order: 'asc',
    generatedAt: 1_764_000_000_000,
  };
}

function response(payload: unknown): Response {
  return new Response(JSON.stringify(payload), {
    status: 200,
    headers: { 'Content-Type': 'application/json' },
  });
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe('QueryClient / QuerySession', () => {
  it('pages with after and never duplicates rows', async () => {
    const urls: string[] = [];
    const fetchImpl = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      urls.push(url);
      const after = Number(new URL(url, 'http://localhost').searchParams.get('after') ?? '0');
      if (after === 0) return response(page([record(1), record(2)], 2, true));
      return response(page([record(2), record(3)], 3, false));
    });
    const client = new QueryClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
    let state: QuerySessionState | null = null;
    const session = new QuerySession(client, (next) => (state = next), { pageLimit: 2, maxRows: 100 });

    await session.start({ q: 'needle' });
    expect(urls[0]).toContain('/lattice/api/query?');
    expect(urls[0]).toContain('q=needle');
    expect(urls[0]).toContain('limit=2');
    expect(state!.rows.map((row) => row.runtime_sequence)).toEqual([1, 2]);
    expect(state!.hasMore).toBe(true);

    await session.loadMore();
    expect(urls[1]).toContain('after=2');
    expect(state!.rows.map((row) => row.runtime_sequence)).toEqual([1, 2, 3]);
    expect(state!.hasMore).toBe(false);
    session.dispose();
  });

  it('aborts the previous request on filter change and ignores the stale page', async () => {
    const deferred: Array<{
      resolve: (value: Response) => void;
      signal: AbortSignal | undefined;
    }> = [];
    const fetchImpl = vi.fn((_input: RequestInfo | URL, init?: RequestInit) => {
      return new Promise<Response>((resolve) => {
        deferred.push({ resolve, signal: init?.signal ?? undefined });
      });
    });
    const client = new QueryClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
    let state: QuerySessionState | null = null;
    const session = new QuerySession(client, (next) => (state = next));

    const first = session.start({ component: 'alpha' });
    await Promise.resolve();
    expect(deferred).toHaveLength(1);

    const second = session.start({ component: 'beta' });
    await Promise.resolve();
    expect(deferred).toHaveLength(2);
    expect(deferred[0].signal?.aborted).toBe(true);

    deferred[0].resolve(response(page([record(99)], 99, false)));
    deferred[1].resolve(response(page([record(5)], 5, false)));
    await first;
    await second;

    expect(state!.rows.map((row) => row.runtime_sequence)).toEqual([5]);
    expect(state!.error).toBeNull();
    session.dispose();
  });

  it('reports HTTP errors without throwing out of the session', async () => {
    const fetchImpl = vi.fn(async () => new Response('nope', { status: 503 }));
    const client = new QueryClient({ fetchImpl: fetchImpl as unknown as typeof fetch });
    let state: QuerySessionState | null = null;
    const session = new QuerySession(client, (next) => (state = next));
    await session.start({ q: 'x' });
    expect(state!.error).toContain('HTTP 503');
    expect(state!.loading).toBe(false);
    session.dispose();
  });
});
