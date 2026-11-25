import { describe, expect, it } from 'vitest';
import {
  HOST_SAMPLE_LIMIT,
  HostMetricsStore,
  appendHostSample,
  buildHostChartOption,
  gpuMemoryPercent,
  memoryUsedPercent,
  parseHostMetrics,
  sampleFromMetrics,
  type HostSample,
} from '../src/host/host-metrics';

const FULL_PAYLOAD = {
  schema: 1,
  generatedAt: 1_764_000_000_000,
  source: 'nvidia-smi',
  cpu: { utilizationPct: 12.5 },
  memory: { totalBytes: 68_559_708_160, availableBytes: 41_234_567_890 },
  gpu: {
    name: 'NVIDIA GeForce RTX 4090',
    utilizationPct: 5,
    memoryUsedBytes: 2_807_037_952,
    memoryTotalBytes: 24_142_446_592,
    temperatureC: 54,
  },
};

function jsonResponse(payload: unknown, status = 200): Response {
  return new Response(JSON.stringify(payload), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

interface LineSeries {
  name: string;
  data: Array<number | null>;
}

function seriesOf(option: unknown): LineSeries[] {
  return (option as { series: LineSeries[] }).series;
}

describe('host metrics shaping', () => {
  it('keeps real GPU, CPU and memory measurements', () => {
    const metrics = parseHostMetrics(FULL_PAYLOAD);
    expect(metrics.source).toBe('nvidia-smi');
    expect(metrics.generatedAt).toBe(1_764_000_000_000);
    expect(metrics.cpu?.utilizationPct).toBe(12.5);
    expect(metrics.memory?.totalBytes).toBe(68_559_708_160);
    expect(metrics.gpu).toMatchObject(FULL_PAYLOAD.gpu);
    expect(metrics.gpu?.id).toBeNull();
    expect(metrics.gpu?.vendor).toBeNull();
    expect(metrics.gpu?.source).toBeNull();
    expect(metrics.gpu?.detail).toBeNull();
    expect(metrics.gpus).toHaveLength(1);
    expect(metrics.npu).toBeNull();
    expect(gpuMemoryPercent(metrics.gpu)).toBeCloseTo(11.627, 2);
    expect(memoryUsedPercent(metrics.memory)).toBeCloseTo(39.86, 1);
  });

  it('renders gpu:null and source:none as unknown, never as zero', () => {
    const metrics = parseHostMetrics({
      schema: 1,
      generatedAt: null,
      source: 'none',
      cpu: { utilizationPct: null },
      memory: null,
      gpu: null,
    });
    expect(metrics.source).toBe('none');
    expect(metrics.gpu).toBeNull();
    expect(metrics.memory).toBeNull();
    expect(metrics.cpu?.utilizationPct).toBeNull();
    expect(gpuMemoryPercent(metrics.gpu)).toBeNull();
    expect(memoryUsedPercent(metrics.memory)).toBeNull();
  });

  it('normalizes malformed fields to null and rejects a foreign schema', () => {
    const metrics = parseHostMetrics({
      schema: 1,
      source: '',
      cpu: { utilizationPct: -1 },
      memory: { totalBytes: -5, availableBytes: 'lots' },
      gpu: { name: '', utilizationPct: 'fast', memoryUsedBytes: -1, temperatureC: 'hot' },
    });
    expect(metrics.source).toBe('unknown');
    expect(metrics.cpu?.utilizationPct).toBeNull();
    expect(metrics.memory?.totalBytes).toBeNull();
    expect(metrics.memory?.availableBytes).toBeNull();
    expect(metrics.gpu?.name).toBeNull();
    expect(metrics.gpu?.utilizationPct).toBeNull();
    expect(metrics.gpu?.memoryUsedBytes).toBeNull();
    expect(metrics.gpu?.temperatureC).toBeNull();
    expect(() => parseHostMetrics({ schema: 2 })).toThrow(/unsupported host metrics schema/);
    expect(() => parseHostMetrics(null)).toThrow(/unexpected payload/);
  });

  it('builds a chart from measured samples only', () => {
    const first = sampleFromMetrics(parseHostMetrics(FULL_PAYLOAD), 1_764_000_002_000);
    const second = sampleFromMetrics(
      parseHostMetrics({
        ...FULL_PAYLOAD,
        generatedAt: 1_764_000_004_000,
        cpu: { utilizationPct: 30 },
        gpu: { ...FULL_PAYLOAD.gpu, utilizationPct: null },
      }),
      1_764_000_004_000,
    );
    const option = buildHostChartOption([first, second]);
    expect(option).not.toBeNull();
    const series = seriesOf(option);
    expect(series.map((entry) => entry.name)).toEqual(['cpu %', 'gpu util %', 'gpu memory %']);
    expect(series[0].data).toEqual([12.5, 30]);
    expect(series[1].data).toEqual([5, null]);
    expect(series[2].data[0]).toBeCloseTo(11.627, 2);

    const empty = sampleFromMetrics(
      parseHostMetrics({ schema: 1, source: 'none', cpu: null, memory: null, gpu: null }),
      1_764_000_006_000,
    );
    expect(buildHostChartOption([empty])).toBeNull();
    expect(buildHostChartOption([])).toBeNull();
  });

  it('does not duplicate a cached host sample and caps the history', () => {
    const sample: HostSample = {
      at: 1_764_000_000_000,
      source: 'nvidia-smi',
      cpuPct: 10,
      gpuPct: 20,
      gpuMemoryPct: 30,
      gpuTemperatureC: 40,
    };
    const once = appendHostSample([], sample);
    expect(appendHostSample(once, sample)).toBe(once);
    let history: readonly HostSample[] = [];
    for (let index = 0; index < HOST_SAMPLE_LIMIT + 5; index += 1) {
      history = appendHostSample(history, { ...sample, at: sample.at + index * 2000 });
    }
    expect(history).toHaveLength(HOST_SAMPLE_LIMIT);
    expect(history[history.length - 1].at).toBe(sample.at + (HOST_SAMPLE_LIMIT + 4) * 2000);
  });
});

describe('host metrics store', () => {
  it('reports a 404 host as "not wired" without inventing values', async () => {
    const fetchImpl = (async () => jsonResponse({}, 404)) as unknown as typeof fetch;
    const store = new HostMetricsStore({ fetchImpl, now: () => 1000 });
    await store.refresh();
    expect(store.unsupported.value).toBe(true);
    expect(store.metrics.value).toBeNull();
    expect(store.history.value).toEqual([]);
    expect(store.error.value).toBeNull();
    expect(store.stale.value).toBe(false);
    store.dispose();
  });

  it('appends one real sample per successful poll and clears unsupported', async () => {
    const responses = [jsonResponse({}, 404), jsonResponse(FULL_PAYLOAD)];
    let call = 0;
    const fetchImpl = (async () =>
      responses[Math.min(call++, responses.length - 1)]) as unknown as typeof fetch;
    const store = new HostMetricsStore({ fetchImpl, now: () => 1_764_000_000_000 });
    await store.refresh();
    expect(store.unsupported.value).toBe(true);
    await store.refresh();
    expect(store.unsupported.value).toBe(false);
    expect(store.metrics.value?.gpu?.name).toBe('NVIDIA GeForce RTX 4090');
    expect(store.history.value).toHaveLength(1);
    expect(store.history.value[0].gpuPct).toBe(5);
    expect(store.lastFetchedAt.value).toBe(1_764_000_000_000);
    store.dispose();
  });

  it('aborts its in-flight request on dispose', async () => {
    let signal: AbortSignal | null = null;
    const fetchImpl = ((_input: RequestInfo | URL, init?: RequestInit) => {
      return new Promise<Response>((_resolve, reject) => {
        signal = init?.signal ?? null;
        signal?.addEventListener('abort', () => {
          const error = new Error('aborted');
          error.name = 'AbortError';
          reject(error);
        });
      });
    }) as unknown as typeof fetch;
    const store = new HostMetricsStore({ fetchImpl });
    const pending = store.refresh();
    expect(signal).not.toBeNull();
    store.dispose();
    expect((signal as AbortSignal | null)?.aborted).toBe(true);
    await pending;
    expect(store.error.value).toBeNull();
  });
});
