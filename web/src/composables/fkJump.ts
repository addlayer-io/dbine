import { reactive } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import type { Cell, ObjectRef } from '../api/types';
import type { TableSchema } from '../api/schema-types';
import { describe, type ColumnFilter, type FilterState } from './gridFilter';
import { dbKey, useConnectionsStore } from '../stores/connections';
import { useTabsStore } from '../stores/tabs';

// Jumping from a cell to related rows by foreign key: "Ir a la fila
// referenciada" (the parent row of an FK cell) and "Filas que la
// referencian" (the child rows of a referenced key). The relations are the
// ones `database_schema` reports (the ER diagram's); the jump opens the other
// table's data with the grid's column filters set to the key, so the filter
// is visible and can be cleared.

/** A foreign key seen from one result: the other table and how the columns pair. */
export interface FkLink {
  /** The other table (the parent for `parents`, the child for `children`). */
  table: ObjectRef;
  /** The result's columns (indexes) holding the key, in the key's order. */
  cols: number[];
  /** The other table's columns, paired with `cols`. */
  otherColumns: string[];
  /** The constraint name, when the engine reports one. */
  name: string | null;
}

export interface GridLinks {
  /** Foreign keys of the result's table whose columns are all in the result. */
  parents: FkLink[];
  /** Foreign keys of other tables to the result's table whose referenced
   *  columns are all in the result. */
  children: FkLink[];
}

const TTL_MS = 5 * 60_000;
const schemas = new Map<string, { at: number; tables: Promise<TableSchema[]> }>();

/** The database's tables with their keys (cached a few minutes per
 *  database; `force`: read again, after the explorer refreshed it). */
export function relationsSchema(connectionId: string, database: string, force = false): Promise<TableSchema[]> {
  const key = dbKey(connectionId, database);
  const hit = schemas.get(key);
  if (!force && hit &&Date.now() - hit.at < TTL_MS) return hit.tables;
  const tables = invoke<TableSchema[]>('database_schema', { args: { connection_id: connectionId, database } });
  schemas.set(key, { at: Date.now(), tables });
  tables.catch(() => { if (schemas.get(key)?.tables === tables) schemas.delete(key); });
  return tables;
}

const eq = (a: string | null | undefined, b: string | null | undefined) => (a ?? '').toLowerCase() === (b ?? '').toLowerCase();

/** `target` among the tables: exact name and schema first, then ignoring
 *  case; without a schema, only when the name is unique. */
export function findTable(tables: TableSchema[], target: { schema: string | null; name: string }): TableSchema | undefined {
  const exact = tables.filter((t) => t.name === target.name && (!target.schema || t.schema === target.schema));
  if (exact.length === 1) return exact[0];
  const loose = tables.filter((t) => eq(t.name, target.name) && (!target.schema || eq(t.schema, target.schema)));
  return loose.length === 1 ? loose[0] : undefined;
}

const refOf = (t: TableSchema): ObjectRef => ({ kind: t.kind || 'table', schema: t.schema, name: t.name });

/**
 * The jumps a result offers. `columns` are the result's column names and
 * `source` says which column of `target` each one is (null: none, as an
 * expression); a key counts only with all its columns in the result.
 */
export function linksFor(tables: TableSchema[], target: { schema: string | null; name: string }, columns: string[], source: (name: string) => string | null = (n) => n): GridLinks {
  const out: GridLinks = { parents: [], children: [] };
  const table = findTable(tables, target);
  if (!table) return out;
  const base = columns.map((c) => source(c));
  const at = (column: string) => {
    let i = base.findIndex((b) => b === column);
    if (i < 0) i = base.findIndex((b) => eq(b, column));
    return i;
  };
  const indexes = (names: string[]) => {
    const cols = names.map(at);
    return names.length && cols.every((i) => i >= 0) ? cols : null;
  };
  // A key may name no columns on the referenced side (SQLite's bare
  // `REFERENCES t`): it's that table's primary key.
  const referenced = (parent: TableSchema | undefined, cols: string[]) => (cols.length ? cols : parent?.primary_key?.columns ?? []);

  for (const fk of table.foreign_keys) {
    const parent = findTable(tables, { schema: fk.ref_schema ?? table.schema, name: fk.ref_table }) ?? findTable(tables, { schema: null, name: fk.ref_table });
    const other = referenced(parent, fk.ref_columns);
    const cols = indexes(fk.columns);
    if (!cols || other.length !== cols.length) continue;
    out.parents.push({
      table: parent ? refOf(parent) : { kind: 'table', schema: fk.ref_schema ?? table.schema, name: fk.ref_table },
      cols, otherColumns: other, name: fk.name,
    });
  }
  for (const child of tables) {
    for (const fk of child.foreign_keys) {
      const parent = findTable(tables, { schema: fk.ref_schema ?? child.schema, name: fk.ref_table }) ?? findTable(tables, { schema: null, name: fk.ref_table });
      if (parent !== table) continue;
      const cols = indexes(referenced(parent, fk.ref_columns));
      if (!cols || cols.length !== fk.columns.length) continue;
      out.children.push({ table: refOf(child), cols, otherColumns: fk.columns, name: fk.name });
    }
  }
  return out;
}

/** The other table's column filters for a row's key values; null when a
 *  value is NULL (it references nothing). */
export function jumpFilters(link: FkLink, values: Cell[]): Record<string, FilterState> | null {
  if (values.some((v) => v === null || v === undefined)) return null;
  const out: Record<string, FilterState> = {};
  link.otherColumns.forEach((column, i) => {
    const f: ColumnFilter = { column, op: 'eq', values: [values[i]] };
    out[column] = { text: describe(f), filters: [f] };
  });
  return out;
}

/** Filters a data tab takes when it opens (or shows again) after a jump. */
export const pendingJumps = reactive(new Map<string, Record<string, FilterState>>());

/** Opens `table`'s data (a preview tab, or the one already open) filtered. */
export async function openFiltered(connectionId: string, database: string, table: ObjectRef, filters: Record<string, FilterState>) {
  const conns = useConnectionsStore();
  const tabs = useTabsStore();
  // The explorer's own reference, so an open tab of the table is reused.
  await conns.loadObjects(connectionId, database).catch(() => undefined);
  const objs = conns.objects[dbKey(connectionId, database)]?.items ?? [];
  const found = objs.find((o) => o.name === table.name && o.schema === table.schema)
    ?? objs.find((o) => eq(o.name, table.name) && eq(o.schema, table.schema));
  const object: ObjectRef = found ? { kind: found.kind, schema: found.schema, name: found.name } : table;
  // A new tab's view mounts after this and takes its filters then.
  tabs.openObject(connectionId, database, object, 'data');
  if (tabs.activeId) pendingJumps.set(tabs.activeId, filters);
}
