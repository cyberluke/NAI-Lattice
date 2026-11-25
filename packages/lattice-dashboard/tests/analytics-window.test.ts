import { describe, expect, it } from 'vitest';
import type { QueryFilters, QueryPage } from '../src/query/client';
import { loadVolumeWindow } from '../src/analytics/volume';
import { loadErrorWindow } from '../src/query/error-window';
import type { LogRecord } from '../src/telemetry/types';

function record(sequence: number, level = 'info'): LogRecord {
  return {
    runtime_sequence: sequence,
    timestamp_utc_ms: 1_000_000 + sequence,
    level,
    component: 'test',
    trace_id: null,
    message: `row-${sequence}`,
  };
}

function page(
  hits: LogRecord[],
  nextAfter: number | null,
  hasMore: boolean,
  scanned: number,
  exhausted = !hasMore,
): QueryPage {
  return {
    schema: 1,
    hits,
    count: hits.length,
    nextAfter,
    hasMore,
    exhausted,
    scanned,
    budgetHit: false,
    order: 'asc',
    generatedAt: 1_000_000,
  };
}

describe('bounded volume window', () => {
  it('starts a real sequence window behind the live cursor and stops at it', async () => {
    const calls: QueryFilters[] = [];
    const client = {
      async fetchPage(filters: QueryFilters): Promise<QueryPage> {
        calls.push(filters);
        if ((filters.after ?? 0) === 90) {
          return page([91, 92, 93, 94, 95].map((seq) => record(seq)), 95, true, 5);
        }
        return page([96, 97, 98, 99, 100].map((seq) => record(seq)), 100, false, 5, false);
      },
    };
    const window = await loadVolumeWindow(client, { maxRecords: 10, highWater: 100 });
    expect(calls[0]).toMatchObject({ after: 90, limit: 10 });
    expect(window.startAfter).toBe(90);
    expect(window.source).toBe('tail');
    expect(window.records.map((row) => row.runtime_sequence)).toEqual([
      91, 92, 93, 94, 95, 96, 97, 98, 99, 100,
    ]);
    expect(window.pages).toBe(2);
    expect(window.scanned).toBe(10);
    expect(window.complete).toBe(true);
    expect(window.truncated).toBe(false);
  });

  it('labels a cursor-less scan as retained-history start and honours the cap', async () => {
    const client = {
      async fetchPage(filters: QueryFilters): Promise<QueryPage> {
        expect(filters.after).toBe(0);
        return page([1, 2, 3].map((seq) => record(seq)), 3, true, 3);
      },
    };
    const window = await loadVolumeWindow(client, { maxRecords: 3, highWater: null });
    expect(window.source).toBe('retained-start');
    expect(window.records.map((row) => row.runtime_sequence)).toEqual([1, 2, 3]);
    expect(window.complete).toBe(false);
    expect(window.limit).toBe(3);
  });

  it('clamps the requested limit to the 5000-record cap', async () => {
    const client = {
      async fetchPage(): Promise<QueryPage> {
        return page([], null, false, 0);
      },
    };
    const window = await loadVolumeWindow(client, { maxRecords: 50_000, highWater: null });
    expect(window.limit).toBe(5000);
  });
});

describe('bounded error window', () => {
  it('queries the requested level in tail order and keeps the newest cap', async () => {
    const calls: QueryFilters[] = [];
    const client = {
      async fetchPage(filters: QueryFilters): Promise<QueryPage> {
        calls.push(filters);
        if ((filters.after ?? 0) === 950) {
          return page([951, 952, 953, 954, 955].map((seq) => record(seq, 'error')), 955, true, 5);
        }
        return page([996, 997, 998, 999, 1000].map((seq) => record(seq, 'error')), 1000, false, 5, false);
      },
    };
    const window = await loadErrorWindow(client, {
      includeWarn: false,
      highWater: 1000,
      maxRecords: 5,
    });
    expect(calls[0]).toMatchObject({ level: 'error', after: 950 });
    expect(window.startAfter).toBe(950);
    expect(window.retainedStart).toBe(false);
    expect(window.records.map((row) => row.runtime_sequence)).toEqual([996, 997, 998, 999, 1000]);
    expect(window.truncated).toBe(true);
    expect(window.complete).toBe(true);
    expect(window.scanned).toBe(10);
  });

  it('uses level=warn when warnings are included', async () => {
    const calls: QueryFilters[] = [];
    const client = {
      async fetchPage(filters: QueryFilters): Promise<QueryPage> {
        calls.push(filters);
        return page([], null, false, 0);
      },
    };
    const window = await loadErrorWindow(client, {
      includeWarn: true,
      highWater: 10,
      maxRecords: 5,
    });
    expect(calls[0].level).toBe('warn');
    expect(window.retainedStart).toBe(false);
    expect(window.records).toEqual([]);
  });
});
