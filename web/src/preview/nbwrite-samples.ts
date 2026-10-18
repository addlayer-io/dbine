// view=nbwrite[&state=form|confirm|result|error|full][&prod=1][&theme=light]
//   A notebook's write cell (NotebookWriteCell.vue): a DataFrame SQL cell's
//   result written into a SQL Server table by key. state=form: the form
//   with its preview; confirm: the confirmation every write asks for
//   (prod=1: a production connection, which asks to type the table's name);
//   result: a finished write; error: a write that failed and rolled back.
import { h } from 'vue';
import NotebookWriteCell, { type FrameColumn } from '../components/notebook/NotebookWriteCell.vue';
import NotebookWriteConfirm from '../components/notebook/NotebookWriteConfirm.vue';
import type { WritePlan, WriteTargetInfo } from '../api/types';
import { newCell, newWriteCell } from '../composables/notebookModel';
import NotebookView from '../views/NotebookView.vue';
import { notebookDraftKey, type NotebookTab } from '../stores/tabs';

const FRAMES = ['ventas_pg', 'clientes_mssql', 'resumen'];
const COLUMNS: FrameColumn[] = [
  { name: 'id', type: 'INTEGER' },
  { name: 'nombre', type: 'VARCHAR' },
  { name: 'total', type: 'DECIMAL(38,2)' },
  { name: 'ultimo', type: 'DATE' },
  { name: 'region', type: 'VARCHAR' },
];
const INFO: WriteTargetInfo = {
  modes: ['insert', 'update', 'upsert', 'replace'],
  atomic: true,
  schemaless: false,
  columns: [
    { name: 'id', data_type: 'int', nullable: false, primary_key: true, auto_increment: false, default_value: null },
    { name: 'nombre', data_type: 'nvarchar(40)', nullable: true, primary_key: false, auto_increment: false, default_value: null },
    { name: 'total', data_type: 'decimal(12,2)', nullable: true, primary_key: false, auto_increment: false, default_value: null },
    { name: 'ultimo', data_type: 'date', nullable: true, primary_key: false, auto_increment: false, default_value: null },
    { name: 'actualizado', data_type: 'datetime2', nullable: true, primary_key: false, auto_increment: false, default_value: '(sysdatetime())' },
  ],
};
const PLAN: WritePlan = {
  token: 'preview',
  rows: 1248,
  target_label: 'Ventas prod › ventas › dbo.resumen_mensual',
  connection: 'Ventas prod',
  database: 'ventas',
  table: 'dbo.resumen_mensual',
  engine: 'sqlserver',
  columns: [
    { source: 'id', source_type: 'INTEGER', target: 'id', target_type: 'int', converted: false },
    { source: 'nombre', source_type: 'VARCHAR', target: 'nombre', target_type: 'nvarchar(40)', converted: false },
    { source: 'total', source_type: 'DECIMAL(38,2)', target: 'total', target_type: 'decimal(12,2)', converted: false },
    { source: 'ultimo', source_type: 'DATE', target: 'ultimo', target_type: 'date', converted: false },
  ],
  unmatched_source: ['region'],
  unmatched_target: ['actualizado'],
  keys: ['id'],
  preview: {
    columns: ['id', 'nombre', 'total', 'ultimo'],
    rows: [
      [10, 'Ana', '13.50', '2026-10-03'],
      [20, 'Luis', '7.25', '2026-10-02'],
      [30, 'Eva', '5.00', '2026-10-03'],
      [41, 'Martín', '1204.10', '2026-09-30'],
      [42, 'Lucía', '88.00', null],
    ],
  },
  warnings: ['«nombre»: hay valores de hasta 52 caracteres y «nombre» es nvarchar(40): el motor los rechaza o los corta'],
  atomic: true,
};

function cell(state: string) {
  const c = newWriteCell('resumen');
  c.write = {
    source: 'resumen', target: { connection: 'Ventas prod', database: 'ventas', environment: null }, schema: 'dbo', table: 'resumen_mensual',
    create: false, mode: 'upsert', keys: ['id'], mapping: null,
  };
  if (state === 'result' || state === 'error') c.execution_count = 5;
  if (state === 'result') {
    c.write_result = {
      inserted: 96, updated: 1152, deleted: 0, ran_at: '2026-10-18T14:32:05Z', duration_ms: 1840,
      target_label: PLAN.target_label, error: null, warnings: PLAN.warnings,
    };
  }
  if (state === 'error') {
    c.write_result = {
      inserted: 0, updated: 0, deleted: 0, ran_at: '2026-10-18T14:32:05Z', duration_ms: 610, target_label: PLAN.target_label,
      error: "No se escribió nada (la transacción se deshizo). Violation of PRIMARY KEY constraint 'PK_resumen_mensual'. Cannot insert duplicate key in object 'dbo.resumen_mensual'. The duplicate key value is (10).",
    };
  }
  return c;
}

/** state=full: the whole NotebookView with a DataFrame SQL cell and a write cell under it. */
function fullNotebook(params: URLSearchParams) {
  const sql = newCell('sql', 'SELECT c.id, c.nombre, sum(v.total) AS total, max(v.dia) AS ultimo\nFROM ventas_pg v JOIN clientes_mssql c ON c.id = v.cliente\nGROUP BY ALL');
  sql.name = 'resumen';
  sql.target = { connection: null, database: null, environment: null, results: true };
  const w = newWriteCell('resumen');
  const tab: NotebookTab = { id: 'nbw-full', kind: 'notebook', connectionId: '', database: '', preview: false, untitled: 1 };
  try {
    localStorage.setItem(notebookDraftKey(tab.id), JSON.stringify({ target: null, save_results: true, save_results_set: true, cells: [sql, w], origin: 'dbine', extra_metadata: {}, warnings: [] }));
  } catch { /* storage blocked */ }
  return h('div', { style: `width: ${params.get('w') ?? 1100}px; height: ${params.get('h') ?? 800}px; display: flex; background: var(--ide-editor)` }, [h(NotebookView, { tab })]);
}

export function notebookWritePreview(params: URLSearchParams) {
  const state = params.get('state') ?? 'form';
  if (state === 'full') return fullNotebook(params);
  const prod = params.get('prod') === '1';
  const c = cell(state);
  return h('div', { style: `width: ${params.get('w') ?? 1100}px; padding: 8px 18px 20px 6px; background: var(--ide-editor); display: flex; flex-direction: column; gap: 10px` }, [
    h(NotebookWriteCell, {
      cell: c, index: 3, count: 4, active: true, running: false, busy: false,
      frames: FRAMES, frameColumns: COLUMNS, targetText: 'Ventas prod › ventas', targetUnresolved: false, prod,
      info: INFO, infoError: null, tables: ['clientes', 'resumen_mensual', 'ventas'], hasSchemas: true,
      plan: state === 'form' ? PLAN : null, planning: false, planError: null,
    }),
    state === 'confirm' ? h(NotebookWriteConfirm, { plan: PLAN, mode: 'upsert', prod, runAll: params.get('all') !== '0' }) : null,
  ]);
}
