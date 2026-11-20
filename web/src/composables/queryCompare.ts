import type { Cell, ResultColumn } from '../api/types';
import type { ColumnPair, QueryOptions } from '../api/dataCompare';
import type { ComparePicks } from '../stores/tabs';

// Data compare of two queries (docs/data-compare.md, "Comparing two
// queries"): pairing the columns of two results that come from different
// engines (DAX's `Date[Year]` and `[Total Sales]` against SQL's `year` and
// `total_sales`), suggesting the key, and the options as the backend takes
// them. Pure functions: tested by queryCompare.test.mjs.

/** One row of the column table: a left column, the right one it's compared
 *  with ('' = none) and whether it's part of the key. */
export interface PairRow { left: string; right: string; key: boolean }

/** The options as the screen holds them: the relative tolerance in percent. */
export interface ToleranceForm { absolute: number; relativePct: number; nullAsBlank: boolean; ignoreCase: boolean; trim: boolean }

export const DEFAULT_FORM: ToleranceForm = { absolute: 0, relativePct: 0, nullAsBlank: false, ignoreCase: false, trim: false };

/** A column's name reduced for matching across engines: the part inside a
 *  DAX name's last brackets (`Date[Year]` → year, `[Total Sales]` → total
 *  sales), the last part of a qualified name, without accents, case,
 *  spaces or punctuation. */
export function bareName(name: string): string {
  const bracket = /\[([^\]]*)\]\s*$/.exec(name);
  let s = bracket ? bracket[1] : name;
  if (!bracket) {
    const dot = s.lastIndexOf('.');
    if (dot >= 0 && dot < s.length - 1) s = s.slice(dot + 1);
  }
  return s.normalize('NFD').replace(/[̀-ͯ]/g, '').toLowerCase().replace(/[^\p{L}\p{N}]+/gu, '');
}

/** The right column for each left one: the same name (ignoring case), then
 *  the same bare name ([`bareName`]); the columns still alone pair in
 *  order when both sides have as many left. '' = no pair. */
export function pairColumns(left: string[], right: string[]): string[] {
  const out: string[] = left.map(() => '');
  const used = new Set<number>();
  const take = (i: number, match: (r: string) => boolean) => {
    const j = right.findIndex((r, k) => !used.has(k) && match(r));
    if (j >= 0) { used.add(j); out[i] = right[j]; }
  };
  left.forEach((l, i) => take(i, (r) => r === l));
  left.forEach((l, i) => { if (!out[i]) take(i, (r) => r.toLowerCase() === l.toLowerCase()); });
  left.forEach((l, i) => {
    const b = bareName(l);
    if (!out[i] && b) take(i, (r) => bareName(r) === b);
  });
  const lonelyLeft = left.map((_, i) => i).filter((i) => !out[i]);
  const lonelyRight = right.map((_, j) => j).filter((j) => !used.has(j));
  if (lonelyLeft.length && lonelyLeft.length === lonelyRight.length) {
    lonelyLeft.forEach((i, n) => { out[i] = right[lonelyRight[n]]; });
  }
  return out;
}

/** DAX's group-by columns come as `Table[Column]` and its measures as
 *  `[Measure]`. */
const DAX_COLUMN = /^.+\[[^\]]+\]$/;
const DAX_MEASURE = /^\[[^\]]+\]$/;
/** Types that measure rather than describe. */
const MEASURE_TYPE = /decimal|numeric|money|currency|float|double|real|number/i;
/** A name that identifies a row: `id`, `code`…, or one ending in `_id`,
 *  `Id`, `ID` or `Key` (`customer_id`, `CustomerKey`; not `valid`). */
function idLike(name: string): boolean {
  const inner = /\[([^\]]*)\]\s*$/.exec(name)?.[1] ?? name;
  return /^(id|uuid|guid|key|code|codigo|clave)$/.test(bareName(name)) || /(?:_id|_key|[a-z]Id|ID|[a-z]Key)$/.test(inner);
}

/** A column that looks like a measure: a fractional type, or (without a
 *  type) a fractional value. */
function measureLike(type: string, sample: Cell | undefined): boolean {
  if (type) return MEASURE_TYPE.test(type) && !/int/i.test(type);
  if (typeof sample === 'number') return !Number.isInteger(sample);
  return typeof sample === 'string' && /^-?\d+\.\d+$/.test(sample.trim());
}

/** What the key suggestion looks at in each compared column. */
export interface KeyHintColumn { left: string; right: string; leftType: string; rightType: string; leftSample?: Cell; rightSample?: Cell }

/** Why the column table's key is what it is: the line under the table. */
export type KeySource =
  | { kind: 'model' }
  | { kind: 'id'; column: string }
  | { kind: 'grouped' }
  | { kind: 'none' }
  | { kind: 'tableCompare' }
  | { kind: 'primaryKey'; table: string }
  | { kind: 'unique'; table: string; index: string };

