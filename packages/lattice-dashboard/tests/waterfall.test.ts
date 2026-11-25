import { describe, expect, it } from 'vitest';
import {
  buildWaterfallBars,
  resolveTraceDomain,
} from '../src/analytics/waterfall';
import type { TraceSpan } from '../src/query/traces';

function span(overrides: Partial<TraceSpan> = {}): TraceSpan {
  return {
    ts: 1000,
    kind: 'log',
    label: 'span',
    runtime_sequence: 1,
    durationMs: null,
    component: null,
    ...overrides,
  };
}

describe('trace waterfall geometry', () => {
  it('uses firstTs/lastTs as the domain when ordered', () => {
    expect(
      resolveTraceDomain({
        firstTs: 1000,
        lastTs: 2000,
        durationMs: 500,
        spans: [],
      }),
    ).toEqual({ startMs: 1000, endMs: 2000, spanMs: 1000 });
  });

  it('falls back to a recorded duration and then to span timestamps', () => {
    expect(
      resolveTraceDomain({ firstTs: 1000, lastTs: null, durationMs: 250, spans: [] }),
    ).toEqual({ startMs: 1000, endMs: 1250, spanMs: 250 });
    expect(
      resolveTraceDomain({
        firstTs: null,
        lastTs: null,
        durationMs: null,
        spans: [span({ ts: 5000 }), span({ ts: 4000 })],
      }),
    ).toEqual({ startMs: 4000, endMs: 5000, spanMs: 1000 });
    expect(
      resolveTraceDomain({ firstTs: null, lastTs: null, durationMs: null, spans: [] }),
    ).toBeNull();
  });

  it('computes real offsets/widths, clamps to the track and keeps a 1 px minimum', () => {
    const domain = { startMs: 1000, endMs: 2000, spanMs: 1000 };
    const bars = buildWaterfallBars(
      [
        span({ ts: 1000, durationMs: 100 }),
        span({ ts: 1500, durationMs: 250 }),
        span({ ts: 1900, durationMs: 500 }),
        span({ ts: 1000, durationMs: null }),
        span({ ts: 1000, durationMs: 0 }),
        span({ ts: null, durationMs: 100 }),
      ],
      domain,
      1000,
      1,
    );

    expect(bars).toHaveLength(5);
    expect(bars[0]).toMatchObject({ index: 0, leftPx: 0, widthPx: 100, minApplied: false });
    expect(bars[1]).toMatchObject({ index: 1, leftPx: 500, widthPx: 250, minApplied: false });
    // 500 ms would run past the end; the bar is clamped to the remaining track.
    expect(bars[2]).toMatchObject({ index: 2, leftPx: 900, widthPx: 100 });
    expect(bars[3]).toMatchObject({ index: 3, leftPx: 0, widthPx: 1, minApplied: true });
    expect(bars[4]).toMatchObject({ index: 4, leftPx: 0, widthPx: 1, minApplied: true });
  });

  it('returns no bars without a domain and never invents a bar for a null ts', () => {
    expect(buildWaterfallBars([span()], null, 600)).toEqual([]);
    expect(
      buildWaterfallBars([span({ ts: null })], { startMs: 1, endMs: 2, spanMs: 1 }, 600),
    ).toEqual([]);
  });

  it('scales offsets by the supplied track width', () => {
    const domain = { startMs: 0, endMs: 1000, spanMs: 1000 };
    const bars = buildWaterfallBars([span({ ts: 250, durationMs: 500 })], domain, 600, 1);
    expect(bars[0].leftPx).toBe(150);
    expect(bars[0].widthPx).toBe(300);
  });
});
