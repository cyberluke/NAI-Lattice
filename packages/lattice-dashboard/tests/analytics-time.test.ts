import { describe, expect, it } from 'vitest';
import {
  bucketValues,
  percentile,
  percentiles,
  timeDomainOf,
  volumeSeriesKey,
} from '../src/analytics/time';
import { buildVolumeSeries } from '../src/analytics/volume';
import type { LogRecord } from '../src/telemetry/types';

function record(
  sequence: number,
  timestamp: number,
  level = 'info',
): LogRecord {
  return {
    runtime_sequence: sequence,
    timestamp_utc_ms: timestamp,
    level,
    component: 'test',
    trace_id: null,
    message: `row-${sequence}`,
  };
}

describe('time bucketing', () => {
  it('returns no buckets and no domain for an empty population', () => {
    const result = bucketValues<number>([], (value) => value, 60);
    expect(result.domain).toBeNull();
    expect(result.buckets).toEqual([]);
    expect(result.untimed).toBe(0);
  });

  it('keeps empty buckets for gaps and clamps boundaries exactly', () => {
    const values = [1000, 1010, 1599, 1600];
    const result = bucketValues(values, (value) => value, 60);
    expect(result.domain).toEqual({ startMs: 1000, endMs: 1600 });
    expect(result.widthMs).toBe(10);
    expect(result.buckets).toHaveLength(60);
    expect(result.buckets[0].items).toEqual([1000]);
    expect(result.buckets[1].items).toEqual([1010]);
    expect(result.buckets[2].items).toEqual([]);
    expect(result.buckets[58].items).toEqual([]);
    expect(result.buckets[59].items).toEqual([1599, 1600]);
    expect(result.buckets.reduce((sum, bucket) => sum + bucket.items.length, 0)).toBe(4);
  });

  it('counts records without a usable timestamp separately instead of dropping them', () => {
    const values = [
      { ts: 1000 },
      { ts: 0 },
      { ts: -1 },
      { ts: null },
      { ts: Number.NaN },
    ];
    const result = bucketValues(values, (value) => value.ts, 10);
    expect(result.untimed).toBe(4);
    expect(result.buckets.reduce((sum, bucket) => sum + bucket.items.length, 0)).toBe(1);
    expect(timeDomainOf([0, -5, null])).toBeNull();
  });

  it('puts every record of a single-instant domain in one real bucket', () => {
    const result = bucketValues([2000, 2000], (value) => value, 60);
    expect(result.domain).toEqual({ startMs: 2000, endMs: 2000 });
    expect(result.widthMs).toBe(1);
    expect(result.buckets[0].items).toEqual([2000, 2000]);
  });
});

describe('percentile math', () => {
  it('returns null for an empty population and the value for one sample', () => {
    expect(percentile([], 0.5)).toBeNull();
    expect(percentile([42], 0.95)).toBe(42);
  });

  it('uses linear interpolation (R-7) and ignores non-finite values', () => {
    const values = [40, 10, 30, 20];
    expect(percentile(values, 0.5)).toBe(25);
    expect(percentile(values, 0.95)).toBeCloseTo(38.5, 10);
    expect(percentile([10, Number.NaN, 20], 0.5)).toBe(15);
    expect(percentiles([])).toEqual({ count: 0, p50: null, p95: null });
  });
});

describe('volume series', () => {
  it('groups trace/debug/info into the info series and measures every record', () => {
    expect(volumeSeriesKey('trace')).toBe('info');
    expect(volumeSeriesKey('debug')).toBe('info');
    expect(volumeSeriesKey('info')).toBe('info');
    expect(volumeSeriesKey('warning')).toBe('warn');
    expect(volumeSeriesKey('fatal')).toBe('error');
  });

  it('counts real buckets and totals while separating untimed records', () => {
    const series = buildVolumeSeries(
      [
        record(1, 1000, 'info'),
        record(2, 1000, 'debug'),
        record(3, 1010, 'warn'),
        record(4, 1600, 'error'),
        record(5, 0, 'info'),
      ],
      60,
    );
    expect(series.domain).toEqual({ startMs: 1000, endMs: 1600 });
    expect(series.totals).toEqual({ info: 3, warn: 1, error: 1 });
    expect(series.untimed).toBe(1);
    expect(series.buckets).toHaveLength(60);
    expect(series.buckets[0]).toMatchObject({ info: 2, warn: 0, error: 0, total: 2 });
    expect(series.buckets[1]).toMatchObject({ info: 0, warn: 1, error: 0, total: 1 });
    expect(series.buckets[59]).toMatchObject({ info: 0, warn: 0, error: 1, total: 1 });
    expect(series.buckets[30].total).toBe(0);
  });
});
