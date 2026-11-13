// view=scheduler[&conn=ms|ora][&kind=job|operator|alert|window][&set=perf][&job=<Oracle job>][&theme=light],
// view=scheduler-tree[&conn=ora], view=scheduler-log and
// view=scheduler-jobtab[&conn=ora&job=<Oracle job>] (preview/main.ts): the
// scheduler designer over a fake SQL Server Agent and a fake Oracle Scheduler.
// `scheduler` mounts a designer tab: a job with its six sections (three
// steps of different types whose actions point at each other, two
// schedules — one shared, whose "Desde las / Hasta las" use `when_all` —,
// an alert, notifications and the targets, read only), a new operator
// (`kind=operator`) or an alert whose fields follow its type (`kind=alert`;
// `set=perf`: a performance condition whose counter depends on the object
// and whose instance on the counter, free text where nothing is listed).
// On Oracle (`conn=ora`): a job whose type picks a tab (Argumentos) and a
// list (Destinos remotos), and a window (`kind=window`) with the "Abrir
// ventana" / "Cerrar ventana" actions. Each Oracle job narrows its kind's
// two actions (`job=EXPORTAR_FACTURAS`: running, "Forzar la detención"
// only; the others none) and SYS's can't be edited nor dropped
// (`job=PURGE_LOG`). `scheduler-tree` shows "Programadas"
// with the connection's tree (jobs, schedules, operators — one that can't
// be deleted —, alerts, proxies, categories and the Agent's log; on Oracle
// jobs and windows) next to the designer; `scheduler-log` the Agent's error
// logs; `scheduler-jobtab` a job's tab alone, as restored with the app with
// its connection closed in the sidebar. On a BigQuery-like connection
// (`conn=bq`): scheduled queries whose "Ejecutar para fechas pasadas…" asks
// for values first, as the driver's form has it — a period of days or a
// single run time (`when`), the schedule read-only, its notes, a script with
// "⚠" lines, and a refusal for a period that ends before it starts, with the
// form's values kept for a retry. The script the fake drivers write
// follows the values as edited.
import { h } from 'vue';
import type { JobDetail, JobList, ServerJob } from '../api/jobs';
import type { FormField, FormKind, FormRow, FormValues, ObjectForm, SchedulerInfo, SchedulerItem, SchedulerList } from '../api/scheduler';
import type { MonitorTable } from '../api/types';
import ScheduledSidebar from '../components/ScheduledSidebar.vue';
import SchedulerLogView from '../views/SchedulerLogView.vue';
import ServerJobView from '../views/ServerJobView.vue';
import SchedulerObjectView from '../views/SchedulerObjectView.vue';
import { useConnectionsStore } from '../stores/connections';
import { useServerJobsStore } from '../stores/serverJobs';
import { useTabsStore, type SchedulerObjectTab } from '../stores/tabs';

const JOB_ID = '6c1f0d2e-9a1b-4c3d-8e7f-001122334455';
const ALERT_ID = 'alert-9002';
const ALERT_PERF_ID = 'alert-log-80';
const ORA_JOB_ID = 'ERP.CIERRE_DIARIO';
const BQ_JOB_ID = 'projects/analytics-prod/locations/us/transferConfigs/6512a3f0-0000-2e4b-9c11-089e08e4d1b2';
const WINDOW_ID = 'SYS.WEEKNIGHT_WINDOW';

const job = (o: Partial<ServerJob> & { id: string; name: string }): ServerJob => ({
  kind: 'SQL Server Agent', group: null, description: null, owner: 'sa', enabled: true, schedule: null,
  running: false, last_outcome: 'succeeded', last_started: '2026-11-12T03:00:00Z', last_duration_ms: 42_000,
  next_run: '2026-11-13T03:00:00Z', can_run: true, can_stop: true, can_enable: true, has_history: true, has_script: true, ...o,
});
const NIGHTLY = job({ id: JOB_ID, name: 'Backup nocturno', group: 'Database Maintenance', description: 'Backup completo de ventas y crm', schedule: 'Todos los días a las 03:00' });
const JOBS: JobList = {
  jobs: [
    NIGHTLY,
    job({ id: 'a1', name: 'Recalcular comisiones', group: 'Ventas', schedule: 'Cada hora', last_outcome: 'failed', next_run: '2026-11-12T15:00:00Z' }),
    job({ id: 'a2', name: 'Carga de stock desde ERP', group: 'ETL', schedule: 'Cada 15 minutos', running: true, last_outcome: 'running', last_duration_ms: null }),
    job({ id: 'a3', name: 'Purgar historial', group: 'Database Maintenance', enabled: false, schedule: 'Domingos a las 02:00', next_run: null }),
    job({ id: 'a4', name: 'syspolicy_purge_history', system: true, schedule: 'Todos los días a las 02:00' }),
  ],
};
const DETAIL: JobDetail = {
  job: NIGHTLY,
  steps: [
    { name: 'Backup de ventas', kind: 'T-SQL', target: 'master', command: 'BACKUP DATABASE [ventas] …' },
    { name: 'Comprimir y subir a Azure', kind: 'PowerShell', target: null, command: 'Compress-Archive …' },
    { name: 'Avisar al DBA', kind: 'CmdExec', target: null, command: 'sqlcmd …' },
  ],
  schedules: [],
  properties: [],
};

const INFO: SchedulerInfo = {
  kinds: [
    { key: 'job', label: 'Jobs', singular: 'job', can_create: true, can_edit: true, can_delete: true, run_from_step: true, clear_history: true },
    { key: 'schedule', label: 'Programaciones', singular: 'programación', can_create: true, can_edit: true, can_delete: true },
    { key: 'operator', label: 'Operadores', singular: 'operador', can_create: true, can_edit: true, can_delete: true },
    { key: 'alert', label: 'Alertas', singular: 'alerta', can_create: true, can_edit: true, can_delete: true },
    { key: 'proxy', label: 'Proxies', singular: 'proxy', can_create: true, can_edit: true, can_delete: true },
    { key: 'category', label: 'Categorías', singular: 'categoría', can_create: true, can_edit: true, can_delete: true },
  ],
  logs: true,
  notes: ['Database Mail no tiene un perfil predeterminado: los avisos por mail no salen hasta que se configure uno.'],
};

const item = (id: string, name: string, o: Partial<Omit<SchedulerItem, 'id' | 'name'>> = {}): SchedulerItem => ({
  ...o, id, name, description: o.description ?? null, enabled: o.enabled === undefined ? true : o.enabled, system: !!o.system,
});
const ITEMS: Record<string, SchedulerList> = {
  schedule: {
    items: [
      item('7', 'Diario 03:00', { description: 'La usan 2 jobs' }),
      item('12', 'Cada 4 horas en horario hábil'),
      item('15', 'Domingos 02:00', { enabled: false }),
      item('1', 'syspolicy_purge_history_schedule', { system: true }),
    ],
  },
  operator: {
    items: [
      item('1', 'DBA de guardia', { description: 'dba@example.com' }),
      item('2', 'Equipo de datos', { description: 'datos@example.com' }),
      item('3', 'Mesa de ayuda', { enabled: false }),
      // The master server's operator: a multiserver job uses it, so it can't go.
      item('4', 'MSXOperator', { description: 'Lo usan los jobs multiservidor', can_delete: false }),
    ],
  },
  alert: {
    items: [
      item(ALERT_ID, 'Log de transacciones lleno (9002)'),
      item(ALERT_PERF_ID, 'Log de ventas al 80 %'),
      item('alert-17', 'Severidad 017: recursos insuficientes'),
      item('alert-823', 'Error de E/S (823)', { enabled: false }),
    ],
  },
  proxy: { items: [item('1', 'Proxy PowerShell', { enabled: true }), item('2', 'Proxy CmdExec ETL')] },
  category: {
    items: [
      // The engine's own: the driver says they can't go (`system` alone doesn't).
      item('0', '[Uncategorized (Local)]', { enabled: null, system: true, can_delete: false }),
      item('3', 'Database Maintenance', { enabled: null, system: true, can_delete: false }),
      item('100', 'ETL', { enabled: null }),
      item('101', 'Ventas', { enabled: null }),
    ],
    notes: ['Las categorías que empiezan con «[» y las de replicación vienen con el motor.'],
  },
};

// -- the forms ----------------------------------------------------------------------------

