<script setup lang="ts">
import { computed } from 'vue';
import type { TraceGraph, TraceSummary } from '../../query/traces';
import { usableTimestamp } from '../../analytics/time';
import { formatDuration, formatTimestamp } from '../../ui/format';
import {
  buildWaterfallGeometry,
  buildWaterfallRows,
  offsetText,
  resolveWaterfallDomain,
  rowDurationText,
  rowDurationTitle,
  type WaterfallGeometry,
  type WaterfallRow,
  type WaterfallTone,
} from './chain';

const props = withDefaults(
  defineProps<{
    trace: TraceSummary;
    graph?: TraceGraph | null;
  }>(),
  { graph: null },
);

/**
 * Logical geometry scale: bars are positioned in percentages of this width and
 * CSS enforces the 1 px minimum. The timeline stays responsive without a
 * per-row ResizeObserver.
 */
const TRACK_WIDTH = 1000;

interface WaterfallRowView extends WaterfallRow {
  geometry: WaterfallGeometry | null;
  offsetMs: number | null;
  tickLeft: string | null;
}

const rows = computed(() => buildWaterfallRows(props.trace, props.graph));
const legacyMode = computed(
  () => props.graph === null || props.graph.coverage === 'legacy',
);
const domain = computed(() => resolveWaterfallDomain(props.trace, rows.value));
const geometry = computed(() =>
  buildWaterfallGeometry(rows.value, domain.value, TRACK_WIDTH),
);

const rowViews = computed<WaterfallRowView[]>(() => {
  const currentDomain = domain.value;
  const currentGeometry = geometry.value;
  return rows.value.map((row) => {
    const geom = currentGeometry.get(row.key) ?? null;
    const timed = currentDomain !== null && usableTimestamp(row.startTs);
    const offsetMs = timed ? row.startTs! - currentDomain!.startMs : null;
    const ratio = timed ? (row.startTs! - currentDomain!.startMs) / currentDomain!.spanMs : null;
    const tickLeft =
      ratio === null || !Number.isFinite(ratio)
        ? null
        : `${Math.min(100, Math.max(0, ratio * 100))}%`;
    return { ...row, geometry: geom, offsetMs, tickLeft };
  });
});

const spanCount = computed(() => rows.value.filter((row) => row.origin === 'span').length);
const eventCount = computed(() => rows.value.filter((row) => row.isPointEvent).length);
const missingTimestamp = computed(
  () => rows.value.filter((row) => !usableTimestamp(row.startTs)).length,
);

function barLeft(geom: WaterfallGeometry): string {
  return `${(geom.leftPx / TRACK_WIDTH) * 100}%`;
}

function barWidth(geom: WaterfallGeometry): string {
  return `${(geom.widthPx / TRACK_WIDTH) * 100}%`;
}

function originLabel(row: WaterfallRow): string {
  if (row.origin === 'span') return 'SPAN';
  if (row.origin === 'legacy') return 'LEGACY';
  return 'EVENT';
}

function rowTitle(row: WaterfallRowView): string {
  const offset = offsetText(row.offsetMs);
  const duration = row.isPointEvent
    ? row.origin === 'legacy'
      ? row.durationMs === null
        ? 'legacy duration N/A'
        : `${formatDuration(row.durationMs)} legacy record duration`
      : 'point event (no span interval)'
    : row.durationMs === null
      ? 'duration N/A'
      : formatDuration(row.durationMs);
  return [
    `component ${row.component ?? '—'}`,
    `operation ${row.label}`,
    `offset ${offset}`,
    `duration ${duration}`,
    `evidence seq ${row.seq ?? '—'}`,
    row.origin === 'span' ? 'timed span' : row.origin === 'legacy' ? 'legacy point marker' : 'point event',
  ].join(' · ');
}

function statusTone(row: WaterfallRow): string {
  const status = (row.status ?? '').toLowerCase();
  if (/error|fail|fatal|panic|abort/.test(status)) return 'tone--bad';
  if (/warn|cancel|timeout|deadline|pending|incomplete|degrad/.test(status)) {
    return 'tone--warn';
  }
  if (row.isPointEvent) {
    return row.tickTone === 'bad'
      ? 'tone--bad'
      : row.tickTone === 'warn'
        ? 'tone--warn'
        : row.tickTone === 'observed'
          ? 'tone--observed'
          : 'tone--muted';
  }
  return 'tone--muted';
}

function barClass(tone: WaterfallTone): string {
  return `wf-bar wf-bar--${tone}`;
}

function tickClass(row: WaterfallRow): string {
  return `wf-tick wf-tick--${row.tickTone}`;
}
</script>

