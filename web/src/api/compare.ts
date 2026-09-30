import { invoke } from '@tauri-apps/api/core';
import type { TableSchema } from './schema-types';

// "Comparar esquemas" (src-tauri/src/commands/compare.rs,
// crates/dbine-schema/src/compare.rs, crates/dbine-driver/src/alter.rs).

export interface CodeObject { kind: string; schema: string | null; name: string; definition: string }
export interface DbModel { driver: string; tables: TableSchema[]; objects: CodeObject[] }
export interface Loaded extends DbModel { warnings: string[] }

export interface CompareOptions { ignore_case: boolean; ignore_schema: boolean; ignore_comments: boolean }
export type Status = 'equal' | 'changed' | 'only_left' | 'only_right';

export interface ItemDiff {
  name: string;
  left: number | null;
  right: number | null;
  status: Status;
  /** type, nullable, default, auto_increment, comment, columns, unique, kind, filter, on_delete, on_update */
  fields: string[];
}
export interface TableDiff {
  key: string;
  left: number | null;
  right: number | null;
  status: Status;
  columns: ItemDiff[];
  indexes: ItemDiff[];
  foreign_keys: ItemDiff[];
  checks: ItemDiff[];
  primary_key: Status;
  fields: string[];
}
export interface ObjectDiff { kind: string; key: string; left: number | null; right: number | null; status: Status }
export interface CompareResult { tables: TableDiff[]; objects: ObjectDiff[] }

export type TableChange =
  | { op: 'create'; table: TableSchema }
  | { op: 'drop'; table: TableSchema }
  | { op: 'alter'; old: TableSchema; new: TableSchema };
export type ObjectChange = { op: 'create' | 'drop' | 'replace'; object: CodeObject };
export interface SyncScript { statements: string[]; warnings: string[] }

export const compareApi = {
  load: (connectionId: string, database: string) =>
    invoke<Loaded>('schema_compare_load', { args: { connection_id: connectionId, database, schemas: [] } }),
  compare: (left: DbModel, right: DbModel, options: CompareOptions) =>
    invoke<CompareResult>('schema_compare', { args: { left, right, options } }),
  convert: (fromDriver: string, toDriver: string, tables: TableSchema[], targetSchema: string | null) =>
    invoke<{ tables: TableSchema[]; warnings: string[] }>('schema_compare_convert', {
      args: { from_driver: fromDriver, to_driver: toDriver, tables, target_schema: targetSchema },
    }),
  /** `views`: the target's views as they'll be (the ones over reshaped tables are made again). */
  script: (connectionId: string, tables: TableChange[], objects: ObjectChange[], views: CodeObject[]) =>
    invoke<SyncScript>('schema_sync_script', { args: { connection_id: connectionId, tables, objects, views } }),
  run: (connectionId: string, database: string, statements: string[], runId: string) =>
    invoke<{ done: number; failed: [number, string] | null }>('schema_sync_run', {
      args: { connection_id: connectionId, database, statements, run_id: runId },
    }),
};
