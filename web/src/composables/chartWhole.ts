// "Calcular sobre todo el resultado": a chart's request to chart_aggregate
// (web/src/api/chart.ts) and its answer as the chart's shape. Pure.
//
// Group charts (an aggregation other than "none") go as `group`; histograms
// and box plots as themselves. A KPI goes as a `group` without X (one row of
// measures): its aggregate, and COUNT of the column for the "rows with a
// value" caption.

import type { ChartAggregate, ChartKind as AggregateKind } from '../api/types';
import { isRawChart, ONE_VALUE, SPLITS, type ChartAgg, type ChartSettings } from './chartSettings';
import { BOX_MAX_GROUPS, BOX_MAX_OUTLIERS, BOX_SERIES, HEATMAP_MAX_X, HEATMAP_MAX_Y, MAX_SERIES, type Shaped, type ShapeLabels } from './chartShape';

/** Groups asked for at most (x, or x × split pairs). */
export const WHOLE_MAX_GROUPS = 2000;

/** The request for these settings; null when the chart can't be computed
 *  over the whole result (a scatter, a value per row). */
export function aggregateRequest(s: ChartSettings): AggregateKind | null {
  const y = s.y[0];
  if (!y) return null;
  if (s.kind === 'histogram') return s.bins ? { kind: 'histogram', column: y, bins: Math.min(200, s.bins) } : { kind: 'histogram', column: y };
  if (s.kind === 'box') return { kind: 'box', column: y, by: s.x, max_groups: BOX_MAX_GROUPS };
  if (s.kind === 'kpi') {
    const func: Exclude<ChartAgg, 'none'> = s.agg === 'none' ? 'sum' : s.agg;
    return {
      kind: 'group', x: null, split: null,
      measures: [{ column: func === 'count' ? null : y, func }, { column: y, func: 'count' }],
    };
  }
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
    const agg: ChartAgg = s.agg === 'none' ? 'sum' : s.agg;
    const at = (m: number) => nan(a.series.find((x) => x.measure === m)?.values[0]);
    const value = at(0);
    // "Cantidad" counts the rows, as over the loaded rows; else the rows with a value.
    const count = agg === 'count' ? value : at(1);
    return { cats: [y], series: [[value]], names: [L.series(agg, y)], count: Number.isFinite(count) ? count : 0 };
  }
  if (s.kind === 'box') {
    // Tukey's whiskers and the points past them, as over the loaded rows.
    const boxes = a.boxes ?? [];
    const cats = boxes.map((b) => (b.category ?? (s.x ? L.nul : y)));
    let order = boxes.map((_, i) => i);
    if (s.sort === 'value') order = order.sort((p, q) => boxes[q].median - boxes[p].median);
    else if (s.sort === 'label') order = order.sort((p, q) => cats[p].localeCompare(cats[q], undefined, { numeric: true }));
    const five = (b: (typeof boxes)[number]) => [b.whisker_low, b.q1, b.median, b.q3, b.whisker_high];
    const outliers: [number, number][] = [];
    order.forEach((i, k) => { for (const o of (boxes[i].outliers ?? []).slice(0, BOX_MAX_OUTLIERS)) outliers.push([k, o]); });
    return {
      cats: order.map((i) => cats[i]),
      series: [0, 1, 2, 3, 4].map((j) => order.map((i) => five(boxes[i])[j])),
      names: [...BOX_SERIES],
      outliers,
    };
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

/** Whether a chart starts with "Calcular sobre todo el resultado" on: only
 *  for a DataFrame SQL cell's result (`notebookTab`: DuckDB aggregates it on
 *  the notebook's own session, no server is involved) that shows fewer rows
 *  than it returned. A connection's chart keeps it off (it would run the
 *  query again on the server); the user can switch either one. */
export function wholeChartByDefault(
  source: { notebookTab?: string } | null | undefined,
  result: { rows: unknown[]; total_rows: number; truncated: boolean } | null | undefined,
): boolean {
  if (!source?.notebookTab || !result) return false;
  return result.truncated || result.total_rows > result.rows.length;
}