const F = (key: string, label: string, kind: FormKind, o: Partial<FormField> = {}): FormField => ({ key, label, kind, ...o });
const sel = (options: [string, string][]): FormKind => ({ type: 'select', options });
const ref = (source: string, multiple = false): FormKind => ({ type: 'ref', source, multiple });
const num = (min: number | null, max: number | null): FormKind => ({ type: 'number', min, max });
const ACTIONS: [string, string][] = [['3', 'Ir al paso siguiente'], ['1', 'Salir del job informando que terminó bien'], ['2', 'Salir del job informando que falló'], ['4', 'Ir al paso:']];
const LEVELS: [string, string][] = [['0', 'No avisar'], ['1', 'Cuando el job termina bien'], ['2', 'Cuando el job falla'], ['3', 'Cuando el job termina']];
const DAYS: [string, string][] = [['mon', 'Lunes'], ['tue', 'Martes'], ['wed', 'Miércoles'], ['thu', 'Jueves'], ['fri', 'Viernes'], ['sat', 'Sábado'], ['sun', 'Domingo']];

const STEP_FIELDS: FormField[] = [
  F('step_name', 'Nombre del paso', { type: 'text' }, { required: true }),
  F('subsystem', 'Tipo', sel([['TSQL', 'Script de Transact-SQL (T-SQL)'], ['PowerShell', 'PowerShell'], ['CmdExec', 'Sistema operativo (CmdExec)'], ['SSIS', 'Paquete de SQL Server Integration Services']]), { required: true, default: 'TSQL' }),
  F('proxy_name', 'Ejecutar como', ref('proxy:PowerShell'), { when: ['subsystem', ['PowerShell']], default: '' }),
  F('proxy_name', 'Ejecutar como', ref('proxy:CmdExec'), { when: ['subsystem', ['CmdExec']], default: '' }),
  F('database_name', 'Base de datos', ref('database'), { when: ['subsystem', ['TSQL']], required: true, default: 'master' }),
  F('database_user_name', 'Ejecutar como usuario', { type: 'text' }, { when: ['subsystem', ['TSQL']], help: 'Solo para jobs de un sysadmin: el usuario de la base con el que corre el comando.' }),
  F('command', 'Comando', { type: 'code', language: 'tsql' }, { when: ['subsystem', ['TSQL']], required: true }),
  F('command', 'Comando', { type: 'code', language: 'powershell' }, { when: ['subsystem', ['PowerShell']], required: true }),
  F('command', 'Comando', { type: 'code', language: 'shell' }, { when: ['subsystem', ['CmdExec']], required: true }),
  F('cmdexec_success_code', 'Código de salida correcto', num(0, null), { when: ['subsystem', ['CmdExec']], default: 0 }),
  F('on_success_action', 'Si termina bien', sel(ACTIONS), { default: '3' }),
  F('on_success_step', 'Paso', ref('list:steps'), { when: ['on_success_action', ['4']], required: true }),
  F('on_fail_action', 'Si falla', sel(ACTIONS), { default: '2' }),
  F('on_fail_step', 'Paso', ref('list:steps'), { when: ['on_fail_action', ['4']], required: true }),
  F('retry_attempts', 'Reintentos', num(0, 9999), { default: 0 }),
  F('retry_interval', 'Intervalo entre reintentos (minutos)', num(0, 9999), { default: 0 }),
  F('output_file_name', 'Archivo de salida', { type: 'text' }, { placeholder: '/var/opt/mssql/log/paso.txt' }),
  F('append_output', 'Agregar al archivo de salida', { type: 'bool' }, { default: false }),
];
const STEPS: FormRow[] = [
  {
    step_id: 1, step_name: 'Backup de ventas', subsystem: 'TSQL', database_name: 'master',
    command: "BACKUP DATABASE [ventas]\n  TO DISK = N'/var/opt/mssql/backup/ventas.bak'\n  WITH COMPRESSION, INIT, CHECKSUM, STATS = 10;\n\nBACKUP DATABASE [crm]\n  TO DISK = N'/var/opt/mssql/backup/crm.bak'\n  WITH COMPRESSION, INIT, CHECKSUM;",
    on_success_action: '3', on_fail_action: '4', on_fail_step: 'Avisar al DBA', retry_attempts: 2, retry_interval: 5, append_output: false,
  },
  {
    step_id: 2, step_name: 'Comprimir y subir a Azure', subsystem: 'PowerShell', proxy_name: 'Proxy PowerShell',
    command: "$files = Get-ChildItem /var/opt/mssql/backup/*.bak\nCompress-Archive -Path $files -DestinationPath /backup/nocturno.zip -Force\nazcopy copy /backup/nocturno.zip 'https://backups.blob.core.windows.net/sql/'",
    on_success_action: '1', on_fail_action: '4', on_fail_step: 'Avisar al DBA', retry_attempts: 1, retry_interval: 10, append_output: false,
  },
  {
    step_id: 3, step_name: 'Avisar al DBA', subsystem: 'CmdExec', proxy_name: '', cmdexec_success_code: 0,
    command: "sqlcmd -S . -E -Q \"EXEC msdb.dbo.sp_send_dbmail @profile_name = N'Alertas', @recipients = N'dba@example.com', @subject = N'Backup nocturno falló'\"",
    on_success_action: '2', on_fail_action: '2', retry_attempts: 0, retry_interval: 0, append_output: false,
  },
];

const SCHEDULE_FIELDS: FormField[] = [
  F('name', 'Nombre', { type: 'text' }, { required: true }),
  F('jobs_using', 'Jobs que la usan', num(null, null), { read_only: true, help: 'Una programación compartida cambia para todos los jobs que la usan.' }),
  F('enabled', 'Habilitada', { type: 'bool' }, { default: true }),
  F('freq_type', 'Frecuencia', sel([['once', 'Una vez'], ['daily', 'Diaria'], ['weekly', 'Semanal'], ['monthly', 'Mensual'], ['monthly_relative', 'Mensual (relativa)'], ['agent_start', 'Al iniciar el Agente'], ['idle', 'Cuando la CPU esté inactiva']]), { required: true, default: 'daily' }),
  F('one_time_date', 'Fecha', { type: 'date' }, { when: ['freq_type', ['once']], required: true }),
  F('one_time_time', 'Hora', { type: 'time' }, { when: ['freq_type', ['once']], required: true }),
  F('daily_interval', 'Cada (días)', num(1, 100), { when: ['freq_type', ['daily']], required: true, default: 1 }),
  F('weekly_interval', 'Cada (semanas)', num(1, 100), { when: ['freq_type', ['weekly']], required: true, default: 1 }),
  F('weekly_days', 'Días', { type: 'multi_select', options: DAYS }, { when: ['freq_type', ['weekly']], required: true }),
  F('monthly_day', 'Día del mes', num(1, 31), { when: ['freq_type', ['monthly']], required: true, default: 1 }),
  F('relative_ordinal', 'El', sel([['1', 'primer'], ['2', 'segundo'], ['4', 'tercer'], ['8', 'cuarto'], ['16', 'último']]), { when: ['freq_type', ['monthly_relative']], required: true, default: '1' }),
  F('relative_day', 'Día', sel([...DAYS, ['day', 'día'], ['weekday', 'día hábil'], ['weekend', 'día de fin de semana']]), { when: ['freq_type', ['monthly_relative']], required: true, default: 'mon' }),
  F('monthly_interval', 'Cada (meses)', num(1, 100), { when: ['freq_type', ['monthly', 'monthly_relative']], required: true, default: 1 }),
  F('subday_type', 'Frecuencia diaria', sel([['once', 'Una vez, a las'], ['every', 'Cada']]), { when: ['freq_type', ['daily', 'weekly', 'monthly', 'monthly_relative']], default: 'once' }),
  F('active_start_time', 'A las', { type: 'time' }, { when: ['subday_type', ['once']], required: true, default: '00:00:00' }),
  F('subday_interval', 'Cada', num(1, 100), { when: ['subday_type', ['every']], required: true, default: 1 }),
  F('subday_unit', 'Unidad', sel([['hours', 'horas'], ['minutes', 'minutos'], ['seconds', 'segundos']]), { when: ['subday_type', ['every']], default: 'hours' }),
  // "Cada … desde / hasta" only on a recurring schedule (`when_all`, as the driver writes it with `and_when`).
  F('window_start', 'Desde las', { type: 'time' }, { when: ['subday_type', ['every']], when_all: [['freq_type', ['daily', 'weekly', 'monthly', 'monthly_relative']]], default: '00:00:00' }),
  F('window_end', 'Hasta las', { type: 'time' }, { when: ['subday_type', ['every']], when_all: [['freq_type', ['daily', 'weekly', 'monthly', 'monthly_relative']]], default: '23:59:59' }),
  F('active_start_date', 'Vigente desde', { type: 'date' }, { required: true }),
  F('active_end_date', 'Vigente hasta', { type: 'date' }, { help: 'Vacío: sin fecha de fin.' }),
];
const SCHEDULES: FormRow[] = [
  { schedule_id: 7, schedule_uid: 'f1d4b8a6-2c5e-4f0a-9b77-3d1e2a6c9f10', name: 'Diario 03:00', jobs_using: 2, enabled: true, freq_type: 'daily', daily_interval: 1, subday_type: 'once', active_start_time: '03:00:00', active_start_date: '2025-03-14', active_end_date: null },
  { schedule_id: 12, schedule_uid: '0b9e1c55-7a4d-4a2b-8e3f-5c6d7e8f9a0b', name: 'Cada 4 horas en horario hábil', jobs_using: 1, enabled: true, freq_type: 'weekly', weekly_interval: 1, weekly_days: ['mon', 'tue', 'wed', 'thu', 'fri'], subday_type: 'every', subday_interval: 4, subday_unit: 'hours', window_start: '08:00:00', window_end: '20:00:00', active_start_date: '2026-01-05', active_end_date: null },
];

