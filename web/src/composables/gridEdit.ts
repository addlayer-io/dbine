import type { Cell, ObjectRef, ResultColumn } from '../api/types';
import { t } from '../i18n';
import { dbKey, useConnectionsStore } from '../stores/connections';

// Editing cells of a result: which table the rows come from, which columns
// identify a row, and the changes to turn into UPDATE code (the driver writes
// it: `update_script`). DBine never runs it: the code goes to the query.

/** Edited values: row index → column index → new value. */
export type Edits = Record<number, Record<number, Cell>>;

export interface RowChange {
  key: [string, Cell][];
  set: [string, Cell][];
  /** The whole original row (engines that rewrite rows whole use it). */
  row: [string, Cell][];
}

export interface EditSetup {
  target: ObjectRef;
  /** Columns in the WHERE: the primary key, or every column (`all`). */
  keyColumns: string[];
  all: boolean;
  note: string | null;
  /** Identity / auto-increment columns: a new row that sets one needs the
   *  engine's explicit-value switch (SQL Server's IDENTITY_INSERT…). */
  identity: string[];
}

/** A new row (or document): the values set, by column / field name. Columns
 *  left out take the table's default (or the engine's generated id). */
export type NewRow = [string, unknown][];

const IDENT = String.raw`(?:"(?:[^"]|"")+"|\[[^\]]+\]|` + '`[^`]+`' + String.raw`|[\p{L}_#@$][\p{L}\p{N}_#@$]*)`;
const QUALIFIED = new RegExp(String.raw`^(${IDENT})(?:\s*\.\s*(${IDENT}))?(?:\s*\.\s*(${IDENT}))?`, 'u');

function unquote(s: string) {
  if (/^".*"$/.test(s)) return s.slice(1, -1).replace(/""/g, '"');
  if (/^\[.*\]$/.test(s) || /^`.*`$/.test(s)) return s.slice(1, -1);
  return s;
}

/** Statements of a script, split on `;` outside quotes and comments. */
function statements(sql: string): string[] {
  const out: string[] = [];
  let cur = '';
  let q: string | null = null;
  for (let i = 0; i < sql.length; i++) {
    const ch = sql[i];
    if (q) {
      cur += ch;
      if (ch === q) q = null;
      continue;
    }
    if (ch === '-' && sql[i + 1] === '-') { const e = sql.indexOf('\n', i); i = e < 0 ? sql.length : e; cur += '\n'; continue; }
    if (ch === '/' && sql[i + 1] === '*') { const e = sql.indexOf('*/', i + 2); i = e < 0 ? sql.length : e + 1; cur += ' '; continue; }
    if (ch === "'" || ch === '"' || ch === '`' || ch === '[') { q = ch === '[' ? ']' : ch; cur += ch; continue; }
    if (ch === ';') { if (cur.trim()) out.push(cur.trim()); cur = ''; continue; }
    cur += ch;
  }
  if (cur.trim()) out.push(cur.trim());
  return out;
}

/**
 * The table a result comes from, when its statement is a plain
 * `SELECT … FROM one_table [WHERE / ORDER BY / LIMIT…]` (no joins, unions,
 * grouping), or a MongoDB `db.coll.find(…)`. `resultIndex` is the result set's
 * position among the script's results.
 */
