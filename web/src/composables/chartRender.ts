// ECharts with the pieces DBine's charts use, the labels charts are drawn
// with, and a chart rendered off screen to a PNG (a notebook export).

import * as echarts from 'echarts/core';
import { BarChart, BoxplotChart, HeatmapChart, LineChart, PieChart, ScatterChart } from 'echarts/charts';
import {
  DataZoomComponent, GraphicComponent, GridComponent, LegendComponent, TooltipComponent, VisualMapComponent,
} from 'echarts/components';
import { CanvasRenderer } from 'echarts/renderers';
import type { Cell, ResultColumn } from '../api/types';
import { locale, t } from '../i18n';
import { CJK_FONTS } from '../styles/fonts';
import { chartOption, INKS, type Theme } from './chartOption';
import { resolveChartSettings, type ChartPoints, type ChartSettings } from './chartSettings';
import { numericColumns, shapeFromPoints, shapeRows, type ResolvedChart, type Shaped, type ShapeLabels } from './chartShape';

echarts.use([
  BarChart, LineChart, PieChart, ScatterChart, BoxplotChart, HeatmapChart,
  GridComponent, TooltipComponent, LegendComponent, DataZoomComponent, VisualMapComponent, GraphicComponent, CanvasRenderer,
]);

export { echarts };

export const fmtNumber = (v: number) => (Number.isFinite(v) ? v.toLocaleString(locale(), { maximumFractionDigits: 2 }) : '—');

/** The texts a chart's shape uses (series names, NULL, "Otros", bins). */
export function chartLabels(): ShapeLabels {
  return {
    nul: t('results:chart.null'),
    others: t('results:chart.others'),
    series: (agg, column) => (agg === 'none' ? column : t('results:chart.seriesName', { agg: t(`results:chart.agg.${agg}`), column })),
    bin: (from, to) => `${fmtNumber(from)} – ${fmtNumber(to)}`,
  };
}

/** A KPI's lines under the number: what it is, and over how many rows. */
export function kpiCaption(shaped: Shaped): string[] {
  const lines = [shaped.names[0] ?? ''];
  if (shaped.count !== undefined) lines.push(t('results:chart.kpiRows', { count: shaped.count, rows: shaped.count.toLocaleString(locale()) }));
  return lines;
}

/** The option drawn in a hidden box of `width` × `height`, as PNG base64 (no
 *  "data:" prefix), at twice the pixels; null when it can't be drawn. */
export function renderPng(option: Record<string, unknown>, width = 900, height = 420, background = '#ffffff'): string | null {
  const box = document.createElement('div');
  box.style.cssText = `position: fixed; left: -10000px; top: 0; width: ${width}px; height: ${height}px; pointer-events: none;`;
  document.body.appendChild(box);
  const chart = echarts.init(box, undefined, { renderer: 'canvas', width, height });
  try {
    chart.setOption({ ...option, animation: false }, true);
    const url = chart.getDataURL({ type: 'png', pixelRatio: 2, backgroundColor: background });
    return url.split(',', 2)[1] || null;
  } catch {
    return null;
  } finally {
    chart.dispose();
    box.remove();
  }
}

/** A stored chart as a PNG (light theme, for an export): from the rows when
 *  there are any, else from the points the cell kept (`preferPoints`: the
 *  points first, e.g. a whole-result aggregate). Null: nothing to draw. */
export function chartPng(
  columns: ResultColumn[], rows: Cell[][] | null, settings: ChartSettings, points: ChartPoints | null, preferPoints: boolean, theme: Theme = 'light',
): string | null {
  const L = chartLabels();
  let shaped: Shaped | null = null;
  let r: ResolvedChart | null = null;
  if (rows?.length && columns.length && !(preferPoints && points)) {
    r = resolveChartSettings(settings, columns.map((c) => c.name), numericColumns(rows, columns.length));
    if (r) shaped = shapeRows(rows, r, columns.map((c) => c.name), L);
  }
  if (!shaped?.cats.length && points?.categories.length) shaped = shapeFromPoints(settings.kind, points);
  if (!shaped?.cats.length) return null;
  const option = chartOption({
    kind: settings.kind,
    shaped,
    theme,
    fontFamily: `-apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, ${CJK_FONTS}, sans-serif`,
    fmt: fmtNumber,
    others: L.others,
    xName: settings.x ?? '',
    kpiCaption: kpiCaption(shaped),
    animation: false,
  });
  return renderPng(option, 900, settings.kind === 'kpi' ? 220 : 420, INKS[theme].surface);
}
