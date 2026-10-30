import { computed, shallowReactive, watch } from 'vue';
import type { DbObject, DriverInfo } from '../api/types';
import type { TableSchema } from '../api/schema-types';
import { dbKey, objKey, useConnectionsStore } from '../stores/connections';
import { findTable, relationsSchema } from './fkJump';
import { tableAt, tableNamed, type HoverIndex, type TableHoverState, type TableHoverTarget, type TableRef } from './tableHover';

// The data behind the editor's table popup (composables/tableHover.ts): the
// columns come from the explorer's store (name, type, nullable, key: one
// request per table, kept until the explorer refreshes it), the comments
// from `database_schema` (the ER diagram's read, cached per database by
// fkJump and read again when the explorer refreshes the database's objects).

const LOADING: TableHoverState = { status: 'loading' };

export function useTableHover(tab: () => { connectionId: string; database: string }, driver: () => DriverInfo | null | undefined, ready: () => boolean) {
  const conns = useConnectionsStore();

  /** The tab's database and, on cross-database engines, the others. */
  const index = computed<HoverIndex | null>(() => {
    const d = driver();
    if (!d || !ready()) return null;
    const { connectionId, database } = tab();
    const own = conns.objects[dbKey(connectionId, database)];
    const objects = own && (own.status === 'ready' || own.status === 'stale') ? own.items : [];
    let databases: Record<string, DbObject[] | null> | undefined;
    if (d.cross_database) {
      databases = {};
      for (const db of conns.live[connectionId]?.databases ?? []) {
        const o = conns.objects[dbKey(connectionId, db)];
        databases[db] = o && (o.status === 'ready' || o.status === 'stale') ? o.items : null;
      }
    }
    return { language: d.language, dialect: d.dialect, database, objects, kinds: d.object_kinds, databases };
  });

  // -- requests, at most one in flight per table (or database) --------------------------------
  const inFlight = new Set<string>();
  /** Out of the reactive read that noticed it's missing. */
  function once(key: string, run: () => Promise<unknown>) {
    if (inFlight.has(key)) return;
    inFlight.add(key);
    queueMicrotask(() => { void run().catch(() => {}).finally(() => inFlight.delete(key)); });
  }

  /** database → its tables as `database_schema` reads them (comments), and
   *  the explorer's object list they were read against. */
  const described = shallowReactive<Record<string, { against: DbObject[] | null; tables: TableSchema[] | null }>>({});
  function tablesOf(connectionId: string, database: string): TableSchema[] | null {
    const k = dbKey(connectionId, database);
    const objs = conns.objects[k];
    const against = objs?.status === 'ready' ? objs.items : null;
    const had = described[k];
    // Read again only when the explorer read the objects again (a refresh).
    const refreshed = !!had && had.against !== null && against !== null && had.against !== against;
    if (!had || refreshed) {
      once(`schema\u0000${k}`, async () => {
        described[k] = { against, tables: had?.tables ?? null };
        const tables = await relationsSchema(connectionId, database, refreshed).catch(() => null);
        described[k] = { against, tables };
      });
    } else if (had.against === null && against !== null) {
      had.against = against;
    }
    return had?.tables ?? null;
  }

  /** The popup's state for `r`, read from the store (reactive). */
  function stateOf(r: TableRef): TableHoverState {
    const { connectionId } = tab();
    let o = r.object;
    if (!o) {
      // Another database, its objects not loaded when hovered.
      const objs = conns.objects[dbKey(connectionId, r.database)];
      if (!objs) { once(`objects\u0000${r.database}`, () => conns.loadObjects(connectionId, r.database)); return LOADING; }
      if (objs.status === 'error') return { status: 'error', message: objs.error ?? '' };
      const idx = index.value;
      o = idx ? tableNamed(idx, r.database, objs.items, r.path) : null;
      if (!o) return objs.status === 'ready' ? { status: 'empty' } : LOADING;
    }
    const c = conns.columns[objKey(connectionId, r.database, o.schema, o.name)];
    if (!c) {
      const obj = o;
      once(`columns\u0000${r.database}\u0000${o.schema ?? ''}\u0000${o.name}`, () => conns.loadColumns(connectionId, r.database, obj));
      return LOADING;
    }
    if (c.status === 'error') return { status: 'error', message: c.error ?? '' };
    if (!c.items.length) return c.status === 'ready' ? { status: 'empty' } : LOADING;
    const tables = tablesOf(connectionId, r.database);
    const t = tables ? findTable(tables, { schema: o.schema, name: o.name }) : undefined;
    return {
      status: 'ready',
      comment: t?.comment || null,
      columns: c.items.map((col) => ({
        name: col.name,
        data_type: col.data_type,
        nullable: col.nullable,
        primary_key: col.primary_key,
        comment: t?.columns.find((x) => x.name === col.name)?.comment || null,
      })),
    };
  }

  /** What CodeEditor's `tableHover` asks: the table named at `pos`. */
  function tableHover(doc: string, pos: number): TableHoverTarget | null {
    const idx = index.value;
    const r = idx ? tableAt(idx, doc, pos) : null;
    if (!r) return null;
    const name = r.object ? [r.object.schema, r.object.name] : r.path;
    const title = [r.database !== tab().database ? r.database : null, ...name].filter((x) => x).join('.');
    return {
      from: r.from,
      to: r.to,
      title,
      watch: (cb) => watch(() => stateOf(r), cb, { immediate: true }),
    };
  }

  return { tableHover };
}
