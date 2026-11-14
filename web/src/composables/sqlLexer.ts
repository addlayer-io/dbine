import type { Language } from '../api/types';

// A small lexer shared by the editor's parameters (composables/queryParams.ts)
// and its name checks (composables/nameRefs.ts): it tells code from strings,
// comments and quoted names, per language and dialect. It never needs the
// grammar: only where a literal or a comment starts and ends.

export interface LexSyntax {
  /** `-- comment` (SQL, CQL). */
  dashComment: boolean;
  /** `// comment` (CQL, mongosh, Flux). */
  slashComment: boolean;
  /** `# comment` (MySQL, MariaDB, BigQuery). */
  hashComment: boolean;
  /** `\'` escapes inside quotes (MySQL, BigQuery, JSON-like consoles…). */
  backslash: boolean;
  /** `"…"` is a string (else a quoted name; both are skipped the same way). */
  doubleQuoteString: boolean;
  /** `[name]` quoting (SQL Server, Sybase, Access). */
  brackets: boolean;
  /** `$$…$$` and `$tag$…$tag$` bodies (PostgreSQL and others, CQL). */
  dollarQuotes: boolean;
  /** Oracle's `q'[…]'`. */
  oracleQ: boolean;
  /** KDB+'s q: `/` comments (after a blank, and `/` … `\` blocks), `"…"`
   *  strings; `'` is an adverb and `` ` `` starts a symbol, neither a quote. */
  q?: boolean;
  /** `'…'` is a quoted name, not a string (DAX's table names). */
  singleQuoteIdent?: boolean;
}

export type RegionKind = 'code' | 'string' | 'comment' | 'ident';

export interface Region {
  kind: RegionKind;
  start: number;
  end: number;
}

const BACKSLASH_DIALECTS = new Set(['mysql', 'mariadb', 'clickhouse', 'bigquery', 'spanner', 'hive', 'sparksql', 'databricks', 'cosmos', 'n1ql']);
const DOUBLE_QUOTE_STRING = new Set(['mysql', 'mariadb', 'bigquery', 'spanner', 'hive', 'sparksql', 'databricks', 'n1ql', 'cosmos']);

/** How `language` + `dialect` writes literals and comments. */
export function lexSyntax(language: Language | undefined, dialect: string): LexSyntax {
  if (dialect === 'q') {
    return { dashComment: false, slashComment: false, hashComment: false, backslash: true, doubleQuoteString: true, brackets: false, dollarQuotes: false, oracleQ: false, q: true };
  }
  if (language === 'sql' || language === undefined) {
    return {
      dashComment: true,
      slashComment: false,
      hashComment: ['mysql', 'mariadb', 'bigquery'].includes(dialect),
      backslash: BACKSLASH_DIALECTS.has(dialect),
      doubleQuoteString: DOUBLE_QUOTE_STRING.has(dialect),
      brackets: ['mssql', 'sybase', 'access'].includes(dialect),
      dollarQuotes: true,
      oracleQ: dialect === 'oracle',
    };
  }
  if (language === 'dax') {
    // DAX: `//` and `--` comments, "strings" with "" inside, 'table names'
    // and [columns or measures], each with its closing character doubled.
    return {
      dashComment: true, slashComment: true, hashComment: false, backslash: false, doubleQuoteString: true, brackets: true,
      dollarQuotes: false, oracleQ: false, singleQuoteIdent: true,
    };
  }
  if (language === 'cql') {
    return { dashComment: true, slashComment: true, hashComment: false, backslash: false, doubleQuoteString: false, brackets: false, dollarQuotes: true, oracleQ: false };
  }
  // mongosh, JSON consoles, Redis, Flux, Cypher: C-like strings.
  return {
    dashComment: false, slashComment: language !== 'redis', hashComment: false, backslash: true,
    doubleQuoteString: language !== 'cypher', brackets: false, dollarQuotes: false, oracleQ: false,
  };
}

