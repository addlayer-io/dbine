// view=nbparams[&theme=light][&open=panel|dialog|edit][&w=…][&h=…]
//   A notebook with parameters (docs/notebooks.md#parameters): the whole
//   NotebookView over an untitled draft whose cells use `{{mes}}`,
//   `{{region}}`, `{{tope}}`…, one result made with other values (marked
//   stale) and one with the values in use. open=panel: the "Parámetros"
//   panel open; open=dialog: the run's dialog ("Ejecutar todo" with a
//   required value missing); open=edit: the dialog from the toolbar's
//   values.
import { h, nextTick } from 'vue';
import NotebookView from '../views/NotebookView.vue';
import { notebookDraftKey, type NotebookTab } from '../stores/tabs';
import type { Cell, NotebookCell, NotebookParameter, QueryOutcome, ResultColumn } from '../api/types';
import { newCell, outcomeToResult } from '../composables/notebookModel';

const PARAMETERS: NotebookParameter[] = [
  { name: 'mes', type: 'text', required: true, description: 'Mes del informe, AAAA-MM' },
  { name: 'region', type: 'list', options: ['Norte', 'Centro', 'Sur'], default: 'Norte' },
  { name: 'desde', type: 'date', default: '2026-01-01' },
  { name: 'tope', type: 'number', default: 100 },
  { name: 'solo_activos', type: 'boolean', default: true, description: 'Excluir clientes dados de baja' },
];

const outcome = (columns: ResultColumn[], rows: Cell[][], ms: number): QueryOutcome =>
  ({ results: [{ columns, rows, total_rows: rows.length, truncated: false, rows_affected: null }], messages: [], error: null, elapsed_ms: ms, plans: [], log: [], errors: [] }) as QueryOutcome;
const cols = (...names: string[]): ResultColumn[] => names.map((name) => ({ name, type_name: '' }));

function sqlCell(name: string, source: string, language: string, n: number | null): NotebookCell {
  const c = newCell('sql', source);
  c.name = name;
  c.language = language;
  c.execution_count = n;
  return c;
}

export function notebookParamsPreview(params: URLSearchParams) {
  const ran = new Date('2026-10-18T14:32:05Z');
  const intro = newCell('markdown', '## Ventas del mes\nCada celda usa los parámetros del notebook: `{{mes}}`, `{{region}}`, `{{tope}}`.');
  const ventas = sqlCell(
    'ventas', "SELECT cliente, sum(total) AS total\nFROM ventas\nWHERE mes = {{mes}} AND region = {{region}} AND activo = {{solo_activos}}\nGROUP BY cliente\nORDER BY total DESC\nLIMIT {{tope}}", 'sql', 1,
  );
  ventas.result = {
    ...outcomeToResult(outcome(cols('cliente', 'total'), [['ACME', '18250.00'], ['Globex', '9120.75'], ['Initech', '6480.00']], 412), {
      script: ventas.source, ranAt: ran, engine: 'postgres', targetLabel: 'Ventas PG › ventas',
    }),
    // Made with September: the values in use say October.
    params: { mes: '2026-09', region: 'Norte', solo_activos: true, tope: 100 },
  };
  const clientes = sqlCell('clientes', 'db.clientes.find({ alta: { $gte: {{desde}} }, region: {{region}} }, { _id: 0, nombre: 1, alta: 1 })', 'json', 2);
  clientes.result = {
    ...outcomeToResult(outcome(cols('nombre', 'alta'), [['ACME', '2026-02-03'], ['Globex', '2026-05-11']], 18), {
      script: clientes.source, ranAt: ran, engine: 'mongodb', targetLabel: 'CRM › crm',
    }),
    params: { desde: '2026-01-01', region: 'Norte' },
  };
  const tab: NotebookTab = { id: 'nbparams', kind: 'notebook', connectionId: '', database: '', preview: false, untitled: 1 };
  try {
    localStorage.setItem(notebookDraftKey(tab.id), JSON.stringify({
      target: null, save_results: true, save_results_set: true, cells: [intro, ventas, clientes], parameters: PARAMETERS,
      origin: 'dbine', extra_metadata: {}, warnings: [],
    }));
  } catch { /* storage blocked */ }
  const open = params.get('open');
  if (open) {
    // The toolbar's own buttons, once the view is up.
    const click = (sel: string) => (document.querySelector(sel) as HTMLElement | null)?.click();
    const tryOpen = (n: number) => {
      const bar = document.querySelector('.nb-bar');
      if (!bar) { if (n > 0) setTimeout(() => tryOpen(n - 1), 100); return; }
      if (open === 'panel') click('.nb-bar [aria-pressed]');
      else if (open === 'edit') click('.nb-pval');
      else if (open === 'dialog') click('.nb-bar .el-button--primary');
    };
    void nextTick(() => setTimeout(() => tryOpen(30), 200));
  }
  return h('div', { style: `width: ${params.get('w') ?? 1180}px; height: ${params.get('h') ?? 760}px; display: flex; background: var(--ide-editor)` }, [h(NotebookView, { tab })]);
}
