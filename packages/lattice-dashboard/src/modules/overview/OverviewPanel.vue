<script setup lang="ts">
import { computed, inject, onBeforeUnmount, onMounted, ref, shallowRef, watch } from 'vue';
import { APP_CONTEXT_KEY } from '../../app-context';
import type { LatticeChartOption } from '../../charts/echarts';
import { CHART_COLORS, CHART_TEXT_STYLE } from '../../charts/palette';
import LazyChart from '../../ui/LazyChart.vue';
import { isAbortError } from '../../query/client';
import {
  buildLatencySeries,
  type LatencySeries,
} from '../../analytics/latency';
import {
  buildVolumeSeries,
  loadVolumeWindow,
  VOLUME_CHOICES,
  VOLUME_DEFAULT_RECORDS,
  type VolumeSeries,
  type VolumeWindow,
} from '../../analytics/volume';
import { useNow } from '../../shell/useNow';
import {
  formatAge,
  formatClock,
  formatCount,
  formatDateTime,
  formatDuration,
} from '../../ui/format';

defineProps<{ instanceId?: string }>();

const context = inject(APP_CONTEXT_KEY);
if (context === undefined) throw new Error('overview context is missing');

const feed = context.feed;
const connection = feed.connection;
const summaryRef = context.summary.summary;
const summaryError = context.summary.error;
const summaryStale = context.summary.stale;
const summaryLastFetchedAt = context.summary.lastFetchedAt;
const summaryLoading = context.summary.loading;
const now = useNow();

const maxRecords = ref<number>(VOLUME_DEFAULT_RECORDS);

interface VolumeState {
  window: VolumeWindow;
  series: VolumeSeries;
}

const volume = shallowRef<VolumeState | null>(null);
const volumeLoading = ref(false);
const volumeError = shallowRef<string | null>(null);
const volumeFetchedAt = ref<number | null>(null);

const latency = shallowRef<LatencySeries | null>(null);
const tracesWindow = ref<number | null>(null);
const latencyLoading = ref(false);
const latencyError = shallowRef<string | null>(null);
const latencyFetchedAt = ref<number | null>(null);

let volumeController: AbortController | null = null;
let latencyController: AbortController | null = null;
let volumeGeneration = 0;
let latencyGeneration = 0;
let timer: ReturnType<typeof setInterval> | null = null;
let disposed = false;
let adoptedCursor = false;

const BUCKETS = 60;

async function loadVolume(): Promise<void> {
  if (disposed) return;
  const generation = (volumeGeneration += 1);
  volumeController?.abort();
  const controller = new AbortController();
  volumeController = controller;
  volumeLoading.value = true;
  volumeError.value = null;
  try {
    const window = await loadVolumeWindow(context!.query, {
      maxRecords: maxRecords.value,
      highWater: feed.cursor.value > 0 ? feed.cursor.value : null,
      signal: controller.signal,
    });
    if (disposed || generation !== volumeGeneration || controller.signal.aborted) return;
    volume.value = { window, series: buildVolumeSeries(window.records, BUCKETS) };
    volumeFetchedAt.value = Date.now();
  } catch (error) {
    if (disposed || generation !== volumeGeneration || isAbortError(error)) return;
    volumeError.value = error instanceof Error ? error.message : 'volume query failed';
  } finally {
    if (generation === volumeGeneration) volumeLoading.value = false;
  }
}

async function loadLatency(): Promise<void> {
  if (disposed) return;
  const generation = (latencyGeneration += 1);
  latencyController?.abort();
  const controller = new AbortController();
  latencyController = controller;
  latencyLoading.value = true;
  latencyError.value = null;
  try {
    const response = await context!.traces.fetch(controller.signal);
    if (disposed || generation !== latencyGeneration || controller.signal.aborted) return;
    latency.value = buildLatencySeries(response.traces, BUCKETS);
    tracesWindow.value = response.window;
    latencyFetchedAt.value = Date.now();
  } catch (error) {
    if (disposed || generation !== latencyGeneration || isAbortError(error)) return;
    latencyError.value = error instanceof Error ? error.message : 'traces request failed';
  } finally {
    if (generation === latencyGeneration) latencyLoading.value = false;
  }
}

function refreshSummary(): void {
  void context?.summary.refresh(true);
}

const volumeAge = computed(() => formatAge(volumeFetchedAt.value, now.value));
const latencyAge = computed(() => formatAge(latencyFetchedAt.value, now.value));
const summaryAge = computed(() => formatAge(summaryLastFetchedAt.value, now.value));

