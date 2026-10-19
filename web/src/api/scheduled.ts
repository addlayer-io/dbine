import { invoke } from '@tauri-apps/api/core';
import { defaultDocOptions } from './dbDocs';

// Scheduled tasks (docs/scheduled-tasks.md). Mirrors dbine_core::tasks
// and src-tauri/src/commands/scheduled.rs.

export type Schedule =
  | { type: 'daily'; time: string }
  | { type: 'weekly'; days: number[]; time: string }
  | { type: 'monthly'; day: number; time: string }
  | { type: 'interval'; minutes: number };

export type Notify = 'never' | 'failure' | 'always';
export type OnError = 'stop' | 'continue';
export type StepKind = 'run_script' | 'export' | 'compare_schemas' | 'backup' | 'send_mail' | 'document' | 'run_notebook';
/** A step's "Solo si…" (`config.when`): the runner skips it otherwise. */
export type StepWhen = 'always' | 'alert' | 'errors';
export type RunStatus = 'running' | 'ok' | 'partial' | 'failed';

export interface Step {
  id: string;
  kind: StepKind | string;
  name: string;
  config: Record<string, any>;
  on_error: OnError;
}

export interface ScheduledTask {
  id: string;
  name: string;
  enabled: boolean;
  schedule: Schedule;
  steps: Step[];
  notify: Notify;
  approved_writes: string | null;
  created_at: string;
  updated_at: string;
}

export interface StepRun {
  step_id: string;
  kind: string;
  status: RunStatus;
  summary: string;
  messages: string[];
  outputs: Record<string, string>;
  started_at: string;
  finished_at: string;
  alert: string | null;
}

export interface TaskRun {
  id: string;
  task_id: string;
  trigger: 'schedule' | 'manual' | string;
  status: RunStatus;
  started_at: string;
  finished_at: string;
  steps: StepRun[];
  error: string | null;
}

export interface WriteScope {
  step_id: string;
  connection_id: string;
  connection: string;
  database: string;
  what: string;
  production: boolean;
  /** A notebook step's cell that writes (its name). */
  cell?: string | null;
}

// -- "Ejecutar notebook" (src-tauri/src/notebook/headless.rs) --------------------------------

/** A cell's target on this machine. */
export interface NotebookResolved {
  connection_id: string;
  database: string;
  label: string;
  environment: string | null;
  confirm_run: boolean;
  production: boolean;
  read_only: boolean;
}

export interface NotebookTargetProblem {
  kind: 'none' | 'connection' | 'ambiguous' | 'environment';
  name: string | null;
  candidates: { id: string; name: string }[];
  message: string;
}

export interface NotebookPlanFile {
  path: string;
  display: string;
  size: number;
  mtime_ms: number;
  pattern?: string | null;
  sidecars?: NotebookPlanFile[];
}

export interface NotebookPlan {
  cells: { id: string; name: string; dataframes: boolean; stored: string; target: NotebookResolved | null; problem: NotebookTargetProblem | null; writes: string | null }[];
  /** Targets that need "Permitir ejecutar en … sin confirmar". */
  confirm: NotebookResolved[];
  files: NotebookPlanFile[];
  /** Without "Permitir siempre" for this notebook: the step fails. */
  pending: NotebookPlanFile[];
  blocked: string[];
  file_problems: string[];
  parameters: string[];
  write_cells: { cell_id: string; name: string; target: string; mode: string }[];
  warnings: string[];
}

/** The step's options the plan depends on. */
export interface NotebookStepOptions {
  path: string;
  params?: Record<string, unknown>;
  targets?: Record<string, { connection_id: string; database: string }>;
  unconfirmed?: { connection_id: string; database: string }[];
  write_cells?: { cell_id: string; target: string; mode: string }[];
}

export interface TaskItem extends ScheduledTask {
  registered: boolean;
  next_run: string | null;
  last_run: TaskRun | null;
  writes: WriteScope[];
  needs_approval: boolean;
}

export interface Saved {
  item: TaskItem;
  schedule_error: string | null;
}

export const scheduledApi = {
  list: () => invoke<TaskItem[]>('scheduled_tasks_list'),
  check: (task: ScheduledTask) => invoke<{ scope: WriteScope[]; needs_approval: boolean; fingerprint: string | null }>('scheduled_task_check', { args: { task } }),
  /** `approved`: the check's fingerprint the user said yes to (null: no yes). */
  save: (task: ScheduledTask, approved: string | null) =>
    invoke<Saved>('scheduled_task_save', { args: { task, approve_writes: approved !== null, approved_fingerprint: approved } }),
  remove: (id: string) => invoke<void>('scheduled_task_delete', { args: { id } }),
  enable: (id: string, enabled: boolean) => invoke<Saved>('scheduled_task_enable', { args: { id, enabled } }),
  runNow: (id: string) => invoke<void>('scheduled_task_run_now', { args: { id } }),
  notebookPlan: (o: NotebookStepOptions) => invoke<NotebookPlan>('scheduled_notebook_plan', { args: o }),
  runs: (taskId: string | null, limit = 50) => invoke<TaskRun[]>('scheduled_task_runs', { args: { task_id: taskId, limit } }),
};

export function newTask(): ScheduledTask {
  return {
    id: '', name: '', enabled: true, schedule: { type: 'daily', time: '08:00' }, steps: [], notify: 'failure',
    approved_writes: null, created_at: '', updated_at: '',
  };
}

export function newStep(kind: StepKind): Step {
  const config: Record<string, any> = {
    run_script: { connection_id: '', database: '', sql: '' },
    export: { connection_id: '', database: '', sql: '', folder: '', file_name: '{task}-{datetime}', options: { format: 'csv', header: true } },
    compare_schemas: {
      source: { connection_id: '', database: '', schemas: [] }, target: { connection_id: '', database: '', schemas: [] },
      options: { ignore_case: false, ignore_schema: false, ignore_comments: true }, include_drops: false, folder: '', file_name: '{task}-{datetime}',
    },
    backup: { connection_id: '', database: '', mode: 'native', options: {}, folder: '', file_name: '{task}-{datetime}', data: true },
    document: { connection_id: '', database: '', folder: '', file_name: '{task}-{date}', options: defaultDocOptions() },
    send_mail: { to: '', cc: '', subject: '{task}: {date}', body: '', html_body: '', attachments: [], when: 'always' },
    run_notebook: {
      path: '', params: {}, targets: {}, unconfirmed: [], write_cells: [],
      save: { mode: 'folder', folder: '', file_name: '{notebook}-{datetime}' },
      export: { format: 'html', folder: '', file_name: '{notebook}-{datetime}' },
    },
  }[kind];
  return { id: '', kind, name: '', config, on_error: 'stop' };
}
