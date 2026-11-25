/**
 * Canvas-safe copies of the NAI tokens. Kept free of ECharts imports so panels
 * can build chart options without pulling the chart engine into the entry chunk.
 */
export const CHART_COLORS = {
  accent: '#19c2d8',
  accentStrong: '#52e0ef',
  ok: '#26de81',
  warn: '#ffce54',
  bad: '#ff6b6b',
  muted: '#8b98a5',
  dim: '#6a7480',
  grid: '#1f2630',
  track: '#1a222c',
  countBar: 'rgba(25, 194, 216, 0.30)',
  fg: '#e6edf3',
} as const;

export const CHART_TEXT_STYLE = {
  color: CHART_COLORS.muted,
  fontFamily:
    'ui-monospace, "Cascadia Mono", "Cascadia Code", Consolas, "SFMono-Regular", Menlo, monospace',
  fontSize: 10,
} as const;
