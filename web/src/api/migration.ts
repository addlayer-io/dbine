import { invoke } from '@tauri-apps/api/core';

// Migration between engines (src-tauri/src/commands/migration.rs,
// docs/migracion.md): the plan (report + script), and the run, whose data
// goes through the bulk transfer engine (tables in parallel, resumable).

export interface ModeSupport { available: boolean; reason: string | null }
export interface MigrationTarget {
  id: string; name: string; family: string; supported: boolean; reason: string | null;
  /** From the source given to `targets` (unavailable without one). */
  clone: ModeSupport; sync: ModeSupport;
}
/** Convert the structure; clone (same engine, identical target); sync only the rows that changed. */
export type MigrationMode = 'convert' | 'clone' | 'sync';
export type DeltaDepth = 'Full' | 'Sizes' | 'Keys';
export interface SyncKey { schema: string | null; name: string; columns: string[] }
export interface SyncOptions { depth: DeltaDepth; max_cores: number; keys: SyncKey[] }
export interface SyncTable {
  schema: string | null; name: string; primary_key: string[] | null; unique_keys: string[][]; key: string[] | null; reason: string | null;
}
export type Severity = 'info' | 'warning' | 'loss' | 'dropped';
export interface MigrationIssue { severity: Severity; code: string; table: string; object: string | null; message: string }
export interface ColumnMapping { table: string; column: string; target_column: string; source_type: string; target_type: string }
export interface MigrationPlan {
  tables: { source: string; target: string; columns: number }[];
  columns: ColumnMapping[];
  issues: MigrationIssue[];
  script: string;
  available: { schema: string | null; name: string; columns: number }[];
  sync_tables: SyncTable[];
}
export interface PlanOptions {
  fold_case: boolean;
  target_schema: string | null;
  drop: boolean;
  if_exists: boolean;
  indexes: boolean;
  foreign_keys: boolean;
  data: boolean;
  keep_schemas: boolean;
}
export type CopyOrder = 'largest_first' | 'smallest_first' | 'alphabetical';
export interface TransferOptions { parallel: number | null; order: CopyOrder | null; commit_rows: number | null }

export type CopyPath = 'native' | 'bulk_load' | 'insert_script' | 'delta';
export type TableStatus = 'pending' | 'running' | 'copied' | 'done' | 'failed' | 'cancelled';
export type RunStatus = 'running' | 'interrupted' | 'done' | 'failed' | 'cancelled';
export interface CopyStats {
  path: CopyPath | null; rows: number; elapsed_ms: number; rows_per_s: number;
  waited_on_source_ms: number; waited_on_destination_ms: number; bottleneck: 'source' | 'destination' | null;
  /** Sync: what changed on the target (`rows` are then the rows reviewed). */
  delta?: { inserted: number; updated: number; deleted: number; notes: string[] } | null;
}
export interface RunTable {
  name: string; source: string; target: string; status: TableStatus; rows_done: number; rows_total: number | null;
  path: CopyPath; stats: CopyStats | null; attempts: number; error: string | null;
}
export interface RunResult {
  run_id: string; status: RunStatus; tables: RunTable[]; foreign_key_errors: string[]; after_errors: string[]; notes: string[];
  elapsed_ms: number; cancelled: boolean; mode: MigrationMode;
}
export interface RunInfo {
  id: string; status: RunStatus; stage: 'structure' | 'data' | 'constraints' | 'finished'; created_at: string; finished_at: string | null;
  source_connection_id: string; source_database: string; target_connection_id: string; target_database: string; target_driver: string;
  parallel: number; resumable: boolean; tables: RunTable[]; foreign_key_errors: string[]; after_errors: string[]; notes: string[];
  mode: MigrationMode;
}

/** `migration-progress`: the engine's events as they are, plus the stage steps and the plan. */
export type MigrationEvent = { id: string } & (
  | { event: 'plan'; tables: RunTable[]; parallel: number }
  | { event: 'step'; phase: 'schemas' | 'drop' | 'create' | 'foreign_keys' | 'identity' | 'script' | 'before' | 'check' | 'after' | 'done'; table: string; done: number; total: number }
  | { event: 'run_started'; run_id: string; tables: number; parallel: number }
  | { event: 'table_started'; table: string; attempt: number }
  | { event: 'table_phase'; table: string; phase: 'check' | 'truncate' | 'copy' | 'indexes' | 'summary' | 'compare' | 'apply' }
  | { event: 'table_progress'; table: string; rows_done: number; rows_total: number | null; rows_per_s: number }
  | { event: 'table_done'; table: string; rows: number; stats: CopyStats }
  | { event: 'table_failed'; table: string; error: string }
  | { event: 'table_cancelled'; table: string }
  | { event: 'run_finished'; summary: { run_id: string; status: string; done: number; failed: number; cancelled: number; pending: number; rows: number; elapsed_ms: number } }
  | { event: 'log'; level: 'info' | 'warn' | 'error'; text: string }
);

export const migrationApi = {
  /** Every engine; with the source connection, also whether each one can clone or sync from it. */
  targets: (sourceConnectionId?: string) =>
    invoke<MigrationTarget[]>('migration_targets', { args: { source_connection_id: sourceConnectionId ?? null } }),
  /** `target`: clone mode's preview asks the target connection what it supports (it only reads). */
  plan: (a: {
    connectionId: string; database: string; tables: { schema: string | null; name: string }[]; targetDriver: string; options: PlanOptions;
    mode: MigrationMode; sync: SyncOptions; target: { connection_id: string; database: string } | null;
  }) =>
    invoke<MigrationPlan>('migration_plan', {
      args: {
        connection_id: a.connectionId, database: a.database, tables: a.tables, target_driver: a.targetDriver, options: a.options,
        mode: a.mode, sync: a.sync, target: a.target,
      },
    }),
  run: (a: {
    migrationId: string; connectionId: string; database: string; tables: { schema: string | null; name: string }[];
    targetDriver: string; options: PlanOptions; targetConnectionId: string; targetDatabase: string; transfer: TransferOptions;
    mode: MigrationMode; sync: SyncOptions;
  }) =>
    invoke<RunResult>('migration_run', {
      args: {
        migration_id: a.migrationId, connection_id: a.connectionId, database: a.database, tables: a.tables,
        target_driver: a.targetDriver, options: a.options, target_connection_id: a.targetConnectionId, target_database: a.targetDatabase,
        transfer: a.transfer, mode: a.mode, sync: a.sync,
      },
    }),
  cancel: (runId: string) => invoke<void>('migration_cancel', { args: { run_id: runId } }),
  cancelTable: (runId: string, table: string) => invoke<boolean>('migration_cancel_table', { args: { run_id: runId, table } }),
  runNow: (runId: string, table: string) => invoke<boolean>('migration_run_now', { args: { run_id: runId, table } }),
  setParallel: (runId: string, n: number) => invoke<number>('migration_set_parallel', { args: { run_id: runId, n } }),
  /** Recent runs, or (`ids`) those runs whatever their age. */
  runs: (limit?: number, ids?: string[]) => invoke<RunInfo[]>('migration_runs', { args: { limit: limit ?? null, ids: ids ?? null } }),
  resume: (runId: string) => invoke<RunResult>('migration_resume', { args: { run_id: runId } }),
  retryFailed: (runId: string) => invoke<RunResult>('migration_retry_failed', { args: { run_id: runId } }),
  forget: (runId: string) => invoke<void>('migration_forget', { args: { run_id: runId } }),
};
