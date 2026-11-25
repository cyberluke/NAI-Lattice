/**
 * Lazy ECharts entry point. Only the chart/component/renderer types used by the
 * Overview module are registered, so the ECharts chunk stays small and is
 * emitted separately from the shell entry.
 */
import { use, init, type ECharts, type EChartsCoreOption } from 'echarts/core';
import { BarChart, GraphChart, LineChart } from 'echarts/charts';
import {
  GridComponent,
  LegendComponent,
  TooltipComponent,
} from 'echarts/components';
import { CanvasRenderer } from 'echarts/renderers';

use([BarChart, GraphChart, LineChart, GridComponent, LegendComponent, TooltipComponent, CanvasRenderer]);

export type LatticeChartInstance = ECharts;
export type LatticeChartOption = EChartsCoreOption;

/** One ECharts interaction payload (graph node/edge picks, axis picks). */
export type LatticeChartPick = Record<string, unknown>;

export { CHART_COLORS, CHART_TEXT_STYLE } from './palette';

export function initLatticeChart(host: HTMLElement): LatticeChartInstance {
  return init(host, undefined, { renderer: 'canvas' });
}
