import { invoke } from '@tauri-apps/api/core';

// The server's jobs: "Programadas › En los servidores" (docs/server-jobs.md).
// Copied by hand from crates/dbine-driver/src/jobs.rs.

/** How a run ended, or that it's still going. */
export type JobOutcome = 'succeeded' | 'failed' | 'canceled' | 'running' | 'retrying' | 'skipped' | 'unknown';

/** A job as the list shows it. Times are RFC 3339 in UTC. */
export interface ServerJob {
  /** The engine's id for it, opaque: what the other calls take. */
  id: string;
  name: string;
  /** The engine's word for it ("SQL Server Agent", "DBMS_SCHEDULER", "Task"…). */
  kind: string;
  group: string | null;
  description: string | null;
  owner: string | null;
  enabled: boolean;
  /** As the engine states it; null: only on demand. */
  schedule: string | null;
  running?: boolean;
  last_outcome: JobOutcome | null;
  last_started: string | null;
  last_duration_ms: number | null;
  next_run: string | null;
  can_run?: boolean;
  can_stop?: boolean;
  can_enable?: boolean;
  has_history?: boolean;
  has_script?: boolean;
  /** It comes with the database or the engine, not made by a user: hidden by default. */
  system?: boolean;
}

export interface JobList {
  jobs: ServerJob[];
  /** In Spanish from the driver: translate with `tb`. */
  notes?: string[];
}

export interface JobStep {
  name: string;
  kind: string | null;
  target: string | null;
  command: string | null;
}

export interface JobDetail {
  job: ServerJob;
  steps: JobStep[];
  schedules: string[];
  /** Labels in Spanish from the driver (translate with `tb`); `group` empty: General. */
  properties: { group: string; label: string; value: string }[];
  /** What to know about this job, in Spanish from the driver: translate with `tb`. */
  notes?: string[];
}

export interface JobStepRun {
  name: string;
  outcome: JobOutcome;
  started: string | null;
  duration_ms: number | null;
  message: string | null;
}

export interface JobRun {
  id: string | null;
  started: string | null;
  duration_ms: number | null;
  outcome: JobOutcome;
  /** In Spanish from the driver ("programada", "manual"…): translate with `tb`. */
  trigger: string | null;
  message: string | null;
  steps: JobStepRun[];
}

const a = (connectionId: string, extra: Record<string, unknown> = {}) => ({ args: { connection_id: connectionId, ...extra } });

export const jobsApi = {
  list: (connectionId: string) => invoke<JobList>('jobs_list', a(connectionId)),
  detail: (connectionId: string, id: string) => invoke<JobDetail>('jobs_detail', a(connectionId, { id })),
  runs: (connectionId: string, id: string, limit: number) => invoke<JobRun[]>('jobs_runs', a(connectionId, { id, limit })),
  run: (connectionId: string, id: string) => invoke<void>('jobs_run', a(connectionId, { id })),
  stop: (connectionId: string, id: string) => invoke<void>('jobs_stop', a(connectionId, { id })),
  setEnabled: (connectionId: string, id: string, enabled: boolean) => invoke<void>('jobs_set_enabled', a(connectionId, { id, enabled })),
  script: (connectionId: string, id: string) => invoke<string>('jobs_script', a(connectionId, { id })),
};
