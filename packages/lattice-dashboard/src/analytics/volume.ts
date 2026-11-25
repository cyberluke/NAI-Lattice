import type { QueryClient } from '../query/client';
import type { LogRecord } from '../telemetry/types';
import { bucketValues, type BucketResult, volumeSeriesKey } from './time';

export const VOLUME_DEFAULT_RECORDS = 3000;
export const VOLUME_MAX_RECORDS = 5000;
export const VOLUME_CHOICES = [1000, 3000, 5000] as const;
const VOLUME_PAGE_LIMIT = 1000;
const VOLUME_MAX_PAGES = 12;

export interface VolumeWindowOptions {
  maxRecords?: number;
  pageLimit?: number;
  maxPages?: number;
  /** Latest real runtime_sequence observed by the live feed (null when unknown). */
  highWater?: number | null;
  signal?: AbortSignal;
}

export interface VolumeWindow {
  records: LogRecord[];
  scanned: number;
  pages: number;
  /** True when the ledger end (or the live cursor) was reached. */
  complete: boolean;
  /** True when a bound dropped measured records or the scan stopped early. */
  truncated: boolean;
  budgetHit: boolean;
  /** Exclusive cursor the scan started from. */
  startAfter: number;
  source: 'tail' | 'retained-start';
  limit: number;
}

function clampInt(value: number | undefined, min: number, max: number, fallback: number): number {
  if (value === undefined || !Number.isFinite(value)) return fallback;
  return Math.min(max, Math.max(min, Math.trunc(value)));
}

/**
 * Loads the newest bounded volume window (default 3000, capped at 5000).
 *
 * The query API is ascending-only, so the window starts at
 * `highWater - maxRecords` when the live feed has measured a cursor. That is a
 * real sequence window: records outside it are never claimed or synthesized.
 * Without a cursor the caller gets the retained-history start and the returned
 * `source` says so.
 */
export async function loadVolumeWindow(
  client: Pick<QueryClient, 'fetchPage'>,
  options: VolumeWindowOptions = {},
): Promise<VolumeWindow> {
  const limit = clampInt(
    options.maxRecords,
    1,
    VOLUME_MAX_RECORDS,
    VOLUME_DEFAULT_RECORDS,
  );
  const pageLimit = clampInt(options.pageLimit, 1, 1000, VOLUME_PAGE_LIMIT);
  const maxPages = clampInt(options.maxPages, 1, 20, VOLUME_MAX_PAGES);
  const highWater =
    typeof options.highWater === 'number' &&
    Number.isFinite(options.highWater) &&
    options.highWater > 0
      ? Math.trunc(options.highWater)
      : null;
  const startAfter = highWater === null ? 0 : Math.max(0, highWater - limit);

  const bySequence = new Map<number, LogRecord>();
  let cursor = startAfter;
  let scanned = 0;
  let pages = 0;
  let exhausted = false;
  let budgetHit = false;
  let reachedHighWater = false;
  let stoppedEarly = false;

  while (bySequence.size < limit && pages < maxPages) {
    const remaining = limit - bySequence.size;
    const page = await client.fetchPage(
      { after: cursor, limit: Math.min(pageLimit, Math.max(1, remaining)) },
      options.signal,
    );
    pages += 1;
    scanned += page.scanned ?? 0;
    if (page.budgetHit) budgetHit = true;
    for (const hit of page.hits) {
      if (hit.runtime_sequence > startAfter) bySequence.set(hit.runtime_sequence, hit);
    }
    const next = page.nextAfter ?? cursor;
    if (page.exhausted) exhausted = true;
    if (next <= cursor) break;
    cursor = next;
    if (highWater !== null && cursor >= highWater) {
      reachedHighWater = true;
      break;
    }
    if (!page.hasMore || page.hits.length === 0) break;
    if (bySequence.size >= limit) break;
  }

  if (pages >= maxPages && !exhausted && !reachedHighWater) stoppedEarly = true;

  let records = [...bySequence.values()].sort(
    (a, b) => a.runtime_sequence - b.runtime_sequence,
  );
  let truncated = stoppedEarly || budgetHit;
  if (records.length > limit) {
    records = records.slice(records.length - limit);
    truncated = true;
  }

  return {
    records,
    scanned,
    pages,
    complete: exhausted || reachedHighWater,
    truncated,
    budgetHit,
    startAfter,
    source: highWater === null ? 'retained-start' : 'tail',
    limit,
  };
}

export interface VolumeBucketSeries {
  index: number;
  startMs: number;
  endMs: number;
  info: number;
  warn: number;
  error: number;
  total: number;
}

export interface VolumeSeries {
  buckets: VolumeBucketSeries[];
  domain: { startMs: number; endMs: number } | null;
  widthMs: number;
  untimed: number;
  totals: { info: number; warn: number; error: number };
}

/** Buckets REAL records into fixed time buckets; untimed records are counted separately. */
export function buildVolumeSeries(
  records: readonly LogRecord[],
  bucketCount = 60,
): VolumeSeries {
  const buckets: BucketResult<LogRecord> = bucketValues(
    records,
    (record) => record.timestamp_utc_ms,
    bucketCount,
  );
  const totals = { info: 0, warn: 0, error: 0 };
  for (const record of records) totals[volumeSeriesKey(record.level)] += 1;
  return {
    buckets: buckets.buckets.map((bucket) => {
      const series: VolumeBucketSeries = {
        index: bucket.index,
        startMs: bucket.startMs,
        endMs: bucket.endMs,
        info: 0,
        warn: 0,
        error: 0,
        total: bucket.items.length,
      };
      for (const record of bucket.items) series[volumeSeriesKey(record.level)] += 1;
      return series;
    }),
    domain: buckets.domain,
    widthMs: buckets.widthMs,
    untimed: buckets.untimed,
    totals,
  };
}
