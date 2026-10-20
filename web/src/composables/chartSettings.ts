// A chart's settings by column NAME (not position), so they survive a
// re-run that reorders or adds columns, and the points it plots (already
// aggregated, capped). ChartView applies them; a notebook cell stores its
// charts in `cell.chart` as
//   { view, active, charts: [{ title, type, x, y, agg, sort, split, bins?, whole?, points?, aggregate? }], map? }
// (older files: one chart, { view, type, x, y, agg, sort, split, points }:
// read as a one-chart list). Every option is declarative: the ECharts option
// is built in code (chartOption.ts), never read from a file.

import type { ResolvedChart } from './chartShape';

export const CHART_KINDS = ['bar', 'hbar', 'stacked', 'line', 'area', 'pie', 'scatter', 'combo', 'histogram', 'heatmap', 'box', 'kpi'] as const;
export const CHART_AGGS = ['none', 'sum', 'avg', 'count', 'min', 'max'] as const;
export const CHART_SORTS = ['none', 'value', 'label'] as const;
/** Histogram bin counts offered; 0: automatic (Sturges). */
export const HISTOGRAM_BINS = [0, 5, 10, 20, 30, 50, 100] as const;
export type ChartKind = (typeof CHART_KINDS)[number];
export type ChartAgg = (typeof CHART_AGGS)[number];
export type ChartSort = (typeof CHART_SORTS)[number];

export interface ChartSettings {
  kind: ChartKind;
  x: string | null;
  y: string[];
  agg: ChartAgg;
  sort: ChartSort;
  /** A pivot column (one series per value); a heatmap's Y category. */
  split: string | null;
  /** Histogram bins (0 or absent: automatic). */
  bins?: number;
  /** "Calcular sobre todo el resultado": aggregated by the backend. */
  whole?: boolean;
}
export interface ChartPoints {
  categories: string[];
  series: { name: string; values: (number | null)[] }[];
}
/** How whole-result points were computed (chart_aggregate). */
export interface AggregateInfo {
  method: 'server' | 'client';
  rows_scanned: number | null;
  rows_capped: boolean;
  groups_truncated: boolean;
  /** Aggregated by the notebook's DuckDB (a DataFrame SQL cell's chart). */
  engine?: 'duckdb';
}
/** What ChartView emits: its settings and what it plots. */
export type ChartState = ChartSettings & { points: ChartPoints; aggregate?: AggregateInfo | null };

/** The kinds that need an X / category column. */
export const NEEDS_X: readonly ChartKind[] = ['bar', 'hbar', 'stacked', 'line', 'area', 'pie', 'scatter', 'combo', 'heatmap'];
/** The kinds that plot one value column. */
export const ONE_VALUE: readonly ChartKind[] = ['pie', 'histogram', 'heatmap', 'box', 'kpi'];
/** The kinds a split (one series per value) applies to. */
export const SPLITS: readonly ChartKind[] = ['bar', 'hbar', 'stacked', 'line', 'area'];
/** The kinds whose aggregation can't be "none". */
const ALWAYS_AGG: readonly ChartKind[] = ['heatmap', 'kpi'];

/** The chart plots raw rows (a scatter, or a value per row): its points
 *  aren't stored. Histograms, box plots and KPIs are always summaries. */
export function isRawChart(s: Pick<ChartSettings, 'kind' | 'agg'>): boolean {
  if (s.kind === 'histogram' || s.kind === 'box' || s.kind === 'kpi') return false;
  return s.kind === 'scatter' || s.agg === 'none';
}

/** The settings as column positions in `names`; null when the chart can't be
 *  rebuilt there (a needed X column or every value column is gone): defaults then. */