<template>
  <div class="waterfall">
    <div v-if="domain === null" class="waterfall-empty">
      No span or event carries a recorded timestamp; no timeline is invented.
    </div>
    <template v-else>
      <div class="waterfall-scale">
        <span class="nai-numbers">{{ formatTimestamp(domain.startMs) }}</span>
        <span class="scale-span nai-numbers">{{ formatDuration(domain.spanMs) }} observed window</span>
        <span class="nai-numbers">{{ formatTimestamp(domain.endMs) }}</span>
      </div>

      <div class="waterfall-legend" aria-label="Waterfall legend">
        <span class="wf-legend-item"
          ><i class="wf-swatch wf-swatch--request" aria-hidden="true" />REQUEST</span
        >
        <span class="wf-legend-item"
          ><i class="wf-swatch wf-swatch--tool" aria-hidden="true" />TOOL</span
        >
        <span class="wf-legend-item"
          ><i class="wf-swatch wf-swatch--log" aria-hidden="true" />LOG</span
        >
        <span class="wf-legend-item"
          ><i class="wf-swatch wf-swatch--terminal" aria-hidden="true" />TERMINAL</span
        >
        <span class="wf-legend-item"
          ><i class="wf-swatch wf-swatch--error" aria-hidden="true" />ERROR</span
        >
        <span class="wf-legend-item"
          ><i class="wf-tick wf-tick--observed wf-legend-tick" aria-hidden="true" />POINT EVENT</span
        >
        <span class="waterfall-counts nai-numbers">
          {{ spanCount }} spans · {{ eventCount }} events
          <template v-if="legacyMode"> · legacy correlation only</template>
        </span>
      </div>

      <div v-if="legacyMode" class="waterfall-note">
        Legacy correlation exposes lifecycle records without span ids; every row is a point
        marker. No duration bar is drawn and no span is invented.
      </div>
      <div v-if="trace.state === 'open'" class="waterfall-note tone--warn">
        open trace — no terminal span was observed; <code>ok</code> is unknown.
      </div>

      <div class="waterfall-scroll">
        <ol class="waterfall-rows">
          <li
            v-for="(row, index) in rowViews"
            :key="`${trace.traceId}-${row.key}-${index}`"
            class="waterfall-row"
            :class="`waterfall-row--${row.origin}`"
          >
            <span class="wf-seq nai-numbers">seq {{ row.seq ?? '—' }}</span>
            <span class="wf-origin" :class="`wf-origin--${row.origin}`">{{ originLabel(row) }}</span>
            <span class="wf-component" :title="row.component ?? 'component not recorded'">{{
              row.component ?? '—'
            }}</span>
            <span class="wf-label" :title="row.label">{{ row.label }}</span>
            <span class="wf-track" aria-hidden="false">
              <span
                v-if="row.isPointEvent && row.tickLeft !== null"
                :class="tickClass(row)"
                :style="{ left: row.tickLeft }"
                :title="rowTitle(row)"
              />
              <span
                v-else-if="row.geometry !== null"
                :class="barClass(row.tone)"
                :style="{ left: barLeft(row.geometry), width: barWidth(row.geometry) }"
                :title="rowTitle(row)"
              />
              <span
                v-else-if="!row.isPointEvent && row.tickLeft !== null"
                class="wf-unknown"
                :style="{ left: row.tickLeft }"
                :title="`${rowTitle(row)} · span duration not recorded`"
              />
              <span v-if="row.tickLeft === null" class="wf-no-ts">ts not recorded</span>
            </span>
            <span class="wf-offset nai-numbers" :title="rowTitle(row)">{{
              offsetText(row.offsetMs)
            }}</span>
            <span
              class="wf-duration nai-numbers"
              :class="{
                'wf-duration--unknown': row.durationUnknown && !row.isPointEvent,
                'wf-duration--event': row.isPointEvent,
              }"
              :title="rowDurationTitle(row)"
              >{{ rowDurationText(row) }}</span
            >
            <span class="wf-status" :class="statusTone(row)">{{ row.status ?? '—' }}</span>
          </li>
        </ol>
      </div>

      <div v-if="missingTimestamp > 0" class="waterfall-note">
        {{ missingTimestamp }} record{{ missingTimestamp === 1 ? '' : 's' }} without a recorded
        timestamp {{ missingTimestamp === 1 ? 'is' : 'are' }} listed without a marker.
      </div>
    </template>
  </div>
</template>

<style scoped>
.waterfall {
  display: flex;
  flex-direction: column;
  gap: var(--nai-space-2);
  padding: var(--nai-space-2) var(--nai-space-4) var(--nai-space-3);
  background: var(--nai-panel);
  font-size: 11px;
}

