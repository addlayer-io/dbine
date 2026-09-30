import type { Cell, ObjectRef } from '../api/types';
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
}

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
  let pk = cols.filter((c) => c.primary_key).map((c) => c.name);
  if (!pk.length && columns.includes('_id')) pk = ['_id'];
  if (pk.length) {
    const missing = pk.filter((k) => !columns.includes(k));
    if (missing.length) return t('core:gridEdit.missingKey', { columns: missing.join(', ') });
    return { target: ref, keyColumns: pk, all: false, note: null };
  }
  return {
    target: ref,
    keyColumns: columns,
    all: true,
    note: t('core:gridEdit.noPrimaryKey'),
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

/** The edits as changes: key = original key values, set = new values. */
export function buildChanges(columns: string[], rows: Cell[][], edits: Edits, keyColumns: string[]): RowChange[] {
  return Object.entries(edits)
    .map(([r, cells]) => {
      const row = rows[Number(r)];
      const set = Object.entries(cells).map(([c, v]) => [columns[Number(c)], v] as [string, Cell]);
      const key = keyColumns.map((k) => [k, row[columns.indexOf(k)] ?? null] as [string, Cell]);
      return { key, set, row: columns.map((c, i) => [c, row[i] ?? null] as [string, Cell]) };
    })
    .filter((c) => c.set.length);
}
