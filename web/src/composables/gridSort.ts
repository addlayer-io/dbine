// Column sort of the result grid (a click on a header). A table's data asks
// the driver to sort on the server (`filtered_browse_query` with `sort`);
// engines that can't, and the results of a query, sort the loaded rows here.

import type { Cell, ResultColumn, SortKey } from '../api/types';
import { columnKind, type ColumnKind } from './gridFilter';
import { compareVectors, parseVector, shownVectorMeta } from './vector';

export type { SortKey };

/**
 * The sort after a click on `column`'s header: ascending → descending →
 * none. A plain click makes it the only key; with `add` (Shift+click) it
 * becomes, or stays, one more key after the others.
 */
export function nextSort(keys: SortKey[], column: string, add: boolean): SortKey[] {
  const at = keys.findIndex((k) => k.column === column);
  const cur = at >= 0 ? keys[at] : null;
  const next: SortKey | null = !cur ? { column, descending: false } : !cur.descending ? { column, descending: true } : null;
  if (!add) return next ? [next] : [];
  if (at < 0) return next ? [...keys, next] : keys;
  return next ? keys.map((k, i) => (i === at ? next : k)) : keys.filter((_, i) => i !== at);
}

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

function asNumber(v: Cell): number | null {
  if (typeof v === 'number') return v;
  if (typeof v === 'boolean') return v ? 1 : 0;
  if (typeof v === 'string' && v.trim() !== '') {
    const n = Number(v);
    return Number.isFinite(n) ? n : null;
  }
  return null;
}

/** Two non-null cells of a column of `kind`, ascending. */
function compareValues(a: Cell, b: Cell, kind: ColumnKind): number {
  if (kind === 'number' || kind === 'bool' || (typeof a === 'number' && typeof b === 'number')) {
    const x = asNumber(a);
    const y = asNumber(b);
    if (x !== null && y !== null) return x - y;
    // A number before text that isn't one (NaN, "Infinity"…).
    if (x !== null) return -1;
    if (y !== null) return 1;
  }
  if (kind === 'date') {
    const x = Date.parse(String(a));
    const y = Date.parse(String(b));
    if (!Number.isNaN(x) && !Number.isNaN(y) && x !== y) return x - y;
  }
  return collator.compare(String(a), String(b));
}

/**
 * The rows sorted by `keys` (a new array; the rows are the same objects).
 * NULLs go last in either direction; equal rows keep their order. Vector
 * columns sort number by number (each cell read once); a cell that isn't a
 * vector goes after the vectors, by its text.
 */
export function sortRows(rows: Cell[][], columns: ResultColumn[], keys: SortKey[]): Cell[][] {
  type Key = { i: number; desc: boolean; kind: ColumnKind; vectors: (number[] | null)[] | null };
  const by = keys
    .map((k): Key | null => {
      const i = columns.findIndex((c) => c.name === k.column);
      if (i < 0) return null;
      const sample = rows.find((r) => r[i] !== null)?.[i];
      const kind = shownVectorMeta(columns[i]) ? 'vector' : columnKind(columns[i].type_name, sample);
      return { i, desc: k.descending, kind, vectors: kind === 'vector' ? rows.map((r) => parseVector(r[i])) : null };
    })
    .filter((k): k is Key => k !== null);
  if (!by.length) return rows.slice();
  return rows
    .map((r, n) => ({ r, n }))
    .sort((p, q) => {
      for (const k of by) {
        const a = p.r[k.i];
        const b = q.r[k.i];
        if (a === null || b === null) {
          if (a === b) continue;
          return a === null ? 1 : -1;
        }
        const x = k.vectors?.[p.n];
        const y = k.vectors?.[q.n];
        const c = x && y ? compareVectors(x, y) : x ? -1 : y ? 1 : compareValues(a, b, k.kind);
        if (c) return k.desc ? -c : c;
      }
      return p.n - q.n;
    })
    .map((x) => x.r);
}