const WORD = /[\p{L}\p{N}_$#@]/u;
const DOLLAR_TAG = /\$([A-Za-z_][\w]*)?\$/y;
const Q_CLOSE: Record<string, string> = { '[': ']', '(': ')', '{': '}', '<': '>' };

/** The text cut into code, strings, comments and quoted names, in order. */
export function lexRegions(doc: string, syn: LexSyntax): Region[] {
  if (syn.q) return lexQ(doc);
  const out: Region[] = [];
  const n = doc.length;
  let codeFrom = 0;
  const push = (kind: RegionKind, start: number, end: number) => {
    if (start > codeFrom) out.push({ kind: 'code', start: codeFrom, end: start });
    out.push({ kind, start, end });
    codeFrom = end;
  };
  /** The end of a quoted run opened at `i` by `q`. */
  const quoted = (i: number, q: string, backslash: boolean): number => {
    let j = i + 1;
    while (j < n) {
      const c = doc[j];
      if (backslash && c === '\\') { j += 2; continue; }
      if (c === q) {
        if (doc[j + 1] === q) { j += 2; continue; }
        return j + 1;
      }
      j++;
    }
    return n;
  };
  const toEol = (i: number) => {
    const e = doc.indexOf('\n', i);
    return e < 0 ? n : e;
  };

  let i = 0;
  while (i < n) {
    const c = doc[i];
    const next = doc[i + 1];
    const prev = i > 0 ? doc[i - 1] : '';
    if ((syn.dashComment && c === '-' && next === '-') || (syn.slashComment && c === '/' && next === '/') || (syn.hashComment && c === '#')) {
      const e = toEol(i);
      push('comment', i, e);
      i = e;
    } else if (c === '/' && next === '*') {
      const e = doc.indexOf('*/', i + 2);
      const end = e < 0 ? n : e + 2;
      push('comment', i, end);
      i = end;
    } else if (syn.oracleQ && (c === 'q' || c === 'Q') && next === "'" && !WORD.test(prev === 'n' || prev === 'N' ? doc[i - 2] ?? '' : prev) && i + 2 < n) {
      const open = doc[i + 2];
      const close = (Q_CLOSE[open] ?? open) + "'";
      const e = doc.indexOf(close, i + 3);
      const end = e < 0 ? n : e + 2;
      push('string', i, end);
      i = end;
    } else if (c === "'") {
      // PostgreSQL's E'…' takes backslash escapes.
      const eString = !syn.singleQuoteIdent && (prev === 'E' || prev === 'e') && !WORD.test(i > 1 ? doc[i - 2] : '');
      const end = quoted(i, "'", syn.backslash || eString);
      push(syn.singleQuoteIdent ? 'ident' : 'string', i, end);
      i = end;
    } else if (c === '"') {
      const end = quoted(i, '"', syn.backslash && syn.doubleQuoteString);
      push(syn.doubleQuoteString ? 'string' : 'ident', i, end);
      i = end;
    } else if (c === '`') {
      const end = quoted(i, '`', false);
      push('ident', i, end);
      i = end;
    } else if (syn.brackets && c === '[') {
      const end = quoted(i, ']', false);
      push('ident', i, end);
      i = end;
    } else if (syn.dollarQuotes && c === '$' && !WORD.test(prev)) {
      DOLLAR_TAG.lastIndex = i;
      const m = DOLLAR_TAG.exec(doc);
      if (m) {
        const e = doc.indexOf(m[0], i + m[0].length);
        const end = e < 0 ? n : e + m[0].length;
        push('string', i, end);
        i = end;
      } else {
        i++;
      }
    } else {
      i++;
    }
  }
  if (codeFrom < n) out.push({ kind: 'code', start: codeFrom, end: n });
  return out;
}

/** q's regions, as kdb+ reads a script (crates/drivers/kdb/src/script.rs). */
function lexQ(doc: string): Region[] {
  const out: Region[] = [];
  const n = doc.length;
  let codeFrom = 0;
  const push = (kind: RegionKind, start: number, end: number) => {
    if (start > codeFrom) out.push({ kind: 'code', start: codeFrom, end: start });
    out.push({ kind, start, end });
    codeFrom = end;
  };
  let block = -1;
  let lineStart = 0;
  while (lineStart <= n) {
    const nl = doc.indexOf('\n', lineStart);
    const lineEnd = nl < 0 ? n : nl;
    const bare = doc.slice(lineStart, lineEnd).trimEnd();
    if (block >= 0) {
      if (bare === '\\') { push('comment', block, lineEnd); block = -1; }
    } else if (bare === '/') {
      block = lineStart;
    } else if (bare === '\\') {
      // The script ends here: the rest is never read.
      push('comment', lineStart, n);
      return out;
    } else {
      for (let i = lineStart; i < lineEnd; i++) {
        const c = doc[i];
        if (c === '/' && (i === lineStart || /\s/.test(doc[i - 1]))) {
          push('comment', i, lineEnd);
          break;
        }
        if (c === '"') {
          let j = i + 1;
          while (j < lineEnd && doc[j] !== '"') j += doc[j] === '\\' ? 2 : 1;
          const end = Math.min(j + 1, lineEnd);
          push('string', i, end);
          i = end - 1;
        }
      }
    }
    if (nl < 0) break;
    lineStart = nl + 1;
  }
  if (block >= 0) push('comment', block, n);
  if (codeFrom < n) out.push({ kind: 'code', start: codeFrom, end: n });
  return out;
}

/** The text with comments blanked and string contents blanked (their quotes
 *  stay), same length: regexes over it only see code and quoted names. */
export function maskCode(doc: string, syn: LexSyntax): string {
  let out = '';
  for (const r of lexRegions(doc, syn)) {
    const s = doc.slice(r.start, r.end);
    if (r.kind === 'comment') out += s.replace(/[^\n]/g, ' ');
    else if (r.kind === 'string') out += s.length < 2 ? ' '.repeat(s.length) : s[0] + s.slice(1, -1).replace(/[^\n]/g, ' ') + s[s.length - 1];
    else out += s;
  }
  return out;
}

/** The kind of region `pos` falls in. */
export function regionAt(regions: Region[], pos: number): RegionKind {
  for (const r of regions) if (pos >= r.start && pos < r.end) return r.kind;
  return 'code';
}
