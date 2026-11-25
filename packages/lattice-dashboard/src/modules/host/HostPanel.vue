<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted } from 'vue';
import type { LatticeChartOption } from '../../charts/echarts';
import {
  HOST_METRICS_INTERVAL_MS,
  HOST_SAMPLE_LIMIT,
  HostMetricsStore,
  buildHostChartOption,
  gpuMemoryPercent,
  memoryUsedPercent,
  type HostGpuMetrics,
  type HostNpuMetrics,
} from '../../host/host-metrics';
import { useNow } from '../../shell/useNow';
import LazyChart from '../../ui/LazyChart.vue';
import {
  formatAge,
  formatBytes,
  formatCount,
  formatDateTime,
  formatPercent,
} from '../../ui/format';

defineProps<{ instanceId?: string }>();

const store = new HostMetricsStore();
const now = useNow();

const metrics = store.metrics;
const history = store.history;
const error = store.error;
const stale = store.stale;
const loading = store.loading;
const unsupported = store.unsupported;
const lastFetchedAt = store.lastFetchedAt;

const age = computed(() => formatAge(lastFetchedAt.value, now.value));
/**
 * Every host-reported adapter. `parseHostMetrics` already folds a legacy `gpu`
 * into `gpus`; this second fallback keeps hand-built/host-payload edge cases
 * honest without rendering an empty list over a real primary device. The
 * legacy `gpu` field remains the single primary sampler behind the chart.
 */
const gpuDevices = computed<HostGpuMetrics[]>(() => {
  const list = metrics.value?.gpus ?? [];
  if (list.length > 0) return list;
  const legacy = metrics.value?.gpu;
  return legacy === null || legacy === undefined ? [] : [legacy];
});
const npu = computed<HostNpuMetrics | null>(() => metrics.value?.npu ?? null);
const cpuPct = computed(() => metrics.value?.cpu?.utilizationPct ?? null);
const ramPct = computed(() => memoryUsedPercent(metrics.value?.memory ?? null));
const memoryUsedBytes = computed(() => {
  const memory = metrics.value?.memory;
  if (memory === null || memory === undefined) return null;
  if (memory.totalBytes === null || memory.availableBytes === null) return null;
  const used = memory.totalBytes - memory.availableBytes;
  return used >= 0 ? used : null;
});

const sourceTone = computed(() => {
  const source = metrics.value?.source ?? 'unknown';
  if (source === 'none') return 'muted';
  if (source === 'nvidia-smi') return 'ok';
  return 'default';
});

const chartOption = computed<LatticeChartOption | null>(() =>
  buildHostChartOption(history.value),
);
const sampleCount = computed(() => history.value.length);
const generatedLabel = computed(() => formatDateTime(metrics.value?.generatedAt ?? null));

function refresh(): void {
  void store.refresh();
}

function percentWidth(value: number | null): string {
  if (value === null || !Number.isFinite(value)) return '0%';
  return `${Math.max(0, Math.min(100, value))}%`;
}

/** A device exposes its own utilization/memory/temperature only when measured. */
function hasGpuMetrics(device: HostGpuMetrics): boolean {
  return (
    device.utilizationPct !== null ||
    device.memoryUsedBytes !== null ||
    device.memoryTotalBytes !== null ||
    device.temperatureC !== null
  );
}

function hasGpuMemory(device: HostGpuMetrics): boolean {
  return device.memoryUsedBytes !== null || device.memoryTotalBytes !== null;
}

function deviceMeta(vendor: string | null, source: string | null): string {
  const parts = [vendor, source].filter(
    (part): part is string => part !== null && part.length > 0,
  );
  return parts.length > 0 ? parts.join(' · ') : 'vendor/source not reported';
}

onMounted(() => store.start());
onBeforeUnmount(() => store.dispose());
</script>

