import type { Cell } from '../api/types';
import { rt } from './i18nLabels';

// Column filters of the data grid (the row under the headers). What's typed
// becomes structured filters the driver turns into its own WHERE / filter
// (`filtered_browse_query`); engines that can't filter on the server get
// them applied to the loaded rows (`matchesLocal`).

export type FilterOp =
  | 'eq' | 'ne' | 'gt' | 'ge' | 'lt' | 'le'
  | 'contains' | 'not_contains' | 'starts_with' | 'ends_with'
  | 'is_null' | 'not_null' | 'is_empty' | 'not_empty'
  | 'in' | 'not_in'
  | 'is_true' | 'is_false' | 'true_or_null' | 'false_or_null'
  | 'sql' | 'sql_right';

export interface ColumnFilter {
  column: string;
  op: FilterOp;
  values: Cell[];
  sql?: string | null;
}

/** A column's filter as the grid keeps it: what's shown in the box, and the
 *  filters it means. */
export interface FilterState {
  text: string;
  filters: ColumnFilter[];
}

export type ColumnKind = 'text' | 'number' | 'date' | 'bool';

/** The kind of a column from its engine type name (or its values). */
export function columnKind(typeName: string, sample?: Cell): ColumnKind {
  const t = typeName.toLowerCase();
  if (/^(bit|bool|boolean)\b/.test(t)) return 'bool';
  if (/(int|dec|numeric|number|float|double|real|money|serial)/.test(t)) return 'number';
  if (/(date|time)/.test(t)) return 'date';
  if (!t) {
    if (typeof sample === 'number') return 'number';
    if (typeof sample === 'boolean') return 'bool';
  }
  return 'text';
}

function value(kind: ColumnKind, s: string): Cell {
  const t = s.trim();
  if (kind === 'number' && t !== '' && !Number.isNaN(Number(t))) return Number(t);
  if (kind === 'bool') {
    if (/^(true|1|sí|si|verdadero)$/i.test(t)) return true;
    if (/^(false|0|no|falso)$/i.test(t)) return false;
  }
  return t;
}

/**
 * What's typed in a filter box, as filters:
 * - `NULL` / `NOT NULL` (also `EMPTY` / `NOT EMPTY` for text);
 * - `=x`, `<>x` or `!=x`, `>x`, `>=x`, `<x`, `<=x`;
 * - `a,b,c` (numbers) or `=a,b,c` → any of them;
 * - text: plain → contains, `!x` → doesn't contain, `^x` → starts with,
 *   `x$` → ends with;
 * - numbers / booleans: plain → equals;
 * - dates: a plain `YYYY-MM-DD` → that whole day.
 */
export function parseFilter(column: string, kind: ColumnKind, raw: string): ColumnFilter[] {
  const text = raw.trim();
  if (!text) return [];
  const f = (op: FilterOp, values: Cell[] = []): ColumnFilter => ({ column, op, values });
  const up = text.toUpperCase();
  if (up === 'NULL') return [f('is_null')];
  if (up === 'NOT NULL' || up === '!NULL') return [f('not_null')];
  if (kind === 'text' && up === 'EMPTY') return [f('is_empty')];
  if (kind === 'text' && up === 'NOT EMPTY') return [f('not_empty')];
  const ops: [string, FilterOp][] = [['>=', 'ge'], ['<=', 'le'], ['<>', 'ne'], ['!=', 'ne'], ['>', 'gt'], ['<', 'lt'], ['=', 'eq']];
  for (const [p, op] of ops) {
    if (text.startsWith(p)) {
      const rest = text.slice(p.length).trim();
      if (op === 'eq' && rest.includes(',')) return [f('in', rest.split(',').map((x) => value(kind, x)))];
      return [f(op, [value(kind, rest)])];
    }
  }
  if (kind === 'bool') {
    const b = value('bool', text);
    return [b === true ? f('is_true') : b === false ? f('is_false') : f('eq', [text])];
  }
  if (kind === 'number') {
    if (text.includes(',')) return [f('in', text.split(',').map((x) => value(kind, x)))];
    return [f('eq', [value(kind, text)])];
  }
  if (kind === 'date' && /^\d{4}-\d{2}-\d{2}$/.test(text)) {
    const next = new Date(`${text}T00:00:00Z`);
    next.setUTCDate(next.getUTCDate() + 1);
    return [f('ge', [text]), f('lt', [next.toISOString().slice(0, 10)])];
  }
  if (text.startsWith('!')) return [f('not_contains', [text.slice(1)])];
  if (text.startsWith('^')) return [f('starts_with', [text.slice(1)])];
  if (text.endsWith('$') && text.length > 1) return [f('ends_with', [text.slice(0, -1)])];
  return [f('contains', [text])];
}

/** The menu entries of a column (the ⋮ in its filter box). `ask` means it
 *  needs a value first. */