const CHOICES: Record<string, [string, string][]> = {
  login: [['sa', 'sa'], ['AD\\dba', 'AD\\dba'], ['app_ventas', 'app_ventas'], ['NT SERVICE\\SQLSERVERAGENT', 'NT SERVICE\\SQLSERVERAGENT']],
  'category:job': [['[Uncategorized (Local)]', '[Uncategorized (Local)]'], ['Database Maintenance', 'Database Maintenance'], ['ETL', 'ETL'], ['Ventas', 'Ventas']],
  database: ['master', 'msdb', 'ventas', 'crm', 'staging'].map((d): [string, string] => [d, d]),
  // As the driver sends them: "" (the Agent's service account) first.
  'proxy:PowerShell': [['', 'Cuenta de servicio del Agente SQL Server'], ['Proxy PowerShell', 'Proxy PowerShell']],
  'proxy:CmdExec': [['', 'Cuenta de servicio del Agente SQL Server'], ['Proxy CmdExec ETL', 'Proxy CmdExec ETL']],
  operator: [['DBA de guardia', 'DBA de guardia'], ['Equipo de datos', 'Equipo de datos'], ['Mesa de ayuda', 'Mesa de ayuda']],
  alert: ITEMS.alert.items.map((a): [string, string] => [a.name, a.name]),
  job: JOBS.jobs.filter((j) => !j.system).map((j): [string, string] => [j.name, j.name]),
  // A performance condition: counters by object, instances by counter. An
  // object or a counter with nothing listed (User Settable, Buffer Manager's
  // counters) takes free text.
  perf_object: ['SQLServer:Databases', 'SQLServer:Buffer Manager', 'SQLServer:Locks', 'SQLServer:User Settable'].map((o): [string, string] => [o, o]),
  'perf_counter:SQLServer:Databases': ['Percent Log Used', 'Log File(s) Used Size (KB)', 'Log Growths', 'Transactions/sec'].map((c): [string, string] => [c, c]),
  'perf_counter:SQLServer:Buffer Manager': ['Page life expectancy', 'Buffer cache hit ratio'].map((c): [string, string] => [c, c]),
  'perf_counter:SQLServer:Locks': ['Lock Waits/sec', 'Number of Deadlocks/sec'].map((c): [string, string] => [c, c]),
  ...Object.fromEntries(['Percent Log Used', 'Log File(s) Used Size (KB)', 'Log Growths', 'Transactions/sec'].map((c) => [
    `perf_instance:${c}`, ['_Total', 'master', 'msdb', 'ventas', 'crm', 'staging'].map((d): [string, string] => [d, d]),
  ])),
  ...Object.fromEntries(['Lock Waits/sec', 'Number of Deadlocks/sec'].map((c) => [
    `perf_instance:${c}`, ['_Total', 'Database', 'File', 'Key', 'Object', 'Page', 'RID'].map((d): [string, string] => [d, d]),
  ])),
};

function jobForm(isNew: boolean): ObjectForm {
  return {
    sections: [
      {
        key: 'general', label: 'General',
        fields: [
          F('name', 'Nombre', { type: 'text' }, { required: true }),
          F('owner', 'Propietario', ref('login'), { required: true, default: 'sa' }),
          F('category', 'Categoría', ref('category:job'), { default: '[Uncategorized (Local)]' }),
          F('description', 'Descripción', { type: 'textarea' }),
          F('enabled', 'Habilitado', { type: 'bool' }, { default: true }),
          F('job_id', 'Id. del job', { type: 'text' }, { read_only: true }),
          F('date_created', 'Creado', { type: 'date_time' }, { read_only: true }),
        ],
      },
      {
        key: 'steps', label: 'Pasos',
        fields: [F('start_step', 'Paso inicial', ref('list:steps'), { required: true, help: 'El primer paso que se ejecuta cuando arranca el job.' })],
        lists: [{ key: 'steps', label: 'Pasos', fields: STEP_FIELDS, rows: isNew ? [] : STEPS, orderable: true, min_rows: 1, title_field: 'step_name' }],
      },
      {
        key: 'schedules', label: 'Programaciones',
        lists: [{ key: 'schedules', label: 'Programaciones', fields: SCHEDULE_FIELDS, rows: isNew ? [] : SCHEDULES, title_field: 'name' }],
      },
      {
        key: 'alerts', label: 'Alertas',
        lists: [{
          key: 'alerts', label: 'Alertas que ejecutan este job', title_field: 'alert_name',
          fields: [
            F('alert_name', 'Alerta', ref('alert'), { required: true }),
            F('alert_type', 'Tipo', { type: 'text' }, { read_only: true }),
            F('enabled', 'Habilitada', { type: 'bool' }, { read_only: true }),
          ],
          rows: isNew ? [] : [{ alert_name: 'Log de transacciones lleno (9002)', alert_type: 'Evento de SQL Server', enabled: true }],
        }],
      },
      {
        key: 'notifications', label: 'Notificaciones',
        fields: [
          F('notify_email_level', 'Avisar por mail', sel(LEVELS), { default: '0' }),
          F('notify_email_operator', 'Operador', ref('operator'), { when: ['notify_email_level', ['1', '2', '3']], required: true }),
          F('notify_page_level', 'Avisar al localizador', sel(LEVELS), { default: '0' }),
          F('notify_page_operator', 'Operador', ref('operator'), { when: ['notify_page_level', ['1', '2', '3']], required: true }),
          F('notify_eventlog_level', 'Escribir en el registro de eventos', sel(LEVELS), { default: '2' }),
          F('delete_level', 'Borrar el job automáticamente', sel(LEVELS), { default: '0', help: 'Un job que se borra solo no deja historial.' }),
        ],
      },
      {
        key: 'targets', label: 'Destinos',
        lists: [{
          key: 'targets', label: 'Servidores donde se ejecuta', title_field: 'server', read_only: true,
          fields: [F('server', 'Servidor', { type: 'text' }, { read_only: true }), F('kind', 'Tipo', { type: 'text' }, { read_only: true })],
          rows: [{ server: '(local)', kind: 'Servidor local' }],
        }],
      },
    ],
    values: isNew ? {} : {
      name: 'Backup nocturno', owner: 'sa', category: 'Database Maintenance', description: 'Backup completo de ventas y crm, comprimido y subido a Azure.',
      enabled: true, job_id: JOB_ID, date_created: '2025-03-14T10:22:31', start_step: 'Backup de ventas',
      notify_email_level: '2', notify_email_operator: 'DBA de guardia', notify_page_level: '0', notify_eventlog_level: '2', delete_level: '0',
    },
    choices: CHOICES,
    notes: isNew ? [] : [
      'La programación «Diario 03:00» la usa también otro job: un cambio en ella lo alcanza.',
      'Los jobs multiservidor se administran desde el servidor maestro: acá solo se ve el destino.',
    ],
  };
}

