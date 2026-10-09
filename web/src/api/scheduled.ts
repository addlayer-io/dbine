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
export type StepKind = 'run_script' | 'export' | 'compare_schemas' | 'backup' | 'send_mail' | 'document';
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
  check: (task: ScheduledTask) => invoke<{ scope: WriteScope[]; needs_approval: boolean }>('scheduled_task_check', { args: { task } }),
  save: (task: ScheduledTask, approveWrites: boolean) =>
    invoke<Saved>('scheduled_task_save', { args: { task, approve_writes: approveWrites } }),
  remove: (id: string) => invoke<void>('scheduled_task_delete', { args: { id } }),
  enable: (id: string, enabled: boolean) => invoke<Saved>('scheduled_task_enable', { args: { id, enabled } }),
  runNow: (id: string) => invoke<void>('scheduled_task_run_now', { args: { id } }),
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
    send_mail: { to: '', cc: '', subject: '{task}: {date}', body: '', attachments: [], when: 'always' },
  }[kind];
  return { id: '', kind, name: '', config, on_error: 'stop' };
}