export function resolveChartSettings(s: ChartSettings, names: string[], numeric: number[]): ResolvedChart | null {
  if (!CHART_KINDS.includes(s.kind)) return null;
  const at = (n: string | null) => (n === null ? -1 : names.indexOf(n));
  const x = at(s.x);
  let y = s.y.map(at).filter((i) => i >= 0 && numeric.includes(i));
  if ((NEEDS_X.includes(s.kind) && x < 0) || !y.length) return null;
  const sp = at(s.split);
  const split = sp >= 0 && sp !== x && (SPLITS.includes(s.kind) || s.kind === 'heatmap') ? sp : null;
  if (ONE_VALUE.includes(s.kind) || (split !== null && SPLITS.includes(s.kind))) y = y.slice(0, 1);
  let agg: ChartAgg = CHART_AGGS.includes(s.agg) ? s.agg : 'none';
  if (agg === 'none' && ALWAYS_AGG.includes(s.kind)) agg = 'sum';
  const bins = typeof s.bins === 'number' && Number.isInteger(s.bins) && s.bins >= 0 && s.bins <= 500 ? s.bins : 0;
  return {
    kind: s.kind,
    x: s.kind === 'histogram' || s.kind === 'kpi' ? -1 : x,
    y,
    agg,
    sort: CHART_SORTS.includes(s.sort) ? s.sort : 'none',
    split,
    bins,
  };
}

/** The plotted series, at most `max` points each (NaN → null, for JSON). */
export function chartPoints(cats: string[], series: number[][], names: string[], max = 500): ChartPoints {
  return {
    categories: cats.slice(0, max),
    series: series.map((s, j) => ({ name: names[j] ?? '', values: s.slice(0, max).map((v) => (Number.isFinite(v) ? v : null)) })),
  };
}

const str = (v: unknown): string | null => (typeof v === 'string' ? v : null);

/** Equal as JSON, whatever the key order (the file's may be sorted). */
export function sameJson(a: unknown, b: unknown): boolean {
  const norm = (v: unknown): unknown => (Array.isArray(v) ? v.map(norm)
    : v && typeof v === 'object' ? Object.fromEntries(Object.keys(v).sort().map((k) => [k, norm((v as Record<string, unknown>)[k])]))
    : v);
  return JSON.stringify(norm(a)) === JSON.stringify(norm(b));
}

/** A value or label longer than this is cut (as the file cuts cell values). */
export const MAX_LABEL_CHARS = 4000;
const cutLabel = (s: string) => (s.length > MAX_LABEL_CHARS ? `${s.slice(0, MAX_LABEL_CHARS)}…` : s);
/** A chart's title in the file. */
export const MAX_TITLE_CHARS = 200;

/** The points a notebook cell may keep: only aggregated ones (isRawChart),
 *  at most `maxRows` each, labels cut. */
export function storablePoints(s: ChartSettings | null, points: ChartPoints | null, maxRows: number): ChartPoints | null {
  if (!s || !points || isRawChart(s)) return null;
  return {
    categories: points.categories.slice(0, maxRows).map(cutLabel),
    series: points.series.slice(0, maxRows).map((x) => ({ name: cutLabel(x.name), values: x.values.slice(0, maxRows) })),
  };
}

// -- the cell's charts (the file's format) ------------------------------------------------

export interface StoredChart {
  title: string;
  settings: ChartSettings | null;
  points: ChartPoints | null;
  aggregate: AggregateInfo | null;
}
/** The map view's source, by column name (a geometry column, else lat / lon). */
export interface MapSettings {
  column: string | null;
  lat: string | null;
  lon: string | null;
}
export interface CellCharts {
  view: string | null;
  /** The chart shown (an index in `charts`). */
  active: number;
  charts: StoredChart[];
  map: MapSettings | null;
}

function readSettings(o: Record<string, unknown>): ChartSettings | null {
  const kind = str(o.type) as ChartKind | null;
  if (!kind || !CHART_KINDS.includes(kind) || !Array.isArray(o.y)) return null;
  const s: ChartSettings = {
    kind,
    x: str(o.x),
    y: o.y.filter((n): n is string => typeof n === 'string'),
    agg: (CHART_AGGS as readonly string[]).includes(str(o.agg) ?? '') ? (o.agg as ChartAgg) : 'none',
    sort: (CHART_SORTS as readonly string[]).includes(str(o.sort) ?? '') ? (o.sort as ChartSort) : 'none',
    split: str(o.split),
  };
  if (typeof o.bins === 'number' && Number.isInteger(o.bins) && o.bins > 0 && o.bins <= 500) s.bins = o.bins;
  if (o.whole === true) s.whole = true;
  // Switched off where it starts on (a DataFrame SQL cell's cut result).
  else if (o.whole === false) s.whole = false;
  return s;
}