function operatorForm(isNew: boolean): ObjectForm {
  return {
    sections: [
      {
        key: 'general', label: 'General',
        fields: [
          F('name', 'Nombre', { type: 'text' }, { required: true, placeholder: 'DBA de guardia' }),
          F('enabled', 'Habilitado', { type: 'bool' }, { default: true }),
          F('email_address', 'Mail', { type: 'text' }, { placeholder: 'dba@example.com', help: 'Varias direcciones, separadas por punto y coma.' }),
          F('pager_address', 'Localizador', { type: 'text' }, { placeholder: '5491100000000@sms.example.com' }),
          F('pager_days', 'Días de guardia del localizador', { type: 'multi_select', options: DAYS }, { default: ['mon', 'tue', 'wed', 'thu', 'fri'] }),
          F('weekday_start', 'De lunes a viernes, desde', { type: 'time' }, { when: ['pager_days', ['mon', 'tue', 'wed', 'thu', 'fri']], default: '08:00:00' }),
          F('weekday_end', 'De lunes a viernes, hasta', { type: 'time' }, { when: ['pager_days', ['mon', 'tue', 'wed', 'thu', 'fri']], default: '18:00:00' }),
          F('saturday_start', 'Sábados, desde', { type: 'time' }, { when: ['pager_days', ['sat']], default: '08:00:00' }),
          F('saturday_end', 'Sábados, hasta', { type: 'time' }, { when: ['pager_days', ['sat']], default: '18:00:00' }),
          F('sunday_start', 'Domingos, desde', { type: 'time' }, { when: ['pager_days', ['sun']], default: '08:00:00' }),
          F('sunday_end', 'Domingos, hasta', { type: 'time' }, { when: ['pager_days', ['sun']], default: '18:00:00' }),
          F('category', 'Categoría', ref('category:operator'), { default: '[Uncategorized]' }),
        ],
      },
      {
        key: 'notifications', label: 'Notificaciones',
        lists: [{
          key: 'alerts', label: 'Alertas que le avisan', title_field: 'alert_name',
          fields: [
            F('alert_name', 'Alerta', ref('alert'), { required: true }),
            F('by_email', 'Por mail', { type: 'bool' }, { default: true }),
            F('by_pager', 'Por localizador', { type: 'bool' }, { default: false }),
          ],
          rows: [],
        }],
      },
    ],
    values: isNew ? {} : { name: 'Equipo de datos', enabled: true, email_address: 'datos@example.com', pager_days: [] },
    choices: { ...CHOICES, 'category:operator': [['[Uncategorized]', '[Uncategorized]']] },
    notes: ['Database Mail no tiene un perfil predeterminado: los mails a este operador no salen hasta que se configure uno.'],
  };
}

function alertForm(id: string | null): ObjectForm {
  return {
    sections: [
      {
        key: 'general', label: 'General',
        fields: [
          F('name', 'Nombre', { type: 'text' }, { required: true }),
          F('enabled', 'Habilitada', { type: 'bool' }, { default: true }),
          F('alert_type', 'Tipo', sel([['event', 'Alerta de evento de SQL Server'], ['performance', 'Alerta de condición de rendimiento'], ['wmi', 'Alerta de evento WMI']]), { required: true, default: 'event' }),
          F('database_name', 'Base de datos', ref('database'), { when: ['alert_type', ['event']], help: 'Vacío: todas las bases.' }),
          F('raise_by', 'Se dispara por', sel([['message', 'Número de error'], ['severity', 'Severidad']]), { when: ['alert_type', ['event']], default: 'message' }),
          F('message_id', 'Número de error', num(1, 2147483647), { when: ['raise_by', ['message']], required: true }),
          F('severity', 'Severidad', sel([['17', '017 - Recursos insuficientes'], ['19', '019 - Error grave en un recurso'], ['20', '020 - Error grave en el proceso actual'], ['23', '023 - Error grave de integridad de la base'], ['24', '024 - Error grave de hardware'], ['25', '025 - Error grave']]), { when: ['raise_by', ['severity']], required: true }),
          F('event_keyword', 'Si el mensaje contiene', { type: 'text' }, { when: ['alert_type', ['event']] }),
          F('performance_object', 'Objeto', ref('perf_object'), { when: ['alert_type', ['performance']], required: true }),
          F('performance_counter', 'Contador', ref('perf_counter:{performance_object}'), { when: ['alert_type', ['performance']], required: true, placeholder: 'Percent Log Used' }),
          F('performance_instance', 'Instancia', ref('perf_instance:{performance_counter}'), { when: ['alert_type', ['performance']], placeholder: 'ventas' }),
          F('comparator', 'Se dispara si el contador', sel([['<', 'es menor que'], ['=', 'es igual a'], ['>', 'es mayor que']]), { when: ['alert_type', ['performance']], default: '>' }),
          F('threshold', 'Valor', num(null, null), { when: ['alert_type', ['performance']], required: true }),
          F('wmi_namespace', 'Espacio de nombres', { type: 'text' }, { when: ['alert_type', ['wmi']], required: true, default: '\\\\.\\root\\Microsoft\\SqlServer\\ServerEvents\\MSSQLSERVER' }),
          F('wmi_query', 'Consulta', { type: 'code', language: 'wql' }, { when: ['alert_type', ['wmi']], required: true }),
        ],
      },
      {
        key: 'response', label: 'Respuesta',
        fields: [F('job_name', 'Ejecutar el job', ref('job'), { help: 'Vacío: no ejecuta ninguno.' })],
        lists: [{
          key: 'notify', label: 'Operadores a los que avisa', title_field: 'operator_name',
          fields: [
            F('operator_name', 'Operador', ref('operator'), { required: true }),
            F('by_email', 'Por mail', { type: 'bool' }, { default: true }),
            F('by_pager', 'Por localizador', { type: 'bool' }, { default: false }),
          ],
          rows: [{ operator_name: 'DBA de guardia', by_email: true, by_pager: false }, { operator_name: 'Equipo de datos', by_email: true, by_pager: false }],
        }],
      },
      {
        key: 'options', label: 'Opciones',
        fields: [
          F('include_event_description', 'Incluir el texto del error en', { type: 'multi_select', options: [['email', 'el mail'], ['pager', 'el localizador']] }),
          F('notification_message', 'Mensaje adicional', { type: 'textarea' }),
          F('delay_between_responses', 'Demora entre respuestas', { type: 'duration' }, { default: 0, help: 'Mientras dura, la alerta no vuelve a avisar.' }),
        ],
      },
    ],
    values: id === ALERT_PERF_ID ? {
      name: 'Log de ventas al 80 %', enabled: true, alert_type: 'performance', performance_object: 'SQLServer:Databases',
      performance_counter: 'Percent Log Used', performance_instance: 'ventas', comparator: '>', threshold: 80, job_name: 'Backup nocturno',
      include_event_description: ['email'], notification_message: 'El log de ventas pasó el 80 %: revisar el último backup de log.', delay_between_responses: 1800,
    } : {
      name: 'Log de transacciones lleno (9002)', enabled: true, alert_type: 'event', database_name: null, raise_by: 'message', message_id: 9002,
      comparator: '>', job_name: null, include_event_description: ['email'], notification_message: 'Revisar el espacio del log y el último backup de log.',
      delay_between_responses: 900,
    },
    choices: CHOICES,
  };
}

// -- Oracle: a job whose type picks a tab and a list; windows with actions -------------------

const ORA_INFO: SchedulerInfo = {
  kinds: [
    // DBMS_SCHEDULER purges a job's log, but has no "start at a step". Its
    // jobs' actions: each job lists the ones it offers now (`oraItems`).
    {
      key: 'job', label: 'Jobs', singular: 'job', can_create: true, can_edit: true, can_delete: true, run_from_step: false, clear_history: true,
      actions: [{ key: 'force_stop', label: 'Forzar la detención' }, { key: 'end_detached', label: 'Dar por terminada la ejecución' }],
    },
    {
      key: 'window', label: 'Ventanas', singular: 'ventana', can_create: true, can_edit: true, can_delete: true,
      actions: [{ key: 'open_window', label: 'Abrir ventana' }, { key: 'close_window', label: 'Cerrar ventana' }],
    },
  ],
};
const ORA_JOBS: JobList = {
  jobs: [
    job({ id: ORA_JOB_ID, name: 'CIERRE_DIARIO', kind: 'DBMS_SCHEDULER', group: 'ERP', owner: 'ERP', schedule: 'FREQ=DAILY;BYHOUR=1;BYMINUTE=30', next_run: '2026-11-13T04:30:00Z' }),
    job({ id: 'ERP.EXPORTAR_FACTURAS', name: 'EXPORTAR_FACTURAS', kind: 'DBMS_SCHEDULER', group: 'ERP', owner: 'ERP', schedule: 'FREQ=HOURLY', running: true, last_outcome: 'running', last_duration_ms: null }),
    job({ id: 'SYS.PURGE_LOG', name: 'PURGE_LOG', kind: 'DBMS_SCHEDULER', group: 'SYS', owner: 'SYS', system: true, schedule: 'DAILY_PURGE_SCHEDULE' }),
  ],
};
/** Windows open now: "Abrir ventana" / "Cerrar ventana" flip them. */
const windowOpen: Record<string, boolean> = { [WINDOW_ID]: true, 'SYS.WEEKEND_WINDOW': false, 'ERP.CIERRE_MENSUAL': false };
const WINDOWS: [string, string, boolean][] = [[WINDOW_ID, 'WEEKNIGHT_WINDOW', true], ['SYS.WEEKEND_WINDOW', 'WEEKEND_WINDOW', true], ['ERP.CIERRE_MENSUAL', 'CIERRE_MENSUAL', false]];
function oraItems(kind: string): SchedulerList {
  if (kind === 'job') {
    // What each job offers now: one running can be forced to stop; one
    // idle offers none of its kind's actions; SYS's can't be changed nor
    // dropped (the driver says so: `system` alone doesn't).
    return {
      items: ORA_JOBS.jobs.map((j) => item(j.id, j.name, {
        system: j.system, enabled: j.enabled, actions: j.running ? ['force_stop'] : [],
        ...(j.system ? { can_edit: false, can_delete: false } : {}),
      })),
    };
  }
  if (kind !== 'window') return { items: [] };
  return {
    items: WINDOWS.map(([id, name, system]) => item(id, name, {
      system, description: windowOpen[id] ? 'Abierta ahora' : null, actions: [windowOpen[id] ? 'close_window' : 'open_window'],
      ...(system ? { can_delete: false } : {}),
    })),
    notes: ['Las ventanas de SYS vienen con la base: se pueden abrir, cerrar y cambiar, pero no borrar.'],
  };
}