export function inferTarget(script: string, resultIndex: number, language: string): { target: ObjectRef | null; reason: string | null } {
  const none = (reason: string) => ({ target: null, reason });
  if (language === 'json') {
    const m = /db\.(?:getCollection\(\s*["']([^"']+)["']\s*\)|([\p{L}_$][\p{L}\p{N}_$.-]*?))\.find(?:One)?\s*\(/u.exec(script)
      ?? /"find"\s*:\s*"([^"]+)"/.exec(script);
    const name = m?.[1] ?? m?.[2];
    return name ? { target: { kind: 'collection', schema: null, name }, reason: null } : none(t('core:gridEdit.notFind'));
  }
  if (language !== 'sql' && language !== 'cql') return none(t('core:gridEdit.engineCantEdit'));
  const selects = statements(script).filter((s) => /^\(?\s*(select|with)\b/i.test(s));
  const st = selects[resultIndex] ?? (selects.length === 1 ? selects[0] : undefined);
  if (!st) return none(t('core:gridEdit.queryNotFound'));
  if (/^\s*with\b/i.test(st)) return none(t('core:gridEdit.usesWith'));
  if (/\b(join|union|intersect|except|group\s+by|having|distinct)\b/i.test(st)) return none(t('core:gridEdit.combinesRows'));
  const from = /\bfrom\s+/i.exec(st);
  if (!from) return none(t('core:gridEdit.noTable'));
  const rest = st.slice(from.index + from[0].length);
  const m = QUALIFIED.exec(rest);
  if (!m) return none(t('core:gridEdit.tableUnknown'));
  // Another table after a comma (implicit join), or a subquery.
  const after = rest.slice(m[0].length);
  if (/^\s*(?:(?:as\s+)?[\p{L}_][\p{L}\p{N}_]*\s*)?,/iu.test(after) || rest.trimStart().startsWith('(')) {
    return none(t('core:gridEdit.combinesTables'));
  }
  const parts = [m[1], m[2], m[3]].filter(Boolean).map((p) => unquote(p!));
  const name = parts[parts.length - 1];
  const schema = parts.length >= 2 ? parts[parts.length - 2] : null;
  return { target: { kind: 'table', schema, name }, reason: null };
}

/**
 * How to identify rows of `target` given the result's `columns`: its primary
 * key (it has to be in the result), `_id` for documents, or else every
 * column.
 */
export async function resolveEditing(connectionId: string, database: string, target: ObjectRef, columns: string[]): Promise<EditSetup | string> {
  const conns = useConnectionsStore();
  await conns.loadObjects(connectionId, database);
  const objs = conns.objects[dbKey(connectionId, database)]?.items ?? [];
  const lower = target.name.toLowerCase();
  const found = objs.find((o) => o.name === target.name && (!target.schema || o.schema === target.schema))
    ?? objs.find((o) => o.name.toLowerCase() === lower && (!target.schema || (o.schema ?? '').toLowerCase() === target.schema!.toLowerCase()));
  const ref: ObjectRef = found ? { kind: found.kind, schema: found.schema, name: found.name } : target;
  const cols = await conns.loadColumns(connectionId, database, { ...ref, parent: found?.parent ?? null });
  const identity = cols.filter((c) => c.auto_increment).map((c) => c.name);
  let pk = cols.filter((c) => c.primary_key).map((c) => c.name);
  if (!pk.length && columns.includes('_id')) pk = ['_id'];
  if (pk.length) {
    const missing = pk.filter((k) => !columns.includes(k));
    if (missing.length) return t('core:gridEdit.missingKey', { columns: missing.join(', ') });
    return { target: ref, keyColumns: pk, all: false, note: null, identity };
  }
  return {
    target: ref,
    keyColumns: columns,
    all: true,
    note: t('core:gridEdit.noPrimaryKey'),
    identity,
  };
}

/** What the user typed as a value of the column's type. */
export function coerce(original: Cell, text: string): Cell {
  const t = text.trim();
  if (typeof original === 'number' && t !== '' && !Number.isNaN(Number(t))) return Number(t);
  if (typeof original === 'boolean') {
    if (/^(true|verdadero|1|sí|si)$/i.test(t)) return true;
    if (/^(false|falso|0|no)$/i.test(t)) return false;
  }
  return text;
}

export function sameValue(a: Cell, b: Cell) {
  return a === b || (a !== null && b !== null && String(a) === String(b) && typeof a === typeof b);
}

/** Column types whose values can't be matched with `=` in a WHERE, or come
 *  cut (binary is shown as `0x…` hex, up to 1 KiB): binary, LOBs, XML,
 *  spatial. SQL Server's legacy `text` and `timestamp` (rowversion) can't
 *  be compared either. Only the column's type decides: a text value that
 *  reads like hex (`'0xAB'`) is still compared, or the WHERE would match
 *  more rows than the one edited. */
