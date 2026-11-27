// Column sort of the Monitor's tables, on the rows already read (every
// engine, no server query). Many cells come already formatted by the driver
// ("1.5 GB", "250ms", "45%"), so those go by what they measure.

export interface TableSort {
  column: string;
  descending: boolean;
}

const UNITS: Record<string, number> = {
  '': 1, '%': 1,
  // Bytes (the drivers count in powers of 1024, whichever way they write it).
  b: 1, bytes: 1, kb: 1024, kib: 1024, mb: 1024 ** 2, mib: 1024 ** 2, gb: 1024 ** 3, gib: 1024 ** 3,
  tb: 1024 ** 4, tib: 1024 ** 4, pb: 1024 ** 5, pib: 1024 ** 5,
  // Time, in seconds.
  ns: 1e-9, us: 1e-6, 'µs': 1e-6, ms: 1e-3, s: 1, m: 60, min: 60, h: 3600, d: 86400,
};
const MEASURE = /^\s*([+-]?(?:\d+(?:\.\d+)?|\.\d+)(?:e[+-]?\d+)?)\s*([a-zµ%]*)\s*$/i;

function measure(v: unknown): number | null {
  if (typeof v === 'number') return Number.isFinite(v) ? v : null;
  if (typeof v === 'boolean') return v ? 1 : 0;
  if (typeof v !== 'string') return null;
  const m = MEASURE.exec(v);
  if (!m) return null;
  const unit = UNITS[m[2].toLowerCase()];
  return unit === undefined ? null : Number(m[1]) * unit;
}

const isEmpty = (v: unknown) => v === null || v === undefined || v === '';
const text = (v: unknown) => (typeof v === 'object' ? JSON.stringify(v) : String(v));
const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

/** The rows sorted by one column (a new array). Empty cells go last either
 *  way, numbers before text, and equal rows keep their order. */
export function sortTable(rows: unknown[][], columns: string[], sort: TableSort | null): unknown[][] {
  const i = sort ? columns.indexOf(sort.column) : -1;
  if (!sort || i < 0) return rows;
  const sign = sort.descending ? -1 : 1;
  return rows
    .map((row) => ({ row, v: row[i], n: measure(row[i]) }))
    .sort((a, b) => {
      if (isEmpty(a.v) || isEmpty(b.v)) return Number(isEmpty(a.v)) - Number(isEmpty(b.v));
      if (a.n !== null && b.n !== null) return sign * (a.n - b.n);
      if (a.n !== null || b.n !== null) return sign * (a.n !== null ? -1 : 1);
      return sign * collator.compare(text(a.v), text(b.v));
    })
    .map((x) => x.row);
}

/** The sort after a click on a header: one way, the other, none. A numeric
 *  column starts with the biggest, as the Monitor's process list does. */
export function clickSort(sort: TableSort | null, column: string, numeric: boolean): TableSort | null {
  if (sort?.column !== column) return { column, descending: numeric };
  return sort.descending === numeric ? { column, descending: !numeric } : null;
}

/** Every cell with something in it measures. */
export function numericColumn(rows: unknown[][], index: number): boolean {
  const filled = rows.filter((r) => !isEmpty(r[index]));
  return filled.length > 0 && filled.every((r) => measure(r[index]) !== null);
}