export function readPoints(v: unknown): ChartPoints | null {
  if (!v || typeof v !== 'object') return null;
  const o = v as Record<string, unknown>;
  if (!Array.isArray(o.categories) || !Array.isArray(o.series)) return null;
  const series: ChartPoints['series'] = [];
  for (const s of o.series) {
    if (!s || typeof s !== 'object' || !Array.isArray((s as { values?: unknown }).values)) return null;
    const x = s as { name?: unknown; values: unknown[] };
    series.push({ name: typeof x.name === 'string' ? x.name : '', values: x.values.map((n) => (typeof n === 'number' && Number.isFinite(n) ? n : null)) });
  }
  return { categories: o.categories.map((c) => (typeof c === 'string' ? c : String(c ?? ''))), series };
}

function readAggregate(v: unknown): AggregateInfo | null {
  if (!v || typeof v !== 'object') return null;
  const o = v as Record<string, unknown>;
  if (o.method !== 'server' && o.method !== 'client') return null;
  return {
    method: o.method,
    rows_scanned: typeof o.rows_scanned === 'number' && Number.isFinite(o.rows_scanned) ? o.rows_scanned : null,
    rows_capped: o.rows_capped === true,
    groups_truncated: o.groups_truncated === true,
    ...(o.engine === 'duckdb' ? { engine: 'duckdb' as const } : {}),
  };
}

function readChart(v: unknown): StoredChart {
  const o = (v && typeof v === 'object' ? v : {}) as Record<string, unknown>;
  const settings = readSettings(o);
  return {
    title: (str(o.title) ?? '').slice(0, MAX_TITLE_CHARS),
    settings,
    points: settings ? readPoints(o.points) : null,
    aggregate: settings?.whole ? readAggregate(o.aggregate) : null,
  };
}

function readMap(v: unknown): MapSettings | null {
  if (!v || typeof v !== 'object') return null;
  const o = v as Record<string, unknown>;
  const m = { column: str(o.column), lat: str(o.lat), lon: str(o.lon) };
  return m.column !== null || (m.lat !== null && m.lon !== null) ? m : null;
}

/** A cell's stored charts (opaque JSON from the file); the old one-chart
 *  format reads as a list of one. Malformed parts read as nothing. */
export function readCellCharts(v: unknown): CellCharts {
  if (!v || typeof v !== 'object') return { view: null, active: 0, charts: [], map: null };
  const o = v as Record<string, unknown>;
  const charts = Array.isArray(o.charts) ? o.charts.map(readChart) : o.type !== undefined ? [readChart(o)] : [];
  const a = o.active;
  const active = typeof a === 'number' && Number.isInteger(a) && a >= 0 && a < charts.length ? a : 0;
  return { view: str(o.view), active, charts, map: readMap(o.map) };
}

/** The cell's chart JSON (the new format). */
export function writeCellCharts(c: CellCharts): Record<string, unknown> {
  const out: Record<string, unknown> = { view: c.view ?? 'table' };
  if (c.charts.length) {
    out.active = Math.min(Math.max(0, c.active), c.charts.length - 1);
    out.charts = c.charts.map((ch) => {
      const o: Record<string, unknown> = { title: ch.title.slice(0, MAX_TITLE_CHARS) };
      const s = ch.settings;
      if (s) {
        Object.assign(o, { type: s.kind, x: s.x, y: s.y, agg: s.agg, sort: s.sort, split: s.split });
        if (s.bins) o.bins = s.bins;
        if (s.whole) o.whole = true;
        else if (s.whole === false) o.whole = false;
      }
      if (ch.points) o.points = ch.points;
      if (ch.aggregate && s?.whole) o.aggregate = ch.aggregate;
      return o;
    });
  }
  if (c.map) out.map = c.map;
  return out;
}

const jsonBytes = (v: unknown) => new TextEncoder().encode(JSON.stringify(v)).length;

/** The charts as a cell may keep them: each one's points storable
 *  (storablePoints), and all of them together within `maxBytes` (the cell's
 *  byte cap): in order, a chart whose points don't fit keeps none. */
export function storableCharts(charts: StoredChart[], maxRows: number, maxBytes: number): StoredChart[] {
  let used = 0;
  return charts.map((ch) => {
    const p = storablePoints(ch.settings, ch.points, maxRows);
    const size = p ? jsonBytes(p) : 0;
    const fits = p !== null && used + size <= maxBytes;
    if (fits) used += size;
    return { ...ch, points: fits ? p : null, aggregate: fits ? ch.aggregate : null };
  });
}
