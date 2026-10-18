// What a chart plots, from a result's rows (or from the points a notebook
// cell stored): pure functions, no Vue, no ECharts. ChartView renders the
// shape (chartOption.ts); a notebook keeps its points (chartSettings.ts).
//
// The shape is the same for every kind: categories, one array of numbers per
// series and the series' names. A histogram's categories are its bins, a box
// plot's series are the five numbers of each box (min, q1, median, q3, max),
// a heatmap's series are its rows (one per Y category), a KPI is one number.

import type { Cell } from '../api/types';
import type { ChartAgg, ChartKind, ChartPoints, ChartSort } from './chartSettings';

export function num(v: Cell): number | null {
  if (typeof v === 'number') return Number.isFinite(v) ? v : null;
  if (typeof v === 'boolean') return v ? 1 : 0;
  if (typeof v === 'string' && v.trim() !== '' && /^-?\d+(\.\d+)?(e[+-]?\d+)?$/i.test(v.trim())) return Number(v);
  return null;
}

function share(rows: Cell[][], i: number, test: (v: Cell) => boolean) {
  const vals = rows.slice(0, 200).map((r) => r[i]).filter((v) => v !== null);
  return vals.length ? vals.filter(test).length / vals.length : 0;
}
/** Columns whose sampled values are (80 % or more) numbers. */
export function numericColumns(rows: Cell[][], count: number): number[] {
  return Array.from({ length: count }, (_, i) => i).filter((i) => share(rows, i, (v) => num(v) !== null) >= 0.8);
}
/** Columns whose sampled values look like dates (2026-01, 2026-01-31…). */
export function dateColumns(rows: Cell[][], count: number): number[] {
  return Array.from({ length: count }, (_, i) => i).filter((i) => share(rows, i, (v) => typeof v === 'string' && /^\d{4}-\d{2}(-\d{2})?([ T]|$)/.test(v)) >= 0.8);
}

/** The settings as column positions (resolveChartSettings). */
export interface ResolvedChart {
  kind: ChartKind;
  /** -1: none (histogram, KPI; a box plot without categories). */
  x: number;
  y: number[];
  agg: ChartAgg;
  sort: ChartSort;
  /** A pivot column; a heatmap's Y category. */
  split: number | null;
  /** Histogram bins; 0: automatic. */
  bins: number;
}

export interface ShapeLabels {
  /** A NULL category. */
  nul: string;
  /** Series or categories past the cap. */
  others: string;
  /** A series' name: "Suma de total" (agg ≠ none) or the column. */
  series: (agg: ChartAgg, column: string) => string;
  /** A histogram bin's label from its edges. */
  bin: (from: number, to: number) => string;
}

export interface Shaped {
  cats: string[];
  series: number[][];
  names: string[];
  /** Scatter: [x, y] pairs per series. */
  pairs?: [number, number][][];
  /** Box plot: [category index, value] of the points past the whiskers. */
  outliers?: [number, number][];
  /** KPI: the rows with a value. */
  count?: number;
}

/** The five numbers of a box, as stored (fixed names, never translated). */
export const BOX_SERIES = ['min', 'q1', 'median', 'q3', 'max'] as const;
/** Series past this fold into "Otros" (fixed palette order, never cycled). */
export const MAX_SERIES = 7;
/** A heatmap's categories on each axis (the rest fold into "Otros"). */
export const HEATMAP_MAX_X = 60;
export const HEATMAP_MAX_Y = 30;
/** Box plot: groups shown; outliers drawn per group. */
export const BOX_MAX_GROUPS = 40;
export const BOX_MAX_OUTLIERS = 200;

export function reduce(a: number[], agg: ChartAgg): number {
  switch (agg) {
    case 'avg': return a.length ? a.reduce((s, n) => s + n, 0) / a.length : NaN;
    case 'min': return a.length ? a.reduce((m, n) => (n < m ? n : m), Infinity) : NaN;
    case 'max': return a.length ? a.reduce((m, n) => (n > m ? n : m), -Infinity) : NaN;
    case 'count': return a.length;
    default: return a.reduce((s, n) => s + n, 0);
  }
}

// -- histogram ---------------------------------------------------------------------------

/** Equal-width bins from min to max (the last one closed). `bins` 0: Sturges'
 *  rule, between 5 and 50. All values equal: one bin. */
