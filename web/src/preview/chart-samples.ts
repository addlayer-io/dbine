// view=chart[&kind=bar|hbar|stacked|line|area|pie|scatter|combo|histogram|heatmap|box|kpi][&theme=light]
//   ChartView alone over sample sales (no kind: the detected defaults).
// view=nbcell[&set=charts|whole|map|latlon][&theme=light]
//   A notebook cell's result (NotebookResult.vue) with several charts
//   (set=charts), or a spatial result in the map view (set=map: points,
//   set=latlon: latitude / longitude columns). The cell's `chart` JSON is
//   printed under it as it changes. set=whole: charts with "Calcular sobre
//   todo el resultado" on, over a mocked chart_aggregate (&fail=1: it fails,
//   &slow=1: it takes 30 s, to cancel it).
import { h, ref } from 'vue';
import ChartView from '../components/ChartView.vue';
import NotebookResult from '../components/notebook/NotebookResult.vue';
import type { Cell, CellResult, QueryOutcome, ResultColumn } from '../api/types';
import type { ChartSettings } from '../composables/chartSettings';
import { sampleResult } from './samples';
import { latLon, points } from './map-samples';

/** Deterministic noise (no Math.random: screenshots stay the same). */
function prng(seed: number) {
  let s = seed >>> 0;
  return () => { s = (s * 1664525 + 1013904223) >>> 0; return s / 2 ** 32; };
}
/** Order lines: a city, a channel, a delivery time (skewed, with outliers) and an amount. */
export function sampleOrders(): { columns: ResultColumn[]; rows: Cell[][] } {
  const r = prng(7);
  const cities = ['Buenos Aires', 'Córdoba', 'Rosario', 'Mendoza', 'La Plata', 'Tucumán'];
  const channels = ['web', 'tienda', 'teléfono', 'app'];
  const rows: Cell[][] = [];
  for (let i = 0; i < 600; i++) {
    const c = Math.floor(r() * cities.length);
    const normal = (r() + r() + r() + r() - 2) * 1.2;
    const days = Math.max(0.5, 2 + c * 0.6 + normal + (r() < 0.02 ? 9 + r() * 6 : 0));
    rows.push([cities[c], channels[Math.floor(r() * channels.length)], Math.round(days * 10) / 10, Math.round((800 + r() * 4200 + c * 300) * 100) / 100]);
  }
  return {
    columns: [{ name: 'ciudad', type_name: 'varchar' }, { name: 'canal', type_name: 'varchar' }, { name: 'dias_entrega', type_name: 'numeric' }, { name: 'importe', type_name: 'numeric' }],
    rows,
  };
}

const KIND_SETTINGS: Record<string, ChartSettings> = {
  bar: { kind: 'bar', x: 'mes', y: ['total'], agg: 'sum', sort: 'none', split: null },
  hbar: { kind: 'hbar', x: 'ciudad', y: ['total'], agg: 'sum', sort: 'value', split: null },
  stacked: { kind: 'stacked', x: 'mes', y: ['total'], agg: 'sum', sort: 'none', split: 'ciudad' },
  line: { kind: 'line', x: 'mes', y: ['total'], agg: 'sum', sort: 'none', split: 'ciudad' },
  area: { kind: 'area', x: 'mes', y: ['total'], agg: 'sum', sort: 'none', split: null },
  pie: { kind: 'pie', x: 'ciudad', y: ['total'], agg: 'sum', sort: 'none', split: null },
  scatter: { kind: 'scatter', x: 'pedidos', y: ['total'], agg: 'none', sort: 'none', split: null },
  combo: { kind: 'combo', x: 'mes', y: ['total', 'pedidos'], agg: 'sum', sort: 'none', split: null },
  heatmap: { kind: 'heatmap', x: 'mes', y: ['total'], agg: 'sum', sort: 'none', split: 'ciudad' },
  histogram: { kind: 'histogram', x: null, y: ['dias_entrega'], agg: 'none', sort: 'none', split: null },
  box: { kind: 'box', x: 'ciudad', y: ['dias_entrega'], agg: 'none', sort: 'none', split: null },
  kpi: { kind: 'kpi', x: null, y: ['importe'], agg: 'sum', sort: 'none', split: null },
};
const ORDERS_KINDS = ['histogram', 'box', 'kpi'];

export function chartPreview(params: URLSearchParams) {
  const k = params.get('kind');
  const data = k && ORDERS_KINDS.includes(k) ? sampleOrders() : sampleResult;
  return h('div', { style: `width: ${params.get('w') ?? 1100}px; height: ${params.get('h') ?? 520}px; display: flex; flex-direction: column; background: var(--ide-editor)` }, [
    h(ChartView, { columns: data.columns, rows: data.rows, settings: k ? KIND_SETTINGS[k] ?? null : undefined }),
  ]);
}

function stored(data: { columns: ResultColumn[]; rows: Cell[][] }): CellResult {
  return {
    results: [{ columns: data.columns.map((c) => ({ name: c.name, type_name: c.type_name })), rows: data.rows, rows_total: data.rows.length, rows_kept: data.rows.length, truncated: false, rows_affected: null, statement: null }],
    messages: [], error: null, ran_at: '2026-10-18T14:32:05Z', duration_ms: 412, engine: 'postgres', target_label: 'Ventas prod › ventas',
  };
}
const outcome = (data: { columns: ResultColumn[]; rows: Cell[][] }): QueryOutcome =>
  ({ results: [{ ...data, total_rows: data.rows.length, truncated: false, rows_affected: null }], messages: [], error: null, elapsed_ms: 412, plans: [] }) as QueryOutcome;

