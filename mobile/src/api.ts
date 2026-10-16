import { invoke } from '@tauri-apps/api/core';
import { tb } from './backend';

// The app's Rust core (src-tauri/src/lib.rs) and, through it, DBine on the
// computer (crates/dbine-link/src/proto.rs).

export interface Status {
  paired: boolean;
  desktop: string | null;
  fingerprint: string | null;
}

export interface Hello {
  desktop: string;
  version: string;
  can_kill: boolean;
  can_run_tasks: boolean;
}

export interface StepRow {
  kind: string;
  status: string;
  summary: string;
  alert: string | null;
}

export interface RunRow {
  id: string;
  status: 'running' | 'ok' | 'partial' | 'failed';
  trigger: string;
  started_at: string;
  finished_at: string;
  error: string | null;
  steps: StepRow[];
}

export interface TaskItem {
  id: string;
  name: string;
  enabled: boolean;
  next_run: string | null;
  last: RunRow | null;
  needs_approval: boolean;
}

export interface HealthRow {
  category: string;
  title: string;
  severity: 'ok' | 'info' | 'warning' | 'critical';
  detail: string | null;
  objects: string[];
}

export interface HealthView {
  checks: HealthRow[];
  skipped: string[];
}

export interface EventRow {
  seq: number;
  kind: 'query' | 'task';
  title: string;
  body: string;
  ok: boolean;
  at: string;
}

export interface ConnectionItem {
  id: string;
  name: string;
  engine: string;
  color: string | null;
  locks: boolean;
  processes: boolean;
  kill: boolean;
  monitor: boolean;
}

export interface MetricRow {
  key: string;
  label: string;
  group: string;
  unit: 'percent' | 'bytes' | 'count' | 'millis' | 'seconds';
  value: number | null;
  max: number | null;
  counter: boolean;
}

export interface MonitorView {
  metrics: MetricRow[];
  info: [string, string][];
  notes: string[];
}

export interface LockRow {
  id: string;
  blocked_by: string | null;
  user: string | null;
  client: string | null;
  database: string | null;
  wait: string | null;
  waited_ms: number | null;
  object: string | null;
  sql: string | null;
}

export interface ProcessRow {
  id: string;
  status: string | null;
  user: string | null;
  host: string | null;
  database: string | null;
  elapsed_ms: number | null;
  blocked_by: string | null;
  sql: string | null;
}

type Call =
  | { call: 'hello' }
  | { call: 'connections' }
  | { call: 'databases'; connection_id: string }
  | { call: 'locks'; connection_id: string; database: string }
  | { call: 'processes'; connection_id: string; database: string }
  | { call: 'monitor'; connection_id: string }
  | { call: 'tasks' }
  | { call: 'task_runs'; task_id: string }
  | { call: 'run_task'; task_id: string }
  | { call: 'health'; connection_id: string; database: string }
  | { call: 'kill'; connection_id: string; database: string; session_id: string };

const ask = <T>(call: Call) => invoke<T>('call', { call });

export const api = {
  status: () => invoke<Status>('status'),
  pair: (code: string, name: string) => invoke<string>('pair', { code, name }),
  unpair: () => invoke<void>('unpair'),
  hello: () => ask<Hello>({ call: 'hello' }),
  connections: () => ask<ConnectionItem[]>({ call: 'connections' }),
  locks: (connection_id: string) => ask<LockRow[]>({ call: 'locks', connection_id, database: '' }),
  processes: (connection_id: string) => ask<ProcessRow[]>({ call: 'processes', connection_id, database: '' }),
  monitor: (connection_id: string) => ask<MonitorView>({ call: 'monitor', connection_id }),
  databases: (connection_id: string) => ask<string[]>({ call: 'databases', connection_id }),
  health: (connection_id: string, database: string) => ask<HealthView>({ call: 'health', connection_id, database }),
  tasks: () => ask<TaskItem[]>({ call: 'tasks' }),
  taskRuns: (task_id: string) => ask<RunRow[]>({ call: 'task_runs', task_id }),
  runTask: (task_id: string) => ask<boolean>({ call: 'run_task', task_id }),
  news: () => invoke<EventRow[]>('news'),
  kill: (connection_id: string, session_id: string) => ask<boolean>({ call: 'kill', connection_id, database: '', session_id }),
};

export function errorText(e: unknown): string {
  return tb(typeof e === 'string' ? e : e instanceof Error ? e.message : JSON.stringify(e));
}

/** "40 s", "7 min", "1 h 05 min". */
export function duration(ms: number | null | undefined): string {
  const s = Math.max(0, Math.floor((ms ?? 0) / 1000));
  if (s < 60) return `${s} s`;
  if (s < 3600) return `${Math.floor(s / 60)} min`;
  return `${Math.floor(s / 3600)} h ${String(Math.floor((s % 3600) / 60)).padStart(2, '0')} min`;
}