/** The key a compare of two results most likely wants (indexes into
 *  `cols`) and why: a DAX side's group-by columns; else an id-like column
 *  alone; else, when a query groups (`grouped`), the columns that describe
 *  rather than measure, or all but the last when none looks like a measure
 *  (the usual `SELECT dims…, total`). Otherwise none: a table's rows have
 *  no reliable hint, and a guessed key of almost every column would only
 *  find missing rows, never changed ones. (DataCompareView first tries the
 *  key of the one table a query reads, from the catalog.) */
export function keySuggestion(cols: KeyHintColumn[], grouped = false): { keys: number[]; source: KeySource } {
  const none = { keys: [], source: { kind: 'none' as const } };
  if (!cols.length) return none;
  const all = cols.map((_, i) => i);
  for (const side of ['left', 'right'] as const) {
    const names = cols.map((c) => c[side]);
    if (names.some((n) => DAX_MEASURE.test(n)) && names.some((n) => DAX_COLUMN.test(n))) {
      return { keys: all.filter((i) => DAX_COLUMN.test(names[i])), source: { kind: 'model' } };
    }
  }
  const measures = cols.map((c) => measureLike(c.leftType, c.leftSample) || measureLike(c.rightType, c.rightSample));
  const id = all.find((i) => !measures[i] && (idLike(cols[i].left) || idLike(cols[i].right)));
  if (id !== undefined) return { keys: [id], source: { kind: 'id', column: cols[id].left } };
  if (!grouped) return none;
  const dims = all.filter((i) => !measures[i]);
  const keys = !dims.length ? [0] : dims.length < cols.length ? dims : cols.length > 1 ? all.slice(0, -1) : [0];
  return { keys, source: { kind: 'grouped' } };
}

/** The suggested key's indexes (see `keySuggestion`). */
export function suggestKey(cols: KeyHintColumn[], grouped = false): number[] {
  return keySuggestion(cols, grouped).keys;
}

/** The column table for two results just read, every left column with its
 *  pair and the suggested key marked, and why that key. */
export function columnTable(left: ResultColumn[], right: ResultColumn[], leftSample?: Cell[] | null, rightSample?: Cell[] | null, grouped = false): { rows: PairRow[]; source: KeySource } {
  const rights = pairColumns(left.map((c) => c.name), right.map((c) => c.name));
  const rows: PairRow[] = left.map((c, i) => ({ left: c.name, right: rights[i], key: false }));
  const paired = rows.map((r, i) => ({ r, i })).filter(({ r }) => r.right);
  const hint = paired.map(({ r, i }) => {
    const j = right.findIndex((c) => c.name === r.right);
    return {
      left: r.left, right: r.right, leftType: left[i].type_name, rightType: right[j]?.type_name ?? '',
      leftSample: leftSample?.[i], rightSample: j >= 0 ? rightSample?.[j] : undefined,
    };
  });
  const { keys, source } = keySuggestion(hint, grouped);
  for (const k of keys) paired[k].r.key = true;
  return { rows, source };
}

/** The column table's rows (see `columnTable`). */
export function pairRows(left: ResultColumn[], right: ResultColumn[], leftSample?: Cell[] | null, rightSample?: Cell[] | null, grouped = false): PairRow[] {
  return columnTable(left, right, leftSample, rightSample, grouped).rows;
}

/** The key's columns, in order, as one string: whether the user changed
 *  the key since it was suggested. */
export function keySignature(rows: PairRow[]): string {
  return rows.filter((r) => r.key && r.right).map((r) => r.left).join('\u0001');
}

/** The column table after reading the columns again (a query changed): a
 *  left column still there keeps the user's pair and key, when its right
 *  column is still there; new columns come as suggested. */
export function mergePairs(before: PairRow[], fresh: PairRow[], right: ResultColumn[]): PairRow[] {
  if (!before.length) return fresh;
  const names = new Set(right.map((c) => c.name));
  const rows = fresh.map((f) => {
    const b = before.find((x) => x.left === f.left);
    return b && (!b.right || names.has(b.right)) ? { row: { ...b }, kept: true } : { row: { ...f }, kept: false };
  });
  // A right column a kept row took isn't a suggestion for another row as
  // well, wherever that row comes: the user's pair stays, the suggestion goes.
  const taken = new Set(rows.filter((r) => r.kept && r.row.right).map((r) => r.row.right));
  return rows.map(({ row, kept }) => (!kept && row.right && taken.has(row.right) ? { ...row, right: '', key: false } : row));
}

/** A data compare tab's picks for "Comparar con…": this query on the left,
 *  the right side to choose. */
export function compareWithPicks(connectionId: string, database: string, query: string): ComparePicks {
  return {
    left: { connectionId, database },
    right: { connectionId, database },
    mode: 'queries',
    queries: { left: { connectionId, database, query }, right: { connectionId: '', database: '', query: '' } },
  };
}

