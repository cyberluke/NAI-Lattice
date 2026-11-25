import { describe, expect, it } from 'vitest';
import { mount } from '@vue/test-utils';
import EChart from '../src/ui/EChart.vue';
import type { LatticeChartOption } from '../src/charts/echarts';

/**
 * Regression: ECharts mutates the DOM inside its container. If Vue also renders
 * the placeholder in that same element, a state change makes Vue patch a tree it
 * no longer owns and the panel crashes with
 * "Cannot read properties of null (reading 'insertBefore')".
 * The chart host must stay empty from Vue's point of view; overlays are siblings.
 */
describe('EChart host ownership', () => {
  it('keeps the chart host free of Vue-managed children', () => {
    const wrapper = mount(EChart, { props: { option: {} as LatticeChartOption } });
    const host = wrapper.find('.chart-host');
    expect(host.exists()).toBe(true);
    expect(host.element.children.length).toBe(0);
    // The placeholder is an overlay sibling, never a child of the host.
    expect(wrapper.find('.chart-host .chart-placeholder').exists()).toBe(false);
    expect(wrapper.find('.chart-wrap > .chart-placeholder').exists()).toBe(true);
  });
});
