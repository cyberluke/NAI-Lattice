import type { LogRecord } from '../telemetry/types';
import { usableTimestamp } from './time';

export interface ErrorGroup {
  key: string;
  component: string;
  /** Lower-cased message with variable tokens replaced by stable placeholders. */
  normalized: string;
  count: number;
  errorCount: number;
  warnCount: number;
  firstMs: number | null;
  lastMs: number | null;
  /** Newest record in the group: the evidence behind the trace sample. */
  sample: LogRecord;
}

const UUID_RE =
  /\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/gi;
const ISO_TS_RE =
  /\b\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?\b/g;
const HEX_RE = /\b(?:0x)?[0-9a-f]{8,}\b/gi;
const DIGITS_RE = /\b\d+\b/g;

/**
 * Normalizes a message for grouping: UUIDs, ISO timestamps, hex blobs and
 * numeric runs become placeholders, then whitespace is collapsed and the text
 * is lower-cased. The original message is always kept in the group sample.
 */
export function normalizeErrorMessage(message: string): string {
  const replaced = message
    .replace(UUID_RE, '<uuid>')
    .replace(ISO_TS_RE, '<ts>')
    .replace(HEX_RE, '<hex>')
    .replace(DIGITS_RE, '<n>')
    .replace(/\s+/g, ' ')
    .trim()
    .toLowerCase();
  return replaced.length > 0 ? replaced : '(empty message)';
}

export function canonicalLevel(level: string): string {
  switch (level.trim().toLowerCase()) {
    case 'warning':
      return 'warn';
    case 'fatal':
      return 'error';
    default:
      return level.trim().toLowerCase();
  }
}

export interface GroupErrorsOptions {
  /** Only group these canonical levels; defaults to error+warn. */
  levels?: readonly string[];
}

/**
 * Groups error/warn records by `component` + normalized message over the
 * bounded result. Counts, first/last and the sample are measured; no record is
 * invented for an empty component.
 */
export function groupErrorRecords(
  records: readonly LogRecord[],
  options: GroupErrorsOptions = {},
): ErrorGroup[] {
  const allowed =
    options.levels === undefined
      ? null
      : new Set(options.levels.map((level) => canonicalLevel(level)));
  const groups = new Map<string, ErrorGroup>();

  for (const record of records) {
    const level = canonicalLevel(record.level);
    if (level !== 'error' && level !== 'warn') continue;
    if (allowed !== null && !allowed.has(level)) continue;
    const component = record.component.length > 0 ? record.component : 'unknown';
    const normalized = normalizeErrorMessage(record.message);
    const key = `${component}\u0000${normalized}`;
    const ts = usableTimestamp(record.timestamp_utc_ms) ? record.timestamp_utc_ms : null;
    const existing = groups.get(key);
    if (existing === undefined) {
      groups.set(key, {
        key,
        component,
        normalized,
        count: 1,
        errorCount: level === 'error' ? 1 : 0,
        warnCount: level === 'warn' ? 1 : 0,
        firstMs: ts,
        lastMs: ts,
        sample: record,
      });
      continue;
    }
    existing.count += 1;
    if (level === 'error') existing.errorCount += 1;
    else existing.warnCount += 1;
    if (ts !== null) {
      existing.firstMs = existing.firstMs === null ? ts : Math.min(existing.firstMs, ts);
      existing.lastMs = existing.lastMs === null ? ts : Math.max(existing.lastMs, ts);
    }
    if (shouldReplaceSample(record, existing.sample)) existing.sample = record;
  }

  return [...groups.values()].sort(
    (a, b) =>
      b.count - a.count ||
      (b.lastMs ?? 0) - (a.lastMs ?? 0) ||
      a.component.localeCompare(b.component) ||
      a.normalized.localeCompare(b.normalized),
  );
}

function hasTrace(record: LogRecord): boolean {
  return record.trace_id !== null && record.trace_id.length > 0;
}

/**
 * The sample should carry a trace id whenever the group has one: the trace
 * action is the main investigation affordance. Otherwise the newest record is
 * the sample.
 */
function shouldReplaceSample(incoming: LogRecord, current: LogRecord): boolean {
  const incomingTrace = hasTrace(incoming);
  const currentTrace = hasTrace(current);
  if (incomingTrace !== currentTrace) return incomingTrace;
  return incomingIsNewer(incoming, current);
}

function incomingIsNewer(incoming: LogRecord, current: LogRecord): boolean {
  if (incoming.runtime_sequence !== current.runtime_sequence) {
    return incoming.runtime_sequence > current.runtime_sequence;
  }
  return incoming.timestamp_utc_ms >= current.timestamp_utc_ms;
}

export function errorGroupTone(group: ErrorGroup): 'bad' | 'warn' {
  return group.errorCount > 0 ? 'bad' : 'warn';
}
