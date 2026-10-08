import { invoke } from '@tauri-apps/api/core';
import type { ObjectRef } from './types';

// "Copiar un subconjunto…" (src-tauri/src/commands/subset): some rows of a
// table, the parents they need and optionally what hangs from them, copied
// to another database with personal data masked. Progress arrives as
// `subset-progress` events tagged with the run id; `cancel_query` on
// `subset:<id>:src` and `subset:<id>:tgt` stops it.

export type FakeKind = 'name' | 'first_name' | 'last_name' | 'email' | 'phone' | 'document' | 'address' | 'city' | 'company';

/** `MaskRule` in Rust. */
export type MaskRule =
  | { rule: 'keep' }
  | { rule: 'fake'; kind: FakeKind }
  | { rule: 'shift_date'; days: number }
  | { rule: 'noise'; percent: number }
  | { rule: 'fixed'; value: string }
  | { rule: 'null' }
  | { rule: 'hash' };

export type FilterOp = 'eq' | 'ne' | 'gt' | 'ge' | 'lt' | 'le' | 'contains' | 'starts_with' | 'is_null' | 'not_null' | 'in';

export interface SubsetColumnFilter { column: string; op: FilterOp; values: unknown[] }

export type SubsetLimit = { kind: 'all' } | { kind: 'rows'; count: number } | { kind: 'percent'; percent: number };

export interface SubsetArgs {
  run_id: string;
  connection_id: string;
  database: string;
  table: ObjectRef;
  filter: { expression: string | null; columns: SubsetColumnFilter[]; limit: SubsetLimit };
  children: { depth: number; max_rows: number } | null;
  target_connection_id: string;
  target_database: string;
}

export interface SubsetTableMask { schema: string | null; name: string; columns: Record<string, MaskRule> }

export interface PlanColumn { name: string; data_type: string; nullable: boolean; key: boolean; suggested: MaskRule; skipped: string | null }

export interface PlanTable {
  schema: string | null;
  name: string;
  role: 'start' | 'parent' | 'child';
  depth: number;
  rows: number;
  capped: boolean;
  target: string;
  exists: boolean;
  create_ddl: string | null;
  error: string | null;
  columns: PlanColumn[];
}

export interface SubsetPlan {
  tables: PlanTable[];
  total_rows: number;
  cycles: string[];
  notes: string[];
  confirm_label: string | null;
  target_engine: string;
}

export interface TableReport {
  table: string;
  target: string;
  created: boolean;
  rows: number;
  written: number;
  masked: string[];
  status: 'done' | 'error' | 'cancelled' | 'skipped';
  error: string | null;
  notes: string[];
}

export interface SubsetReport { tables: TableReport[]; notes: string[]; elapsed_ms: number; cancelled: boolean }

export interface SubsetProgress {
  runId: string;
  phase: 'read' | 'collect' | 'create' | 'insert' | 'cycles' | 'constraints' | 'done';
  table?: string;
  rows: number;
  total?: number;
}

export const subsetApi = {
  plan: (args: SubsetArgs) => invoke<SubsetPlan>('subset_plan', { args }),
  run: (args: SubsetArgs, masks: SubsetTableMask[], confirm: string) => invoke<SubsetReport>('subset_run', { args: { ...args, masks, confirm } }),
  cancel: (runId: string) =>
    Promise.all(['src', 'tgt'].map((side) => invoke('cancel_query', { args: { session_id: `subset:${runId}:${side}` } }).catch(() => {}))),
};
