// view=nbxresults[&theme=light][&dialog=1][&full=1][&set=geo]
//   A notebook's cells (NotebookCell.vue) querying three engines, and a
//   "DataFrame SQL" cell joining their results in DuckDB, with
//   the tables it loaded. dialog=1: the target dialog with that option.
//   full=1: the whole NotebookView over an untitled draft of those cells
//   (the last one never run), for its editor's completion. set=geo: a
//   results cell joining a shapefile (st_read) with a cell, its geometry
//   in the map view and the shapefile with its sidecars in «Tablas:».
import { h } from 'vue';
import NotebookView from '../views/NotebookView.vue';
import { notebookDraftKey, type NotebookTab } from '../stores/tabs';
import NotebookCellView, { type CellContext } from '../components/notebook/NotebookCell.vue';
import NotebookTargetDialog from '../components/notebook/NotebookTargetDialog.vue';
import type { Cell, LoadedTable, NotebookCell, QueryOutcome, ResultColumn } from '../api/types';
import { newCell, outcomeToResult } from '../composables/notebookModel';

interface Sample { name: string; target: string; language: string; source: string; columns: ResultColumn[]; rows: Cell[][]; engine: string; ms: number }

const SAMPLES: Sample[] = [
  {
    name: 'ventas_pg', target: 'Ventas PG › ventas', language: 'sql', engine: 'postgres', ms: 38,
    source: "SELECT id, cliente, total, dia\nFROM ventas\nWHERE dia >= '2026-10-01'",
    columns: [{ name: 'id', type_name: '' }, { name: 'cliente', type_name: '' }, { name: 'total', type_name: '' }, { name: 'dia', type_name: '' }],
    rows: [[1, 10, '12.50', '2026-10-01'], [2, 20, '7.25', '2026-10-02'], [3, 10, '1.00', '2026-10-03'], [4, 30, '5.00', '2026-10-03']],
  },
  {
    name: 'clientes_mssql', target: 'CRM SQL Server › crm', language: 'sql', engine: 'sqlserver', ms: 21,
    source: 'SELECT id, nombre, alta\nFROM dbo.clientes',
    columns: [{ name: 'id', type_name: 'Int4' }, { name: 'nombre', type_name: 'NVarchar' }, { name: 'alta', type_name: 'Datetime2' }],
    rows: [[10, 'Ana', '2026-01-02 03:04:05'], [20, 'Luis', '2026-02-03 04:05:06'], [30, 'Eva', null]],
  },
  {
    name: 'regiones_mongo', target: 'Logística › logistica', language: 'json', engine: 'mongodb', ms: 12,
    source: 'db.regiones.find({}, { _id: 0 })',
    columns: [{ name: 'cliente', type_name: '' }, { name: 'region', type_name: '' }],
    rows: [[10, 'norte'], [20, 'sur'], [30, 'norte']],
  },
];

const JOIN = {
  source: 'SELECT c.nombre, r.region, sum(v.total) AS total, count(*) AS ventas\nFROM ventas_pg v\nJOIN clientes_mssql c ON c.id = v.cliente\nJOIN regiones_mongo r ON r.cliente = v.cliente\nGROUP BY ALL\nORDER BY total DESC',
  columns: [{ name: 'nombre', type_name: 'VARCHAR' }, { name: 'region', type_name: 'VARCHAR' }, { name: 'total', type_name: 'DECIMAL(38,2)' }, { name: 'ventas', type_name: 'BIGINT' }],
  rows: [['Ana', 'norte', '13.50', 2], ['Luis', 'sur', '7.25', 1], ['Eva', 'norte', '5.00', 1]] as Cell[][],
};

const LOADED: LoadedTable[] = [
  { name: 'ventas_pg', rows: 4, capped: false, cached: false, origin: 'run', as_text: false, elapsed_ms: 3 },
  { name: 'clientes_mssql', rows: 3, capped: false, cached: false, origin: 'query', as_text: false, elapsed_ms: 41 },
  { name: 'regiones_mongo', rows: 3, capped: false, cached: true, origin: 'run', as_text: false, elapsed_ms: 0 },
];

