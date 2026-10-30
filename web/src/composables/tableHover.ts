import type { DbObject, Language, ObjectKindInfo } from '../api/types';
import { KEYWORDS, aliasesIn, chainAt, findObjects, nameAt } from './nameRefs';
import { lexRegions, lexSyntax, maskCode, regionAt } from './sqlLexer';

// Hovering a table's name in the editor shows its columns (CodeEditor's
// `tableHover`). This finds which table the name under the pointer stands
// for, with the same name reading as ⌘/Ctrl+click (composables/nameRefs.ts):
// quoted identifiers, `schema.table`, an alias's table and, on engines whose
// queries read other databases, `db.table` / `db.schema.table`.

/** What the editor knows of the tab's database and the connection's others. */
export interface HoverIndex {
  language: Language;
  dialect: string;
  database: string;
  objects: DbObject[];
  kinds: ObjectKindInfo[];
  /** The connection's other databases (cross-database engines only): each
   *  one's objects, null while not loaded. */
  databases?: Record<string, DbObject[] | null>;
}

/** The table a name stands for. `object` is null when it's in another
 *  database whose objects aren't loaded yet: `path` is then the rest of the
 *  name (`[table]` or `[schema, table]`) to look for once they are. */
export interface TableRef {
  from: number;
  to: number;
  database: string;
  object: DbObject | null;
  path: string[];
}

const eq = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();

/** Objects with columns (tables, views, collections) named `path` in `objects`. */
export function tableNamed(idx: HoverIndex, database: string, objects: DbObject[], path: string[]): DbObject | null {
  const found = findObjects({ ...idx, database, objects, schemas: [], columns: () => null }, path);
  return found.find((o) => idx.kinds.find((k) => k.id === o.kind)?.has_columns) ?? null;
}

/** A database of the connection named `name`: the exact spelling first. */
function databaseNamed(idx: HoverIndex, name: string): string | null {
  const names = Object.keys(idx.databases ?? {});
  return names.find((d) => d === name) ?? names.find((d) => eq(d, name)) ?? null;
}

/** The table (view, collection) named at `pos` in `doc`, or null: not a
 *  name, a keyword, a column, a schema or a database, or nothing known by
 *  that name. */
export function tableAt(idx: HoverIndex, doc: string, pos: number): TableRef | null {
  const sqlLike = idx.language === 'sql' || idx.language === 'cql';
  if (!sqlLike) {
    // mongosh, HTTP consoles, Cypher…: the word the explorer knows.
    const hit = idx.objects.length ? nameAt({ ...idx, schemas: [], columns: () => null }, doc, pos) : null;
    if (!hit || !idx.kinds.find((k) => k.id === hit.object.kind)?.has_columns) return null;
    return { from: hit.from, to: hit.to, database: idx.database, object: hit.object, path: [hit.object.name] };
  }
  const syn = lexSyntax(idx.language, idx.dialect);
  const kind = regionAt(lexRegions(doc, syn), pos);
  if (kind === 'string' || kind === 'comment') return null;
  const chain = chainAt(doc, pos);
  if (!chain) return null;
  const { parts, k } = chain;
  const at = parts[k];
  // A word of the language (`SELECT`, `FROM`, `JOIN`) is never a table
  // unless quoted.
  if (!at.quoted && parts.length === 1 && KEYWORDS.has(at.text.toLowerCase())) return null;
  let path = parts.slice(0, k + 1).map((p) => p.text);
  // An alias stands for its table, even when a table has the alias's name
  // (`FROM orders customers`: `customers` is `orders` here).
  if (k === 0) {
    const via = aliasesIn(maskCode(doc, syn)).get(path[0].toLowerCase());
    if (via) path = via;
  }
  const ref = (database: string, object: DbObject | null, rest: string[]): TableRef => ({ from: at.from, to: at.to, database, object, path: rest });
  const here = tableNamed(idx, idx.database, idx.objects, path);
  if (here) return ref(idx.database, here, path);
  // `db.table`, `db.schema.table`: another database of the connection.
  if (path.length >= 2 && path.length <= 3 && idx.databases) {
    const db = databaseNamed(idx, path[0]);
    if (db === null) return null;
    const rest = path.slice(1);
    if (db === idx.database) {
      const own = tableNamed(idx, db, idx.objects, rest);
      return own ? ref(db, own, rest) : null;
    }
    const objs = idx.databases[db];
    if (objs === null || objs === undefined) return ref(db, null, rest);
    const there = tableNamed(idx, db, objs, rest);
    return there ? ref(db, there, rest) : null;
  }
  return null;
}

// -- the popup's content ----------------------------------------------------------------------

/** One column as the popup shows it. */
export interface HoverColumn {
  name: string;
  data_type: string;
  /** Null: the engine doesn't say. */
  nullable: boolean | null;
  primary_key: boolean;
  comment: string | null;
}

export type TableHoverState =
  | { status: 'loading' }
  | { status: 'ready'; columns: HoverColumn[]; comment: string | null }
  | { status: 'empty' }
  | { status: 'error'; message: string };

/** What the editor asks for a name: where it is, its title and its columns
 *  (`watch` calls back with each new state until the returned stop). */
export interface TableHoverTarget {
  from: number;
  to: number;
  title: string;
  watch: (cb: (s: TableHoverState) => void) => () => void;
}