export interface FilterMenuEntry {
  label: string;
  op?: FilterOp;
  ask?: 'value' | 'values' | 'sql' | 'sql_right';
  divided?: boolean;
  clear?: boolean;
}
export function menuFor(kind: ColumnKind): FilterMenuEntry[] {
  const out: FilterMenuEntry[] = [
    { label: rt('core:filter.clear'), clear: true },
    { label: rt('core:filter.values'), op: 'in', ask: 'values' },
    { label: rt('core:filter.isNull'), op: 'is_null', divided: true },
    { label: rt('core:filter.notNull'), op: 'not_null' },
  ];
  if (kind === 'bool') {
    out.push(
      { label: rt('core:filter.isTrue'), op: 'is_true', divided: true },
      { label: rt('core:filter.isFalse'), op: 'is_false' },
      { label: rt('core:filter.trueOrNull'), op: 'true_or_null' },
      { label: rt('core:filter.falseOrNull'), op: 'false_or_null' },
    );
  } else if (kind === 'text') {
    out.push(
      { label: rt('core:filter.isEmpty'), op: 'is_empty' },
      { label: rt('core:filter.notEmpty'), op: 'not_empty' },
      { label: rt('core:filter.eq'), op: 'eq', ask: 'value', divided: true },
      { label: rt('core:filter.ne'), op: 'ne', ask: 'value' },
      { label: rt('core:filter.contains'), op: 'contains', ask: 'value' },
      { label: rt('core:filter.notContains'), op: 'not_contains', ask: 'value' },
      { label: rt('core:filter.startsWith'), op: 'starts_with', ask: 'value' },
      { label: rt('core:filter.endsWith'), op: 'ends_with', ask: 'value' },
    );
  } else {
    out.push(
      { label: rt('core:filter.eq'), op: 'eq', ask: 'value', divided: true },
      { label: rt('core:filter.ne'), op: 'ne', ask: 'value' },
      { label: rt('core:filter.gt'), op: 'gt', ask: 'value' },
      { label: rt('core:filter.ge'), op: 'ge', ask: 'value' },
      { label: rt('core:filter.lt'), op: 'lt', ask: 'value' },
      { label: rt('core:filter.le'), op: 'le', ask: 'value' },
    );
  }
  out.push(
    { label: rt('core:filter.sql'), op: 'sql', ask: 'sql', divided: true },
    { label: rt('core:filter.sqlRight'), op: 'sql_right', ask: 'sql_right' },
  );
  return out;
}

/** How a filter built from the menu reads in the box. */
export function describe(f: ColumnFilter): string {
  const v = f.values.map((x) => (x === null ? 'NULL' : String(x)));
  switch (f.op) {
    case 'eq': return `=${v[0]}`;
    case 'ne': return `<>${v[0]}`;
    case 'gt': return `>${v[0]}`;
    case 'ge': return `>=${v[0]}`;
    case 'lt': return `<${v[0]}`;
    case 'le': return `<=${v[0]}`;
    case 'contains': return v[0];
    case 'not_contains': return `!${v[0]}`;
    case 'starts_with': return `^${v[0]}`;
    case 'ends_with': return `${v[0]}$`;
    case 'is_null': return 'NULL';
    case 'not_null': return 'NOT NULL';
    case 'is_empty': return 'EMPTY';
    case 'not_empty': return 'NOT EMPTY';
    case 'in': return `=${v.join(',')}`;
    case 'not_in': return `not in ${v.join(',')}`;
    case 'is_true': return 'TRUE';
    case 'is_false': return 'FALSE';
    case 'true_or_null': return rt('core:filter.describeTrueOrNull');
    case 'false_or_null': return rt('core:filter.describeFalseOrNull');
    case 'sql': return `SQL: ${f.sql ?? ''}`;
    case 'sql_right': return `SQL: … ${f.sql ?? ''}`;
  }
}

function cmp(a: Cell, b: Cell): number {
  if (typeof a === 'number' && typeof b === 'number') return a - b;
  return String(a).localeCompare(String(b), undefined, { numeric: true });
}

/** A row against the filters, for engines that don't filter on the server
 *  (SQL conditions can't be checked here: they pass). */
export function matchesLocal(row: Cell[], columns: string[], filters: ColumnFilter[]): boolean {
  return filters.every((f) => {
    const i = columns.indexOf(f.column);
    if (i < 0) return true;
    const v = row[i] ?? null;
    const s = v === null ? '' : String(v).toLowerCase();
    const w = f.values[0] ?? null;
    const ws = w === null ? '' : String(w).toLowerCase();
    const truthy = v === true || v === 1 || v === '1' || s === 'true';
    const falsy = v === false || v === 0 || v === '0' || s === 'false';
    switch (f.op) {
      case 'eq': return v !== null && (v === w || s === ws);
      case 'ne': return v !== null && !(v === w || s === ws);
      case 'gt': return v !== null && cmp(v, w) > 0;
      case 'ge': return v !== null && cmp(v, w) >= 0;
      case 'lt': return v !== null && cmp(v, w) < 0;
      case 'le': return v !== null && cmp(v, w) <= 0;
      case 'contains': return v !== null && s.includes(ws);
      case 'not_contains': return v !== null && !s.includes(ws);
      case 'starts_with': return v !== null && s.startsWith(ws);
      case 'ends_with': return v !== null && s.endsWith(ws);
      case 'is_null': return v === null;
      case 'not_null': return v !== null;
      case 'is_empty': return v === '';
      case 'not_empty': return v !== null && v !== '';
      case 'in': return v !== null && f.values.some((x) => x === v || String(x).toLowerCase() === s);
      case 'not_in': return v !== null && !f.values.some((x) => x === v || String(x).toLowerCase() === s);
      case 'is_true': return truthy;
      case 'is_false': return falsy;
      case 'true_or_null': return v === null || truthy;
      case 'false_or_null': return v === null || falsy;
      default: return true;
    }
  });
}