const volumeWindowLabel = computed(() => {
  const state = volume.value;
  if (state === null) return 'no window loaded';
  const records = state.window.records;
  if (records.length === 0) {
    return state.window.source === 'tail'
      ? `seq ${formatCount(state.window.startAfter)}–${formatCount(feed.cursor.value)} · 0 records`
      : 'retained start · 0 records';
  }
  const first = records[0].runtime_sequence;
  const last = records[records.length - 1].runtime_sequence;
  return `seq ${formatCount(first)}–${formatCount(last)} · ${formatCount(records.length)} records`;
});

const volumeDomainLabel = computed(() => {
  const domain = volume.value?.series.domain ?? null;
  if (domain === null) return 'no timed records';
  return `${formatDateTime(domain.startMs)} → ${formatDateTime(domain.endMs)}`;
});

const volumeOption = computed<LatticeChartOption | null>(() => {
  const series = volume.value?.series;
  if (series === undefined || series.buckets.length === 0) return null;
  const labels = series.buckets.map((bucket) => formatClock(bucket.startMs));
  const interval = Math.max(0, Math.ceil(labels.length / 6) - 1);
  return {
    animation: false,
    backgroundColor: 'transparent',
    textStyle: CHART_TEXT_STYLE,
    color: [CHART_COLORS.accent, CHART_COLORS.warn, CHART_COLORS.bad],
    tooltip: {
      trigger: 'axis',
      axisPointer: { type: 'line', lineStyle: { color: CHART_COLORS.grid } },
      backgroundColor: '#11151c',
      borderColor: CHART_COLORS.grid,
      textStyle: { color: CHART_COLORS.fg, fontSize: 10 },
    },
    legend: {
      top: 0,
      right: 0,
      itemWidth: 8,
      itemHeight: 8,
      textStyle: { color: CHART_COLORS.muted, fontSize: 9 },
    },
    grid: { left: 42, right: 10, top: 26, bottom: 20 },
    xAxis: {
      type: 'category',
      boundaryGap: false,
      data: labels,
      axisLine: { lineStyle: { color: CHART_COLORS.grid } },
      axisTick: { show: false },
      axisLabel: { color: CHART_COLORS.dim, fontSize: 9, interval },
    },
    yAxis: {
      type: 'value',
      minInterval: 1,
      splitLine: { lineStyle: { color: CHART_COLORS.grid } },
      axisLabel: { color: CHART_COLORS.dim, fontSize: 9 },
    },
    series: [
      {
        name: 'info',
        type: 'line',
        symbol: 'none',
        data: series.buckets.map((bucket) => bucket.info),
        lineStyle: { width: 1.5 },
        areaStyle: { opacity: 0.08 },
      },
      {
        name: 'warn',
        type: 'line',
        symbol: 'none',
        data: series.buckets.map((bucket) => bucket.warn),
        lineStyle: { width: 1.5 },
      },
      {
        name: 'error',
        type: 'line',
        symbol: 'none',
        data: series.buckets.map((bucket) => bucket.error),
        lineStyle: { width: 1.5 },
      },
    ],
  };
});

const latencyEmpty = computed(() => latency.value === null || latency.value.insufficient);
const latencyNoTimed = computed(
  () =>
    latency.value !== null &&
    latency.value.measured > 0 &&
    latency.value.buckets.every((bucket) => bucket.count === 0),
);

