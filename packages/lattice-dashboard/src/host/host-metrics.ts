import { shallowRef, type ShallowRef } from 'vue';
import type { LatticeChartOption } from '../charts/echarts';
import { CHART_COLORS, CHART_TEXT_STYLE } from '../charts/palette';
import { isAbortError } from '../query/client';
import { formatClock } from '../ui/format';

/**
 * `GET /lattice/api/host-metrics` (frozen contract §8).
 *
 * Every number is a host measurement. An absent sampler is `null`, never a
 * synthesized zero: `gpu: null` means "no NVIDIA sampler on this host" and
 * `source: "none"` means the host measured no GPU at all.
 */
export interface HostCpuMetrics {
  utilizationPct: number | null;
}

export interface HostMemoryMetrics {
  totalBytes: number | null;
  availableBytes: number | null;
}

export interface HostGpuMetrics {
  id: string | null;
  vendor: string | null;
  source: string | null;
  name: string | null;
  utilizationPct: number | null;
  memoryUsedBytes: number | null;
  memoryTotalBytes: number | null;
  temperatureC: number | null;
  detail: string | null;
}

export interface HostNpuMetrics {
  id: string | null;
  vendor: string | null;
  name: string | null;
  source: string | null;
  status: string | null;
  utilizationPct: number | null;
  detail: string | null;
}

export interface HostMetrics {
  schema: number;
  generatedAt: number | null;
  source: string;
  cpu: HostCpuMetrics | null;
  memory: HostMemoryMetrics | null;
  gpu: HostGpuMetrics | null;
  gpus: HostGpuMetrics[];
  npu: HostNpuMetrics | null;
}

/** One chart/series point. Values stay nullable; a missing value is a gap. */
export interface HostSample {
  /** Host `generatedAt` when present; local receipt time only as a fallback. */
  at: number;
  source: string;
  cpuPct: number | null;
  gpuPct: number | null;
  gpuMemoryPct: number | null;
  gpuTemperatureC: number | null;
}