const JOB_TYPES: [string, string][] = [['PLSQL_BLOCK', 'Bloque PL/SQL'], ['STORED_PROCEDURE', 'Procedimiento almacenado'], ['EXECUTABLE', 'Ejecutable externo']];

function oraJobForm(id: string | null): ObjectForm {
  const isNew = id === null;
  return {
    sections: [
      {
        key: 'general', label: 'General',
        fields: [
          F('name', 'Nombre', { type: 'text' }, { required: true, placeholder: 'CIERRE_DIARIO' }),
          F('job_type', 'Tipo', sel(JOB_TYPES), { required: true, default: 'PLSQL_BLOCK' }),
          F('job_action', 'Bloque PL/SQL', { type: 'code', language: 'plsql' }, { when: ['job_type', ['PLSQL_BLOCK']], required: true }),
          F('procedure', 'Procedimiento', { type: 'text' }, { when: ['job_type', ['STORED_PROCEDURE']], required: true, placeholder: 'ERP.PKG_CIERRE.CERRAR_DIA' }),
          F('executable', 'Ejecutable', { type: 'text' }, { when: ['job_type', ['EXECUTABLE']], required: true, placeholder: '/u01/app/erp/bin/exportar.sh' }),
          F('enabled', 'Habilitado', { type: 'bool' }, { default: true }),
          F('comments', 'Comentarios', { type: 'textarea' }),
        ],
      },
      {
        key: 'schedule', label: 'Programación',
        fields: [
          F('repeat_interval', 'Repetición', { type: 'text' }, { placeholder: 'FREQ=DAILY;BYHOUR=1;BYMINUTE=30', help: 'Vacío: solo a pedido.' }),
          F('start_date', 'Desde', { type: 'date_time' }),
          F('end_date', 'Hasta', { type: 'date_time' }, { help: 'Vacío: sin fecha de fin.' }),
        ],
      },
      {
        // Only a procedure or an executable takes arguments.
        key: 'arguments', label: 'Argumentos', when: ['job_type', ['STORED_PROCEDURE', 'EXECUTABLE']],
        lists: [{
          key: 'arguments', label: 'Argumentos', title_field: 'value', orderable: true,
          fields: [F('value', 'Valor', { type: 'text' }, { required: true }), F('type', 'Tipo de dato', sel([['VARCHAR2', 'VARCHAR2'], ['NUMBER', 'NUMBER'], ['DATE', 'DATE']]), { default: 'VARCHAR2' })],
          rows: isNew ? [] : [{ value: 'SYSDATE - 1', type: 'DATE' }],
        }],
      },
      {
        key: 'options', label: 'Opciones',
        fields: [
          F('auto_drop', 'Borrarlo cuando termine', { type: 'bool' }, { default: false }),
          F('restartable', 'Reiniciable', { type: 'bool' }, { default: false, help: 'Si la base se cae a mitad de una ejecución, vuelve a empezar.' }),
          F('max_runs', 'Ejecuciones como máximo', num(1, null), { help: 'Vacío: sin límite.' }),
        ],
        lists: [{
          // Only an external executable runs on remote agents.
          key: 'destinations', label: 'Destinos remotos', title_field: 'destination', when: ['job_type', ['EXECUTABLE']], min_rows: 1,
          fields: [F('destination', 'Destino', ref('destination'), { required: true }), F('credential', 'Credencial', ref('credential'), { required: true })],
          rows: [{ destination: 'AGENTE_ERP_01', credential: 'ERP.CRED_AGENTE' }],
        }],
      },
    ],
    values: isNew ? {} : {
      name: ORA_JOBS.jobs.find((j) => j.id === id)?.name ?? 'CIERRE_DIARIO', job_type: 'PLSQL_BLOCK', enabled: true, comments: 'Cierra el día contable y recalcula los saldos.',
      job_action: 'BEGIN\n  erp.pkg_cierre.cerrar_dia(p_fecha => TRUNC(SYSDATE) - 1);\n  COMMIT;\nEND;',
      repeat_interval: 'FREQ=DAILY;BYHOUR=1;BYMINUTE=30', start_date: '2025-07-01T01:30:00-03:00', auto_drop: false, restartable: true,
    },
    choices: {
      destination: [['AGENTE_ERP_01', 'AGENTE_ERP_01'], ['AGENTE_ERP_02', 'AGENTE_ERP_02']],
      credential: [['ERP.CRED_AGENTE', 'ERP.CRED_AGENTE']],
    },
  };
}

function windowForm(id: string | null): ObjectForm {
  const w = WINDOWS.find(([x]) => x === id);
  return {
    sections: [{
      key: 'general', label: 'General',
      fields: [
        F('name', 'Nombre', { type: 'text' }, { required: true, read_only: id !== null }),
        F('enabled', 'Habilitada', { type: 'bool' }, { default: true }),
        F('resource_plan', 'Plan de recursos', ref('resource_plan'), { help: 'Vacío: queda el plan que esté activo.' }),
        F('repeat_interval', 'Se abre', { type: 'text' }, { required: true, placeholder: 'FREQ=DAILY;BYDAY=MON,TUE,WED,THU,FRI;BYHOUR=22' }),
        F('duration', 'Queda abierta', { type: 'duration' }, { required: true, default: 14400 }),
        F('window_priority', 'Prioridad', sel([['LOW', 'Baja'], ['HIGH', 'Alta']]), { default: 'LOW', help: 'Si dos ventanas se superponen, queda abierta la de prioridad alta.' }),
        F('comments', 'Comentarios', { type: 'textarea' }),
      ],
    }],
    values: w ? {
      name: w[1], enabled: true, resource_plan: 'DEFAULT_MAINTENANCE_PLAN', repeat_interval: 'FREQ=WEEKLY;BYDAY=MON,TUE,WED,THU,FRI;BYHOUR=22;BYMINUTE=0;BYSECOND=0',
      duration: 14400, window_priority: 'LOW', comments: 'Ventana de mantenimiento de lunes a viernes',
    } : {},
    choices: { resource_plan: [['DEFAULT_MAINTENANCE_PLAN', 'DEFAULT_MAINTENANCE_PLAN'], ['ERP_NOCTURNO', 'ERP_NOCTURNO']] },
    notes: id && windowOpen[id] ? ['La ventana está abierta ahora: se cierra sola a las 02:00, o con «Cerrar ventana».'] : [],
  };
}

// -- BigQuery: scheduled queries, and an action that asks for values first ------------------

const BQ_INFO: SchedulerInfo = {
  kinds: [{
    key: 'job', label: 'Consultas programadas', singular: 'consulta programada', can_create: true, can_edit: true, can_delete: true,
    run_from_step: false, clear_history: true,
    // `params`: its form (a period, or a run time) comes first (`bqActionForm`).
    actions: [{ key: 'backfill', label: 'Ejecutar para fechas pasadas', params: true }],
  }],
};
const BQ_JOBS: JobList = {
  jobs: [
    job({ id: BQ_JOB_ID, name: 'ventas_diarias', kind: 'Consulta programada', owner: 'etl@analytics-prod.iam.gserviceaccount.com', schedule: 'every 24 hours', can_stop: false, next_run: '2026-11-14T03:00:00Z' }),
    job({ id: 'projects/analytics-prod/locations/us/transferConfigs/7f3c9b21-0000-2a1d-8e4b-30fd38152c07', name: 'refresco_tablero', kind: 'Consulta programada', owner: 'etl@analytics-prod.iam.gserviceaccount.com', schedule: 'every 1 hours', can_stop: false, last_outcome: 'failed' }),
  ],
};
const bqItems = (): SchedulerList => ({ items: BQ_JOBS.jobs.map((j) => item(j.id, j.name, { enabled: j.enabled })) });