const latencyOption = computed<LatticeChartOption | null>(() => {
  const series = latency.value;
  if (series === null || series.insufficient || series.buckets.length === 0) return null;
  const labels = series.buckets.map((bucket) => formatClock(bucket.startMs));
  const interval = Math.max(0, Math.ceil(labels.length / 6) - 1);
  return {
    animation: false,
    backgroundColor: 'transparent',
    textStyle: CHART_TEXT_STYLE,
    color: [CHART_COLORS.countBar, CHART_COLORS.accent, CHART_COLORS.warn],
    tooltip: {
      trigger: 'axis',
      axisPointer: { type: 'line', lineStyle: { color: CHART_COLORS.grid } },
      backgroundColor: '#11151c',
      borderColor: CHART_COLORS.grid,
      textStyle: { color: CHART_COLORS.fg, fontSize: 10 },
    },
    legend: {
      top: 0,
      right: 0,
      itemWidth: 8,
      itemHeight: 8,
      textStyle: { color: CHART_COLORS.muted, fontSize: 9 },
    },
    grid: { left: 42, right: 46, top: 26, bottom: 20 },
    xAxis: {
      type: 'category',
      data: labels,
      axisLine: { lineStyle: { color: CHART_COLORS.grid } },
      axisTick: { show: false },
      axisLabel: { color: CHART_COLORS.dim, fontSize: 9, interval },
    },
    yAxis: [
      {
        type: 'value',
        minInterval: 1,
        splitLine: { lineStyle: { color: CHART_COLORS.grid } },
        axisLabel: { color: CHART_COLORS.dim, fontSize: 9 },
        name: 'traces',
        nameTextStyle: { color: CHART_COLORS.dim, fontSize: 9 },
      },
      {
        type: 'value',
        splitLine: { show: false },
        axisLabel: { color: CHART_COLORS.dim, fontSize: 9, formatter: '{value} ms' },
        name: 'latency',
        nameTextStyle: { color: CHART_COLORS.dim, fontSize: 9 },
      },
    ],
    series: [
      {
        name: 'count',
        type: 'bar',
        yAxisIndex: 0,
        data: series.buckets.map((bucket) => bucket.count),
        itemStyle: { color: CHART_COLORS.countBar },
        barMaxWidth: 10,
      },
      {
        name: 'p50',
        type: 'line',
        yAxisIndex: 1,
        symbol: 'none',
        connectNulls: false,
        data: series.buckets.map((bucket) => bucket.p50Ms),
        lineStyle: { width: 1.5 },
      },
      {
        name: 'p95',
        type: 'line',
        yAxisIndex: 1,
        symbol: 'none',
        connectNulls: false,
        data: series.buckets.map((bucket) => bucket.p95Ms),
        lineStyle: { width: 1.5, type: 'dashed' },
      },
    ],
  };
});

const componentEntries = computed(() => {
  const summary = summaryRef.value;
  if (summary === null) return [];
  return Object.entries(summary.components)
    .map(([name, count]) => ({ name, count }))
    .sort((a, b) => b.count - a.count || a.name.localeCompare(b.name));
});

const MAX_COMPONENT_BARS = 12;
const topComponents = computed(() => componentEntries.value.slice(0, MAX_COMPONENT_BARS));
const hiddenComponents = computed(() =>
  Math.max(0, componentEntries.value.length - topComponents.value.length),
);

const componentOption = computed<LatticeChartOption | null>(() => {
  const entries = topComponents.value;
  if (entries.length === 0) return null;
  return {
    animation: false,
    backgroundColor: 'transparent',
    textStyle: CHART_TEXT_STYLE,
    color: [CHART_COLORS.accent],
    tooltip: {
      trigger: 'axis',
      axisPointer: { type: 'shadow', shadowStyle: { color: 'rgba(25,194,216,0.08)' } },
      backgroundColor: '#11151c',
      borderColor: CHART_COLORS.grid,
      textStyle: { color: CHART_COLORS.fg, fontSize: 10 },
    },
    grid: { left: 132, right: 48, top: 6, bottom: 18 },
    xAxis: {
      type: 'value',
      minInterval: 1,
      splitLine: { lineStyle: { color: CHART_COLORS.grid } },
      axisLabel: { color: CHART_COLORS.dim, fontSize: 9 },
    },
    yAxis: {
      type: 'category',
      inverse: true,
      data: entries.map((entry) => entry.name),
      axisLine: { lineStyle: { color: CHART_COLORS.grid } },
      axisTick: { show: false },
      axisLabel: { color: CHART_COLORS.muted, fontSize: 9, width: 124, overflow: 'truncate' },
    },
    series: [
      {
        name: 'records',
        type: 'bar',
        data: entries.map((entry) => entry.count),
        itemStyle: { color: CHART_COLORS.accent },
        barMaxWidth: 10,
        label: {
          show: true,
          position: 'right',
          color: CHART_COLORS.muted,
          fontSize: 9,
        },
      },
    ],
  };
});

const stream = computed(() => summaryRef.value?.stream ?? null);

interface StreamCard {
  id: string;
  label: string;
  value: string;
  tone: 'default' | 'warn' | 'bad';
  hint: string;
}

function countOrDash(value: number | null | undefined): string {
  return value === null || value === undefined ? '—' : formatCount(value);
}

