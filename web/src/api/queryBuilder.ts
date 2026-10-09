import { invoke } from '@tauri-apps/api/core';
import type { Cell, ResultColumn } from './types';

// "Diseñar consulta" (docs/query-builder.md). Mirrors
// src-tauri/src/commands/query_builder.rs. The spec is what the builder tab
// keeps; the backend turns it into the engine's SQL.

export type JoinKind = 'inner' | 'left' | 'right' | 'full';
export type Aggregate = 'none' | 'count' | 'sum' | 'avg' | 'min' | 'max' | 'count_distinct';
export type SortDir = 'none' | 'asc' | 'desc';
export type FilterOp =
  | 'eq' | 'ne' | 'lt' | 'le' | 'gt' | 'ge' | 'like' | 'not_like' | 'in' | 'not_in' | 'between' | 'is_null' | 'is_not_null';

export interface SpecTable {
  id: string;
  kind: string;
  schema: string | null;
  name: string;
  /** Empty: the table's name. */
  alias: string;
  /** Position on the canvas (UI only). */
  x: number;
  y: number;
}

export interface JoinPair { left: string; right: string }

export interface SpecJoin {
  id: string;
  kind: JoinKind;
  /** Table ids. */
  left: string;
  right: string;
  on: JoinPair[];
}

export interface Condition {
  op: FilterOp;
  value: string;
  /** Upper end of `between`. */
  value2: string;
  /** The value is an SQL expression, not a literal. */
  raw: boolean;
}

export interface SpecColumn {
  id: string;
  /** The table's id in the spec. */
  table: string;
  /** `*`: every column. */
  column: string;
  data_type: string;
  alias: string;
  show: boolean;
  sort: SortDir;
  sort_order: number | null;
  aggregate: Aggregate;
  /** One cell per filter group (ANDed inside a group, groups ORed). */
  filters: (Condition | null)[];
}

export interface QuerySpec {
  database: string;
  tables: SpecTable[];
  joins: SpecJoin[];
  columns: SpecColumn[];
  distinct: boolean;
  limit: number | null;
  /** Filter columns shown in the grid (UI only). */
  filter_groups: number;
}

/** What the builder offers on the engine. */
export interface BuilderFeatures {
  /** Empty: one table per query. */
  joins: JoinKind[];
  group_by: boolean;
  having: boolean;
  aggregates: Aggregate[];
  distinct: boolean;
  order_by: boolean;
  limit: boolean;
  or_groups: boolean;
  operators: FilterOp[];
}

export interface BuiltQuery {
  sql: string;
  /** Spanish backend messages (translate with `tb`). */
  warnings: string[];
  features: BuilderFeatures;
}

export interface BuiltPreview {
  sql: string;
  columns: ResultColumn[];
  rows: Cell[][];
  truncated: boolean;
  elapsed_ms: number;
}

export function emptySpec(database: string): QuerySpec {
  return { database, tables: [], joins: [], columns: [], distinct: false, limit: null, filter_groups: 1 };
}

export const queryBuilderApi = {
  build: (connectionId: string, spec: QuerySpec, sessionId: string) =>
    invoke<BuiltQuery>('build_query', { args: { connection_id: connectionId, spec, session_id: sessionId } }),
  preview: (connectionId: string, spec: QuerySpec, sessionId: string) =>
    invoke<BuiltPreview>('preview_built_query', { args: { connection_id: connectionId, spec, session_id: sessionId } }),
  /** The preview's session (`cancel_query` key). */
  previewKey: (sessionId: string) => `qb-preview:${sessionId}`,
};