function bqJobForm(id: string | null): ObjectForm {
  const j = BQ_JOBS.jobs.find((x) => x.id === id);
  return {
    sections: [
      {
        key: 'general', label: 'General',
        fields: [
          F('display_name', 'Nombre', { type: 'text' }, { required: true }),
          F('enabled', 'Habilitada', { type: 'bool' }, { default: true }),
          F('query', 'Consulta', { type: 'code', language: 'sql' }, { required: true }),
        ],
      },
      {
        key: 'destination', label: 'Destino',
        fields: [
          F('dataset', 'Dataset de destino', ref('dataset')),
          F('table', 'Tabla de destino', { type: 'text' }, { placeholder: 'ventas_{run_date}' }),
          F('write_disposition', 'Si la tabla existe', sel([['WRITE_APPEND', 'Agregar las filas'], ['WRITE_TRUNCATE', 'Reemplazarla']]), { default: 'WRITE_APPEND' }),
        ],
      },
      {
        key: 'schedule', label: 'Programación',
        fields: [
          F('schedule', 'Programación', { type: 'text' }, { required: true, placeholder: 'every 24 hours' }),
          F('start_time', 'Empieza', { type: 'date_time' }),
          F('end_time', 'Termina', { type: 'date_time' }, { help: 'Vacío: sin fecha de fin.' }),
        ],
      },
    ],
    values: j ? {
      display_name: j.name, enabled: true, schedule: j.schedule, dataset: 'reportes', table: 'ventas_{run_date}', write_disposition: 'WRITE_TRUNCATE',
      query: 'SELECT fecha, sucursal, SUM(importe) AS total\nFROM `analytics-prod.ventas.tickets`\nWHERE fecha = @run_date\nGROUP BY fecha, sucursal',
    } : {},
    choices: { dataset: [['reportes', 'reportes'], ['ventas', 'ventas'], ['staging', 'staging']] },
  };
}

/** "Ejecutar para fechas pasadas" (crates/drivers/bigquery's backfill): the
 *  schedule's runs of a period of past days, or one run for a past time, as
 *  `startManualRuns` takes them. */
function bqActionForm(): ObjectForm {
  return {
    sections: [{
      key: 'backfill', label: 'Ejecución',
      fields: [
        F('mode', 'Ejecutar', sel([['range', 'Para un período: cada ejecución programada de esos días'], ['once', 'Una vez, para una fecha y hora']]), { required: true, default: 'range' }),
        F('start_date', 'Desde', { type: 'date' }, { required: true, when: ['mode', ['range']], help: 'El primer día, incluido, en UTC.' }),
        F('end_date', 'Hasta', { type: 'date' }, { required: true, when: ['mode', ['range']], help: 'El último día, incluido, en UTC. Como mucho ayer: el servicio no ejecuta un día que no terminó.' }),
        F('run_time', 'Fecha y hora', { type: 'date_time' }, { required: true, when: ['mode', ['once']], help: 'En UTC. La consulta la recibe como @run_time, y su día como @run_date. No puede ser futura.' }),
        F('schedule', 'Programación', { type: 'text' }, { read_only: true, when: ['mode', ['range']], help: 'En UTC: cada día del período corre a estas horas.' }),
      ],
    }],
    values: { mode: 'range', start_date: '2026-11-12', end_date: '2026-11-12', run_time: '2026-11-12T03:00:00Z', schedule: 'every 24 hours' },
    notes: ['Los días y las horas van en UTC, como la programación de la consulta.'],
  };
}

function bqActionScript(id: string, v: FormValues): string {
  const x = v.values;
  const range = x.mode !== 'once';
  if (range && String(x.start_date ?? '') > String(x.end_date ?? '')) {
    // As the driver refuses it: the form stays open with its values.
    throw { kind: 'query', message: '«Desde» no puede ser posterior a «Hasta».' };
  }
  // A day after another, as the service takes a period: to the midnight after the last day.
  const next = (d: unknown) => new Date(Date.parse(`${String(d)}T00:00:00Z`) + 86_400_000).toISOString().slice(0, 10);
  const utc = (t: unknown) => (/(?:Z|[+-]\d{2}:?\d{2})$/i.test(String(t)) ? String(t) : `${String(t)}Z`);
  const body = range
    ? ['# {', '#   "requestedTimeRange": {', `#     "startTime": "${String(x.start_date)}T00:00:00Z",`, `#     "endTime": "${next(x.end_date)}T00:00:00Z"`, '#   }', '# }']
    : ['# {', `#   "requestedRunTime": "${utc(x.run_time)}"`, '# }'];
  return [
    `# «${BQ_JOBS.jobs.find((j) => j.id === id)?.name ?? id}» ${range ? `para los días del ${String(x.start_date)} al ${String(x.end_date)} (UTC), en cada hora de su programación` : `una vez, para ${utc(x.run_time)}`}.`,
    '# ⚠ Cada ejecución corre la consulta y se factura como cualquier consulta de BigQuery.',
    '# ⚠ Cada ejecución reemplaza los datos de su tabla de destino: si el nombre de la tabla no lleva la fecha, queda solo el resultado de la última.',
    '#',
    `# POST https://bigquerydatatransfer.googleapis.com/v1/${id}:startManualRuns`,
    ...body,
  ].join('\n') + '\n';
}

function bqScript(id: string | null, v: FormValues): string {
  const x = v.values;
  const body = JSON.stringify({ displayName: x.display_name, schedule: x.schedule, disabled: !x.enabled, destinationDatasetId: x.dataset, params: { query: x.query, destination_table_name_template: x.table, write_disposition: x.write_disposition } }, null, 2);
  const head = id ? `# PATCH https://bigquerydatatransfer.googleapis.com/v1/${id}?updateMask=displayName,schedule,disabled,destinationDatasetId,params` : '# POST https://bigquerydatatransfer.googleapis.com/v1/projects/analytics-prod/locations/us/transferConfigs';
  return [head, ...body.split('\n').map((l) => `# ${l}`)].join('\n') + '\n';
}

function formFor(conn: string, kind: string, id: string | null): ObjectForm {
  if (conn === 'bq') return bqJobForm(id);
  if (conn === 'ora') return kind === 'window' ? windowForm(id) : oraJobForm(id);
  if (kind === 'operator') return operatorForm(id === null);
  if (kind === 'alert') return alertForm(id);
  return jobForm(id === null);
}

// -- the scripts ---------------------------------------------------------------------------

const s = (v: unknown) => (v === null || v === undefined || v === '' ? 'NULL' : `N'${String(v).replace(/'/g, "''")}'`);
const b = (v: unknown) => (v ? 1 : 0);
const stepNo = (rows: FormRow[], name: unknown) => Math.max(1, rows.findIndex((r) => r.step_name === name) + 1);

