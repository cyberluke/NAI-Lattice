import { describe, expect, it } from 'vitest';
import { buildLatencySeries } from '../src/analytics/latency';
import type { TraceSummary } from '../src/query/traces';

function trace(overrides: Partial<TraceSummary>): TraceSummary {
  return {
    traceId: 'trace-a',
    firstTs: 1000,
    lastTs: 1000,
    durationMs: null,
    state: 'closed',
    terminal: 'completed',
    ok: true,
    spans: [],
    ...overrides,
  };
}

describe('latency series', () => {
  it('reports insufficient closed traces without inventing a bucket', () => {
    const series = buildLatencySeries([], 60);
    expect(series.insufficient).toBe(true);
    expect(series.measured).toBe(0);
    expect(series.buckets).toEqual([]);
    expect(series.overall).toEqual({ count: 0, p50Ms: null, p95Ms: null });
  });

  it('excludes open traces and null durations from p50/p95', () => {
    const series = buildLatencySeries(
      [
        trace({ traceId: 'a', durationMs: 10 }),
        trace({ traceId: 'b', durationMs: 20 }),
        trace({ traceId: 'c', durationMs: 30 }),
        trace({ traceId: 'open', state: 'open', ok: null, durationMs: 999 }),
        trace({ traceId: 'null', durationMs: null }),
      ],
      60,
    );
    expect(series.measured).toBe(3);
    expect(series.excludedOpen).toBe(1);
    expect(series.excludedNullDuration).toBe(1);
    expect(series.overall.count).toBe(3);
    expect(series.overall.p50Ms).toBe(20);
    expect(series.overall.p95Ms).toBeCloseTo(29, 10);
  });

  it('buckets by real completion time and measures p50/p95 per bucket', () => {
    const series = buildLatencySeries(
      [
        trace({ traceId: 'a', durationMs: 10, lastTs: 1000 }),
        trace({ traceId: 'b', durationMs: 30, lastTs: 1000 }),
        trace({ traceId: 'c', durationMs: 50, lastTs: 1600 }),
      ],
      60,
    );
    // 1000..1600 => 60 buckets of 10ms; the 1600 record is clamped into bucket 59.
    expect(series.buckets[0].count).toBe(2);
    expect(series.buckets[0].p50Ms).toBe(20);
    expect(series.buckets[0].p95Ms).toBeCloseTo(29, 10);
    expect(series.buckets[59].count).toBe(1);
    expect(series.buckets[59].p50Ms).toBe(50);
    expect(series.buckets[30].count).toBe(0);
    expect(series.buckets[30].p50Ms).toBeNull();
  });

  it('keeps measured durations that have no timestamp out of time buckets', () => {
    const series = buildLatencySeries(
      [trace({ traceId: 'a', durationMs: 10, lastTs: null, firstTs: null })],
      60,
    );
    expect(series.insufficient).toBe(false);
    expect(series.measured).toBe(1);
    expect(series.timed).toBe(0);
    expect(series.untimed).toBe(1);
    expect(series.buckets).toEqual([]);
    expect(series.overall.p50Ms).toBe(10);
  });
});
