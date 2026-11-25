/**
 * Time bucketing and percentile math shared by the Overview charts.
 *
 * All inputs are REAL measurements. A record without a recorded timestamp is
 * reported as untimed by the caller; no synthetic timestamp (including the
 * 1970 epoch) is ever introduced here.
 */

export interface TimeDomain {
  startMs: number;
  endMs: number;
}

export interface TimeBucket<T> {
  index: number;
  startMs: number;
  endMs: number;
  items: T[];
}

export interface BucketResult<T> {
  domain: TimeDomain | null;
  widthMs: number;
  bucketCount: number;
  buckets: TimeBucket<T>[];
  untimed: number;
}

export function usableTimestamp(value: number | null | undefined): value is number {
  return typeof value === 'number' && Number.isFinite(value) && value > 0;
}

export function timeDomainOf(
  timestamps: readonly (number | null | undefined)[],
): TimeDomain | null {
  let start = Number.POSITIVE_INFINITY;
  let end = Number.NEGATIVE_INFINITY;
  for (const ts of timestamps) {
    if (!usableTimestamp(ts)) continue;
    if (ts < start) start = ts;
    if (ts > end) end = ts;
  }
  if (!Number.isFinite(start) || !Number.isFinite(end)) return null;
  return { startMs: start, endMs: end };
}

function clampBucketCount(value: number | undefined): number {
  if (value === undefined || !Number.isFinite(value)) return 60;
  return Math.min(600, Math.max(1, Math.trunc(value)));
}

/**
 * Buckets items by their recorded timestamp.
 *
 * Boundary rule: a timestamp exactly on a bucket start belongs to that new
 * bucket; the domain end is clamped into the last bucket so no measured record
 * is dropped by floating-point division.
 */
export function bucketValues<T>(
  items: readonly T[],
  tsOf: (item: T) => number | null | undefined,
  bucketCount = 60,
): BucketResult<T> {
  const count = clampBucketCount(bucketCount);
  const untimedItems: T[] = [];
  const timed: Array<{ item: T; ts: number }> = [];
  for (const item of items) {
    const ts = tsOf(item);
    if (usableTimestamp(ts)) timed.push({ item, ts });
    else untimedItems.push(item);
  }
  const domain = timeDomainOf(timed.map((entry) => entry.ts));
  const buckets: TimeBucket<T>[] = [];
  if (domain === null) {
    return { domain: null, widthMs: 0, bucketCount: count, buckets, untimed: untimedItems.length };
  }

  const rawSpan = domain.endMs - domain.startMs;
  const widthMs = rawSpan > 0 ? rawSpan / count : 1;
  for (let index = 0; index < count; index += 1) {
    buckets.push({
      index,
      startMs: domain.startMs + index * widthMs,
      endMs: domain.startMs + (index + 1) * widthMs,
      items: [],
    });
  }

  for (const entry of timed) {
    const relative = entry.ts - domain.startMs;
    let index = Math.floor(relative / widthMs);
    if (!Number.isFinite(index)) index = 0;
    // The domain end (and anything at/beyond it) belongs to the last bucket.
    index = Math.min(count - 1, Math.max(0, index));
    buckets[index].items.push(entry.item);
  }

  return { domain, widthMs, bucketCount: count, buckets, untimed: untimedItems.length };
}

/**
 * Linear-interpolated percentile (R-7 / Excel PERCENTILE.INC).
 * Returns null for an empty population and never invents a value.
 */
export function percentile(values: readonly number[], p: number): number | null {
  const finite = values.filter((value) => Number.isFinite(value)).sort((a, b) => a - b);
  if (finite.length === 0) return null;
  const clamped = Math.min(1, Math.max(0, Number.isFinite(p) ? p : 0));
  if (finite.length === 1) return finite[0];
  const rank = (finite.length - 1) * clamped;
  const lower = Math.floor(rank);
  const upper = Math.ceil(rank);
  if (lower === upper) return finite[lower];
  const weight = rank - lower;
  return finite[lower] + (finite[upper] - finite[lower]) * weight;
}

export interface Percentiles {
  count: number;
  p50: number | null;
  p95: number | null;
}

export function percentiles(values: readonly number[]): Percentiles {
  const finite = values.filter((value) => Number.isFinite(value));
  return {
    count: finite.length,
    p50: percentile(finite, 0.5),
    p95: percentile(finite, 0.95),
  };
}

export type VolumeSeriesKey = 'info' | 'warn' | 'error';

/** Collapses producer level spellings into the three measured series. */
export function volumeSeriesKey(level: string): VolumeSeriesKey {
  switch (level.trim().toLowerCase()) {
    case 'warn':
    case 'warning':
      return 'warn';
    case 'error':
    case 'fatal':
      return 'error';
    default:
      // trace/debug/info are informational levels; the chart footer says so.
      return 'info';
  }
}