const outcome = (columns: ResultColumn[], rows: Cell[][], ms: number): QueryOutcome =>
  ({ results: [{ columns, rows, total_rows: rows.length, truncated: false, rows_affected: null }], messages: [], error: null, elapsed_ms: ms, plans: [], log: [], errors: [] }) as QueryOutcome;

function cellOf(s: { name: string; source: string; language: string }, n: number, results: boolean): NotebookCell {
  const c = newCell('sql', s.source);
  c.name = s.name;
  c.language = s.language;
  c.execution_count = n;
  if (results) c.target = { connection: null, database: null, environment: null, results: true };
  return c;
}

/** full=1: NotebookView itself, its cells from an untitled draft. */
function fullNotebook(params: URLSearchParams) {
  const ran = new Date('2026-10-18T14:32:05Z');
  const cells = SAMPLES.map((s, i) => {
    const cell = cellOf(s, i + 1, false);
    cell.result = outcomeToResult(outcome(s.columns, s.rows, s.ms), { script: s.source, ranAt: ran, engine: s.engine, targetLabel: s.target });
    return cell;
  });
  // Never run: still a table, without columns.
  const pending = cellOf({ name: 'pendiente', source: 'SELECT 1 AS uno', language: 'sql' }, 0, true);
  pending.execution_count = null;
  const last = cellOf({ name: 'resumen', source: '', language: 'sql' }, 0, true);
  last.execution_count = null;
  const tab: NotebookTab = { id: 'nbx-full', kind: 'notebook', connectionId: '', database: '', preview: false, untitled: 1 };
  try {
    localStorage.setItem(notebookDraftKey(tab.id), JSON.stringify({
      target: null, save_results: true, save_results_set: true, cells: [...cells, pending, last], origin: 'dbine', extra_metadata: {}, warnings: [],
    }));
  } catch { /* storage blocked */ }
  return h('div', { style: `width: ${params.get('w') ?? 1100}px; height: ${params.get('h') ?? 900}px; display: flex; background: var(--ide-editor)` }, [h(NotebookView, { tab })]);
}

/** WKB (little-endian) of a polygon's one ring, as `0x…` hex: what DuckDB
 *  gives for a GEOMETRY column. */
function wkbPolygon(ring: [number, number][]): string {
  const buf = new DataView(new ArrayBuffer(9 + 4 + ring.length * 16));
  buf.setUint8(0, 1);
  buf.setUint32(1, 3, true);
  buf.setUint32(5, 1, true);
  buf.setUint32(9, ring.length, true);
  ring.forEach(([x, y], i) => { buf.setFloat64(13 + i * 16, x, true); buf.setFloat64(21 + i * 16, y, true); });
  return `0x${[...new Uint8Array(buf.buffer)].map((b) => b.toString(16).padStart(2, '0')).join('').toUpperCase()}`;
}
const box = (w: number, s: number, e: number, n: number) => wkbPolygon([[w, s], [e, s], [e, n], [w, n], [w, s]]);

const GEO = {
  source: "SELECT z.nombre AS zona, sum(v.total) AS total, count(*) AS ventas, z.geom\nFROM st_read('~/geo/zonas.shp') z\nJOIN ventas_pg v ON ST_Contains(z.geom, ST_Point(v.lon, v.lat))\nGROUP BY ALL\nORDER BY total DESC",
  columns: [{ name: 'zona', type_name: 'VARCHAR' }, { name: 'total', type_name: 'DECIMAL(38,2)' }, { name: 'ventas', type_name: 'BIGINT' }, { name: 'geom', type_name: 'GEOMETRY' }],
  rows: [
    ['Norte', '18250.00', 41, box(-68.5, -28, -62, -22)],
    ['Centro', '15410.50', 37, box(-66, -35, -58.5, -28)],
    ['Cuyo', '9120.75', 18, box(-70.5, -37, -66, -28)],
    ['Patagonia', '6480.00', 12, box(-72, -50, -63, -37)],
  ] as Cell[][],
};
const at = (s: string) => new Date(s).getTime();
const GEO_LOADED: LoadedTable[] = [
  { name: 'ventas_pg', rows: 4, capped: false, cached: false, origin: 'run', as_text: false, elapsed_ms: 3 },
  {
    name: '~/geo/zonas.shp', rows: 4, capped: false, cached: false, origin: 'file', as_text: false, elapsed_ms: 58,
    files: [{
      path: '/Users/ana/geo/zonas.shp', display: '~/geo/zonas.shp', size: 1_482_204, mtime_ms: at('2026-10-14T10:05:00'),
      sidecars: ['cpg', 'dbf', 'prj', 'shx'].map((e, i) => ({ path: `/Users/ana/geo/zonas.${e}`, display: `~/geo/zonas.${e}`, size: [5, 88_410, 145, 1_220][i], mtime_ms: at('2026-10-14T10:05:00') })),
    }],
  },
];

