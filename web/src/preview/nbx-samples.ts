// view=nbxresults[&theme=light][&dialog=1][&full=1]
//   A notebook's cells (NotebookCell.vue) querying three engines, and a
//   "Resultados del notebook" cell joining their results in DuckDB, with
//   the tables it loaded. dialog=1: the target dialog with that option.
//   full=1: the whole NotebookView over an untitled draft of those cells
//   (the last one never run), for its editor's completion.
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

export function notebookResultsPreview(params: URLSearchParams) {
  if (params.get('full')) return fullNotebook(params);
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
  cell.result = outcomeToResult(o, { script: JOIN.source, ranAt: ran, engine: 'duckdb', targetLabel: 'Resultados del notebook' });
  rows.push({
    cell, live: o, loaded: LOADED,
    ctx: {
      language: 'sql', dialect: 'standard', schema: Object.fromEntries(SAMPLES.map((s) => [s.name, s.columns.map((c) => c.name)])),
      snippets: [], target: 'Resultados del notebook', overridden: true, unresolved: false, localFiles: true, grantedFiles: [],
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
