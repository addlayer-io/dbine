// Searching a result's loaded rows (the results pane's search bar, issue #142).
//
// The search looks at every loaded row, not only the ones the grid draws,
// so a result of millions of rows is searched in slices (`searchRows`
// yields between them) and keeps little per match: the rows that have one
// and how many of their cells do. Which cells those are is worked out again
// for the row at hand (`rowColumns`), which costs a few tests per row.
//
// NULL is not text: it never matches (the column filters have NULL / NOT
// NULL). Vector columns are left out by the caller: their cells show a
// preview, not the stored text.

import type { Cell } from '../api/types';

export interface SearchSpec {
  text: string;
  caseSensitive: boolean;
}

/** The test for one cell's text, or null when there's nothing to look for. */
export function matcher(spec: SearchSpec): RegExp | null {
  if (!spec.text) return null;
  return new RegExp(escapeRegExp(spec.text), spec.caseSensitive ? 'u' : 'iu');
}

function escapeRegExp(s: string) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/** A cell's text as the grid shows it (without its 500-character cut); null for NULL. */
export function cellText(v: Cell | undefined): string | null {
  if (v === null || v === undefined) return null;
  return typeof v === 'string' ? v : String(v);
}

/** What the search found so far. The arrays grow as rows are scanned; only
 *  the first `n` entries count. */
export interface Hits {
  /** The rows with a match (indexes into the searched rows), ascending. */
  rows: Uint32Array;
  /** How many of each such row's cells match. */
  counts: Uint16Array;
  /** Rows with a match. */
  n: number;
  /** Cells with a match. */
  cells: number;
  /** Cells with a match, per column (by column index). */
  perColumn: number[];
  /** Rows scanned. */
  scanned: number;
  /** Rows to scan. */
  total: number;
}

export function emptyHits(columns: number, total: number): Hits {
  return { rows: new Uint32Array(1024), counts: new Uint16Array(1024), n: 0, cells: 0, perColumn: new Array(columns).fill(0), scanned: 0, total };
}

function push(h: Hits, row: number, count: number) {
  if (h.n === h.rows.length) {
    const rows = new Uint32Array(h.rows.length * 2);
    rows.set(h.rows);
    h.rows = rows;
    const counts = new Uint16Array(h.counts.length * 2);
    counts.set(h.counts);
    h.counts = counts;
  }
  h.rows[h.n] = row;
  h.counts[h.n] = Math.min(count, 0xffff);
  h.n++;
}

/** Scans rows `from`..`to` (exclusive) of `rows`, looking at columns `cols`. */
export function scan(rows: Cell[][], cols: number[], re: RegExp, from: number, to: number, h: Hits) {
  for (let r = from; r < to; r++) {
    const row = rows[r];
    if (!row) continue;
    let count = 0;
    for (let k = 0; k < cols.length; k++) {
      const c = cols[k];
      const s = cellText(row[c]);
      if (s !== null && re.test(s)) {
        count++;
        h.perColumn[c]++;
      }
    }
    if (count) {
      push(h, r, count);
      h.cells += count;
    }
  }
  h.scanned = Math.max(h.scanned, to);
}

/** The columns of `row` that match (ascending). */
export function rowColumns(row: Cell[] | undefined, cols: number[], re: RegExp): number[] {
  if (!row) return [];
  return cols.filter((c) => {
    const s = cellText(row[c]);
    return s !== null && re.test(s);
  });
}

/** The positions (indexes into `h.rows`) of the rows whose column `c` matches. */
export function rowsWithColumn(rows: Cell[][], h: Hits, c: number, re: RegExp): Uint32Array {
  const out = new Uint32Array(h.perColumn[c] ?? 0);
  let n = 0;
  for (let k = 0; k < h.n && n < out.length; k++) {
    const s = cellText(rows[h.rows[k]]?.[c]);
    if (s !== null && re.test(s)) out[n++] = k;
  }
  return n === out.length ? out : out.slice(0, n);
}

/** Where a match is: `k` indexes `Hits.rows`, `c` is the column. */
export interface Spot {
  k: number;
  c: number;
}

/**
 * The next (`dir` 1) or previous (-1) matching cell after `from`, wrapping
 * around; the first (or last) one when `from` is null. `colsOf(k)` gives a
 * row's matching columns. Null when there are none.
 */
export function step(n: number, colsOf: (k: number) => number[], from: Spot | null, dir: 1 | -1): Spot | null {
  if (!n) return null;
  if (from) {
    const cols = colsOf(from.k);
    const at = cols.indexOf(from.c);
    const j = at < 0 ? (dir > 0 ? cols.findIndex((c) => c > from.c) : findLastIndex(cols, (c) => c < from.c)) : at + dir;
    if (j >= 0 && j < cols.length) return { k: from.k, c: cols[j] };
  }
  // The next rows, wrapping around; a row with no columns left (an edit
  // since the search) is skipped.
  const start = from ? from.k + dir : dir > 0 ? 0 : n - 1;
  for (let i = 0; i < n; i++) {
    const k = (((start + dir * i) % n) + n) % n;
    const cols = colsOf(k);
    if (cols.length) return { k, c: dir > 0 ? cols[0] : cols[cols.length - 1] };
  }
  return null;
}