<template>
  <section class="host-panel" aria-label="Host CPU, memory and GPU measurements">
    <div class="panel-toolbar">
      <span class="panel-title">host &amp; gpu</span>
      <span class="source-chip" :class="`tone--${sourceTone}`">
        source {{ metrics?.source ?? 'unknown' }}
      </span>
      <span class="panel-meta nai-numbers">
        {{ formatCount(sampleCount) }}/{{ formatCount(HOST_SAMPLE_LIMIT) }} samples ·
        {{ Math.round(HOST_METRICS_INTERVAL_MS / 1000) }}s poll
      </span>
      <span class="toolbar-spacer" />
      <span v-if="stale" class="tone--warn">stale · {{ age }}</span>
      <span v-else-if="error !== null" class="tone--bad" :title="error">host metrics error</span>
      <span v-else class="panel-meta">measured {{ age }}</span>
      <button type="button" :disabled="loading" @click="refresh">REFRESH</button>
    </div>

    <div v-if="unsupported" class="panel-state panel-state--unsupported" role="status">
      <strong>host metrics not wired</strong>
      <span>
        This host does not serve <code>/lattice/api/host-metrics</code> (HTTP 404). No CPU, memory
        or GPU value is invented here.
      </span>
    </div>
    <div v-else-if="metrics === null && error !== null" class="panel-state panel-state--error" role="alert">
      <span>{{ error }}</span>
      <button type="button" @click="refresh">RETRY</button>
    </div>
    <div v-else-if="metrics === null" class="panel-state">
      <span v-if="loading">Loading host measurements…</span>
      <span v-else>Host measurements are unavailable.</span>
    </div>

    <div v-else class="host-scroll nai-scroll">
      <section class="block block--devices">
        <div class="block-head">
          <h2>gpu</h2>
          <span v-if="gpuDevices.length === 0" class="gpu-absent">no NVIDIA sampler on this host</span>
          <span v-else class="panel-meta">
            {{ formatCount(gpuDevices.length) }} adapter{{ gpuDevices.length === 1 ? '' : 's' }}
          </span>
          <span v-if="metrics.source === 'none'" class="gpu-absent">source none</span>
          <span v-else class="panel-meta">{{ metrics.source }}</span>
          <span class="toolbar-spacer" />
          <span class="panel-meta">sample {{ generatedLabel }}</span>
        </div>

        <div v-if="gpuDevices.length === 0" class="honest-gap" role="status">
          The host reported <code>"gpu": null</code>. GPU utilization, memory and temperature stay
          unknown; nothing is synthesized.
        </div>
        <div v-else class="device-list">
          <article
            v-for="(device, index) in gpuDevices"
            :key="device.id ?? `gpu-${index}`"
            class="device-block"
            :data-device-id="device.id ?? undefined"
          >
            <div class="device-head">
              <span class="device-name">{{ device.name ?? 'name not reported' }}</span>
              <span class="device-meta">{{ deviceMeta(device.vendor, device.source) }}</span>
            </div>

            <div v-if="hasGpuMetrics(device)" class="card-grid">
              <article v-if="device.utilizationPct !== null" class="metric-card">
                <span class="metric-label">utilization</span>
                <span class="metric-value nai-numbers">{{ formatPercent(device.utilizationPct) }}</span>
                <span class="meter" aria-hidden="true">
                  <i :style="{ width: percentWidth(device.utilizationPct) }" />
                </span>
              </article>
              <article v-if="hasGpuMemory(device)" class="metric-card">
                <span class="metric-label">memory</span>
                <span class="metric-value nai-numbers">
                  {{ formatBytes(device.memoryUsedBytes) }} / {{ formatBytes(device.memoryTotalBytes) }}
                </span>
                <span v-if="gpuMemoryPercent(device) !== null" class="meter" aria-hidden="true">
                  <i :style="{ width: percentWidth(gpuMemoryPercent(device)) }" />
                </span>
                <span class="metric-hint nai-numbers">
                  {{ formatPercent(gpuMemoryPercent(device)) }} used
                </span>
              </article>
              <article v-if="device.temperatureC !== null" class="metric-card">
                <span class="metric-label">temperature</span>
                <span class="metric-value nai-numbers">{{ device.temperatureC }} °C</span>
              </article>
            </div>
            <p v-else class="honest-gap honest-gap--device" role="status">
              utilization, memory and temperature are not exposed by this host API.
              <span v-if="device.detail !== null" class="device-detail">{{ device.detail }}</span>
            </p>

            <p v-if="hasGpuMetrics(device) && device.detail !== null" class="device-detail">
              {{ device.detail }}
            </p>
          </article>
        </div>
      </section>

      <section v-if="npu !== null" class="block block--devices">
        <div class="block-head">
          <h2>npu</h2>
          <span class="panel-meta">{{ deviceMeta(npu.vendor, npu.source) }}</span>
          <span class="toolbar-spacer" />
          <span class="panel-meta">sample {{ generatedLabel }}</span>
        </div>

        <article class="device-block device-block--npu" :data-device-id="npu.id ?? undefined">
          <div class="device-head">
            <span class="device-name">{{ npu.name ?? 'NPU name not reported' }}</span>
            <span v-if="npu.status !== null" class="device-status">{{ npu.status }}</span>
            <span class="device-meta">{{ deviceMeta(npu.vendor, npu.source) }}</span>
          </div>
          <div v-if="npu.utilizationPct !== null" class="card-grid">
            <article class="metric-card">
              <span class="metric-label">utilization</span>
              <span class="metric-value nai-numbers">{{ formatPercent(npu.utilizationPct) }}</span>
              <span class="meter" aria-hidden="true">
                <i :style="{ width: percentWidth(npu.utilizationPct) }" />
              </span>
            </article>
          </div>
          <p v-else class="honest-gap honest-gap--device" role="status">
            NPU utilization is not exposed by this host API.
            <span v-if="npu.detail !== null" class="device-detail">{{ npu.detail }}</span>
          </p>
          <p v-if="npu.utilizationPct !== null && npu.detail !== null" class="device-detail">
            {{ npu.detail }}
          </p>
        </article>
      </section>

      <section class="block">
        <h2>cpu</h2>
        <div class="card-grid">
          <article class="metric-card">
            <span class="metric-label">utilization</span>
            <span class="metric-value nai-numbers">{{ formatPercent(cpuPct) }}</span>
            <span class="meter" aria-hidden="true">
              <i :style="{ width: percentWidth(cpuPct) }" />
            </span>
          </article>
        </div>
      </section>

      <section class="block">
        <h2>memory</h2>
        <div v-if="metrics.memory === null" class="honest-gap">
          The host did not report a memory block; byte counts stay unknown.
        </div>
        <div v-else class="card-grid">
          <article class="metric-card">
            <span class="metric-label">used / total</span>
            <span class="metric-value nai-numbers">
              {{ formatBytes(memoryUsedBytes) }} / {{ formatBytes(metrics.memory.totalBytes) }}
            </span>
            <span class="meter" aria-hidden="true">
              <i :style="{ width: percentWidth(ramPct) }" />
            </span>
            <span class="metric-hint nai-numbers">{{ formatPercent(ramPct) }} used</span>
          </article>
          <article class="metric-card">
            <span class="metric-label">available</span>
            <span class="metric-value nai-numbers">{{ formatBytes(metrics.memory.availableBytes) }}</span>
          </article>
        </div>
      </section>

      <section class="block block--chart">
        <div class="block-head">
          <h2>last {{ formatCount(sampleCount) }} sample{{ sampleCount === 1 ? '' : 's' }}</h2>
          <span class="panel-meta">measured utilization and memory, gaps stay gaps</span>
        </div>
        <div class="chart-body">
          <div v-if="chartOption === null" class="chart-state">
            No numeric utilization sample has been measured yet.
          </div>
          <LazyChart
            v-else
            :option="chartOption"
            aria-label="Host CPU and GPU utilization timeseries"
          />
        </div>
      </section>
    </div>
  </section>