export function histogram(values: number[], bins = 0): { edges: number[]; counts: number[] } {
  const v = values.filter(Number.isFinite);
  if (!v.length) return { edges: [], counts: [] };
  let lo = Infinity;
  let hi = -Infinity;
  for (const n of v) { if (n < lo) lo = n; if (n > hi) hi = n; }
  if (lo === hi) return { edges: [lo, hi], counts: [v.length] };
  const k = bins > 0 ? Math.min(Math.round(bins), 500) : Math.min(50, Math.max(5, Math.ceil(Math.log2(v.length)) + 1));
  const w = (hi - lo) / k;
  const edges = Array.from({ length: k + 1 }, (_, i) => (i === k ? hi : lo + i * w));
  const counts = new Array<number>(k).fill(0);
  for (const n of v) counts[Math.min(k - 1, Math.floor((n - lo) / w))]++;
  return { edges, counts };
}

// -- box plot ----------------------------------------------------------------------------

/** Quartiles by linear interpolation (Excel's QUARTILE.INC, R's type 7). */
export function quartiles(values: number[]): { min: number; q1: number; median: number; q3: number; max: number } | null {
  const s = values.filter(Number.isFinite).sort((a, b) => a - b);
  if (!s.length) return null;
  const q = (p: number) => {
    const h = (s.length - 1) * p;
    const i = Math.floor(h);
    return i + 1 < s.length ? s[i] + (h - i) * (s[i + 1] - s[i]) : s[i];
  };
  return { min: s[0], q1: q(0.25), median: q(0.5), q3: q(0.75), max: s[s.length - 1] };
}

/** A box with Tukey's whiskers (the furthest values within 1.5 IQR of the
 *  box) and the values past them. */
export function boxStats(values: number[]): { box: [number, number, number, number, number]; outliers: number[] } | null {
  const qs = quartiles(values);
  if (!qs) return null;
  const iqr = qs.q3 - qs.q1;
  const loFence = qs.q1 - 1.5 * iqr;
  const hiFence = qs.q3 + 1.5 * iqr;
  let low = Infinity;
  let high = -Infinity;
  const outliers: number[] = [];
  for (const n of values) {
    if (!Number.isFinite(n)) continue;
    if (n < loFence || n > hiFence) { outliers.push(n); continue; }
    if (n < low) low = n;
    if (n > high) high = n;
  }
  return { box: [low, qs.q1, qs.median, qs.q3, high], outliers };
}

// -- shaping -----------------------------------------------------------------------------

const label = (v: Cell, L: ShapeLabels) => (v === null ? L.nul : String(v));

function sortShape(cats: string[], series: number[][], sort: ChartSort) {
  if (sort === 'none' || !series.length) return { cats, series };
  const idx = cats.map((_, i) => i);
  if (sort === 'value') idx.sort((a, b) => (series[0][b] || 0) - (series[0][a] || 0));
  else idx.sort((a, b) => cats[a].localeCompare(cats[b], undefined, { numeric: true }));
  return { cats: idx.map((i) => cats[i]), series: series.map((s) => idx.map((i) => s[i])) };
}

/** One series per value column, a value per row (agg none) or per category. */
function grouped(rows: Cell[][], s: ResolvedChart, names: string[], L: ShapeLabels): Shaped {
  const { x, y: ys, agg } = s;
  let cats: string[];
  let series: number[][];
  if (agg === 'none') {
    cats = rows.map((r) => label(r[x], L));
    series = ys.map((y) => rows.map((r) => num(r[y]) ?? NaN));
  } else {
    const groups = new Map<string, number[][]>();
    for (const r of rows) {
      const k = label(r[x], L);
      let g = groups.get(k);
      if (!g) { g = ys.map(() => []); groups.set(k, g); }
      // "Cantidad" counts rows per category; the others reduce the values.
      ys.forEach((y, j) => { const v = agg === 'count' ? 1 : num(r[y]); if (v !== null) g![j].push(v); });
    }
    cats = [...groups.keys()];
    series = ys.map((_, j) => cats.map((c) => {
      const a = groups.get(c)![j];
      return agg === 'count' ? a.length : reduce(a, agg);
    }));
  }
  const sorted = sortShape(cats, series, s.sort);
  return { ...sorted, names: ys.map((y) => L.series(agg, names[y] ?? '')) };
}