/** set=geo: a results cell over a shapefile, in the map view. */
function geoCell(params: URLSearchParams) {
  const ran = new Date('2026-10-18T14:32:05Z');
  const o = outcome(GEO.columns, GEO.rows, 92);
  const cell = cellOf({ name: 'zonas', source: GEO.source, language: 'sql' }, 2, true);
  cell.result = outcomeToResult(o, { script: GEO.source, ranAt: ran, engine: 'duckdb', targetLabel: 'DataFrame SQL' });
  cell.chart = { view: 'map' };
  const ctx: CellContext = {
    language: 'sql', dialect: 'standard', schema: { ventas_pg: ['id', 'cliente', 'total', 'lon', 'lat'] },
    snippets: [], target: 'DataFrame SQL', overridden: true, unresolved: false, localFiles: true, grantedFiles: [],
  };
  return h('div', { style: `width: ${params.get('w') ?? 1100}px; padding: 8px 18px 20px 6px; background: var(--ide-editor); display: flex; flex-direction: column; gap: 10px` }, [
    h(NotebookCellView, {
      cell, index: 0, count: 1, active: true, running: false, busy: false, editing: false,
      ctx, live: o, fallback: null, source: null, title: 'Zonas', loaded: GEO_LOADED,
    }),
  ]);
}

export function notebookResultsPreview(params: URLSearchParams) {
  if (params.get('full')) return fullNotebook(params);
  if (params.get('set') === 'geo') return geoCell(params);
  const ran = new Date('2026-10-18T14:32:05Z');
  const rows = SAMPLES.map((s, i) => {
    const o = outcome(s.columns, s.rows, s.ms);
    const cell = cellOf(s, i + 1, false);
    cell.result = outcomeToResult(o, { script: s.source, ranAt: ran, engine: s.engine, targetLabel: s.target });
    const ctx: CellContext = { language: s.language as CellContext['language'], schema: {}, snippets: [], target: s.target, overridden: true, unresolved: false };
    return { cell, ctx, live: o, loaded: null as LoadedTable[] | null };
  });
  const o = outcome(JOIN.columns, JOIN.rows, 64);
  const cell = cellOf({ name: 'resumen', source: JOIN.source, language: 'sql' }, 4, true);
  cell.result = outcomeToResult(o, { script: JOIN.source, ranAt: ran, engine: 'duckdb', targetLabel: 'DataFrame SQL' });
  rows.push({
    cell, live: o, loaded: LOADED,
    ctx: {
      language: 'sql', dialect: 'standard', schema: Object.fromEntries(SAMPLES.map((s) => [s.name, s.columns.map((c) => c.name)])),
      snippets: [], target: 'DataFrame SQL', overridden: true, unresolved: false, localFiles: true, grantedFiles: [],
    },
  });
  return h('div', { style: `width: ${params.get('w') ?? 1100}px; padding: 8px 18px 20px 6px; background: var(--ide-editor); display: flex; flex-direction: column; gap: 10px` }, [
    ...rows.map((r, i) => h(NotebookCellView, {
      cell: r.cell, index: i, count: rows.length, active: i === rows.length - 1, running: false, busy: false, editing: false,
      ctx: r.ctx, live: r.live, fallback: null, source: null, title: `Cruce ${i + 1}`, loaded: r.loaded,
    })),
    params.get('dialog')
      ? h(NotebookTargetDialog, { mode: 'pick', initial: { connection: null, database: null, environment: null, results: true }, inherit: true, projectId: null, file: 'u:preview' })
      : null,
  ]);
}
