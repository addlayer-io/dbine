import { invoke } from '@tauri-apps/api/core';
import type { Asker } from './aiTask';
import type { ColumnFilter } from '../composables/gridFilter';
import type { Cell, ObjectRef, ResultColumn, SortKey } from './types';

// Data compare (docs/data-compare.md).

export interface DataSide { connection_id: string; database: string; object: ObjectRef }

/** A side of a query compare (`data_compare_query`): where its query runs. */
export interface QuerySide { connection_id: string; database: string; query: string }
/** A compared column: its name in each result; `key` ones pair the rows. */
export interface ColumnPair { left: string; right: string; key: boolean }
/** How a query compare's values compare beyond their type. */
export interface QueryOptions {
  /** Two numbers at most this far apart are the same (0: exactly)… */
  absolute: number;
  /** …or this far relative to the larger (0.001 is 0.1 %). */
  relative: number;
  /** NULL equals a blank value (0, "" or false), as DAX's BLANK. */
  null_as_blank: boolean;
  ignore_case: boolean;
  trim: boolean;
}
/** The table compare's result, plus each compared column's name and type on
 *  the right (`columns` and `types` are the left's). */
export interface QueryCompareResult extends DataCompareResult { right_columns: string[]; right_types: string[] }
/** A query's result columns, and its first row (what the suggested key looks at). */
export interface QueryColumns { columns: ResultColumn[]; sample: Cell[] | null }

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
  /** Always false: whole tables are compared (kept for older backends). */
  truncated_left: boolean;
  truncated_right: boolean;
  /** The lists come in key order. */
  ordered: boolean;
  duplicate_keys: number;
  /** More rows differ than `diff_limit`: the screen and the script cover the
   *  first ones in key order (the counts are exact). */
  diff_truncated: boolean;
  diff_limit: number;
  /** Rows of each kind in the lists the screen filters and the script syncs
   *  (`counts` unless `diff_truncated`). */
  listed: { changed: number; only_left: number; only_right: number };
}

/** `data-compare-progress`, about four times a second per side while a
 *  compare runs. `rows`: rows read on that side; `differences`: differing
 *  rows brought back (while loading them). `preparing`: a read waits while
 *  the local database for big tables opens. */
export interface DataCompareProgress {
  run: string;
  side: 'left' | 'right';
  rows: number;
  phase: 'reading' | 'preparing' | 'comparing' | 'loading_differences';
  differences: number;
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

/** A request in words, as the grid's filters (src-tauri/src/commands/data_compare_ai.rs). */
export interface AiFilter { filters: ColumnFilter[]; changed_in: string[]; sort: SortKey[]; note: string | null }
/** Key columns for a table without a primary key: `unique` (a unique index of
 *  NOT NULL columns), `ai`, or "" when there's none to suggest. */
export interface KeyHint { key: string[]; by: '' | 'unique' | 'ai' }

export const dataCompareApi = {
  /** Whole tables; `run` is this compare's id (its progress events carry it, `cancel` stops it). */
  compare: (left: DataSide, right: DataSide, key: string[], run: string) =>
    invoke<DataCompareResult>('data_compare', { args: { left, right, key, columns: [], run } }),
  /** Stops a running compare: it then fails as cancelled. */
  cancel: (run: string) => invoke<void>('data_compare_cancel', { args: { run } }),
  /** One kind of difference, filtered and sorted over all its rows. */
  rows: (id: string, kind: 'changed' | 'only_left' | 'only_right', sort: SortKey[], filters: ColumnFilter[], changedIn: string[]) =>
    invoke<DataRows>('data_compare_rows', { args: { id, kind, sort, filters, changed_in: changedIn } }),
  /** The differences in words, from counts and typical gaps: no row's values go to the AI. */
  explain: (id: string, asker: Asker) => invoke<{ text: string }>('data_compare_explain', { args: { id, asker } }),
  /** A request in words as filters; the AI gets the columns and the words. */
  aiFilter: (id: string, kind: 'changed' | 'only_left' | 'only_right', request: string, asker: Asker) =>
    invoke<AiFilter>('data_compare_ai_filter', { args: { id, kind, request, asker } }),
  /** Without an AI set up (`asker` null) only a unique index is suggested. */
  keyHint: (left: DataSide, asker: Asker | null) => invoke<KeyHint>('data_compare_key_hint', { args: { left, asker } }),
  /** One script per side that changes. */
  scripts: (id: string, choices: Choices) => invoke<DataScript[]>('data_compare_script', { args: { id, choices } }),
  /** Two queries' whole results, read-only (no script); `run` as in `compare`. */
  compareQueries: (left: QuerySide, right: QuerySide, columns: ColumnPair[], options: QueryOptions, run: string) =>
    invoke<QueryCompareResult>('data_compare_query', { args: { left, right, columns, options, run } }),
  /** A query's result columns (it stops at its first row); `cancel(run)` stops it. */
  queryColumns: (side: QuerySide, run: string) => invoke<QueryColumns>('data_compare_query_columns', { args: { side, run } }),
};
