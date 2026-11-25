import type { TraceSpan, TraceSummary } from '../query/traces';
import { usableTimestamp } from './time';

export interface WaterfallDomain {
  startMs: number;
  endMs: number;
  spanMs: number;
}

export interface WaterfallBar {
  /** Index into the trace's `spans` array. */
  index: number;
  span: TraceSpan;
  leftPx: number;
  widthPx: number;
  /** True when duration was null/zero and the 1 px minimum marker was used. */
  minApplied: boolean;
}

type TraceDomainSource = Pick<TraceSummary, 'firstTs' | 'lastTs' | 'durationMs' | 'spans'>;

/**
 * Resolves the timeline domain from recorded values only:
 * firstTs/lastTs when ordered, else a recorded duration, else the span
 * timestamps. Returns null when the trace carries no usable time at all.
 */
export function resolveTraceDomain(trace: TraceDomainSource): WaterfallDomain | null {
  const first = usableTimestamp(trace.firstTs) ? trace.firstTs : null;
  const last = usableTimestamp(trace.lastTs) ? trace.lastTs : null;
  if (first !== null && last !== null && last > first) {
    return { startMs: first, endMs: last, spanMs: last - first };
  }
  if (first !== null && trace.durationMs !== null && trace.durationMs > 0) {
    return {
      startMs: first,
      endMs: first + trace.durationMs,
      spanMs: trace.durationMs,
    };
  }
  let min = Number.POSITIVE_INFINITY;
  let max = Number.NEGATIVE_INFINITY;
  for (const span of trace.spans) {
    if (!usableTimestamp(span.ts)) continue;
    if (span.ts < min) min = span.ts;
    if (span.ts > max) max = span.ts;
  }
  if (Number.isFinite(min) && Number.isFinite(max)) {
    const endMs = max > min ? max : min + 1;
    return { startMs: min, endMs, spanMs: endMs - min };
  }
  if (first !== null) {
    return { startMs: first, endMs: first + 1, spanMs: 1 };
  }
  return null;
}

/**
 * Bar geometry for a fixed-width timeline track. Offset and width come from
 * real `ts`/`durationMs`; absent durations become a 1 px marker, never a
 * guessed span length.
 */
export function buildWaterfallBars(
  spans: readonly TraceSpan[],
  domain: WaterfallDomain | null,
  trackWidth: number,
  minWidth = 1,
): WaterfallBar[] {
  if (domain === null) return [];
  const width = Number.isFinite(trackWidth) && trackWidth > 0 ? trackWidth : 1;
  const floor = Number.isFinite(minWidth) && minWidth > 0 ? Math.min(minWidth, width) : 1;
  const bars: WaterfallBar[] = [];

  spans.forEach((span, index) => {
    if (!usableTimestamp(span.ts)) return;
    const ratio = (span.ts - domain.startMs) / domain.spanMs;
    let leftPx = Math.min(width - floor, Math.max(0, ratio * width));
    if (!Number.isFinite(leftPx)) leftPx = 0;
    const duration = span.durationMs;
    const hasDuration =
      duration !== null && Number.isFinite(duration) && duration >= 0;
    const rawWidth =
      duration !== null && Number.isFinite(duration) && duration >= 0
        ? (duration / domain.spanMs) * width
        : floor;
    const widthPx = Math.min(
      width - leftPx,
      Math.max(floor, Number.isFinite(rawWidth) ? rawWidth : floor),
    );
    bars.push({
      index,
      span,
      leftPx,
      widthPx,
      minApplied: !hasDuration || duration === 0 || rawWidth < floor,
    });
  });

  return bars;
}