function findLastIndex<T>(a: T[], f: (x: T) => boolean) {
  for (let i = a.length - 1; i >= 0; i--) if (f(a[i])) return i;
  return -1;
}

/** 1-based place of `spot` among all matching cells ("3 de 47"). */
export function placeOf(h: Hits, spot: Spot, colsOf: (k: number) => number[]): number {
  let before = 0;
  for (let k = 0; k < spot.k && k < h.n; k++) before += h.counts[k];
  return before + Math.max(0, colsOf(spot.k).indexOf(spot.c)) + 1;
}

/** `text` split around the matches, for highlighting. */
export function parts(text: string, re: RegExp | null): { s: string; hit: boolean }[] {
  if (!re) return [{ s: text, hit: false }];
  const g = new RegExp(re.source, re.flags.includes('g') ? re.flags : re.flags + 'g');
  const out: { s: string; hit: boolean }[] = [];
  let at = 0;
  for (const m of text.matchAll(g)) {
    const i = m.index ?? 0;
    if (!m[0]) break;
    if (i > at) out.push({ s: text.slice(at, i), hit: false });
    out.push({ s: m[0], hit: true });
    at = i + m[0].length;
  }
  if (at < text.length) out.push({ s: text.slice(at), hit: false });
  return out.length ? out : [{ s: text, hit: false }];
}

// -- "Solo filas con coincidencias": the grid shows some of the rows ----------------------------
// The pane keeps edits, deletions and new rows by index into all of its rows
// (`gridRows`); the filtered grid numbers its rows 0.. in what it shows. These
// map between the two, so an edit lands on the row it was made on.

/** The rows the filtered grid shows, as indexes into all the pane's rows:
 *  the matching result rows (`shown`, ascending), then the `added` new rows
 *  that follow the `base` result rows (always shown: they're being typed). */
export function filteredView(shown: ArrayLike<number>, base: number, added: number): number[] {
  const out = Array.from(shown);
  for (let k = 0; k < added; k++) out.push(base + k);
  return out;
}

/** Row `i` of all the pane's rows → its place in `view` (-1: not shown). */
export function placeIn(view: number[] | null, i: number): number {
  if (!view) return i;
  const p = lowerBound(view, i);
  return view[p] === i ? p : -1;
}

/** The row of all the pane's rows at place `p` of `view`. */
export function rowAt(view: number[] | null, p: number): number {
  return view ? view[p] ?? -1 : p;
}

/** Per-row data (edits) keyed by place in `view`; rows not shown are left out. */
export function keyedIn<T>(view: number[] | null, byRow: Record<number, T>): Record<number, T> {
  if (!view) return byRow;
  const out: Record<number, T> = {};
  for (const [k, x] of Object.entries(byRow)) {
    const p = placeIn(view, Number(k));
    if (p >= 0) out[p] = x;
  }
  return out;
}

/** A set of rows (deletions) as places in `view`. */
export function setIn(view: number[] | null, rows: Set<number>): Set<number> {
  if (!view) return rows;
  return new Set([...rows].map((i) => placeIn(view, i)).filter((p) => p >= 0));
}

/** First index of `a` (ascending) whose value is >= `x`. */
export function lowerBound(a: ArrayLike<number>, x: number): number {
  let lo = 0, hi = a.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (a[mid] < x) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

/** Waits for the next turn of the event loop without the 4 ms clamp of
 *  nested timers. */
function yieldNow(): Promise<void> {
  return new Promise((resolve) => {
    const ch = new MessageChannel();
    ch.port1.onmessage = () => {
      // An open port keeps Node's event loop (the tests) alive.
      ch.port1.close();
      resolve();
    };
    ch.port2.postMessage(null);
  });
}

/**
 * Searches `rows` in slices of about `sliceMs`, so the window keeps
 * responding; `onSlice` sees the hits after each one. Stops (returns null)
 * as soon as `cancelled()` says so.
 */
export async function searchRows(
  rows: Cell[][],
  columns: number,
  cols: number[],
  re: RegExp,
  opts: { cancelled: () => boolean; onSlice?: (h: Hits) => void; sliceMs?: number },
): Promise<Hits | null> {
  const h = emptyHits(columns, rows.length);
  const budget = opts.sliceMs ?? 12;
  let r = 0;
  while (r < rows.length) {
    const t0 = performance.now();
    while (r < rows.length && performance.now() - t0 < budget) {
      const to = Math.min(rows.length, r + 512);
      scan(rows, cols, re, r, to, h);
      r = to;
    }
    opts.onSlice?.(h);
    if (r < rows.length) {
      await yieldNow();
      if (opts.cancelled()) return null;
    }
  }
  return h;
}
