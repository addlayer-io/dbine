// "Calcular sobre todo el resultado": a chart's request to chart_aggregate
// (web/src/api/chart.ts) and its answer as the chart's shape. Pure.
//
// Group charts (an aggregation other than "none") go as `group`; histograms
// and box plots as themselves. A KPI goes as a box over the whole column:
// its count (COUNT(*) is the rows read), min and max come from there; a sum
// or an average over the whole result isn't offered (the command has no
// ungrouped aggregate).

import type { ChartAggregate, ChartKind as AggregateKind } from '../api/types';
import { isRawChart, ONE_VALUE, SPLITS, type ChartAgg, type ChartSettings } from './chartSettings';
import { BOX_MAX_GROUPS, BOX_SERIES, HEATMAP_MAX_X, HEATMAP_MAX_Y, MAX_SERIES, type Shaped, type ShapeLabels } from './chartShape';

/** Groups asked for at most (x, or x × split pairs). */
export const WHOLE_MAX_GROUPS = 2000;

/** The request for these settings; null when the chart can't be computed
 *  over the whole result (a scatter, a value per row, a KPI's sum / average). */
export function aggregateRequest(s: ChartSettings): AggregateKind | null {
  const y = s.y[0];
  if (!y) return null;
  if (s.kind === 'histogram') return s.bins ? { kind: 'histogram', column: y, bins: Math.min(200, s.bins) } : { kind: 'histogram', column: y };
  if (s.kind === 'box') return { kind: 'box', column: y, by: s.x, max_groups: BOX_MAX_GROUPS };
  if (s.kind === 'kpi') return ['count', 'min', 'max'].includes(s.agg) ? { kind: 'box', column: y, by: null, max_groups: 1 } : null;
  if (isRawChart(s) || !s.x || s.agg === 'none') return null;
  const split = s.kind === 'heatmap' || SPLITS.includes(s.kind) ? s.split : null;
  if (s.kind === 'heatmap' && !split) return null;
  const func = s.agg as Exclude<ChartAgg, 'none'>;
  const cols = split || ONE_VALUE.includes(s.kind) ? s.y.slice(0, 1) : s.y;
  return {
    kind: 'group', x: s.x, split, max_groups: WHOLE_MAX_GROUPS,
    measures: cols.map((c) => ({ column: func === 'count' ? null : c, func })),
  };
}

const nan = (v: number | null | undefined) => (v === null || v === undefined ? NaN : v);

function sortShape(sh: Shaped, sort: ChartSettings['sort'], byTotal: boolean): Shaped {
  if (sort === 'none' || !sh.series.length) return sh;
  const idx = sh.cats.map((_, i) => i);
  const key = (i: number) => (byTotal ? sh.series.reduce((a, s) => a + (Number.isFinite(s[i]) ? s[i] : 0), 0) : sh.series[0][i] || 0);
  if (sort === 'value') idx.sort((a, b) => key(b) - key(a));
  else idx.sort((a, b) => sh.cats[a].localeCompare(sh.cats[b], undefined, { numeric: true }));
  return { ...sh, cats: idx.map((i) => sh.cats[i]), series: sh.series.map((s) => idx.map((i) => s[i])) };
}

/** Split series past `max` fold into "Otros" where the aggregate allows it
 *  (sum, count, min, max); an average's extra series are left out. */
function foldSeries(series: number[][], names: string[], agg: ChartAgg, max: number, others: string) {
  if (series.length <= max) return { series, names };
  const order = series.map((_, j) => j).sort((a, b) =>
    series[b].reduce((s, v) => s + (Number.isFinite(v) ? Math.abs(v) : 0), 0) - series[a].reduce((s, v) => s + (Number.isFinite(v) ? Math.abs(v) : 0), 0));
  const keep = order.slice(0, max);
  const rest = order.slice(max);
  const outS = keep.map((j) => series[j]);
  const outN = keep.map((j) => names[j]);
  if (agg !== 'avg') {
    const combine = (a: number, b: number) => (!Number.isFinite(a) ? b : !Number.isFinite(b) ? a : agg === 'min' ? Math.min(a, b) : agg === 'max' ? Math.max(a, b) : a + b);
    outS.push(series[0].map((_, i) => rest.reduce((acc, j) => combine(acc, series[j][i]), NaN)));
    outN.push(others);
  }
  return { series: outS, names: outN };
}

/** The answer as the chart's shape (what ChartView draws and a cell keeps). */
export function shapeFromAggregate(s: ChartSettings, a: ChartAggregate, L: ShapeLabels): Shaped {
  const y = s.y[0] ?? '';
  if (s.kind === 'histogram') {
    const h = a.histogram;
    if (!h) return { cats: [], series: [], names: [] };
    return { cats: h.counts.map((_, i) => L.bin(h.edges[i], h.edges[i + 1])), series: [[...h.counts]], names: [y] };
  }
  if (s.kind === 'kpi') {
    const b = a.boxes?.[0];
    if (!b) return { cats: [y], series: [[NaN]], names: [L.series(s.agg, y)], count: 0 };
    const value = s.agg === 'count' ? a.rows_scanned ?? b.count : s.agg === 'min' ? b.min : b.max;
    return { cats: [y], series: [[value]], names: [L.series(s.agg, y)], count: b.count };
  }
  if (s.kind === 'box') {
    const boxes = a.boxes ?? [];
    const sh: Shaped = {
      cats: boxes.map((b) => (b.category ?? (s.x ? L.nul : y))),
      series: BOX_SERIES.map((k) => boxes.map((b) => b[k])),
      names: [...BOX_SERIES],
    };
    if (s.sort === 'value') {
      const idx = sh.cats.map((_, i) => i).sort((p, q) => sh.series[2][q] - sh.series[2][p]);
      return { ...sh, cats: idx.map((i) => sh.cats[i]), series: sh.series.map((v) => idx.map((i) => v[i])) };
    }
    return sortShape(sh, s.sort === 'label' ? 'label' : 'none', false);
  }
  const cats = a.categories.map((c) => c ?? L.nul);
  const split = a.series.some((x) => x.split !== null) || (s.split !== null && (s.kind === 'heatmap' || SPLITS.includes(s.kind)));
  if (!split) {
    const cols = ONE_VALUE.includes(s.kind) ? s.y.slice(0, 1) : s.y;
    const series = cols.map((_, j) => (a.series.find((x) => x.measure === j)?.values ?? []).map(nan));
    return sortShape({ cats, series, names: cols.map((c) => L.series(s.agg, c)) }, s.sort, false);
  }
  let names = a.series.map((x) => x.split ?? L.nul);
  let series = a.series.map((x) => x.values.map(nan));
  if (s.kind === 'heatmap') {
    // The Y categories with the most cells, and the first X ones.
    const filled = (v: number[]) => v.filter(Number.isFinite).length;
    const order = series.map((_, j) => j).sort((p, q) => filled(series[q]) - filled(series[p])).slice(0, HEATMAP_MAX_Y);
    return { cats: cats.slice(0, HEATMAP_MAX_X), names: order.map((j) => names[j]), series: order.map((j) => series[j].slice(0, HEATMAP_MAX_X)) };
  }
  ({ series, names } = foldSeries(series, names, s.agg, MAX_SERIES, L.others));
  return sortShape({ cats, series, names }, s.sort, true);
}