const streamCards = computed<StreamCard[]>(() => {
  const info = stream.value;
  const active = info?.activeSseClients ?? null;
  const max = info?.maxSseClients ?? null;
  const dropped = info?.droppedTotal ?? null;
  return [
    {
      id: 'subscribers',
      label: 'subscribers',
      value: countOrDash(info?.subscribers),
      tone:
        active !== null && max !== null && max > 0 && active >= max
          ? 'warn'
          : 'default',
      hint: 'Active subscribers served by the shared scan broker.',
    },
    {
      id: 'dropped',
      label: 'dropped total',
      value: countOrDash(dropped),
      tone: dropped !== null && dropped > 0 ? 'bad' : 'default',
      hint: 'Cumulative events dropped by bounded per-subscriber backpressure.',
    },
    {
      id: 'scans',
      label: 'broker scans',
      value: countOrDash(info?.brokerScansTotal),
      tone: 'default',
      hint: 'Ledger scans performed by the shared broker (not UI ticks).',
    },
    {
      id: 'sse',
      label: 'active / max SSE',
      value:
        active === null && max === null ? '—' : `${countOrDash(active)} / ${countOrDash(max)}`,
      tone:
        active !== null && max !== null && max > 0 && active >= max
          ? 'warn'
          : 'default',
      hint: 'Connected SSE clients against the host subscriber cap.',
    },
  ];
});

const summaryWindow = computed(() => summaryRef.value?.window ?? null);

watch(
  () => feed.cursor.value,
  (value) => {
    if (adoptedCursor || value <= 0) return;
    const source = volume.value?.window.source;
    if (source === 'tail') {
      adoptedCursor = true;
      return;
    }
    adoptedCursor = true;
    void loadVolume();
  },
);

watch(maxRecords, () => {
  void loadVolume();
});

onMounted(() => {
  void loadVolume();
  void loadLatency();
  timer = setInterval(() => {
    if (typeof document !== 'undefined' && document.visibilityState === 'hidden') return;
    void loadVolume();
    void loadLatency();
  }, 30_000);
});

onBeforeUnmount(() => {
  disposed = true;
  if (timer !== null) {
    clearInterval(timer);
    timer = null;
  }
  volumeGeneration += 1;
  latencyGeneration += 1;
  volumeController?.abort();
  volumeController = null;
  latencyController?.abort();
  latencyController = null;
});
</script>

