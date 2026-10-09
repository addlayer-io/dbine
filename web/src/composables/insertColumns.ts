import type { Language } from '../api/types';
import { lexSyntax, maskCode } from './sqlLexer';

// "Which column is this value?": with the cursor on a value of an INSERT's
// VALUES, the column it goes to (from the statement's own column list, or,
// without one, the table's columns in order). Works on the text with
// strings and comments masked, so commas and parentheses inside them don't
// count. SQL and CQL only: document engines name each value already.

export interface InsertColumn {
  /** The column's name as the list writes it, or the table's column. */
  name: string | null;
  /** 0-based position of the value in its row. */
  index: number;
  /** How many columns the list has (or the table, without a list). */
  total: number;
  /** Where the name is in the column list, to highlight it. */
  from: number | null;
  to: number | null;
}

/** `doc` from `pos` backwards to the start of its statement (after a `;`). */
function statementStart(masked: string, pos: number): number {
  const i = masked.lastIndexOf(';', pos - 1);
  return i < 0 ? 0 : i + 1;
}

/** The items of a parenthesized list starting at `open` (the `(`): their
 *  ranges, split at commas at depth 1, and where the list closes. */
function items(masked: string, open: number): { ranges: [number, number][]; close: number } | null {
  const ranges: [number, number][] = [];
  let depth = 0;
  let start = open + 1;
  for (let i = open; i < masked.length; i++) {
    const c = masked[i];
    if (c === '(' || c === '[' || c === '{') depth++;
    else if (c === ')' || c === ']' || c === '}') {
      depth--;
      if (depth === 0) {
        ranges.push([start, i]);
        return { ranges, close: i };
      }
    } else if (c === ',' && depth === 1) {
      ranges.push([start, i]);
      start = i + 1;
    } else if (c === ';' && depth > 0) {
      return null;
    }
  }
  // Still being typed: the rest counts.
  ranges.push([start, masked.length]);
  return { ranges, close: masked.length };
}

/** A column name as written (`[Name]`, `"Name"`, `Name`), trimmed of
 *  blanks and comments (blank in `masked`). */
function trimmed(doc: string, masked: string, r: [number, number]): { text: string; from: number; to: number } {
  let [a, b] = r;
  while (a < b && /\s/.test(masked[a])) a++;
  while (b > a && /\s/.test(masked[b - 1])) b--;
  return { text: doc.slice(a, b), from: a, to: b };
}

const unquote = (s: string) => s.replace(/^[[`"](.*)[\]`"]$/s, '$1');

/**
 * The column of the value at `pos`, or null when `pos` isn't inside a row
 * of an INSERT's VALUES. `columnsOf` gives a table's columns (`schema.table`
 * or `table`, as written) for an INSERT without a column list.
 */
export function insertColumnAt(
  doc: string,
  pos: number,
  language: Language | undefined,
  dialect: string,
  columnsOf?: (table: string) => string[] | null,
): InsertColumn | null {
  if (language !== 'sql' && language !== 'cql') return null;
  const masked = maskCode(doc, lexSyntax(language, dialect));
  const start = statementStart(masked, pos);
  const head = /\binsert\s+(?:(?:ignore|or\s+\w+|low_priority|delayed|high_priority)\s+)*into\s+([^\s(]+(?:\s*\.\s*[^\s(]+)*)\s*/iy;
  // The INSERT this statement starts with (after leading blanks or a WITH … AS (…)).
  const stmt = masked.slice(start, pos);
  const at = stmt.search(/\binsert\b/i);
  if (at < 0) return null;
  head.lastIndex = start + at;
  const m = head.exec(masked);
  if (!m) return null;
  const table = doc.slice(m.index + m[0].indexOf(m[1]), m.index + m[0].indexOf(m[1]) + m[1].length).replace(/\s+/g, '');
  let i = head.lastIndex;
  // The column list, when there is one.
  let columns: { text: string; from: number; to: number }[] | null = null;
  if (masked[i] === '(') {
    const list = items(masked, i);
    if (!list) return null;
    if (pos <= list.close) return null;
    columns = list.ranges.map((r) => trimmed(doc, masked, r)).filter((c) => c.text);
    i = list.close + 1;
  }
  const values = /\s*values?\b\s*/iy;
  values.lastIndex = i;
  if (!values.exec(masked)) return null;
  i = values.lastIndex;
  // Each row: `(…)`, separated by commas; find the one holding `pos`.
  while (i < masked.length && i < pos) {
    while (i < masked.length && /[\s,]/.test(masked[i])) i++;
    if (masked[i] !== '(') return null;
    const row = items(masked, i);
    if (!row) return null;
    if (pos > i && pos <= row.close) {
      const index = row.ranges.findIndex(([a, b]) => pos >= a && pos <= b);
      if (index < 0) return null;
      if (columns) {
        const c = columns[index];
        return { name: c ? unquote(c.text) : null, index, total: columns.length, from: c?.from ?? null, to: c?.to ?? null };
      }
      const cols = columnsOf?.(unquote(table)) ?? columnsOf?.(table) ?? null;
      if (!cols?.length) return null;
      return { name: cols[index] ?? null, index, total: cols.length, from: null, to: null };
    }
    i = row.close + 1;
  }
  return null;
}
