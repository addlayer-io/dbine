import { invoke } from '@tauri-apps/api/core';
import type { ColumnFilter } from '../composables/gridFilter';
import type { Cell, ObjectRef, SortKey } from './types';

// Data compare (docs/data-compare.md).

export interface DataSide { connection_id: string; database: string; object: ObjectRef }

export interface ChangedRow { left: Cell[]; right: Cell[]; diff: number[] }

export interface DataCompareResult {
  id: string;
  key: string[];
  columns: string[];
  /** Their types, on the left table (what a filter offers for each). */
  types: string[];
  only_left_columns: string[];
  only_right_columns: string[];
  counts: { left: number; right: number; same: number; only_left: number; only_right: number; changed: number };
  only_left: Cell[][];
  only_right: Cell[][];
  changed: ChangedRow[];
  truncated_left: boolean;
  truncated_right: boolean;
  /** Both sides came sorted by the key. */
  ordered: boolean;
  duplicate_keys: number;
}

/** Where a row goes: right = make the right like the left for it (update,
 *  insert there, or delete there when only the right has it); left = the
 *  other way; none = leave it. */
export type Dir = 'none' | 'right' | 'left';
/** One kind of difference: `all` for every row, `rows` overriding some
 *  (index into that kind's list). */
export interface Pick { all: Dir; rows: [number, Dir][] }
export interface Choices { changed: Pick; only_left: Pick; only_right: Pick }

/** A row of one kind, with its place in that kind's whole list (what `Pick` counts by). */
export interface DataRow { index: number; left: Cell[]; right: Cell[] | null; diff: number[] }
export interface DataRows {
  /** The first rows that pass, in the order asked for. */
  rows: DataRow[];
  /** How many pass, among all the kind's rows. */
  total: number;
  /** With a filter, every row that passes. */
  indices: number[];
}

export interface DataScript {
  connection_id: string;
  database: string;
  /** The side it changes. */
  side: 'left' | 'right';
  script: string;
  inserts: number;
  updates: number;
  deletes: number;
  /** Statements in `script` (progress total). Older backends leave it out. */
  statements?: number;
  /** Runs inside one transaction (the engine supports manual transactions). */
  atomic?: boolean;
}

export const dataCompareApi = {
  compare: (left: DataSide, right: DataSide, key: string[], limit: number) =>
    invoke<DataCompareResult>('data_compare', { args: { left, right, key, columns: [], limit } }),
  /** One kind of difference, filtered and sorted over all its rows. */
  rows: (id: string, kind: 'changed' | 'only_left' | 'only_right', sort: SortKey[], filters: ColumnFilter[], changedIn: string[]) =>
    invoke<DataRows>('data_compare_rows', { args: { id, kind, sort, filters, changed_in: changedIn } }),
  /** One script per side that changes. */
  scripts: (id: string, choices: Choices) => invoke<DataScript[]>('data_compare_script', { args: { id, choices } }),
};
