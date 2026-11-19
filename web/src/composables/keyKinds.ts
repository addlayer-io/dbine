import type { DbObject, ObjectKindInfo } from '../api/types';
import type { TableSchema } from '../api/schema-types';

// A key database (Redis, etcd) shows its keys through the key search: they
// are never listed, so the explorer doesn't call `list_objects` for it
// (Redis' walks the keyspace). Its driver's other kinds (Redis' search
// indexes, `index`) come from the database's schema read
// (`database_schema`), which has them without reading a key.

/** Whether the driver declares kinds besides its keys (the first one). */
export const hasOtherKinds = (kinds: ObjectKindInfo[] | undefined) => (kinds?.length ?? 0) > 1;

/** The objects of the other kinds the driver declares, from its schema
 *  read: never the keys' kind, nor a kind it doesn't declare; by name. */
export function otherKindObjects(kinds: ObjectKindInfo[], tables: TableSchema[]): DbObject[] {
  const others = new Set(kinds.slice(1).map((k) => k.id));
  const seen = new Set<string>();
  const out: DbObject[] = [];
  for (const t of tables) {
    const kind = t.kind || 'table';
    const id = `${kind}\u0001${t.schema ?? ''}\u0001${t.name}`;
    if (!others.has(kind) || seen.has(id)) continue;
    seen.add(id);
    out.push({ kind, schema: t.schema ?? null, name: t.name, parent: null });
  }
  return out.sort((a, b) => a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: 'base' }));
}
