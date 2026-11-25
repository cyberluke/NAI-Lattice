import type { QueryClient } from './client';
import type { LogRecord } from '../telemetry/types';

export const ERROR_MAX_RECORDS = 5000;
export const ERROR_LOOKBACK_FACTOR = 10;

export interface ErrorWindowOptions {
  includeWarn: boolean;
  highWater?: number | null;
  maxRecords?: number;
  pageLimit?: number;
  maxPages?: number;
  signal?: AbortSignal;
}

export interface ErrorWindow {
  records: LogRecord[];
  scanned: number;
  pages: number;
  /** Inclusive sequence window the scan examined (startAfter is exclusive). */
  startAfter: number;
  highWater: number | null;
  complete: boolean;
  truncated: boolean;
  /** True when no live cursor was available; the retained-history start was used. */
  retainedStart: boolean;
}

function clampInt(value: number | undefined, min: number, max: number, fallback: number): number {
  if (value === undefined || !Number.isFinite(value)) return fallback;
  return Math.min(max, Math.max(min, Math.trunc(value)));
}

/**
 * Loads the newest bounded error/warn window.
 *
 * The frozen query API is ascending-only, so the window starts a real
 * `maxRecords * 10` sequence lookback behind the live cursor and follows pages
 * to the cursor. If the lookback contains more than `maxRecords` matches, the
 * oldest are dropped from the display but every count remains measured. With no
 * cursor the scan starts at the retained-history beginning and stops at the cap;
 * `retainedStart` makes that visible in the UI.
 */
export async function loadErrorWindow(
  client: Pick<QueryClient, 'fetchPage'>,
  options: ErrorWindowOptions,
): Promise<ErrorWindow> {
  const maxRecords = clampInt(options.maxRecords, 1, ERROR_MAX_RECORDS, ERROR_MAX_RECORDS);
  const pageLimit = clampInt(options.pageLimit, 1, 1000, 1000);
  const maxPages = clampInt(options.maxPages, 1, 20, 12);
  const highWater =
    typeof options.highWater === 'number' &&
    Number.isFinite(options.highWater) &&
    options.highWater > 0
      ? Math.trunc(options.highWater)
      : null;
  const retainedStart = highWater === null;
  const startAfter =
    highWater === null ? 0 : Math.max(0, highWater - maxRecords * ERROR_LOOKBACK_FACTOR);

  const bySequence = new Map<number, LogRecord>();
  let cursor = startAfter;
  let pages = 0;
  let scanned = 0;
  let exhausted = false;
  let reachedHighWater = false;
  let stoppedEarly = false;

  while (pages < maxPages) {
    const page = await client.fetchPage(
      {
        level: options.includeWarn ? 'warn' : 'error',
        after: cursor,
        limit: pageLimit,
      },
      options.signal,
    );
    pages += 1;
    scanned += page.scanned ?? 0;
    for (const hit of page.hits) {
      if (hit.runtime_sequence > startAfter) bySequence.set(hit.runtime_sequence, hit);
    }
    if (page.exhausted) {
      exhausted = true;
      break;
    }
    const next = page.nextAfter ?? cursor;
    if (next <= cursor) break;
    cursor = next;
    if (highWater !== null && cursor >= highWater) {
      reachedHighWater = true;
      break;
    }
    if (!page.hasMore) break;
    if (retainedStart && bySequence.size >= maxRecords) break;
  }

  if (pages >= maxPages && !exhausted && !reachedHighWater) stoppedEarly = true;
  if (retainedStart && bySequence.size >= maxRecords && !exhausted) stoppedEarly = true;

  let records = [...bySequence.values()].sort(
    (a, b) => a.runtime_sequence - b.runtime_sequence,
  );
  let truncated = stoppedEarly;
  if (records.length > maxRecords) {
    records = records.slice(records.length - maxRecords);
    truncated = true;
  }

  return {
    records,
    scanned,
    pages,
    startAfter,
    highWater,
    complete: exhausted || reachedHighWater,
    truncated,
    retainedStart,
  };
}