export const HOST_SAMPLE_LIMIT = 150;
export const HOST_METRICS_INTERVAL_MS = 2000;

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function numberOrNull(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function nonNegativeOrNull(value: unknown): number | null {
  const number = numberOrNull(value);
  return number !== null && number >= 0 ? number : null;
}

function nonEmptyStringOrNull(value: unknown): string | null {
  return typeof value === 'string' && value.length > 0 ? value : null;
}

function parseCpu(value: unknown): HostCpuMetrics | null {
  if (!isRecord(value)) return null;
  return { utilizationPct: nonNegativeOrNull(value['utilizationPct']) };
}

function parseMemory(value: unknown): HostMemoryMetrics | null {
  if (!isRecord(value)) return null;
  return {
    totalBytes: nonNegativeOrNull(value['totalBytes']),
    availableBytes: nonNegativeOrNull(value['availableBytes']),
  };
}

function parseGpu(value: unknown): HostGpuMetrics | null {
  // Explicit null (or an absent field) is the honest "no sampler" state.
  if (value === null || value === undefined) return null;
  if (!isRecord(value)) return null;
  return {
    id: nonEmptyStringOrNull(value['id']),
    vendor: nonEmptyStringOrNull(value['vendor']),
    source: nonEmptyStringOrNull(value['source']),
    name: nonEmptyStringOrNull(value['name']),
    utilizationPct: nonNegativeOrNull(value['utilizationPct']),
    memoryUsedBytes: nonNegativeOrNull(value['memoryUsedBytes']),
    memoryTotalBytes: nonNegativeOrNull(value['memoryTotalBytes']),
    temperatureC: numberOrNull(value['temperatureC']),
    detail: nonEmptyStringOrNull(value['detail']),
  };
}

function parseNpu(value: unknown): HostNpuMetrics | null {
  if (value === null || value === undefined) return null;
  if (!isRecord(value)) return null;
  return {
    id: nonEmptyStringOrNull(value['id']),
    vendor: nonEmptyStringOrNull(value['vendor']),
    name: nonEmptyStringOrNull(value['name']),
    source: nonEmptyStringOrNull(value['source']),
    status: nonEmptyStringOrNull(value['status']),
    utilizationPct: nonNegativeOrNull(value['utilizationPct']),
    detail: nonEmptyStringOrNull(value['detail']),
  };
}

export function parseHostMetrics(payload: unknown): HostMetrics {
  if (!isRecord(payload)) throw new Error('host metrics returned an unexpected payload');
  if (payload['schema'] !== 1) {
    throw new Error(`unsupported host metrics schema: ${String(payload['schema'])}`);
  }
  const gpu = parseGpu(payload['gpu']);
  const rawGpus = payload['gpus'];
  let gpus = Array.isArray(rawGpus)
    ? rawGpus.map(parseGpu).filter((device): device is HostGpuMetrics => device !== null)
    : [];
  if (gpus.length === 0) gpus = gpu === null ? [] : [gpu];
  return {
    schema: 1,
    generatedAt: numberOrNull(payload['generatedAt']),
    source: nonEmptyStringOrNull(payload['source']) ?? 'unknown',
    cpu: parseCpu(payload['cpu']),
    memory: parseMemory(payload['memory']),
    gpu,
    gpus,
    npu: parseNpu(payload['npu']),
  };
}

/** Used GPU memory percentage, derived only when both real byte counts exist. */
export function gpuMemoryPercent(gpu: HostGpuMetrics | null): number | null {
  if (gpu === null) return null;
  const used = gpu.memoryUsedBytes;
  const total = gpu.memoryTotalBytes;
  if (used === null || total === null || total <= 0) return null;
  return (used / total) * 100;
}

/** Same derived percentage for CPU memory, from measured total/available. */
export function memoryUsedPercent(memory: HostMemoryMetrics | null): number | null {
  if (memory === null) return null;
  const total = memory.totalBytes;
  const available = memory.availableBytes;
  if (total === null || available === null || total <= 0) return null;
  const used = total - available;
  return used < 0 ? null : (used / total) * 100;
}

export function sampleFromMetrics(metrics: HostMetrics, receiptNow: number): HostSample {
  return {
    at: metrics.generatedAt ?? receiptNow,
    source: metrics.source,
    cpuPct: metrics.cpu?.utilizationPct ?? null,
    gpuPct: metrics.gpu?.utilizationPct ?? null,
    gpuMemoryPct: gpuMemoryPercent(metrics.gpu),
    gpuTemperatureC: metrics.gpu?.temperatureC ?? null,
  };
}

function sameSample(a: HostSample, b: HostSample): boolean {
  return (
    a.at === b.at &&
    a.source === b.source &&
    a.cpuPct === b.cpuPct &&
    a.gpuPct === b.gpuPct &&
    a.gpuMemoryPct === b.gpuMemoryPct &&
    a.gpuTemperatureC === b.gpuTemperatureC
  );
}

/**
 * Appends one real sample, capped to the newest `limit`. A repeated host
 * `generatedAt` (the host caches at most every 2 s) is not duplicated.
 */
export function appendHostSample(
  history: readonly HostSample[],
  sample: HostSample,
  limit = HOST_SAMPLE_LIMIT,
): readonly HostSample[] {
  const last = history[history.length - 1];
  if (last !== undefined && sameSample(last, sample)) return history;
  const next = [...history, sample];
  return next.length > limit ? next.slice(next.length - limit) : next;
}

interface ChartSeries {
  name: string;
  type: 'line';
  symbol: 'none';
  connectNulls: false;
  data: Array<number | null>;
  lineStyle: { width: number; type?: 'dashed'; color: string };
  itemStyle: { color: string };
}

/** Real utilization/memory series only; an all-null history yields no chart. */
export function buildHostChartOption(history: readonly HostSample[]): LatticeChartOption | null {
  if (history.length === 0) return null;
  const labels = history.map((sample) => formatClock(sample.at));
  const interval = Math.max(0, Math.ceil(labels.length / 6) - 1);
  const series: ChartSeries[] = [];

  const cpu = history.map((sample) => sample.cpuPct);
  if (cpu.some((value) => value !== null)) {
    series.push({
      name: 'cpu %',
      type: 'line',
      symbol: 'none',
      connectNulls: false,
      data: cpu,
      lineStyle: { width: 1.5, color: CHART_COLORS.accentStrong },
      itemStyle: { color: CHART_COLORS.accentStrong },
    });
  }
  const gpu = history.map((sample) => sample.gpuPct);
  if (gpu.some((value) => value !== null)) {
    series.push({
      name: 'gpu util %',
      type: 'line',
      symbol: 'none',
      connectNulls: false,
      data: gpu,
      lineStyle: { width: 1.5, color: CHART_COLORS.ok },
      itemStyle: { color: CHART_COLORS.ok },
    });
  }
  const gpuMemory = history.map((sample) => sample.gpuMemoryPct);
  if (gpuMemory.some((value) => value !== null)) {
    series.push({
      name: 'gpu memory %',
      type: 'line',
      symbol: 'none',
      connectNulls: false,
      data: gpuMemory,
      lineStyle: { width: 1.5, type: 'dashed', color: CHART_COLORS.warn },
      itemStyle: { color: CHART_COLORS.warn },
    });
  }
  if (series.length === 0) return null;

  return {
    animation: false,
    backgroundColor: 'transparent',
    textStyle: CHART_TEXT_STYLE,
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
    grid: { left: 40, right: 12, top: 26, bottom: 20 },
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
      axisLabel: { color: CHART_COLORS.dim, fontSize: 9, formatter: '{value}%' },
    },
    series,
  };
}