</template>

<style scoped>
.host-panel {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
  background: var(--nai-panel-2);
}

.panel-toolbar {
  display: flex;
  align-items: center;
  gap: var(--nai-space-4);
  flex: 0 0 auto;
  min-height: var(--nai-toolbar-h);
  padding: 0 var(--nai-space-4);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel);
}

.panel-title {
  color: var(--nai-accent);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.8px;
}

.panel-meta {
  color: var(--nai-dim);
  font-size: 9px;
  white-space: nowrap;
}

.source-chip {
  padding: 0 var(--nai-space-3);
  border: var(--nai-hairline) solid var(--nai-line-strong);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.5px;
  white-space: nowrap;
}

.toolbar-spacer {
  flex: 1 1 auto;
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
}

button:hover:not(:disabled) {
  color: var(--nai-accent);
  border-color: var(--nai-accent);
  background: var(--nai-hover);
}

button:disabled {
  opacity: 0.45;
}

.panel-state {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: var(--nai-space-3);
  flex: 1 1 auto;
  padding: var(--nai-space-5);
  color: var(--nai-muted);
  font-size: var(--nai-font-size-sm);
  text-align: center;
}

.panel-state--error {
  color: var(--nai-bad);
}

.panel-state--unsupported strong {
  color: var(--nai-fg);
  font-weight: 400;
  text-transform: uppercase;
  letter-spacing: 0.6px;
  font-size: 10px;
}

