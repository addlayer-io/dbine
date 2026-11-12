// view=jobs[&theme=light] (preview/main.ts): "Programadas" with its two
// sections over a fake backend — a SQL Server connection with Agent jobs
// (one failed, one running, one disabled), a Postgres one whose pg_cron
// lives in another database (a note) and a DuckDB one (no jobs: not
// listed) — next to a job's tab with its steps and runs.
import { h } from 'vue';
import type { JobDetail, JobList, JobRun, ServerJob } from '../api/jobs';
import ScheduledSidebar from '../components/ScheduledSidebar.vue';
import ServerJobView from '../views/ServerJobView.vue';
import { useConnectionsStore } from '../stores/connections';
import { useServerJobsStore } from '../stores/serverJobs';

const job = (o: Partial<ServerJob> & { id: string; name: string }): ServerJob => ({
  kind: 'SQL Server Agent', group: null, description: null, owner: 'sa', enabled: true, schedule: null,
  running: false, last_outcome: 'succeeded', last_started: '2026-11-12T03:00:00Z', last_duration_ms: 42_000,
  next_run: '2026-11-13T03:00:00Z', can_run: true, can_stop: true, can_enable: true, has_history: true, has_script: true, ...o,
});

const NIGHTLY = job({
  id: '6c1f0d2e-9a1b-4c3d-8e7f-001122334455', name: 'Backup nocturno', group: 'Database Maintenance',
  description: 'Backup completo de ventas y crm', schedule: 'Todos los días a las 03:00',
});
const LISTS: Record<string, JobList> = {
  ms: {
    jobs: [
      NIGHTLY,
      job({ id: 'a1', name: 'Recalcular comisiones', group: 'Ventas', schedule: 'Cada hora', last_outcome: 'failed', last_duration_ms: 3_100, next_run: '2026-11-12T15:00:00Z' }),
      job({ id: 'a2', name: 'Carga de stock desde ERP', group: 'ETL', schedule: 'Cada 15 minutos', running: true, last_outcome: 'running', last_duration_ms: null }),
      job({ id: 'a3', name: 'Purgar historial', group: 'Database Maintenance', enabled: false, schedule: 'Domingos a las 02:00', next_run: null }),
      job({ id: 'a4', name: 'Reindexar a pedido', schedule: null, next_run: null, last_outcome: null, last_started: null, last_duration_ms: null }),
    ],
  },
  pg: { jobs: [], notes: ['pg_cron está instalado en la base «postgres»: conectate a esa base para ver sus jobs'] },
};
const DETAIL: JobDetail = {
  job: NIGHTLY,
  steps: [
    { name: 'Backup de ventas', kind: 'T-SQL', target: 'ventas', command: "BACKUP DATABASE [ventas] TO DISK = N'/var/opt/mssql/backup/ventas.bak' WITH COMPRESSION, INIT" },
    { name: 'Backup de crm', kind: 'T-SQL', target: 'crm', command: "BACKUP DATABASE [crm] TO DISK = N'/var/opt/mssql/backup/crm.bak' WITH COMPRESSION, INIT" },
    { name: 'Avisar', kind: 'T-SQL', target: 'msdb', command: "EXEC msdb.dbo.sp_send_dbmail @recipients = N'dba@example.com', @subject = N'Backup OK'" },
  ],
  schedules: [],
  properties: [
    { group: '', label: 'Categoría', value: 'Database Maintenance' },
    { group: '', label: 'Creado', value: '2025-03-14 10:22' },
    { group: 'Notificaciones', label: 'Si falla, avisar a', value: 'Operador DBA (mail)' },
  ],
};
const RUNS: JobRun[] = [
  { id: '9', started: '2026-11-12T03:00:00Z', duration_ms: 42_000, outcome: 'succeeded', trigger: 'programada', message: 'El job terminó bien. Lo inició la programación 7 (Diario 03:00).', steps: [
    { name: 'Backup de ventas', outcome: 'succeeded', started: '2026-11-12T03:00:00Z', duration_ms: 31_000, message: 'Processed 52128 pages for database ventas.' },
    { name: 'Backup de crm', outcome: 'succeeded', started: '2026-11-12T03:00:31Z', duration_ms: 10_000, message: 'Processed 9120 pages for database crm.' },
    { name: 'Avisar', outcome: 'succeeded', started: '2026-11-12T03:00:41Z', duration_ms: 1_000, message: 'Mail queued.' },
  ] },
  { id: '8', started: '2026-11-11T03:00:00Z', duration_ms: 12_000, outcome: 'failed', trigger: 'programada', message: 'El job falló en el paso 2 (Backup de crm).', steps: [
    { name: 'Backup de ventas', outcome: 'succeeded', started: '2026-11-11T03:00:00Z', duration_ms: 11_000, message: 'Processed 52011 pages for database ventas.' },
    { name: 'Backup de crm', outcome: 'failed', started: '2026-11-11T03:00:11Z', duration_ms: 1_000, message: "Cannot open backup device '/var/opt/mssql/backup/crm.bak'. Operating system error 28 (No space left on device). [SQLSTATE 42000] (Error 3201)" },
  ] },
  { id: '7', started: '2026-11-10T14:12:09Z', duration_ms: 39_500, outcome: 'succeeded', trigger: 'manual', message: null, steps: [] },
  { id: '6', started: '2026-11-10T03:00:00Z', duration_ms: 40_100, outcome: 'canceled', trigger: 'programada', message: 'Lo detuvo el usuario sa.', steps: [] },
];