export interface HostMetricsStoreOptions {
  url?: string;
  fetchImpl?: typeof fetch;
  intervalMs?: number;
  historyLimit?: number;
  now?: () => number;
}

/**
 * Panel-scoped poller: one request per interval, aborted on unmount. A 404 is
 * an explicit "not wired by this host" state, not an error to retry forever.
 */
export class HostMetricsStore {
  readonly metrics: ShallowRef<HostMetrics | null> = shallowRef(null);
  readonly history: ShallowRef<readonly HostSample[]> = shallowRef<readonly HostSample[]>([]);
  readonly error: ShallowRef<string | null> = shallowRef(null);
  readonly stale: ShallowRef<boolean> = shallowRef(false);
  readonly loading: ShallowRef<boolean> = shallowRef(false);
  readonly unsupported: ShallowRef<boolean> = shallowRef(false);
  readonly lastFetchedAt: ShallowRef<number | null> = shallowRef(null);

  private readonly url: string;
  private readonly fetchImpl: typeof fetch | null;
  private readonly intervalMs: number;
  private readonly historyLimit: number;
  private readonly now: () => number;
  private timer: ReturnType<typeof setInterval> | null = null;
  private controller: AbortController | null = null;
  private visibilityHandler: (() => void) | null = null;
  private inFlight = false;
  private disposed = false;

  constructor(options: HostMetricsStoreOptions = {}) {
    this.url = options.url ?? '/lattice/api/host-metrics';
    this.fetchImpl =
      options.fetchImpl ??
      // `fetch` must keep its Window receiver in a real browser.
      (typeof globalThis.fetch === 'function' ? globalThis.fetch.bind(globalThis) : null);
    this.intervalMs = options.intervalMs ?? HOST_METRICS_INTERVAL_MS;
    this.historyLimit = options.historyLimit ?? HOST_SAMPLE_LIMIT;
    this.now = options.now ?? (() => Date.now());
  }

  start(): void {
    if (this.disposed || this.timer !== null) return;
    void this.refresh();
    this.timer = setInterval(() => {
      if (typeof document !== 'undefined' && document.visibilityState === 'hidden') return;
      void this.refresh();
    }, this.intervalMs);
    if (typeof document !== 'undefined' && typeof document.addEventListener === 'function') {
      this.visibilityHandler = () => {
        if (document.visibilityState === 'visible') void this.refresh();
      };
      document.addEventListener('visibilitychange', this.visibilityHandler);
    }
  }

  async refresh(): Promise<void> {
    if (this.disposed || this.inFlight) return;
    if (this.fetchImpl === null) {
      this.error.value = 'fetch is unavailable in this runtime';
      return;
    }
    this.inFlight = true;
    this.loading.value = true;
    const controller = new AbortController();
    this.controller = controller;
    try {
      const response = await this.fetchImpl(this.url, {
        method: 'GET',
        headers: { Accept: 'application/json' },
        cache: 'no-store',
        credentials: 'same-origin',
        signal: controller.signal,
      });
      if (response.status === 404) {
        if (this.disposed || controller.signal.aborted) return;
        this.unsupported.value = true;
        this.metrics.value = null;
        this.history.value = [];
        this.error.value = null;
        this.stale.value = false;
        this.lastFetchedAt.value = null;
        return;
      }
      if (!response.ok) throw new Error(`host metrics failed: HTTP ${response.status}`);
      const payload: unknown = await response.json();
      const metrics = parseHostMetrics(payload);
      if (this.disposed || controller.signal.aborted) return;
      const receipt = this.now();
      this.metrics.value = metrics;
      this.history.value = appendHostSample(
        this.history.value,
        sampleFromMetrics(metrics, receipt),
        this.historyLimit,
      );
      this.unsupported.value = false;
      this.lastFetchedAt.value = receipt;
      this.error.value = null;
      this.stale.value = false;
    } catch (error) {
      if (this.disposed || controller.signal.aborted || isAbortError(error)) return;
      this.error.value = error instanceof Error ? error.message : 'host metrics failed';
      if (this.metrics.value !== null) this.stale.value = true;
    } finally {
      if (this.controller === controller) this.controller = null;
      this.inFlight = false;
      this.loading.value = false;
    }
  }

  stop(): void {
    if (this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
    if (this.visibilityHandler !== null && typeof document !== 'undefined') {
      document.removeEventListener('visibilitychange', this.visibilityHandler);
      this.visibilityHandler = null;
    }
    if (this.controller !== null) {
      this.controller.abort();
      this.controller = null;
    }
    this.inFlight = false;
    this.loading.value = false;
  }

  dispose(): void {
    this.disposed = true;
    this.stop();
  }
}