/** The pairs the backend compares: those with a right column. */
export function comparedPairs(rows: PairRow[]): ColumnPair[] {
  return rows.filter((r) => r.right).map((r) => ({ left: r.left, right: r.right, key: r.key }));
}

/** The right columns no left one is paired with. */
export function unpairedRight(rows: PairRow[], right: ResultColumn[]): string[] {
  const used = new Set(rows.map((r) => r.right).filter(Boolean));
  return right.map((c) => c.name).filter((n) => !used.has(n));
}

/** The form's options as `data_compare_query` takes them: blank or
 *  negative tolerances are 0, the relative one a fraction. */
export function queryOptions(f: ToleranceForm): QueryOptions {
  const n = (x: number) => (Number.isFinite(x) && x > 0 ? x : 0);
  return { absolute: n(f.absolute), relative: n(f.relativePct) / 100, null_as_blank: f.nullAsBlank, ignore_case: f.ignoreCase, trim: f.trim };
}

/** The row count `browse_query` is asked for to read a table whole
 *  (`u32::MAX`): the drivers' own convention for it (`read_via_execute`). */
export const WHOLE_ROWS = 4294967295;

const W = String(WHOLE_ROWS);
/** The limit clauses the drivers end a whole-table browse with… */
const WHOLE_TAILS = [`LIMIT ${W}`, `FETCH FIRST ${W} ROWS ONLY`, `FETCH NEXT ${W} ROWS ONLY`];
/** …the ones they put right after its leading SELECT… */
const WHOLE_HEADS = [`TOP (${W}) `, `TOP ${W} `, `FIRST ${W} `];
/** …MongoDB's, and Flux's step (a line of its own). */
const WHOLE_MONGO = `.limit(${W})`;
const WHOLE_FLUX = `|> limit(n: ${W})`;
const isSpace = (c: string | undefined) => c === ' ' || c === '\t' || c === '\n' || c === '\r';

/** A table's browse query asked for `WHOLE_ROWS` rows, as a query to start
 *  from ("Pasar a consultas", issue #118): without its limit clause when
 *  it's one of the usual ones (LIMIT, TOP, FETCH FIRST, FIRST, MongoDB's
 *  `.limit()`, Flux's `limit()`); any other stays as the driver wrote it,
 *  which reads that many rows. Only where the drivers write the clause
 *  (the end, or right after the leading SELECT) and with fixed-text checks:
 *  a table name is the database's own text, so it's never searched (a long
 *  run of spaces in one made a backtracking regex hang the window). */
export function wholeQuery(browse: string): string {
  let q = browse.split('\n').filter((line) => line.trim() !== WHOLE_FLUX).join('\n');
  // The end: `… LIMIT n`, `… FETCH FIRST n ROWS ONLY`, `….limit(n)`, before a final `;`.
  let end = q.trimEnd();
  const semi = end.endsWith(';') ? ';' : '';
  if (semi) end = end.slice(0, -1).trimEnd();
  // Cut first, then compare without case: uppercasing can change a text's
  // length (ﬁ is FI, ß is SS), so the cut is always measured on the original.
  const sameText = (part: string, clause: string) => part.length === clause.length && part.toUpperCase() === clause;
  const tail = WHOLE_TAILS.find((t) => sameText(end.slice(-t.length), t) && isSpace(end[end.length - t.length - 1]));
  if (tail) q = end.slice(0, end.length - tail.length).trimEnd() + semi;
  else if (end.endsWith(WHOLE_MONGO)) q = end.slice(0, end.length - WHOLE_MONGO.length) + semi;
  // The start: `SELECT TOP (n) …`, `SELECT FIRST n …`, on the first line that begins with SELECT.
  const lines = q.split('\n');
  const i = lines.findIndex((line) => sameText(line.trimStart().slice(0, 7), 'SELECT '));
  if (i >= 0) {
    const line = lines[i];
    const at = line.length - line.trimStart().length + 7;
    const head = WHOLE_HEADS.find((h) => sameText(line.slice(at, at + h.length), h));
    if (head) lines[i] = line.slice(0, at) + line.slice(at + head.length);
  }
  return lines.join('\n');
}

/** The column table with the table compare's key (issue #118): the left
 *  columns named as the key (ignoring case) are the key and no others. If
 *  a key column isn't among the paired ones, the rows stay as they were. */
export function withKey(rows: PairRow[], key: string[]): PairRow[] {
  const names = new Set(key.map((k) => k.toLowerCase()));
  const found = rows.filter((r) => r.right && names.has(r.left.toLowerCase()));
  if (!key.length || found.length !== names.size) return rows;
  return rows.map((r) => ({ ...r, key: !!r.right && names.has(r.left.toLowerCase()) }));
}

/** What a side's columns were read for: reading them again is needed once
 *  its connection, database or query changes. */
export function sideSignature(s: { connectionId: string; database: string; query: string }): string {
  return [s.connectionId, s.database, s.query.trim()].join('\u0001');
}
