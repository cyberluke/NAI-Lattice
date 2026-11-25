<script setup lang="ts">
import { defineAsyncComponent } from 'vue';
import type { LatticeChartOption, LatticeChartPick } from '../charts/echarts';

/**
 * Lazy wrapper: the panel code imports this component synchronously, while the
 * chart surface (and then the ECharts engine) arrive as separate chunks.
 */
defineProps<{
  option: LatticeChartOption;
  ariaLabel?: string;
  onPick?: (pick: LatticeChartPick) => void;
}>();

const EChart = defineAsyncComponent({
  loader: () => import('./EChart.vue'),
  delay: 0,
  timeout: 20_000,
});
</script>

<template>
  <EChart :option="option" :aria-label="ariaLabel" :on-pick="onPick" />
</template>