/** Rows → one series per value of `split` (top values; the rest "Otros"). */
function pivot(rows: Cell[][], s: ResolvedChart, L: ShapeLabels): Shaped {
  const x = s.x;
  const split = s.split!;
  const y = s.y[0];
  const agg: ChartAgg = s.agg === 'none' ? 'sum' : s.agg;
  const totals = new Map<string, number>();
  for (const r of rows) totals.set(label(r[split], L), (totals.get(label(r[split], L)) ?? 0) + Math.abs(num(r[y]) ?? 0));
  const keep = [...totals.entries()].sort((a, b) => b[1] - a[1]).slice(0, MAX_SERIES).map(([k]) => k);
  const kept = new Set(keep);
  const names = totals.size > keep.length ? [...keep, L.others] : keep;
  const cells = new Map<string, Map<string, number[]>>();
  const cats: string[] = [];
  for (const r of rows) {
    const c = label(r[x], L);
    if (!cells.has(c)) { cells.set(c, new Map()); cats.push(c); }
    const sk = kept.has(label(r[split], L)) ? label(r[split], L) : L.others;
    const v = agg === 'count' ? 1 : num(r[y]);
    if (v === null) continue;
    const m = cells.get(c)!;
    (m.get(sk) ?? m.set(sk, []).get(sk)!).push(v);
  }
  const red = (a: number[] | undefined) => (!a?.length ? NaN : agg === 'count' ? a.length : reduce(a, agg));
  let order = cats.map((_, i) => i);
  if (s.sort === 'label') order.sort((a, b) => cats[a].localeCompare(cats[b], undefined, { numeric: true }));
  const series = names.map((nm) => order.map((i) => red(cells.get(cats[i])!.get(nm))));
  if (s.sort === 'value') {
    const tot = order.map((_, k) => series.reduce((acc, ser) => acc + (Number.isFinite(ser[k]) ? ser[k] : 0), 0));
    const idx = order.map((_, k) => k).sort((a, b) => tot[b] - tot[a]);
    order = idx.map((k) => order[k]);
    return { cats: order.map((i) => cats[i]), series: series.map((ser) => idx.map((k) => ser[k])), names };
  }
  return { cats: order.map((i) => cats[i]), series, names };
}

/** The `max` most frequent labels of column `c`, in first-seen order. */
function topLabels(rows: Cell[][], c: number, max: number, L: ShapeLabels): { keep: string[]; folded: boolean } {
  const freq = new Map<string, number>();
  for (const r of rows) { const k = label(r[c], L); freq.set(k, (freq.get(k) ?? 0) + 1); }
  if (freq.size <= max) return { keep: [...freq.keys()], folded: false };
  const top = new Set([...freq.entries()].sort((a, b) => b[1] - a[1]).slice(0, max).map(([k]) => k));
  return { keep: [...freq.keys()].filter((k) => top.has(k)), folded: true };
}

/** X category × Y category → the aggregated value (null: no rows there). */
export function heatmapShape(rows: Cell[][], x: number, ycat: number, v: number, agg: ChartAgg, L: ShapeLabels): Shaped {
  const a: ChartAgg = agg === 'none' ? 'sum' : agg;
  const xs = topLabels(rows, x, HEATMAP_MAX_X, L);
  const ys = topLabels(rows, ycat, HEATMAP_MAX_Y, L);
  const cats = xs.folded ? [...xs.keep, L.others] : xs.keep;
  const names = ys.folded ? [...ys.keep, L.others] : ys.keep;
  const xi = new Map(xs.keep.map((k, i) => [k, i]));
  const yi = new Map(ys.keep.map((k, i) => [k, i]));
  const cells: number[][][] = names.map(() => cats.map(() => []));
  for (const r of rows) {
    const val = a === 'count' ? 1 : num(r[v]);
    if (val === null) continue;
    const i = xi.get(label(r[x], L)) ?? cats.length - 1;
    const j = yi.get(label(r[ycat], L)) ?? names.length - 1;
    cells[j][i].push(val);
  }
  return { cats, names, series: cells.map((row) => row.map((c) => (c.length ? (a === 'count' ? c.length : reduce(c, a)) : NaN))) };
}

function histogramShape(rows: Cell[][], s: ResolvedChart, names: string[], L: ShapeLabels): Shaped {
  const vals = rows.map((r) => num(r[s.y[0]])).filter((n): n is number => n !== null);
  const { edges, counts } = histogram(vals, s.bins);
  return { cats: counts.map((_, i) => L.bin(edges[i], edges[i + 1])), series: [counts], names: [names[s.y[0]] ?? ''] };
}