<template>
  <section class="overview-panel" aria-label="Overview analytics">
    <div class="overview-grid">
      <article class="overview-card">
        <header class="card-head">
          <span class="card-title">volume + errors over time</span>
          <span class="card-meta">{{ volumeWindowLabel }}</span>
          <span class="card-spacer" />
          <label class="window-select">
            <span>last</span>
            <select v-model.number="maxRecords" :disabled="volumeLoading">
              <option v-for="choice in VOLUME_CHOICES" :key="choice" :value="choice">
                {{ formatCount(choice) }}
              </option>
            </select>
          </label>
          <button type="button" :disabled="volumeLoading" @click="loadVolume">REFRESH</button>
        </header>
        <div class="card-body">
          <div v-if="volumeError !== null" class="card-state card-state--error" role="alert">
            <span>{{ volumeError }}</span>
            <button type="button" @click="loadVolume">RETRY</button>
          </div>
          <div v-else-if="volume === null" class="card-state">
            <span>{{ volumeLoading ? 'Querying the host ledger…' : 'No volume window loaded.' }}</span>
          </div>
          <div v-else-if="volumeOption === null" class="card-state">
            <span>No timed records in the bounded window.</span>
          </div>
          <LazyChart v-else :option="volumeOption" aria-label="Volume and errors over time" />
        </div>
        <footer class="card-foot">
          <span>{{ volumeDomainLabel }} · 60 buckets</span>
          <span v-if="volume !== null" class="nai-numbers">
            scanned {{ formatCount(volume.window.scanned) }} · info {{ formatCount(volume.series.totals.info) }}
            · warn {{ formatCount(volume.series.totals.warn) }} · error
            {{ formatCount(volume.series.totals.error) }}
          </span>
          <span v-if="volume !== null && volume.series.untimed > 0" class="tone--warn">
            {{ formatCount(volume.series.untimed) }} record(s) without a recorded timestamp are not charted
          </span>
          <span v-if="volume !== null && volume.window.truncated" class="tone--warn">
            bounded scan stopped early; the newest measured records are shown
          </span>
          <span v-if="connection !== 'live'" class="tone--warn">
            stream {{ connection }} · the window ends at the last observed cursor
          </span>
          <span class="muted">info groups trace/debug/info. measured {{ volumeAge }}</span>
        </footer>
      </article>

      <article class="overview-card">
        <header class="card-head">
          <span class="card-title">closed-trace latency</span>
          <span class="card-meta">{{ tracesWindow === null ? 'window —' : `window ${formatCount(tracesWindow)}` }}</span>
          <span class="card-spacer" />
          <button type="button" :disabled="latencyLoading" @click="loadLatency">REFRESH</button>
        </header>
        <div class="card-body">
          <div v-if="latencyError !== null" class="card-state card-state--error" role="alert">
            <span>{{ latencyError }}</span>
            <button type="button" @click="loadLatency">RETRY</button>
          </div>
          <div v-else-if="latency === null" class="card-state">
            <span>{{ latencyLoading ? 'Loading traces…' : 'No traces window loaded.' }}</span>
          </div>
          <div v-else-if="latencyEmpty" class="card-state">
            <span>insufficient closed traces</span>
            <span class="muted">
              {{ formatCount(latency.excludedOpen) }} open · {{ formatCount(latency.excludedNullDuration) }}
              closed without a recorded duration
            </span>
          </div>
          <div v-else-if="latencyOption === null" class="card-state">
            <span>Closed traces carry durations but no recorded timestamp; overall p50/p95 are measured.</span>
          </div>
          <LazyChart v-else :option="latencyOption" aria-label="Closed trace latency p50 and p95" />
        </div>
        <footer class="card-foot">
          <span v-if="latency !== null" class="nai-numbers">
            p50 {{ formatDuration(latency.overall.p50Ms) }} · p95
            {{ formatDuration(latency.overall.p95Ms) }} · count {{ formatCount(latency.overall.count) }}
          </span>
          <span v-if="latency !== null && latency.untimed > 0" class="tone--warn">
            {{ formatCount(latency.untimed) }} measured trace(s) without a timestamp are not bucketed
          </span>
          <span v-if="latency !== null && latency.excludedNullDuration > 0" class="muted">
            {{ formatCount(latency.excludedNullDuration) }} closed trace(s) had no durationMs and are excluded
          </span>
          <span class="muted">measured {{ latencyAge }}</span>
          <span v-if="latencyNoTimed" class="muted">time buckets are empty</span>
        </footer>
      </article>

      <article class="overview-card">
        <header class="card-head">
          <span class="card-title">component activity</span>
          <span class="card-meta">
            summary window {{ formatCount(summaryWindow) }} · {{ formatCount(componentEntries.length) }} components
          </span>
          <span class="card-spacer" />
          <button type="button" :disabled="summaryLoading" @click="refreshSummary">REFRESH</button>
        </header>
        <div class="card-body">
          <div v-if="summaryRef === null && summaryError !== null" class="card-state card-state--error" role="alert">
            <span>{{ summaryError }}</span>
            <button type="button" @click="refreshSummary">RETRY</button>
          </div>
          <div v-else-if="summaryRef === null" class="card-state">
            <span>{{ summaryLoading ? 'Loading host summary…' : 'Host summary is unavailable.' }}</span>
          </div>
          <div v-else-if="componentOption === null" class="card-state">
            <span>No component activity in the bounded summary window.</span>
          </div>
          <LazyChart v-else :option="componentOption" aria-label="Component activity" />
        </div>
        <footer class="card-foot">
          <span>
            Top {{ Math.min(MAX_COMPONENT_BARS, componentEntries.length) }} of
            {{ formatCount(componentEntries.length) }} measured component(s).
          </span>
          <span v-if="hiddenComponents > 0" class="muted">
            {{ formatCount(hiddenComponents) }} more not charted; open Components for the full list.
          </span>
          <span class="muted">measured {{ summaryAge }}</span>
          <span v-if="summaryStale" class="tone--warn">stale · last measured values retained</span>
          <span v-else-if="summaryError !== null" class="tone--bad">summary unavailable</span>
        </footer>
      </article>

      <article class="overview-card">
        <header class="card-head">
          <span class="card-title">stream health</span>
          <span class="card-meta">summary stream</span>
          <span class="card-spacer" />
          <button type="button" :disabled="summaryLoading" @click="refreshSummary">REFRESH</button>
        </header>
        <div class="card-body stream-body">
          <div v-if="summaryRef === null" class="card-state">
            <span>{{ summaryLoading ? 'Loading host summary…' : 'Host summary is unavailable.' }}</span>
          </div>
          <div v-else class="stream-grid">
            <div v-for="card in streamCards" :key="card.id" class="stream-card" :class="`stream-card--${card.tone}`">
              <span class="stream-label">{{ card.label }}</span>
              <span class="stream-value nai-numbers">{{ card.value }}</span>
              <span class="stream-hint">{{ card.hint }}</span>
            </div>
          </div>
        </div>
        <footer class="card-foot">
          <span v-if="stream === null">The host summary did not include a stream block; no counters are invented.</span>
          <span v-else class="nai-numbers">
            pushes {{ countOrDash(stream.pushTotal) }} · last scan
            {{ formatDateTime(stream.lastScanUnixMs) }}
          </span>
          <span class="muted">measured {{ summaryAge }}</span>
        </footer>
      </article>
    </div>
  </section>
