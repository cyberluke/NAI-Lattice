<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from 'vue';
import type {
  LatticeChartInstance,
  LatticeChartOption,
  LatticeChartPick,
} from '../charts/echarts';

/**
 * One ECharts instance for the lifetime of the panel.
 *
 * The ECharts module is imported dynamically only after the host has a real
 * layout size, so the shell entry never pays for the chart engine and jsdom
 * tests never initialise a canvas. `setOption` updates the existing instance;
 * a data tick never re-creates or re-registers it.
 */
const props = defineProps<{
  option: LatticeChartOption;
  ariaLabel?: string;
  /** Optional pick handler (graph node/edge clicks). */
  onPick?: (pick: LatticeChartPick) => void;
}>();

type ChartState = 'pending' | 'ready' | 'unavailable' | 'error';

const host = ref<HTMLElement | null>(null);
const state = ref<ChartState>('pending');
let chart: LatticeChartInstance | null = null;
let observer: ResizeObserver | null = null;
let resizeFrame: number | null = null;
let disposed = false;
let initStarted = false;
let latest: LatticeChartOption = props.option;

function hasLayout(): boolean {
  const element = host.value;
  return element !== null && element.clientWidth > 0 && element.clientHeight > 0;
}

function scheduleResize(): void {
  if (resizeFrame !== null) return;
  const run = (): void => {
    resizeFrame = null;
    if (chart !== null && !disposed && hasLayout()) chart.resize();
  };
  if (typeof requestAnimationFrame === 'function') {
    resizeFrame = requestAnimationFrame(run);
  } else {
    run();
  }
}

async function initChart(): Promise<void> {
  if (initStarted || disposed || !hasLayout()) return;
  initStarted = true;
  try {
    const module = await import('../charts/echarts');
    if (disposed || host.value === null) return;
    chart = module.initLatticeChart(host.value);
    // ECharts owns the DOM here; route clicks through the optional callback so
    // panels can act on a real node/edge payload without exposing the instance.
    chart.on('click', (params: unknown) => {
      if (params === null || typeof params !== 'object') return;
      try {
        props.onPick?.(params as LatticeChartPick);
      } catch {
        // A panel callback must never break the chart engine's event loop.
      }
    });
    chart.setOption(latest, { notMerge: true });
    state.value = 'ready';
    scheduleResize();
  } catch {
    if (!disposed) state.value = 'error';
  }
}

function onSurfaceChange(): void {
  if (chart === null) {
    if (!initStarted && hasLayout()) void initChart();
    return;
  }
  scheduleResize();
}

watch(
  () => props.option,
  (next) => {
    latest = next;
    if (chart !== null) chart.setOption(next, { notMerge: true });
  },
  { flush: 'post' },
);

onMounted(() => {
  if (typeof ResizeObserver !== 'undefined' && host.value !== null) {
    observer = new ResizeObserver(onSurfaceChange);
    observer.observe(host.value);
  }
  if (hasLayout()) {
    void initChart();
  } else if (typeof ResizeObserver === 'undefined') {
    state.value = 'unavailable';
  }
});

onBeforeUnmount(() => {
  disposed = true;
  if (resizeFrame !== null && typeof cancelAnimationFrame === 'function') {
    cancelAnimationFrame(resizeFrame);
    resizeFrame = null;
  }
  if (observer !== null) {
    observer.disconnect();
    observer = null;
  }
  if (chart !== null) {
    chart.dispose();
    chart = null;
  }
});
</script>

<template>
  <!--
    ECharts owns the DOM inside `.chart-host`. It must have NO Vue-managed
    children: when the placeholder was rendered inside the same element, Vue
    patched a tree that ECharts had already mutated and the panel crashed with
    "Cannot read properties of null (reading 'insertBefore')". The placeholder
    is now a sibling overlay, so Vue never touches the chart host's children.
  -->
  <div class="chart-wrap">
    <div ref="host" class="chart-host" role="img" :aria-label="ariaLabel ?? 'chart'"></div>
    <div v-if="state === 'pending'" class="chart-placeholder">waiting for chart surface…</div>
    <div v-else-if="state === 'unavailable'" class="chart-placeholder">
      chart surface is not sized; no chart was initialised.
    </div>
    <div v-else-if="state === 'error'" class="chart-placeholder chart-placeholder--error" role="alert">
      chart renderer failed to initialise.
    </div>
  </div>
</template>

<style scoped>
.chart-wrap {
  position: relative;
  width: 100%;
  height: 100%;
  min-height: 96px;
}

.chart-host {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
}

.chart-placeholder {
  position: absolute;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: var(--nai-space-4);
  color: var(--nai-dim);
  font-size: 9px;
  text-transform: uppercase;
  letter-spacing: 0.5px;
  text-align: center;
}

.chart-placeholder--error {
  color: var(--nai-bad);
}
</style>
