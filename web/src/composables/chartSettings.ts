// A chart's settings by column NAME (not position), so they survive a
// re-run that reorders or adds columns, and the points it plots (already
// aggregated, capped). ChartView applies them; a notebook cell stores them
// in `cell.chart` as { view, type, x, y, agg, sort, split, points }.

export const CHART_KINDS = ['bar', 'hbar', 'stacked', 'line', 'area', 'pie', 'scatter'] as const;
export const CHART_AGGS = ['none', 'sum', 'avg', 'count', 'min', 'max'] as const;
export const CHART_SORTS = ['none', 'value', 'label'] as const;
export type ChartKind = (typeof CHART_KINDS)[number];
export type ChartAgg = (typeof CHART_AGGS)[number];
export type ChartSort = (typeof CHART_SORTS)[number];

export interface ChartSettings {
  kind: ChartKind;
  x: string | null;
  y: string[];
  agg: ChartAgg;
  sort: ChartSort;
  split: string | null;
}
export interface ChartPoints {
  categories: string[];
  series: { name: string; values: (number | null)[] }[];
}
/** What ChartView emits: its settings and what it plots. */
export type ChartState = ChartSettings & { points: ChartPoints };

/** The settings as column positions in `names`; null when the chart can't be
 *  rebuilt there (the X column or every value column is gone): defaults then. */
export function resolveChartSettings(s: ChartSettings, names: string[], numeric: number[]) {
  const at = (n: string | null) => (n === null ? -1 : names.indexOf(n));
  const x = at(s.x);
  const y = s.y.map(at).filter((i) => i >= 0 && numeric.includes(i));
  if (x < 0 || !y.length || !CHART_KINDS.includes(s.kind)) return null;
  const sp = at(s.split);
  const split = sp >= 0 && sp !== x ? sp : null;
  return {
    kind: s.kind,
    x,
    y: split !== null ? y.slice(0, 1) : y,
    agg: CHART_AGGS.includes(s.agg) ? s.agg : 'none',
    sort: CHART_SORTS.includes(s.sort) ? s.sort : 'none',
    split,
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

/** A cell's stored chart (opaque JSON from the file): the view it showed and
 *  the chart settings, each null when absent or malformed. */
export function readCellChart(v: unknown): { view: string | null; settings: ChartSettings | null } {
  if (!v || typeof v !== 'object') return { view: null, settings: null };
  const o = v as Record<string, unknown>;
  const kind = str(o.type) as ChartKind | null;
  const settings: ChartSettings | null = kind && CHART_KINDS.includes(kind) && Array.isArray(o.y)
    ? {
      kind,
      x: str(o.x),
      y: o.y.filter((n): n is string => typeof n === 'string'),
      agg: (str(o.agg) ?? 'none') as ChartAgg,
      sort: (str(o.sort) ?? 'none') as ChartSort,
      split: str(o.split),
    }
    : null;
  return { view: str(o.view), settings };
}

/** The cell's chart JSON: the view, the settings (as `type`…) and the points. */
export function cellChart(view: string, s: ChartSettings | null, points: ChartPoints | null): Record<string, unknown> {
  const out: Record<string, unknown> = { view };
  if (s) Object.assign(out, { type: s.kind, x: s.x, y: s.y, agg: s.agg, sort: s.sort, split: s.split });
  if (points) out.points = points;
  return out;
}

/** Equal as JSON, whatever the key order (the file's may be sorted). */
export function sameJson(a: unknown, b: unknown): boolean {
  const norm = (v: unknown): unknown => (Array.isArray(v) ? v.map(norm)
    : v && typeof v === 'object' ? Object.fromEntries(Object.keys(v).sort().map((k) => [k, norm((v as Record<string, unknown>)[k])]))
    : v);
  return JSON.stringify(norm(a)) === JSON.stringify(norm(b));
}
