import { describe, expect, it } from 'vitest';
import rawFixtures from '../../../scripts/qa/dashboard-demo-fixtures.json';

interface FixtureRecord {
  key: string;
  offsetMs: number;
  level: string;
  component: string;
  message: string;
  extras: Record<string, unknown>;
}

interface FixtureTrace {
  traceId: string;
  baseOffsetMs: number;
  records: FixtureRecord[];
}

const fixtures = rawFixtures as {
  scenario: string;
  traces: FixtureTrace[];
};

function callOf(record: FixtureRecord): Record<string, unknown> | null {
  const call = record.extras['call'];
  return call !== null && typeof call === 'object' && !Array.isArray(call)
    ? (call as Record<string, unknown>)
    : null;
}

describe('dashboard demo explicit-span fixtures', () => {
  it('labels every record as dashboard-demo with unique idempotency keys', () => {
    expect(fixtures.scenario).toBe('dashboard-demo');
    expect(fixtures.traces.map((trace) => trace.traceId)).toEqual([
      'demo-explicit-chain',
      'demo-explicit-fanout',
    ]);

    const keys = new Set<string>();
    for (const trace of fixtures.traces) {
      expect(trace.traceId).not.toBe('demo-trace-full');
      expect(trace.traceId.startsWith('qa-flow-')).toBe(false);
      for (const record of trace.records) {
        expect(record.extras['scenario']).toBe('dashboard-demo');
        expect(record.key.length).toBeGreaterThan(0);
        expect(keys.has(record.key)).toBe(false);
        keys.add(record.key);
        expect(Number.isFinite(record.offsetMs)).toBe(true);
        expect(record.component.length).toBeGreaterThan(0);
        expect(record.message.length).toBeGreaterThan(0);
      }
    }
  });

  it('carries real span ids, parents, operations, durations and kinds', () => {
    const spans: Array<{ trace: FixtureTrace; record: FixtureRecord }> = [];
    for (const trace of fixtures.traces) {
      for (const record of trace.records) {
        if (typeof record.extras['span_id'] === 'string') spans.push({ trace, record });
      }
    }
    expect(spans).toHaveLength(8);
    const idsByTrace = new Map<string, Set<string>>();
    for (const { trace, record } of spans) {
      const spanId = record.extras['span_id'] as string;
      const ids = idsByTrace.get(trace.traceId) ?? new Set<string>();
      expect(ids.has(spanId)).toBe(false);
      ids.add(spanId);
      idsByTrace.set(trace.traceId, ids);
      expect('parent_span_id' in record.extras).toBe(true);
      expect(typeof record.extras['operation']).toBe('string');
      expect((record.extras['operation'] as string).length).toBeGreaterThan(0);
      expect(typeof record.extras['duration_ms']).toBe('number');
      expect(typeof record.extras['span_kind']).toBe('string');
    }
    for (const { trace, record } of spans) {
      const parent = record.extras['parent_span_id'];
      if (typeof parent === 'string') {
        expect(idsByTrace.get(trace.traceId)?.has(parent)).toBe(true);
      }
    }
    for (const trace of fixtures.traces) {
      const ids = idsByTrace.get(trace.traceId) ?? new Set<string>();
      expect(ids.size).toBeGreaterThan(0);
    }
  });

  it('has one explicit call observation between real spans and one point event', () => {
    const all = fixtures.traces.flatMap((trace) =>
      trace.records.map((record) => ({ trace, record })),
    );
    const calls = all.filter(({ record }) => callOf(record) !== null);
    expect(calls).toHaveLength(1);
    const call = callOf(calls[0].record);
    expect(call).not.toBeNull();
    expect(typeof call?.['invocation_id']).toBe('string');
    expect(typeof call?.['source_span_id']).toBe('string');
    expect(typeof call?.['target_span_id']).toBe('string');
    expect(typeof call?.['operation']).toBe('string');
    expect(typeof call?.['transport']).toBe('string');
    // The host reads extras.call from the source span record; a standalone
    // call-only record would never become an edge.
    expect(typeof calls[0].record.extras['span_id']).toBe('string');
    expect(calls[0].record.extras['span_id']).toBe(call?.['source_span_id']);

    const spanIds = new Set(
      calls[0].trace.records
        .map((record) => record.extras['span_id'])
        .filter((value): value is string => typeof value === 'string'),
    );
    expect(spanIds.has(call?.['source_span_id'] as string)).toBe(true);
    expect(spanIds.has(call?.['target_span_id'] as string)).toBe(true);

    const pointEvents = all.filter(
      ({ record }) => typeof record.extras['span_id'] !== 'string' && callOf(record) === null,
    );
    expect(pointEvents).toHaveLength(1);
    expect(pointEvents[0].record.extras['point']).toBe(true);
    expect(typeof pointEvents[0].record.extras['event_type']).toBe('string');
  });
});