.waterfall-empty,
.waterfall-note {
  padding: var(--nai-space-2) 0;
  color: var(--nai-dim);
  font-size: 10px;
}

.waterfall-note code {
  color: var(--nai-search);
  font-family: inherit;
}

.waterfall-scale {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  color: var(--nai-dim);
  font-size: 9px;
}

.scale-span {
  color: var(--nai-muted);
  text-transform: uppercase;
  letter-spacing: 0.5px;
}

.waterfall-legend {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: var(--nai-space-4);
  padding: var(--nai-space-2) 0;
  border-top: var(--nai-hairline) solid var(--nai-row-line);
  border-bottom: var(--nai-hairline) solid var(--nai-row-line);
  color: var(--nai-dim);
  font-size: 8px;
  letter-spacing: 0.4px;
}

.wf-legend-item {
  display: inline-flex;
  align-items: center;
  gap: 4px;
}

.wf-swatch {
  display: inline-block;
  width: 10px;
  height: 5px;
}

.wf-swatch--request {
  background: var(--nai-accent);
}

.wf-swatch--tool {
  background: var(--nai-search);
}

.wf-swatch--log {
  background: var(--nai-dim);
}

.wf-swatch--terminal {
  background: var(--nai-ok);
}

.wf-swatch--error {
  background: var(--nai-bad);
}

.wf-legend-tick {
  position: static;
  transform: rotate(45deg);
  margin: 0;
}

.waterfall-counts {
  margin-left: auto;
  color: var(--nai-muted);
  font-size: 9px;
  letter-spacing: 0;
}

.waterfall-scroll {
  overflow-x: auto;
  scrollbar-gutter: stable;
}

.waterfall-rows {
  list-style: none;
  margin: 0;
  padding: 0;
  min-width: 760px;
}

.waterfall-row {
  display: grid;
  grid-template-columns: 62px 48px 108px minmax(120px, 1fr) minmax(180px, 2.6fr) 62px 58px 70px;
  align-items: center;
  gap: var(--nai-space-3);
  min-height: 20px;
  border-bottom: 1px dotted var(--nai-bar-track);
}

.wf-seq,
.wf-offset {
  color: var(--nai-dim);
  text-align: right;
}

.wf-origin {
  color: var(--nai-accent);
  font-size: 8px;
  letter-spacing: 0.4px;
}

.wf-origin--event {
  color: var(--nai-search);
}

.wf-origin--legacy {
  color: var(--nai-dim);
}

.wf-component {
  color: var(--nai-muted);
  font-size: 10px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.wf-label {
  color: var(--nai-fg-soft);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.wf-track {
  position: relative;
  height: 12px;
  border: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-bar-track);
  overflow: hidden;
}

.wf-bar {
  position: absolute;
  top: 1px;
  bottom: 1px;
  min-width: 1px;
}

.wf-bar--request {
  background: var(--nai-accent);
}

.wf-bar--tool {
  background: var(--nai-search);
}

.wf-bar--terminal {
  background: var(--nai-ok);
}

.wf-bar--log {
  background: var(--nai-dim);
}

.wf-bar--error {
  background: var(--nai-bad);
}

.wf-tick {
  position: absolute;
  top: 50%;
  width: 6px;
  height: 6px;
  margin-top: -3px;
  margin-left: -3px;
  transform: rotate(45deg);
  background: var(--nai-accent);
}

.wf-tick--bad {
  background: var(--nai-bad);
}

.wf-tick--warn {
  background: var(--nai-warn);
}

.wf-tick--muted {
  background: var(--nai-dim);
}

.wf-tick--observed {
  background: var(--nai-accent);
}

.wf-unknown {
  position: absolute;
  top: 1px;
  bottom: 1px;
  width: 1px;
  margin-left: -1px;
  border-left: 1px dashed var(--nai-muted);
}

.wf-no-ts {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  padding-left: var(--nai-space-2);
  color: var(--nai-dim);
  font-size: 8px;
  text-transform: uppercase;
  letter-spacing: 0.4px;
}

.wf-duration {
  color: var(--nai-muted);
  text-align: right;
}

.wf-duration--unknown {
  color: var(--nai-warn);
}

.wf-duration--event {
  color: var(--nai-dim);
}

.wf-status {
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.3px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.tone--observed {
  color: var(--nai-accent);
}

.tone--warn {
  color: var(--nai-warn);
}

.tone--bad {
  color: var(--nai-bad);
}

.tone--muted {
  color: var(--nai-dim);
}

.tone--ok {
  color: var(--nai-ok);
}
</style>
