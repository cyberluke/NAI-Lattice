import { describe, expect, it } from 'vitest';
import type { QueryFilters, QueryPage } from '../src/query/client';
import {
  DiscoverSession,
  buildDiscoverServerFilters,
  mergeFacetComponents,
  recordMatchesFacets,
  serverLevelFor,
  type DiscoverState,
} from '../src/query/discover';
import type { LogRecord } from '../src/telemetry/types';

function record(
  sequence: number,
  level: string,
  component: string,
  message = `row-${sequence}`,
): LogRecord {
  return {
    runtime_sequence: sequence,
    timestamp_utc_ms: 1000 + sequence,
    level,
    component,
    trace_id: null,
    message,
  };
}

function page(
  hits: LogRecord[],
  nextAfter: number | null,
  hasMore: boolean,
  scanned: number,
): QueryPage {
  return {
    schema: 1,
    hits,
    count: hits.length,
    nextAfter,
    hasMore,
    exhausted: !hasMore,
    scanned,
    budgetHit: false,
    order: 'asc',
    generatedAt: 1000,
  };
}

describe('discover facets', () => {
  it('uses the minimum selected level for the server and exact matches client-side', () => {
    expect(serverLevelFor([])).toBeNull();
    expect(serverLevelFor(['trace', 'error'])).toBe('trace');
    expect(serverLevelFor(['warn', 'error'])).toBe('warn');
    expect(serverLevelFor(['bogus'])).toBeNull();

    expect(
      recordMatchesFacets(record(1, 'warn', 'alpha'), { levels: ['warn', 'error'], components: [] }),
    ).toBe(true);
    expect(
      recordMatchesFacets(record(2, 'info', 'alpha'), { levels: ['warn', 'error'], components: [] }),
    ).toBe(false);
    expect(
      recordMatchesFacets(record(3, 'error', 'beta'), { levels: [], components: ['alpha'] }),
    ).toBe(false);
    expect(recordMatchesFacets(record(4, 'error', 'beta'), { levels: [], components: [] })).toBe(true);
  });

  it('pushes a single component to the server but filters a multi-component OR client-side', () => {
    expect(buildDiscoverServerFilters({ levels: ['error'], components: ['alpha'] }, 10, 100)).toEqual({
      level: 'error',
      component: 'alpha',
      after: 10,
      limit: 100,
    });
    expect(
      buildDiscoverServerFilters({ levels: [], components: ['alpha', 'beta'] }, 0, 50),
    ).toEqual({ after: 0, limit: 50 });
  });

  it('unions summary and loaded-result component facets with real counts', () => {
    const facets = mergeFacetComponents(
      { alpha: 10, gamma: 4 },
      [record(1, 'info', 'alpha'), record(2, 'info', 'beta'), record(3, 'info', 'alpha')],
    );
    expect(facets).toEqual([
      { name: 'alpha', loaded: 2, summary: 10 },
      { name: 'beta', loaded: 1, summary: null },
      { name: 'gamma', loaded: 0, summary: 4 },
    ]);
  });
});

describe('DiscoverSession paging', () => {
  it('follows after-pages, applies exact facets and reports the scanned window', async () => {
    const calls: QueryFilters[] = [];
    const client = {
      async fetchPage(filters: QueryFilters): Promise<QueryPage> {
        calls.push(filters);
        const after = filters.after ?? 0;
        if (after === 0) {
          return page(
            [
              record(1, 'info', 'alpha'),
              record(2, 'error', 'alpha'),
            ],
            2,
            true,
            7,
          );
        }
        if (after === 2) {
          return page(
            [
              record(3, 'warn', 'beta'),
              record(4, 'error', 'beta'),
            ],
            4,
            false,
            5,
          );
        }
        return page([], null, false, 0);
      },
    };
    let state: DiscoverState | null = null;
    const session = new DiscoverSession(
      client,
      (next) => {
        state = next;
      },
      { pageLimit: 5, maxPagesPerLoad: 4 },
    );

    await session.start({ levels: ['error'], components: [] });
    expect(calls[0]).toMatchObject({ level: 'error', after: 0, limit: 5 });
    expect(state!.rows.map((row) => row.runtime_sequence)).toEqual([2, 4]);
    expect(state!.scanned).toBe(12);
    expect(state!.scanStart).toBe(0);
    expect(state!.scanEnd).toBe(4);
    expect(state!.pages).toBe(2);
    expect(state!.hasMore).toBe(false);
    expect(state!.exhausted).toBe(true);
    expect(state!.truncated).toBe(false);
    session.dispose();
  });

  it('keeps a progress cursor for load more after a bounded page of matches', async () => {
    const calls: QueryFilters[] = [];
    const client = {
      async fetchPage(filters: QueryFilters): Promise<QueryPage> {
        calls.push(filters);
        const after = filters.after ?? 0;
        if (after === 0) {
          return page(
            [
              record(1, 'info', 'alpha'),
              record(2, 'error', 'alpha'),
              record(3, 'error', 'alpha'),
            ],
            3,
            true,
            9,
          );
        }
        if (after === 3) {
          return page([record(4, 'error', 'beta')], 4, true, 6);
        }
        return page([record(5, 'error', 'beta')], 5, false, 4);
      },
    };
    let state: DiscoverState | null = null;
    const session = new DiscoverSession(
      client,
      (next) => {
        state = next;
      },
      { pageLimit: 1, maxPagesPerLoad: 3 },
    );

    await session.start({ levels: ['error'], components: ['alpha', 'beta'] });
    // A page is delivered whole; the cursor cannot advance into the middle of it.
    expect(state!.rows.map((row) => row.runtime_sequence)).toEqual([2, 3]);
    expect(state!.hasMore).toBe(true);
    expect(state!.nextAfter).toBe(3);

    await session.loadMore();
    expect(calls.at(-1)).toMatchObject({ after: 3, limit: 1 });
    expect(state!.rows.map((row) => row.runtime_sequence)).toEqual([2, 3, 4]);
    expect(state!.scanned).toBe(15);
    session.dispose();
  });

  it('aborts a filter change without publishing the stale page', async () => {
    const pending: Array<{
      resolve: (value: QueryPage) => void;
      signal: AbortSignal | undefined;
    }> = [];
    const client = {
      fetchPage(_filters: QueryFilters, signal?: AbortSignal): Promise<QueryPage> {
        return new Promise((resolve) => pending.push({ resolve, signal }));
      },
    };
    let state: DiscoverState | null = null;
    const session = new DiscoverSession(client, (next) => {
      state = next;
    });

    const first = session.start({ levels: ['error'], components: [] });
    await Promise.resolve();
    expect(pending).toHaveLength(1);
    const second = session.start({ levels: ['info'], components: [] });
    await Promise.resolve();
    expect(pending[0].signal?.aborted).toBe(true);
    expect(pending).toHaveLength(2);

    pending[0].resolve(page([record(99, 'error', 'alpha')], null, false, 1));
    pending[1].resolve(page([record(5, 'info', 'alpha')], null, false, 1));
    await first;
    await second;
    expect(state!.rows.map((row) => row.runtime_sequence)).toEqual([5]);
    session.dispose();
  });
});
