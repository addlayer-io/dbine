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

/** The key a compare of two results most likely wants (indexes into
 *  `cols`): a DAX side's group-by columns; else an id-like column alone;
 *  else the columns that describe rather than measure; when none looks
 *  like a measure, all but the last (the usual `SELECT dims…, measure`). */
export function suggestKey(cols: KeyHintColumn[]): number[] {
  if (!cols.length) return [];
  const all = cols.map((_, i) => i);
  for (const side of ['left', 'right'] as const) {
    const names = cols.map((c) => c[side]);
    if (names.some((n) => DAX_MEASURE.test(n)) && names.some((n) => DAX_COLUMN.test(n))) {
      return all.filter((i) => DAX_COLUMN.test(names[i]));
    }
  }
  const measures = cols.map((c) => measureLike(c.leftType, c.leftSample) || measureLike(c.rightType, c.rightSample));
  const id = all.find((i) => !measures[i] && (idLike(cols[i].left) || idLike(cols[i].right)));
  if (id !== undefined) return [id];
  const dims = all.filter((i) => !measures[i]);
  if (!dims.length) return [0];
  if (dims.length < cols.length) return dims;
  return cols.length > 1 ? all.slice(0, -1) : [0];
}

/** The column table for two results just read: every left column with its
 *  pair, the suggested key marked. */
export function pairRows(left: ResultColumn[], right: ResultColumn[], leftSample?: Cell[] | null, rightSample?: Cell[] | null): PairRow[] {
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
  for (const k of suggestKey(hint)) paired[k].r.key = true;
  return rows;
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

/** What a side's columns were read for: reading them again is needed once
 *  its connection, database or query changes. */
export function sideSignature(s: { connectionId: string; database: string; query: string }): string {
  return [s.connectionId, s.database, s.query.trim()].join('\u0001');
}
