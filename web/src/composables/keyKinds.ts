import type { DbObject, DriverInfo, ObjectKindInfo } from '../api/types';
import type { TableSchema } from '../api/schema-types';

// A key database (Redis, etcd) shows its keys through the key search: they
// are never listed, so the explorer doesn't call `list_objects` for it
// (Redis' walks the keyspace). Its driver's other kinds (Redis' search
// indexes, `index`) come from the database's schema read
// (`database_schema`), which has them without reading a key. No runtime
// imports, so `node --test` loads it directly (keyKinds.test.mjs).

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

/** What the explorer has of a key database's other kinds. A refresh keeps
 *  the last list while it reads (`loading` with items); a failed read says
 *  why (`error`), so "none" and "couldn't read" don't look the same. */
export interface OtherKinds {
  status: 'loading' | 'ready' | 'error';
  items: DbObject[];
  error: string | null;
}

/** The reads in flight, by database: only the latest one started for a
 *  database lands, and none started before its connection was forgotten
 *  (a disconnect, an edit or a delete), even when a new one has started
 *  since: the numbers never repeat. */
export class LatestRead {
  private last = 0;
  private at = new Map<string, number>();

  start(key: string): number {
    this.last += 1;
    this.at.set(key, this.last);
    return this.last;
  }

  current(key: string, n: number): boolean {
    return this.at.get(key) === n;
  }

  /** Every database whose key starts with `prefix` (a connection's). */
  forget(prefix: string) {
    for (const k of [...this.at.keys()]) if (k.startsWith(prefix)) this.at.delete(k);
  }
}

/** A Tauri command call (`invoke`), injected so the tests see what's read. */
export type Call = (cmd: string, args: Record<string, unknown>) => Promise<unknown>;

/**
 * Read a key database's other kinds into `state[key]`, from its schema
 * read (`database_schema`), never from `list_objects` (which walks the
 * keys). A reply that isn't the latest read for that database, or that
 * comes after its connection was forgotten, is dropped. Returns the error
 * of a failed read that landed (the store checks the connection with it),
 * else null.
 */
export async function readOtherKinds(o: {
  state: Record<string, OtherKinds>;
  latest: LatestRead;
  key: string;
  kinds: ObjectKindInfo[];
  connectionId: string;
  database: string;
  call: Call;
  errorText: (e: unknown) => string;
}): Promise<unknown> {
  const { state, latest, key } = o;
  const n = latest.start(key);
  const had = state[key];
  state[key] = { status: 'loading', items: had?.status === 'ready' || had?.status === 'loading' ? had.items : [], error: null };
  try {
    const tables = (await o.call('database_schema', { args: { connection_id: o.connectionId, database: o.database } })) as TableSchema[];
    if (latest.current(key, n)) state[key] = { status: 'ready', items: otherKindObjects(o.kinds, tables ?? []), error: null };
    return null;
  } catch (e) {
    if (!latest.current(key, n)) return null;
    state[key] = { status: 'error', items: [], error: o.errorText(e) };
    return e;
  }
}

/** One folder of a key database's other kinds, as the explorer shows it. */
export interface OtherKindFolder {
  kind: ObjectKindInfo;
  /** `empty`: read, and none of this kind. */
  status: 'loading' | 'ready' | 'empty' | 'error';
  items: DbObject[];
  error: string | null;
  /** The count by the folder's name: only once read. */
  count: number | undefined;
}

/** The folders of a key database's other kinds: one per kind the driver
 *  declares besides its keys, in its order, shown before the first one
 *  exists too (its menu has the kind's templates), and while the read runs
 *  or after it failed (with why). Nothing for a driver with keys only. */
export function otherKindFolders(kinds: ObjectKindInfo[] | undefined, entry: OtherKinds | undefined): OtherKindFolder[] {
  if (!kinds || !hasOtherKinds(kinds)) return [];
  return kinds.slice(1).map((kind) => {
    const items = (entry?.items ?? []).filter((x) => x.kind === kind.id);
    if (entry?.status === 'error') return { kind, status: 'error' as const, items: [], error: entry.error, count: undefined };
    // Not read yet, or read for the first time: nothing to show meanwhile.
    if (!entry || (entry.status === 'loading' && !entry.items.length)) return { kind, status: 'loading' as const, items: [], error: null, count: undefined };
    return { kind, status: items.length ? 'ready' as const : 'empty' as const, items, error: null, count: items.length };
  });
}

/** The separator "Mostrar en el explorador" splits an object's name by to
 *  open its namespace folders: a key's only (`user:1:cart` → user ›
 *  user:1); another kind (a search index called `idx:docs`) isn't nested. */
export function revealSeparator(d: Pick<DriverInfo, 'key_search' | 'object_kinds'> | undefined, kind: string): string | undefined {
  return d?.key_search && kind === d.object_kinds[0]?.id ? d.key_search.separator || undefined : undefined;
}
