import { describe, expect, it } from 'vitest';
import {
  errorGroupTone,
  groupErrorRecords,
  normalizeErrorMessage,
} from '../src/analytics/errors';
import type { LogRecord } from '../src/telemetry/types';

function record(
  sequence: number,
  level: 'error' | 'warn' | 'info',
  message: string,
  overrides: Partial<LogRecord> = {},
): LogRecord {
  return {
    runtime_sequence: sequence,
    timestamp_utc_ms: 1000 + sequence,
    level,
    component: 'alpha',
    trace_id: null,
    message,
    ...overrides,
  };
}

describe('error message normalization', () => {
  it('strips UUIDs, hex blobs, ISO timestamps, numeric runs and whitespace', () => {
    const message =
      'Request 550e8400-e29b-41d4-a716-446655440000 failed at 2026-11-24T20:00:00.123Z token 0xdeadbeefcafe after 1234 ms';
    const normalized = normalizeErrorMessage(message);
    expect(normalized).toBe(
      'request <uuid> failed at <ts> token <hex> after <n> ms',
    );
    expect(normalized).not.toContain('550e8400');
    expect(normalized).not.toContain('1234');
  });

  it('lowercases, collapses whitespace and labels an empty message', () => {
    expect(normalizeErrorMessage('  TIMEOUT\n\n  after   retry ')).toBe('timeout after retry');
    expect(normalizeErrorMessage('   ')).toBe('(empty message)');
  });
});

describe('error grouping', () => {
  it('groups by component + normalized message with measured counts and bounds', () => {
    const groups = groupErrorRecords([
      record(1, 'error', 'timeout after 100 ms', {
        timestamp_utc_ms: 2000,
        trace_id: 'trace-a',
      }),
      record(2, 'error', 'timeout after 250 ms', {
        timestamp_utc_ms: 3000,
        trace_id: 'trace-b',
      }),
      record(3, 'warn', 'timeout after 999 ms', { timestamp_utc_ms: 1000 }),
      record(4, 'error', 'timeout after 5 ms', {
        component: 'beta',
        timestamp_utc_ms: 4000,
      }),
      record(5, 'info', 'timeout after 999 ms', { timestamp_utc_ms: 5000 }),
    ]);
    expect(groups).toHaveLength(2);
    const alpha = groups[0];
    expect(alpha.component).toBe('alpha');
    expect(alpha.normalized).toBe('timeout after <n> ms');
    expect(alpha.count).toBe(3);
    expect(alpha.errorCount).toBe(2);
    expect(alpha.warnCount).toBe(1);
    expect(alpha.firstMs).toBe(1000);
    expect(alpha.lastMs).toBe(3000);
    expect(alpha.sample.trace_id).toBe('trace-b');
    expect(errorGroupTone(alpha)).toBe('bad');

    expect(groups[1].component).toBe('beta');
    expect(groups[1].count).toBe(1);
    expect(errorGroupTone(groups[1])).toBe('bad');
  });

  it('returns an empty grouping for an empty or info-only result', () => {
    expect(groupErrorRecords([])).toEqual([]);
    expect(groupErrorRecords([record(1, 'info', 'nothing')])).toEqual([]);
  });

  it('honours an explicit level restriction', () => {
    const warnOnly = groupErrorRecords(
      [
        record(1, 'warn', 'careful'),
        record(2, 'error', 'broken'),
      ],
      { levels: ['warn'] },
    );
    expect(warnOnly).toHaveLength(1);
    expect(errorGroupTone(warnOnly[0])).toBe('warn');
  });
});