function boxShape(rows: Cell[][], s: ResolvedChart, names: string[], L: ShapeLabels): Shaped {
  const y = s.y[0];
  const groups = new Map<string, number[]>();
  if (s.x < 0) groups.set(names[y] ?? '', []);
  else for (const k of topLabels(rows, s.x, BOX_MAX_GROUPS, L).keep) groups.set(k, []);
  for (const r of rows) {
    const v = num(r[y]);
    if (v === null) continue;
    groups.get(s.x < 0 ? names[y] ?? '' : label(r[s.x], L))?.push(v);
  }
  let cats = [...groups.keys()].filter((k) => groups.get(k)!.length);
  if (s.sort === 'label') cats = [...cats].sort((a, b) => a.localeCompare(b, undefined, { numeric: true }));
  const stats = cats.map((c) => boxStats(groups.get(c)!)!);
  let order = cats.map((_, i) => i);
  if (s.sort === 'value') order = order.sort((a, b) => stats[b].box[2] - stats[a].box[2]);
  const outliers: [number, number][] = [];
  order.forEach((i, k) => { for (const o of stats[i].outliers.slice(0, BOX_MAX_OUTLIERS)) outliers.push([k, o]); });
  return {
    cats: order.map((i) => cats[i]),
    series: BOX_SERIES.map((_, j) => order.map((i) => stats[i].box[j])),
    names: [...BOX_SERIES],
    outliers,
  };
}

function kpiShape(rows: Cell[][], s: ResolvedChart, names: string[], L: ShapeLabels): Shaped {
  const y = s.y[0];
  const agg: ChartAgg = s.agg === 'none' ? 'sum' : s.agg;
  const vals = rows.map((r) => num(r[y])).filter((n): n is number => n !== null);
  // "Cantidad": the rows (with or without a value), as SQL's COUNT(*).
  const value = agg === 'count' ? rows.length : reduce(vals, agg);
  return { cats: [names[y] ?? ''], series: [[value]], names: [L.series(agg, names[y] ?? '')], count: agg === 'count' ? rows.length : vals.length };
}

function scatterShape(rows: Cell[][], s: ResolvedChart, names: string[], L: ShapeLabels): Shaped {
  const base = grouped(rows, { ...s, agg: 'none', sort: 'none' }, names, L);
  const xs = rows.map((r) => num(r[s.x]));
  const pairs = s.y.map((y) => {
    const out: [number, number][] = [];
    rows.forEach((r, i) => { const a = xs[i]; const b = num(r[y]); if (a !== null && b !== null) out.push([a, b]); });
    return out;
  });
  return { ...base, pairs };
}

/** What the chart plots from the rows. */
export function shapeRows(rows: Cell[][], s: ResolvedChart, columnNames: string[], L: ShapeLabels): Shaped {
  if (!s.y.length) return { cats: [], series: [], names: [] };
  switch (s.kind) {
    case 'histogram': return histogramShape(rows, s, columnNames, L);
    case 'box': return boxShape(rows, s, columnNames, L);
    case 'kpi': return kpiShape(rows, s, columnNames, L);
    case 'scatter': return scatterShape(rows, s, columnNames, L);
    case 'heatmap': return s.split === null ? { cats: [], series: [], names: [] } : heatmapShape(rows, s.x, s.split, s.y[0], s.agg, L);
    default: {
      const splits = s.split !== null && ['bar', 'hbar', 'stacked', 'line', 'area'].includes(s.kind);
      return splits ? pivot(rows, s, L) : grouped(rows, s, columnNames, L);
    }
  }
}

/** The plotted shape as points (NaN → null, for JSON), at most `max` each. */
export function shapePoints(sh: Shaped, max = 500): ChartPoints {
  const series = sh.series.map((v, j) => ({ name: sh.names[j] ?? '', values: v.slice(0, max).map((n) => (Number.isFinite(n) ? n : null)) }));
  if (sh.count !== undefined) series.push({ name: 'rows', values: [sh.count] });
  return { categories: sh.cats.slice(0, max), series };
}

/** Stored points back as a shape (a notebook's whole-result aggregate, or an
 *  export without the rows). A KPI's row count is its second series. */
export function shapeFromPoints(kind: ChartKind, p: ChartPoints): Shaped {
  const series = p.series.map((s) => s.values.map((v) => (v === null ? NaN : v)));
  const names = p.series.map((s) => s.name);
  if (kind === 'kpi') {
    const count = series[1]?.[0];
    return { cats: p.categories.slice(0, 1), series: series.slice(0, 1), names: names.slice(0, 1), count: Number.isFinite(count) ? count : undefined };
  }
  return { cats: [...p.categories], series, names };
}