</template>

<style scoped>
.overview-panel {
  height: 100%;
  min-height: 0;
  overflow: auto;
  scrollbar-gutter: stable;
  background: var(--nai-panel-2);
}

.overview-grid {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  grid-template-rows: repeat(2, minmax(240px, 1fr));
  gap: var(--nai-hairline);
  min-height: 100%;
  background: var(--nai-line);
}

.overview-card {
  display: flex;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
  background: var(--nai-panel-2);
}

.card-head {
  display: flex;
  align-items: center;
  gap: var(--nai-space-4);
  flex: 0 0 auto;
  min-height: var(--nai-toolbar-h);
  padding: 0 var(--nai-space-4);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel);
}

.card-title {
  color: var(--nai-accent);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.8px;
  white-space: nowrap;
}

.card-meta {
  color: var(--nai-dim);
  font-size: 9px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.card-spacer {
  flex: 1 1 auto;
}

.window-select {
  display: inline-flex;
  align-items: center;
  gap: var(--nai-space-2);
  color: var(--nai-dim);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.5px;
}

.window-select select {
  height: 20px;
  padding: 0 var(--nai-space-2);
  border: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel-2);
  color: var(--nai-fg);
  font-size: 9px;
}

button {
  height: 20px;
  padding: 0 var(--nai-space-4);
  border: var(--nai-hairline) solid var(--nai-line);
  border-radius: 0;
  background: transparent;
  color: var(--nai-muted);
  font-size: 9px;
  letter-spacing: 0.6px;
  text-transform: uppercase;
  white-space: nowrap;
}

button:hover:not(:disabled) {
  color: var(--nai-accent);
  border-color: var(--nai-accent);
  background: var(--nai-hover);
}

button:disabled {
  opacity: 0.45;
}

.card-body {
  position: relative;
  flex: 1 1 auto;
  min-height: 0;
  padding: var(--nai-space-3) var(--nai-space-4) var(--nai-space-2);
}

.card-state {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: var(--nai-space-3);
  height: 100%;
  min-height: 96px;
  padding: var(--nai-space-4);
  color: var(--nai-muted);
  font-size: var(--nai-font-size-sm);
  text-align: center;
}

.card-state--error {
  color: var(--nai-bad);
}

.card-state .muted {
  color: var(--nai-dim);
  font-size: 10px;
}

.card-foot {
  display: flex;
  align-items: center;
  gap: var(--nai-space-4);
  flex: 0 0 auto;
  min-height: 24px;
  padding: 0 var(--nai-space-4);
  border-top: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel);
  color: var(--nai-dim);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.3px;
  overflow: hidden;
}

.card-foot > span {
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.stream-body {
  display: flex;
}

.stream-grid {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: var(--nai-hairline);
  width: 100%;
  align-content: stretch;
  background: var(--nai-line);
}

.stream-card {
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 0;
  padding: var(--nai-space-3) var(--nai-space-4);
  background: var(--nai-panel);
}

.stream-label {
  color: var(--nai-muted);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.6px;
}

.stream-value {
  color: var(--nai-fg);
  font-size: 22px;
  font-weight: 700;
  line-height: 1.2;
}

.stream-hint {
  color: var(--nai-dim);
  font-size: 9px;
  line-height: 1.35;
}

.stream-card--warn .stream-value {
  color: var(--nai-warn);
}

.stream-card--bad .stream-value {
  color: var(--nai-bad);
}

.muted {
  color: var(--nai-dim);
}

.tone--warn {
  color: var(--nai-warn);
}

.tone--bad {
  color: var(--nai-bad);
}

@media (max-width: 980px) {
  .overview-grid {
    grid-template-columns: 1fr;
    grid-template-rows: repeat(4, minmax(240px, 1fr));
  }
}
</style>