.host-scroll {
  flex: 1 1 auto;
  min-height: 0;
  overflow-x: hidden;
  overflow-y: auto;
  overscroll-behavior: contain;
  padding-bottom: var(--nai-space-5);
  scrollbar-gutter: stable;
}

.block {
  padding: var(--nai-space-4) var(--nai-space-5);
  border-bottom: var(--nai-hairline) solid var(--nai-line);
}

.block--chart {
  display: flex;
  flex-direction: column;
  min-height: 220px;
}

.block h2 {
  margin: 0 0 var(--nai-space-3);
  color: var(--nai-accent);
  font-size: 9px;
  font-weight: 400;
  text-transform: uppercase;
  letter-spacing: 0.8px;
}

.block-head {
  display: flex;
  align-items: baseline;
  gap: var(--nai-space-4);
  margin-bottom: var(--nai-space-3);
}

.block-head h2 {
  margin: 0;
}

.gpu-absent {
  color: var(--nai-muted);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.5px;
}

.device-list {
  display: flex;
  flex-direction: column;
  gap: var(--nai-space-3);
}

.device-block {
  min-width: 0;
  padding: var(--nai-space-3) var(--nai-space-4);
  border: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel);
}

.device-block--npu {
  border-left: 2px solid var(--nai-accent);
}

.device-head {
  display: flex;
  align-items: baseline;
  gap: var(--nai-space-3);
  min-width: 0;
  margin-bottom: var(--nai-space-2);
}

.device-name {
  min-width: 0;
  overflow: hidden;
  color: var(--nai-fg);
  font-size: var(--nai-font-size-sm);
  text-overflow: ellipsis;
  white-space: nowrap;
}

.device-meta {
  flex: 0 0 auto;
  color: var(--nai-dim);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.5px;
  white-space: nowrap;
}

.device-status {
  padding: 0 var(--nai-space-3);
  border: var(--nai-hairline) solid var(--nai-line-strong);
  color: var(--nai-fg-soft);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.5px;
}

.device-detail {
  display: block;
  margin-top: var(--nai-space-2);
  color: var(--nai-muted);
  font-size: 10px;
  line-height: 1.45;
  overflow-wrap: anywhere;
}

.honest-gap--device {
  margin-top: 0;
}

.card-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(170px, 1fr));
  gap: var(--nai-space-3);
}

.metric-card {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
  padding: var(--nai-space-3) var(--nai-space-4);
  border: var(--nai-hairline) solid var(--nai-line);
  background: var(--nai-panel);
}

.metric-label {
  color: var(--nai-dim);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.5px;
}

.metric-value {
  color: var(--nai-fg);
  font-size: 16px;
  font-weight: 700;
  line-height: 1.25;
  overflow: hidden;
  text-overflow: ellipsis;
}

.metric-value--name {
  font-size: var(--nai-font-size-sm);
  font-weight: 400;
  color: var(--nai-fg-soft);
  white-space: nowrap;
}

.metric-hint {
  color: var(--nai-dim);
  font-size: 9px;
}

.meter {
  display: block;
  height: 4px;
  margin-top: 2px;
  background: var(--nai-bar-track);
  overflow: hidden;
}

.meter i {
  display: block;
  height: 100%;
  background: var(--nai-bar-start);
}

.honest-gap {
  margin: 0;
  color: var(--nai-muted);
  font-size: 10px;
  line-height: 1.5;
}

.honest-gap code {
  color: var(--nai-search);
  font-family: inherit;
}

.chart-body {
  position: relative;
  flex: 1 1 auto;
  min-height: 160px;
}

.chart-state {
  display: flex;
  align-items: center;
  justify-content: center;
  height: 100%;
  min-height: 120px;
  color: var(--nai-dim);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.5px;
  text-align: center;
}

.tone--ok {
  color: var(--nai-ok);
}

.tone--warn {
  color: var(--nai-warn);
}

.tone--bad {
  color: var(--nai-bad);
}

.tone--muted,
.tone--default {
  color: var(--nai-dim);
}
</style>
