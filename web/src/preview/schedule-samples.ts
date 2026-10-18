// view=schedule[&set=report|problems][&theme=light] (preview/main.ts): the
// "Ejecutar notebook" step of a scheduled task followed by "Enviar un mail"
// with its HTML body, over a fake backend. `report`: a notebook whose cells
// resolve, with a production base waiting for its opt-in and one file
// allowed; `problems`: a name several connections share, a missing
// connection, files without "Permitir siempre", a cell that writes, a
// write cell and parameters.
import { h, reactive } from 'vue';
import type { NotebookPlan, NotebookResolved, Step } from '../api/scheduled';
import ScheduledStepEditor from '../components/ScheduledStepEditor.vue';
import { useConnectionsStore } from '../stores/connections';

const at = (s: string) => new Date(s).getTime();
const resolved = (connection_id: string, label: string, extra: Partial<NotebookResolved> = {}): NotebookResolved => ({
  connection_id, database: '', label, environment: null, confirm_run: false, production: false, read_only: false, ...extra,
});
const PG_PROD = resolved('pg', 'prod · Ventas PG › ventas', { database: 'ventas', environment: 'prod', confirm_run: true });

const PLANS: Record<string, NotebookPlan> = {
  report: {
    cells: [
      { id: 'a', name: 'ventas_pg', dataframes: false, stored: 'prod', target: PG_PROD, problem: null, writes: null },
      { id: 'b', name: 'clientes_ms', dataframes: false, stored: 'SQL Server · CRM › crm', target: resolved('ms', 'SQL Server · CRM › crm', { database: 'crm', read_only: true }), problem: null, writes: null },
      { id: 'c', name: 'resumen', dataframes: true, stored: 'DataFrame SQL', target: null, problem: null, writes: null },
    ],
    confirm: [PG_PROD],
    files: [{ path: '/Users/ana/Proyectos/ventas/datos/zonas.csv', display: 'datos/zonas.csv', size: 4_210, mtime_ms: at('2026-10-17T09:12:00') }],
    pending: [], blocked: [], file_problems: [], parameters: [], write_cells: [], warnings: [],
  },
  problems: {
    cells: [
      { id: 'a', name: 'ventas', dataframes: false, stored: 'Ventas', target: null, problem: { kind: 'ambiguous', name: 'Ventas', candidates: [{ id: 'pg', name: 'Ventas' }, { id: 'pg2', name: 'Ventas' }], message: 'hay varias conexiones «Ventas» en esta máquina: elegí cuál es en el paso de la tarea' }, writes: null },
      { id: 'b', name: 'stock', dataframes: false, stored: 'Depósito', target: null, problem: { kind: 'connection', name: 'Depósito', candidates: [], message: 'no hay ninguna conexión «Depósito» en esta máquina: elegí cuál es en el paso de la tarea' }, writes: null },
      { id: 'c', name: 'limpieza', dataframes: false, stored: 'SQL Server · CRM', target: resolved('ms', 'SQL Server · CRM › crm', { database: 'crm', production: true }), problem: null, writes: 'DELETE' },
      { id: 'd', name: 'cruce', dataframes: true, stored: 'DataFrame SQL', target: null, problem: null, writes: null },
      { id: 'e', name: 'guardar', dataframes: true, stored: 'DataFrame SQL', target: null, problem: null, writes: null },
    ],
    confirm: [resolved('ms', 'SQL Server · CRM › crm', { database: 'crm', production: true })],
    files: [],
    pending: [
      { path: '/Users/ana/Downloads/CustomerTest.csv', display: '~/Downloads/CustomerTest.csv', size: 48_213, mtime_ms: at('2026-10-17T09:12:00') },
      { path: '/Users/ana/exports/ventas_2026-10.csv', display: '~/exports/ventas_2026-10.csv', size: 402_118, mtime_ms: at('2026-10-18T07:00:00'), pattern: '/Users/ana/exports/ventas_*.csv' },
    ],
    blocked: [], file_problems: [],
    parameters: ['desde', 'region'],
    write_cells: [{ cell_id: 'e', name: 'guardar', target: 'dbo.resumen_ventas', mode: 'reemplazar' }],
    warnings: [],
  },
};

export function installScheduleMock(params: URLSearchParams) {
  const plan = PLANS[params.get('set') ?? 'report'] ?? PLANS.report;
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => 0,
    invoke: async (cmd: string) => {
      switch (cmd) {
        case 'scheduled_notebook_plan': return plan;
        case 'mail_settings_get': return { settings: { host: 'smtp.example.com', port: 587, security: 'starttls', user: '', from_address: 'dbine@example.com', from_name: '' }, has_password: false };
        default: return null;
      }
    },
  };
}

export function setupScheduleConnections() {
  const conns = useConnectionsStore();
  const conn = (id: string, name: string, driver: string, tags: string[] = []) => ({
    id, name, color: null, folder_id: null, tags, save_password: true, updated_at: '',
    config: { driver, host: 'db', port: 0, database: '', username: 'app', password: null, encrypt: true, trust_server_certificate: false, read_only: false, options: {} },
  });
  conns.list = [conn('pg', 'Ventas', 'postgres', ['prod']), conn('pg2', 'Ventas', 'postgres'), conn('ms', 'SQL Server · CRM', 'sqlserver', ['prod']), conn('dk', 'Depósito (DuckDB)', 'duckdb')];
}

export function schedulePreview(params: URLSearchParams) {
  const problems = params.get('set') === 'problems';
  const notebook = reactive<Step>({
    id: 'nb', kind: 'run_notebook', name: 'Reporte semanal', on_error: 'stop',
    config: {
      path: problems ? '/Users/ana/Proyectos/ventas/notebooks/Cierre mensual.ipynb' : '/Users/ana/Proyectos/ventas/notebooks/Ventas por zona.ipynb',
      params: problems ? { desde: '{date}' } : {}, targets: {}, unconfirmed: [], write_cells: [],
      save: { mode: 'folder', folder: '/Users/ana/Reportes', file_name: '{notebook}-{date}' },
      export: { format: 'html', folder: '', file_name: '{notebook}-{date}' },
    },
  });
  const mail = reactive<Step>({
    id: 'm', kind: 'send_mail', name: '', on_error: 'stop',
    config: { to: 'gerencia@example.com', cc: '', subject: '{task}: {date}', body: 'Va el reporte de la semana.', html_body: '{steps.1.html}', attachments: ['{steps.1.notebook}'], when: 'always' },
  });
  return h('div', { style: 'width: 980px; padding: 12px 16px; background: var(--nm-bg); overflow: auto; height: 100vh' }, [
    h(ScheduledStepEditor, { step: notebook, index: 0, count: 2 }),
    h(ScheduledStepEditor, { step: mail, index: 1, count: 2 }),
  ]);
}
