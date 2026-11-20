import type { Language } from '../api/types';
import { lexSyntax, maskCode } from './sqlLexer';

// What a query compare's key can learn from the queries themselves
// (docs/data-compare.md, "Comparing two queries"): the one table a plain
// `SELECT … FROM t …` reads (DataCompareView then takes its primary key, or
// a unique index, from the catalog) and whether a query groups (then the
// columns that describe rather than measure are the key). Tested by
// queryKey.test.mjs.

type Token = { word: string } | { name: string } | { punct: string };

/** Words that make a query more than one table's rows: joins, set
 *  operations, grouping, subqueries' and table functions' own clauses. */
const NOT_ONE_TABLE = new Set([
  'join', 'union', 'intersect', 'except', 'minus', 'group', 'having', 'pivot', 'unpivot',
  'apply', 'lateral', 'into', 'connect', 'match_recognize', 'unnest',
]);
/** What may follow the table's name (or its alias). */
const AFTER_TABLE = new Set(['where', 'order', 'limit', 'fetch', 'offset', 'for', 'option', 'with', 'tablesample', 'qualify', 'sample', 'final', 'version', 'timestamp']);

/** The query's tokens, from its masked text (comments and string contents
 *  blanked): words, quoted names unquoted, and single punctuation. A plain
 *  scan, no backtracking. */
function tokens(masked: string): Token[] {
  const out: Token[] = [];
  const n = masked.length;
  let i = 0;
  while (i < n) {
    const c = masked[i];
    if (c === ' ' || c === '\t' || c === '\n' || c === '\r') { i++; continue; }
    if (/[A-Za-z_]/.test(c)) {
      let j = i + 1;
      while (j < n && /[\w$#@]/.test(masked[j])) j++;
      out.push({ word: masked.slice(i, j) });
      i = j;
    } else if (c === '"' || c === '`' || c === '[') {
      const close = c === '[' ? ']' : c;
      let j = i + 1;
      let name = '';
      while (j < n) {
        if (masked[j] === close) {
          if (masked[j + 1] === close) { name += close; j += 2; continue; }
          break;
        }
        name += masked[j++];
      }
      out.push({ name });
      i = j + 1;
    } else {
      out.push({ punct: c });
      i++;
    }
  }
  return out;
}

const word = (t: Token | undefined) => (t && 'word' in t ? t.word.toLowerCase() : null);
const nameOf = (t: Token | undefined) => (t && 'word' in t ? t.word : t && 'name' in t ? t.name : null);
const isPunct = (t: Token | undefined, p: string) => !!t && 'punct' in t && t.punct === p;

function sqlTokens(query: string, language: Language | undefined, dialect: string): Token[] | null {
  if (language !== undefined && language !== 'sql') return null;
  return tokens(maskCode(query, lexSyntax(language, dialect)));
}

/** The one table a plain `SELECT … FROM t [alias] [WHERE …] [ORDER BY …]`
 *  reads, as its name's parts (`[database.][schema.]name`, unquoted), or
 *  null: not SQL, a join, a set operation, a subquery, a CTE, a grouping, a
 *  table function, or anything else. */
export function singleTable(query: string, language: Language | undefined, dialect: string): string[] | null {
  const ts = sqlTokens(query, language, dialect);
  if (!ts?.length || word(ts[0]) !== 'select') return null;
  const words = ts.map(word);
  if (words.some((w) => w !== null && NOT_ONE_TABLE.has(w))) return null;
  if (words.filter((w) => w === 'select').length !== 1 || words.filter((w) => w === 'from').length !== 1) return null;
  let i = words.indexOf('from') + 1;
  const parts: string[] = [];
  for (;;) {
    const part = nameOf(ts[i]);
    if (part === null) return null;
    parts.push(part);
    i++;
    if (!isPunct(ts[i], '.')) break;
    i++;
    if (parts.length === 3) return null;
  }
  // Then nothing, `;`, a clause, or an alias (`AS a`, `a`) and then those.
  const ends = (k: number) => k >= ts.length || isPunct(ts[k], ';') || AFTER_TABLE.has(word(ts[k]) ?? '');
  if (ends(i)) return parts;
  if (word(ts[i]) === 'as') i++;
  if (nameOf(ts[i]) !== null && ends(i + 1)) return parts;
  return null;
}

/** Whether the query groups its rows (`GROUP BY`). */
export function groups(query: string, language: Language | undefined, dialect: string): boolean {
  const ts = sqlTokens(query, language, dialect);
  return !!ts && ts.some((t, i) => word(t) === 'group' && word(ts[i + 1]) === 'by');
}