function jobScript(id: string | null, v: FormValues): string {
  const x = v.values;
  const steps = v.lists.steps ?? [];
  const at = id ? `@job_id = N'${id}'` : '@job_id = @job_id';
  const out = ['USE [msdb];', 'GO', 'BEGIN TRANSACTION;'];
  if (id) {
    out.push(`EXEC msdb.dbo.sp_update_job ${at},\n    @new_name = ${s(x.name)}, @enabled = ${b(x.enabled)}, @description = ${s(x.description)},\n    @category_name = ${s(x.category)}, @owner_login_name = ${s(x.owner)},\n    @notify_level_email = ${x.notify_email_level ?? 0}, @notify_email_operator_name = ${s(x.notify_email_operator)},\n    @notify_level_eventlog = ${x.notify_eventlog_level ?? 0}, @delete_level = ${x.delete_level ?? 0};`);
  } else {
    out.push('DECLARE @job_id uniqueidentifier;', `EXEC msdb.dbo.sp_add_job @job_name = ${s(x.name)}, @enabled = ${b(x.enabled)}, @description = ${s(x.description)},\n    @category_name = ${s(x.category)}, @owner_login_name = ${s(x.owner)}, @job_id = @job_id OUTPUT;`);
  }
  steps.forEach((r, i) => {
    const proc = id && r.step_id ? 'sp_update_jobstep' : 'sp_add_jobstep';
    const extra = r.subsystem === 'TSQL' ? `@database_name = ${s(r.database_name)}` : `@proxy_name = ${s(r.proxy_name)}`;
    out.push(`EXEC msdb.dbo.${proc} ${at}, @step_id = ${i + 1}, @step_name = ${s(r.step_name)},\n    @subsystem = N'${String(r.subsystem ?? 'TSQL')}', ${extra},\n    @command = ${s(r.command)},\n    @on_success_action = ${r.on_success_action ?? 3}${r.on_success_action === '4' ? `, @on_success_step_id = ${stepNo(steps, r.on_success_step)}` : ''},\n    @on_fail_action = ${r.on_fail_action ?? 2}${r.on_fail_action === '4' ? `, @on_fail_step_id = ${stepNo(steps, r.on_fail_step)}` : ''},\n    @retry_attempts = ${r.retry_attempts ?? 0}, @retry_interval = ${r.retry_interval ?? 0};`);
  });
  if (id) out.push(`EXEC msdb.dbo.sp_update_job ${at}, @start_step_id = ${stepNo(steps, x.start_step)};`);
  for (const r of v.lists.schedules ?? []) {
    const freq = ({ once: 1, daily: 4, weekly: 8, monthly: 16, monthly_relative: 32, agent_start: 64, idle: 128 } as Record<string, number>)[String(r.freq_type)] ?? 4;
    const time = String(r.subday_type === 'every' ? r.window_start ?? '00:00:00' : r.active_start_time ?? '00:00:00').replace(/:/g, '');
    const head = r.schedule_id ? `sp_update_schedule @schedule_id = ${r.schedule_id}, @new_name = ${s(r.name)}` : `sp_add_jobschedule ${at}, @name = ${s(r.name)}`;
    out.push(`EXEC msdb.dbo.${head}, @enabled = ${b(r.enabled)},\n    @freq_type = ${freq}, @freq_interval = ${r.daily_interval ?? r.weekly_interval ?? 1},\n    @freq_subday_type = ${r.subday_type === 'every' ? 8 : 1}, @freq_subday_interval = ${r.subday_interval ?? 0},\n    @active_start_time = ${Number(time)}, @active_start_date = ${String(r.active_start_date ?? '').replace(/-/g, '') || 'NULL'};`);
  }
  if (!id) out.push(`EXEC msdb.dbo.sp_add_jobserver ${at}, @server_name = N'(local)';`);
  out.push('COMMIT TRANSACTION;', 'GO');
  return out.join('\n');
}

const q = (v: unknown) => (v === null || v === undefined || v === '' ? 'NULL' : `'${String(v).replace(/'/g, "''")}'`);

function oraScript(kind: string, id: string | null, v: FormValues): string {
  const x = v.values;
  const name = id ?? `ERP.${String(x.name ?? 'NUEVO')}`;
  const what = kind === 'window' ? 'window' : 'job';
  const action = x.job_type === 'PLSQL_BLOCK' ? x.job_action : x.job_type === 'STORED_PROCEDURE' ? x.procedure : x.executable;
  const set = (attr: string, val: string) => `  DBMS_SCHEDULER.SET_ATTRIBUTE(name => ${q(name)}, attribute => '${attr}', value => ${val});`;
  const out = ['BEGIN'];
  if (kind === 'window') {
    out.push(set('resource_plan', q(x.resource_plan)), set('repeat_interval', q(x.repeat_interval)),
      set('duration', `NUMTODSINTERVAL(${Number(x.duration ?? 0)}, 'SECOND')`), set('window_priority', q(x.window_priority)), set('comments', q(x.comments)));
  } else if (!id) {
    out.push(`  DBMS_SCHEDULER.CREATE_JOB(\n    job_name => ${q(name)},\n    job_type => ${q(x.job_type)},\n    job_action => ${q(action)},\n    number_of_arguments => ${(v.lists.arguments ?? []).length},\n    repeat_interval => ${q(x.repeat_interval)},\n    enabled => FALSE);`);
  } else {
    out.push(set('job_type', q(x.job_type)), set('job_action', q(action)), set('number_of_arguments', String((v.lists.arguments ?? []).length)), set('repeat_interval', q(x.repeat_interval)));
  }
  // A list that doesn't show isn't sent: a PL/SQL block has no arguments nor destinations.
  (v.lists.arguments ?? []).forEach((r, i) => out.push(`  DBMS_SCHEDULER.SET_JOB_ARGUMENT_VALUE(job_name => ${q(name)}, argument_position => ${i + 1}, argument_value => ${q(r.value)});`));
  if (v.lists.destinations) out.push(set('destination_name', q((v.lists.destinations).map((r) => r.destination).join(','))), set('credential_name', q(v.lists.destinations[0]?.credential)));
  if (what === 'job') out.push(`  DBMS_SCHEDULER.${x.enabled ? 'ENABLE' : 'DISABLE'}(${q(name)});`);
  out.push('END;', '/');
  return out.join('\n');
}

function actionScriptFor(id: string, action: string): string {
  if (action === 'open_window') return `BEGIN\n  DBMS_SCHEDULER.OPEN_WINDOW(window_name => ${q(id)}, duration => NULL, force => FALSE);\nEND;\n/`;
  if (action === 'close_window') return `BEGIN\n  DBMS_SCHEDULER.CLOSE_WINDOW(window_name => ${q(id)});\nEND;\n/`;
  if (action === 'force_stop') return `BEGIN\n  DBMS_SCHEDULER.STOP_JOB(job_name => ${q(id)}, force => TRUE);\nEND;\n/`;
  return `-- ${action} ${id}`;
}

function scriptFor(conn: string, kind: string, id: string | null, v: FormValues): string {
  const x = v.values;
  if (conn === 'bq') return bqScript(id, v);
  if (conn === 'ora') return oraScript(kind, id, v);
  if (kind === 'job') return jobScript(id, v);
  if (kind === 'operator') {
    const proc = id ? 'sp_update_operator' : 'sp_add_operator';
    return `USE [msdb];\nGO\nEXEC msdb.dbo.${proc} @name = ${s(x.name)}, @enabled = ${b(x.enabled)},\n    @email_address = ${s(x.email_address)}, @pager_address = ${s(x.pager_address)},\n    @weekday_pager_start_time = ${String(x.weekday_start ?? '080000').replace(/:/g, '')}, @weekday_pager_end_time = ${String(x.weekday_end ?? '180000').replace(/:/g, '')},\n    @pager_days = 62;\nGO`;
  }
  if (kind === 'alert') {
    const lines = [`USE [msdb];`, 'GO', `EXEC msdb.dbo.${id ? 'sp_update_alert' : 'sp_add_alert'} @name = ${s(x.name)}, @enabled = ${b(x.enabled)},`];
    if (x.alert_type === 'performance') lines.push(`    @performance_condition = N'${x.performance_object}|${x.performance_counter}|${x.performance_instance ?? ''}|${x.comparator}|${x.threshold}',`);
    else if (x.alert_type === 'wmi') lines.push(`    @wmi_namespace = ${s(x.wmi_namespace)}, @wmi_query = ${s(x.wmi_query)},`);
    else lines.push(`    @message_id = ${x.raise_by === 'message' ? x.message_id ?? 0 : 0}, @severity = ${x.raise_by === 'severity' ? x.severity ?? 0 : 0}, @database_name = ${s(x.database_name)},`);
    lines.push(`    @delay_between_responses = ${x.delay_between_responses ?? 0}, @notification_message = ${s(x.notification_message)}, @job_name = ${s(x.job_name)};`);
    for (const r of v.lists.notify ?? []) lines.push(`EXEC msdb.dbo.sp_update_notification @alert_name = ${s(x.name)}, @operator_name = ${s(r.operator_name)}, @notification_method = ${(r.by_email ? 1 : 0) + (r.by_pager ? 2 : 0)};`);
    lines.push('GO');
    return lines.join('\n');
  }
  return `-- ${kind} ${id ?? '(nuevo)'}`;
}

function deleteScriptFor(conn: string, kind: string, id: string): string {
  if (conn === 'bq') return `# DELETE https://bigquerydatatransfer.googleapis.com/v1/${id}\n`;
  if (conn === 'ora') return `BEGIN\n  DBMS_SCHEDULER.DROP_${kind === 'window' ? 'WINDOW' : 'JOB'}(${q(id)});\nEND;\n/`;
  if (kind === 'job') return `USE [msdb];\nGO\nEXEC msdb.dbo.sp_delete_job @job_id = N'${id}', @delete_unused_schedule = 1;\nGO`;
  return `USE [msdb];\nGO\nEXEC msdb.dbo.sp_delete_${kind} @name = N'${id}';\nGO`;
}

