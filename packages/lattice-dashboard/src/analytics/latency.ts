import type { TraceSummary } from '../query/traces';
import { bucketValues, percentiles } from './time';

export interface LatencyBucket {
  index: number;
  startMs: number;
  endMs: number;
  count: number;
  p50Ms: number | null;
  p95Ms: number | null;
}

export interface LatencySeries {
  buckets: LatencyBucket[];
  domain: { startMs: number; endMs: number } | null;
  widthMs: number;
  /** Closed traces with a recorded duration (the population behind p50/p95). */
  measured: number;
  /** Closed traces with a real timestamp (the population behind the buckets). */
  timed: number;
  /** Closed traces whose `durationMs` is null: excluded from p50/p95. */
  excludedNullDuration: number;
  /** Open traces: no verdict/duration yet, excluded from latency math. */
  excludedOpen: number;
  /** Measured durations without a usable timestamp. */
  untimed: number;
  overall: { count: number; p50Ms: number | null; p95Ms: number | null };
  insufficient: boolean;
}

function traceTimestamp(trace: TraceSummary): number | null {
  if (typeof trace.lastTs === 'number' && Number.isFinite(trace.lastTs) && trace.lastTs > 0) {
    return trace.lastTs;
  }
  if (typeof trace.firstTs === 'number' && Number.isFinite(trace.firstTs) && trace.firstTs > 0) {
    return trace.firstTs;
  }
  return null;
}

/**
 * Latency series from `/lattice/api/traces`. Only closed traces with a recorded
 * `durationMs` contribute; open traces have no verdict and null durations are
 * never replaced by zero.
 */
export function buildLatencySeries(
  traces: readonly TraceSummary[],
  bucketCount = 60,
): LatencySeries {
  let excludedOpen = 0;
  let excludedNullDuration = 0;
  const measured: Array<{ durationMs: number; ts: number | null }> = [];

  for (const trace of traces) {
    if (trace.state === 'open') {
      excludedOpen += 1;
      continue;
    }
    if (
      trace.durationMs === null ||
      !Number.isFinite(trace.durationMs) ||
      trace.durationMs < 0
    ) {
      excludedNullDuration += 1;
      continue;
    }
    measured.push({ durationMs: trace.durationMs, ts: traceTimestamp(trace) });
  }

  const timed = measured.filter(
    (entry): entry is { durationMs: number; ts: number } => entry.ts !== null,
  );
  const bucketResult = bucketValues(timed, (entry) => entry.ts, bucketCount);
  const overall = percentiles(measured.map((entry) => entry.durationMs));
  const buckets: LatencyBucket[] = bucketResult.buckets.map((bucket) => {
    const durations = bucket.items.map((entry) => entry.durationMs);
    const stats = percentiles(durations);
    return {
      index: bucket.index,
      startMs: bucket.startMs,
      endMs: bucket.endMs,
      count: stats.count,
      p50Ms: stats.p50,
      p95Ms: stats.p95,
    };
  });

  return {
    buckets,
    domain: bucketResult.domain,
    widthMs: bucketResult.widthMs,
    measured: measured.length,
    timed: timed.length,
    excludedNullDuration,
    excludedOpen,
    untimed: timed.length === 0 ? measured.length : measured.length - timed.length,
    overall: { count: overall.count, p50Ms: overall.p50, p95Ms: overall.p95 },
    insufficient: measured.length === 0,
  };
}