export function installJobsMock() {
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => 0,
    invoke: async (cmd: string, payload?: { args?: { connection_id?: string } }) => {
      const cid = payload?.args?.connection_id ?? '';
      switch (cmd) {
        case 'scheduled_tasks_list': return [];
        case 'jobs_list': return LISTS[cid] ?? { jobs: [] };
        case 'jobs_detail': return DETAIL;
        case 'jobs_runs': return RUNS;
        default: return null;
      }
    },
  };
}

export function setupJobs() {
  const conns = useConnectionsStore();
  const driver = (id: string, name: string, jobs: boolean) =>
    ({ id, name, capabilities: { create_database: true, drop_database: true, foreign_keys: true, monitor: true, jobs } }) as never;
  conns.drivers = [driver('sqlserver', 'SQL Server', true), driver('postgres', 'PostgreSQL', true), driver('duckdb', 'DuckDB', false)];
  const conn = (id: string, name: string, drv: string) => ({
    id, name, color: null, folder_id: null, tags: [], save_password: true, updated_at: '',
    config: { driver: drv, host: 'db', port: 0, database: '', username: 'app', password: null, encrypt: true, trust_server_certificate: false, read_only: false, options: {} },
  });
  conns.list = [conn('ms', 'SQL Server · Producción', 'sqlserver'), conn('pg', 'Ventas PG', 'postgres'), conn('dk', 'Depósito (DuckDB)', 'duckdb')] as never;
  conns.ensureConnected = async () => true;
  const jobs = useServerJobsStore();
  jobs.expanded = ['ms', 'pg'];
}

export function jobsPreview() {
  return h('div', { style: 'display: flex; height: 100vh; background: var(--nm-bg)' }, [
    h('div', { style: 'width: 300px; flex: none; border-right: 1px solid var(--nm-border-soft); background: var(--ide-sidebar-bg, var(--nm-bg))' }, [h(ScheduledSidebar)]),
    h('div', { style: 'flex: 1; min-width: 0; display: flex; flex-direction: column' }, [
      h(ServerJobView, { tab: { id: 'sj', kind: 'serverJob', connectionId: 'ms', database: '', jobId: NIGHTLY.id, jobName: NIGHTLY.name, preview: false } as never, active: true }),
    ]),
  ]);
}