const LOGS: MonitorTable[] = [
  {
    key: '0', title: 'Actual: 12/11/2026 08:00', columns: ['Fecha', 'Tipo', 'Mensaje'],
    rows: [
      ['2026-11-12 08:00:02', 'Información', '[393] Waiting for SQL Server to recover database \'msdb\'...'],
      ['2026-11-12 08:00:04', 'Información', '[100] Microsoft SQLServerAgent version 16.0.4135.4 (X64 unicode retail build) : Process ID 4012'],
      ['2026-11-12 08:00:04', 'Información', '[101] SQL Server SQL-PROD-01 version 16.00.4135 (Microsoft SQL Server 2022)'],
      ['2026-11-12 08:00:05', 'Advertencia', '[260] Unable to start mail session (reason: No mail profile defined)'],
      ['2026-11-12 08:00:05', 'Información', '[129] SQLSERVERAGENT starting under Windows NT service control'],
      ['2026-11-12 08:00:06', 'Información', '[146] Request servicer engine started'],
    ],
  },
  {
    key: '1', title: 'Archivo n.º 1: 11/11/2026 08:00', columns: ['Fecha', 'Tipo', 'Mensaje'],
    rows: [['2026-11-11 23:59:58', 'Información', '[098] SQLServerAgent terminated (normally)']],
  },
];

// -- mock and mount ------------------------------------------------------------------------

export function installSchedulerMock() {
  const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => 0,
    invoke: async (cmd: string, payload?: { args?: Record<string, unknown> }) => {
      const a = payload?.args ?? {};
      const kind = String(a.kind ?? '');
      const id = (a.id as string | null | undefined) ?? null;
      const conn = String(a.connection_id ?? '');
      switch (cmd) {
        case 'scheduled_tasks_list': return [];
        case 'jobs_list': return conn === 'ms' ? JOBS : conn === 'ora' ? ORA_JOBS : conn === 'bq' ? BQ_JOBS : { jobs: [], notes: ['pg_cron está instalado en la base «postgres»: conectate a esa base para ver sus jobs'] };
        case 'jobs_detail': {
          const j = conn === 'ora' || conn === 'bq' ? (conn === 'ora' ? ORA_JOBS : BQ_JOBS).jobs.find((x) => x.id === id) : null;
          return j ? { job: j, steps: [], schedules: [], properties: [] } : DETAIL;
        }
        case 'jobs_runs': return [];
        case 'scheduler_info': return conn === 'ora' ? ORA_INFO : conn === 'bq' ? BQ_INFO : INFO;
        case 'scheduler_items': await wait(150); return conn === 'ora' ? oraItems(kind) : conn === 'bq' ? bqItems() : ITEMS[kind] ?? { items: [] };
        case 'scheduler_form': await wait(120); return formFor(conn, kind, id);
        case 'scheduler_script': return scriptFor(conn, kind, id, a.values as FormValues);
        case 'scheduler_save': await wait(400); return id ?? `${kind}-new`;
        case 'scheduler_delete_script': return deleteScriptFor(conn, kind, String(id));
        case 'scheduler_action_form': await wait(120); return bqActionForm();
        case 'scheduler_action_script':
          if (a.action === 'backfill') { await wait(150); return bqActionScript(String(id), a.values as FormValues); }
          return actionScriptFor(String(id), String(a.action));
        case 'scheduler_action':
          await wait(300);
          if (a.action === 'open_window' || a.action === 'close_window') windowOpen[String(id)] = a.action === 'open_window';
          if (a.action === 'backfill') {
            const j = BQ_JOBS.jobs.find((x) => x.id === id);
            if (j) Object.assign(j, { running: true, last_outcome: 'running', last_started: '2026-11-13T15:00:00Z', last_duration_ms: null });
          }
          if (a.action === 'force_stop') {
            const j = ORA_JOBS.jobs.find((x) => x.id === id);
            if (j) Object.assign(j, { running: false, last_outcome: 'canceled', last_duration_ms: 95_000 });
          }
          return null;
        case 'scheduler_logs': return LOGS;
        default: return null;
      }
    },
  };
}

let tabId = '';

export function setupScheduler(params: URLSearchParams) {
  const conns = useConnectionsStore();
  const driver = (id: string, name: string, caps: Record<string, boolean>) =>
    ({ id, name, language: 'sql', dialect: id === 'sqlserver' ? 'mssql' : id === 'oracle' ? 'oracle' : id === 'bigquery' ? 'bigquery' : 'postgres', capabilities: { create_database: true, drop_database: true, foreign_keys: true, monitor: true, ...caps } }) as never;
  conns.drivers = [
    driver('sqlserver', 'SQL Server', { jobs: true, scheduler: true }), driver('oracle', 'Oracle', { jobs: true, scheduler: true }),
    driver('postgres', 'PostgreSQL', { jobs: true }), driver('bigquery', 'BigQuery', { jobs: true, scheduler: true }),
  ];
  const conn = (id: string, name: string, drv: string, tags: string[]) => ({
    id, name, color: null, folder_id: null, tags, save_password: true, updated_at: '',
    config: { driver: drv, host: 'db', port: 0, database: '', username: 'app', password: null, encrypt: true, trust_server_certificate: false, read_only: false, options: {} },
  });
  conns.list = [
    conn('ms', 'SQL Server · Producción', 'sqlserver', ['prod']), conn('ora', 'Oracle · ERP', 'oracle', ['prod']), conn('pg', 'Ventas PG', 'postgres', []),
    conn('bq', 'BigQuery · analytics-prod', 'bigquery', ['prod']),
  ] as never;
  conns.ensureConnected = async () => true;
  const jobs = useServerJobsStore();
  const on = params.get('conn') === 'ora' ? 'ora' : params.get('conn') === 'bq' ? 'bq' : 'ms';
  // The job's tab alone: a tab restored with the app, its connection closed
  // in the sidebar (nothing else reads the scheduler).
  jobs.expanded = params.get('view') === 'scheduler-jobtab' ? [] : [on];
  jobs.kindsOpen = on === 'ora' ? { 'ora\u0000window': true } : { 'ms\u0000schedule': true, 'ms\u0000operator': true };
  const kind = params.get('kind') ?? 'job';
  const tabs = useTabsStore();
  let id: string | null;
  let title: string;
  if (on === 'bq') {
    id = BQ_JOB_ID;
    title = BQ_JOBS.jobs[0].name;
  } else if (on === 'ora') {
    // `job=EXPORTAR_FACTURAS` (running: "Forzar la detención" only) or `job=PURGE_LOG` (SYS's: read-only, no Borrar).
    const oraJob = ORA_JOBS.jobs.find((j) => j.name === params.get('job')) ?? ORA_JOBS.jobs[0];
    id = kind === 'window' ? WINDOW_ID : oraJob.id;
    title = kind === 'window' ? 'WEEKNIGHT_WINDOW' : oraJob.name;
  } else {
    const perf = params.get('set') === 'perf';
    id = kind === 'operator' ? null : kind === 'alert' ? (perf ? ALERT_PERF_ID : ALERT_ID) : JOB_ID;
    title = kind === 'alert' ? (perf ? 'Log de ventas al 80 %' : 'Log de transacciones lleno (9002)') : kind === 'job' ? NIGHTLY.name : '';
  }
  tabs.openSchedulerObject(on, kind, id, title);
  tabId = tabs.activeId ?? '';
}

function designer() {
  const t = useTabsStore().tabs.find((x) => x.id === tabId) as SchedulerObjectTab | undefined;
  return t ? h(SchedulerObjectView, { tab: t }) : null;
}

export function schedulerPreview() {
  return h('div', { style: 'height: 100vh; display: flex; flex-direction: column; background: var(--nm-bg)' }, [designer()]);
}

export function schedulerTreePreview() {
  return h('div', { style: 'display: flex; height: 100vh; background: var(--nm-bg)' }, [
    h('div', { style: 'width: 320px; flex: none; overflow: auto; border-right: 1px solid var(--nm-border-soft); background: var(--ide-sidebar, var(--nm-bg))' }, [h(ScheduledSidebar)]),
    h('div', { style: 'flex: 1; min-width: 0; display: flex; flex-direction: column' }, [designer()]),
  ]);
}

export function schedulerLogPreview() {
  return h('div', { style: 'height: 100vh; display: flex; flex-direction: column; background: var(--nm-bg)' }, [
    h(SchedulerLogView, { tab: { id: 'slog', kind: 'schedulerLog', connectionId: 'ms', database: '', preview: false }, active: true }),
  ]);
}

/** A job's tab on its own (`conn=ora&job=EXPORTAR_FACTURAS`): its "Más acciones" from the job kind and the job's abilities. */
export function schedulerJobTabPreview() {
  const t = useTabsStore().tabs.find((x) => x.id === tabId) as SchedulerObjectTab | undefined;
  if (!t || t.objectId === null) return null;
  return h('div', { style: 'height: 100vh; display: flex; flex-direction: column; background: var(--nm-bg)' }, [
    h(ServerJobView, { tab: { id: 'sj', kind: 'serverJob', connectionId: t.connectionId, database: '', jobId: t.objectId, jobName: t.title, preview: false } as never, active: true }),
  ]);
}