const UNCOMPARABLE = /blob|binary|bytea|^bytes\b|^byte$|bindata|bytearray|octets|image|clob|ntext|xml|geometry|geography|^raw$|long raw|hierarchyid|sql_variant/i;

function comparableType(col: ResultColumn | undefined, dialect: string) {
  const ty = col?.type_name ?? '';
  // SQL Server's `timestamp` is rowversion (binary), not a date.
  return !UNCOMPARABLE.test(ty) && !((dialect === 'mssql' || dialect === 'sybase') && /^(text|timestamp|rowversion)$/i.test(ty));
}

/** Columns left out of an all-columns WHERE (their values can't be
 *  compared): exactly the ones `rowKey` leaves out. */
export function skippedKeyColumns(columns: ResultColumn[], setup: EditSetup, dialect: string): string[] {
  if (!setup.all) return [];
  const byName = new Map(columns.map((c) => [c.name, c]));
  return setup.keyColumns.filter((k) => !comparableType(byName.get(k), dialect));
}

/**
 * How the WHERE finds a row: its key values (NULLs stay NULL: the driver
 * writes `IS NULL`). With every column as the key, binary and large values
 * are left out; a primary key with one of them can't identify the row
 * (throws, with the reason).
 */
export function rowKey(columns: ResultColumn[], row: Cell[], setup: EditSetup, dialect = ''): [string, Cell][] {
  const names = columns.map((c) => c.name);
  const key: [string, Cell][] = [];
  for (const k of setup.keyColumns) {
    const i = names.indexOf(k);
    const v = row[i] ?? null;
    if (comparableType(columns[i], dialect)) key.push([k, v]);
    else if (!setup.all) throw new Error(t('core:gridEdit.keyNotComparable', { column: k }));
  }
  if (!key.length) throw new Error(t('core:gridEdit.noComparableColumns'));
  return key;
}

/** The edits as changes: key = original key values, set = new values. Rows
 *  in `skip` (marked for deletion) are left out. */
export function buildChanges(columns: ResultColumn[], rows: Cell[][], edits: Edits, setup: EditSetup, dialect = '', skip: Set<number> = new Set()): RowChange[] {
  const names = columns.map((c) => c.name);
  return Object.entries(edits)
    .filter(([r]) => !skip.has(Number(r)))
    .map(([r, cells]) => {
      const row = rows[Number(r)];
      const set = Object.entries(cells).map(([c, v]) => [names[Number(c)], v] as [string, Cell]);
      return { key: rowKey(columns, row, setup, dialect), set, row: names.map((c, i) => [c, row[i] ?? null] as [string, Cell]) };
    })
    .filter((c) => c.set.length);
}

/** Rows added in the grid as new rows: only the cells the user set (an
 *  untouched cell takes the column's default). Rows with nothing set are
 *  left out. */
export function buildInserts(columns: ResultColumn[], added: Record<number, Cell>[]): NewRow[] {
  return added
    .map((cells) => Object.entries(cells).map(([c, v]) => [columns[Number(c)]?.name, v] as [string, Cell]).filter(([n]) => n !== undefined))
    .filter((r) => r.length);
}

/** What the user typed in a new row's cell, as a value of the column's type
 *  (a new row has no original value to take the type from). */
export function coerceNew(typeName: string, text: string): Cell {
  const v = text.trim();
  const ty = typeName.toLowerCase();
  if (/^(bit|bool|boolean)\b/.test(ty)) {
    if (/^(true|verdadero|1|sí|si)$/i.test(v)) return true;
    if (/^(false|falso|0|no)$/i.test(v)) return false;
  }
  if (/(int|dec|numeric|number|float|double|real|money|serial)/.test(ty) && v !== '' && !Number.isNaN(Number(v))) return Number(v);
  return text;
}