/** The cell's `chart` (kept across renders). */
let nbChart: ReturnType<typeof ref<Record<string, unknown> | null>> | null = null;

export function notebookCellPreview(params: URLSearchParams) {
  const set = params.get('set') ?? 'charts';
  const data = set === 'map' ? points() : set === 'latlon' ? latLon() : sampleResult;
  const chart = nbChart ??= ref<Record<string, unknown> | null>(set === 'whole'
    ? {
      view: 'chart', active: Number(params.get('active') ?? 0),
      charts: [
        { title: 'Ventas (todo)', type: 'bar', x: 'ciudad', y: ['total'], agg: 'sum', sort: 'value', split: null, whole: true },
        { title: 'Pedidos', type: 'box', x: 'ciudad', y: ['pedidos'], agg: 'none', sort: 'none', split: null, whole: true },
        { title: 'Promedio', type: 'kpi', x: null, y: ['total'], agg: 'avg', sort: 'none', split: null, whole: true },
      ],
    }
    : set === 'charts'
    ? {
      view: 'chart', active: Number(params.get('active') ?? 0),
      charts: [
        { title: 'Ventas por mes', type: 'combo', x: 'mes', y: ['total', 'pedidos'], agg: 'sum', sort: 'none', split: null },
        { title: '', type: 'heatmap', x: 'mes', y: ['total'], agg: 'sum', sort: 'none', split: 'ciudad' },
        { title: 'Total', type: 'kpi', x: null, y: ['total'], agg: 'sum', sort: 'none', split: null },
      ],
    }
    // An older file's one-chart format would read the same; the map keeps its view.
    : { view: 'map' });
  return h('div', { style: `width: ${params.get('w') ?? 1100}px; padding: 8px; background: var(--ide-editor); display: flex; flex-direction: column; gap: 8px` }, [
    h(NotebookResult, {
      result: stored(data), live: outcome(data), fallback: null, running: false, dropped: false,
      source: { connectionId: 'x', database: 'ventas', sql: 'SELECT …' }, dialect: 'postgres', title: 'celda', chart: chart.value,
      onChart: (c: Record<string, unknown>) => { chart.value = c; },
    }),
    h('pre', { class: 'pv-json', style: 'margin: 0; font-size: 11px; color: var(--nm-text-dim); white-space: pre-wrap; max-height: 200px; overflow: auto' },
      JSON.stringify(chart.value, (k, v) => (k === 'points' ? `[${(v as { categories: unknown[] }).categories.length} categorías]` : v))),
  ]);
}

/** A fake chart_aggregate (and cancel_query) for set=whole. */
export function installWholeMock(params: URLSearchParams) {
  let cancelled: (() => void) | null = null;
  const invoke = async (cmd: string, args: { args: Record<string, unknown> }) => {
    if (cmd === 'cancel_query') { cancelled?.(); return null; }
    if (cmd !== 'chart_aggregate') return null;
    await new Promise<void>((res, rej) => {
      const t = setTimeout(res, params.get('slow') ? 30_000 : 400);
      cancelled = () => { clearTimeout(t); rej({ kind: 'cancelled', message: 'cancelada' }); };
    });
    if (params.get('fail')) throw { kind: 'query', message: 'relation "ventas" does not exist' };
    const chart = args.args.chart as { kind: string; x?: string | null; measures?: unknown[] };
    const cities = ['Buenos Aires', 'Córdoba', 'Rosario', 'Mendoza', 'La Plata', 'Tucumán', null];
    if (chart.kind === 'box') {
      return {
        categories: [], series: [], histogram: null, method: 'client', rows_scanned: 1_000_000, rows_capped: true, groups_truncated: false,
        boxes: cities.map((c, i) => {
          const [q1, median, q3] = [80 + i * 4, 120 + i * 6, 160 + i * 5];
          const outliers = [-40 - i * 7, q3 + 140 + i * 3, q3 + 190 + i * 11, 420 + i * 9];
          return {
            category: c, count: 140_000 + i, min: outliers[0], q1, median, q3, max: outliers[3],
            whisker_low: 18 + i, whisker_high: q3 + 95 + i * 2, outlier_count: 2_400 + i * 50, outliers,
          };
        }),
      };
    }
    if (chart.kind === 'group' && !chart.x) {
      // A KPI: one row of measures over the whole result.
      return {
        categories: [null], boxes: null, histogram: null, method: 'server', rows_scanned: 1_284_310, rows_capped: false, groups_truncated: false,
        series: [{ measure: 0, split: null, values: [7_342.55] }, { measure: 1, split: null, values: [1_280_002] }],
      };
    }
    return {
      categories: cities, boxes: null, histogram: null, method: 'server', rows_scanned: null, rows_capped: false, groups_truncated: true,
      series: (chart.measures ?? []).map((_, m) => ({ measure: m, split: null, values: cities.map((_, i) => 9_000_000 - i * 850_000 + m) })),
    };
  };
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = { transformCallback: () => 0, invoke };
}
