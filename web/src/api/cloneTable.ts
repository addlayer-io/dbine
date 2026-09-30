import { invoke } from '@tauri-apps/api/core';
import type { ObjectRef } from './types';

// "Clonar tabla" (src-tauri/src/commands/clone_table.rs): a copy of a table
// next to it, under a new name, with its data. Progress arrives as
// `clone-table-progress` events tagged with the run id.

export type ClonePhase = 'read' | 'create' | 'copy' | 'identity' | 'indexes' | 'foreign_keys' | 'cleanup';

/** `CloneEvent` in Rust, plus the run id. */
export type CloneTableEvent = { runId: string } & (
  | { event: 'phase'; phase: ClonePhase }
  | { event: 'progress'; rows_done: number; rows_total: number | null; rows_per_s: number }
  | { event: 'log'; level: 'info' | 'warn' | 'error'; text: string }
);

/** `Rename` in Rust: a constraint / index name the clone got. */
export interface CloneRename { from: string; to: string; shortened: boolean }

/** `CloneResult` in Rust. */
export interface CloneTableResult {
  table: ObjectRef;
  rows: number;
  elapsed_ms: number;
  notes: string[];
  renames: CloneRename[];
}

export const cloneTableApi = {
  run: (a: { runId: string; connectionId: string; database: string; object: ObjectRef; newName: string; withData: boolean; withIndexes: boolean }) =>
    invoke<CloneTableResult>('clone_table', {
      args: {
        run_id: a.runId, connection_id: a.connectionId, database: a.database, object: a.object,
        new_name: a.newName, with_data: a.withData, with_indexes: a.withIndexes,
      },
    }),
  cancel: (runId: string) => invoke<boolean>('clone_table_cancel', { args: { run_id: runId } }),
};

/** The proposed name: `<name>_yyyyMMdd_HHmmss`, local time. */
export function defaultCloneName(name: string, at = new Date()): string {
  const p = (n: number) => String(n).padStart(2, '0');
  return `${name}_${at.getFullYear()}${p(at.getMonth() + 1)}${p(at.getDate())}_${p(at.getHours())}${p(at.getMinutes())}${p(at.getSeconds())}`;
}
