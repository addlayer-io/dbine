import { StreamLanguage, type StreamParser, type StringStream } from '@codemirror/language';
import type { Completion, CompletionContext, CompletionResult } from '@codemirror/autocomplete';

// KDB+'s q in the query editor (the driver says `dialect: "q"`): a small
// tokenizer for highlighting and the completion of q's words and the
// connection's names. q has no official CodeMirror mode; this follows how
// kdb+ reads a script (crates/drivers/kdb/src/script.rs):
//
// - `/` starts a comment at the start of a line or after a blank (elsewhere
//   it's the adverb "over"); a line that is only `/` opens a comment block,
//   closed by a line that is only `\`; a line that is only `\` outside a
//   block ends the script (the rest is ignored, shown as a comment).
// - `\l`, `\t`, `\d`… at the start of a line are system commands.
// - "strings" with `\` escapes; `sym and `a`b symbols; `:path file handles.
// - numbers with q's type suffixes, temporal literals, nulls and infinities.
//
// Completion: q's keywords and built-ins (`.Q`, `.z`, `.h`, `.j` included),
// the connection's tables, views, functions and variables (`.u.upd`), and the
// columns of the tables the statement at the cursor names.

/** q-sql's words. */
export const Q_KEYWORDS = ['select', 'exec', 'update', 'delete', 'from', 'by', 'where'];
/** Control words. */
export const Q_CONTROL = ['if', 'do', 'while'];
/** The iterators written as words (highlighted as the adverbs are). */
const ITERATORS = new Set(['each', 'over', 'scan', 'prior', 'peach']);

/** The `.q` namespace's functions, the ones written without a namespace. */
export const Q_BUILTINS = [
  'abs', 'acos', 'aj', 'aj0', 'ajf', 'ajf0', 'all', 'and', 'any', 'asc', 'asin', 'asof', 'atan', 'attr', 'avg', 'avgs',
  'bin', 'binr', 'ceiling', 'cols', 'cor', 'cos', 'count', 'cov', 'cross', 'csv', 'cut', 'deltas', 'desc', 'dev',
  'differ', 'distinct', 'div', 'dsave', 'each', 'ej', 'ema', 'enlist', 'eval', 'except', 'exit', 'exp', 'fby', 'fills',
  'first', 'fkeys', 'flip', 'floor', 'get', 'getenv', 'group', 'gtime', 'hclose', 'hcount', 'hdel', 'hopen', 'hsym',
  'iasc', 'idesc', 'ij', 'ijf', 'in', 'insert', 'inter', 'inv', 'key', 'keys', 'last', 'like', 'lj', 'ljf', 'load',
  'log', 'lower', 'lsq', 'ltime', 'ltrim', 'mavg', 'max', 'maxs', 'mcount', 'md5', 'mdev', 'med', 'meta', 'min',
  'mins', 'mmax', 'mmin', 'mmu', 'mod', 'msum', 'neg', 'next', 'not', 'null', 'or', 'over', 'parse', 'peach', 'pj',
  'prd', 'prds', 'prev', 'prior', 'rand', 'rank', 'ratios', 'raze', 'read0', 'read1', 'reciprocal', 'reval',
  'reverse', 'rload', 'rotate', 'rsave', 'rtrim', 'save', 'scan', 'scov', 'sdev', 'set', 'setenv', 'show', 'signum',
  'sin', 'sqrt', 'ss', 'ssr', 'string', 'sublist', 'sum', 'sums', 'sv', 'svar', 'system', 'tables', 'tan', 'til',
  'trim', 'type', 'uj', 'ujf', 'ungroup', 'union', 'upper', 'upsert', 'value', 'var', 'view', 'views', 'vs', 'wavg',
  'within', 'wj', 'wj1', 'wsum', 'xasc', 'xbar', 'xcol', 'xcols', 'xdesc', 'xexp', 'xgroup', 'xkey',
  'xlog', 'xprev', 'xrank',
];

/** kdb+'s own namespaces' best-known names (any `.Q.x`, `.z.x`… is
 *  highlighted as built-in; these are the ones completion offers). */
export const Q_NAMESPACED = [
  '.Q.A', '.Q.a', '.Q.addmonths', '.Q.addr', '.Q.an', '.Q.atob', '.Q.btoa', '.Q.bv', '.Q.chk', '.Q.cn', '.Q.dd',
  '.Q.def', '.Q.dpft', '.Q.dpfts', '.Q.dsftg', '.Q.en', '.Q.ens', '.Q.f', '.Q.fc', '.Q.fmt', '.Q.fs', '.Q.fsn',
  '.Q.fu', '.Q.gc', '.Q.gz', '.Q.hdpf', '.Q.hg', '.Q.host', '.Q.hp', '.Q.id', '.Q.ind', '.Q.j10', '.Q.j12', '.Q.k',
  '.Q.l', '.Q.ld', '.Q.lo', '.Q.M', '.Q.n', '.Q.nA', '.Q.opt', '.Q.par', '.Q.pd', '.Q.pf', '.Q.pn', '.Q.prf0',
  '.Q.pt', '.Q.pv', '.Q.qp', '.Q.qt', '.Q.res', '.Q.s', '.Q.sbt', '.Q.sha1', '.Q.trp', '.Q.ts', '.Q.ty', '.Q.u',
  '.Q.V', '.Q.v', '.Q.view', '.Q.vp', '.Q.w', '.Q.x', '.Q.x10', '.Q.x12', '.Q.Xf',
  '.z.a', '.z.b', '.z.bm', '.z.c', '.z.D', '.z.d', '.z.e', '.z.ex', '.z.exit', '.z.ey', '.z.f', '.z.h', '.z.i',
  '.z.K', '.z.k', '.z.l', '.z.N', '.z.n', '.z.o', '.z.P', '.z.p', '.z.pc', '.z.pd', '.z.pg', '.z.ph', '.z.pi',
  '.z.pm', '.z.po', '.z.pp', '.z.pq', '.z.ps', '.z.pw', '.z.q', '.z.s', '.z.T', '.z.t', '.z.ts', '.z.u', '.z.vs',
  '.z.W', '.z.w', '.z.wc', '.z.wo', '.z.ws', '.z.X', '.z.x', '.z.Z', '.z.z', '.z.zd',
  '.h.br', '.h.c0', '.h.c1', '.h.cd', '.h.code', '.h.d', '.h.ed', '.h.edsn', '.h.fram', '.h.ha', '.h.hb', '.h.hc',
  '.h.he', '.h.hn', '.h.HOME', '.h.hp', '.h.hr', '.h.ht', '.h.hta', '.h.htac', '.h.htc', '.h.html', '.h.http',
  '.h.hu', '.h.hug', '.h.hy', '.h.iso8601', '.h.jx', '.h.nbr', '.h.pre', '.h.sa', '.h.sb', '.h.sc', '.h.td',
  '.h.text', '.h.tx', '.h.ty', '.h.uh', '.h.val', '.h.xd', '.h.xmp', '.h.xs', '.h.xt',
  '.j.j', '.j.jd', '.j.k',
];

const KEYWORDS = new Set(Q_KEYWORDS);
const CONTROL = new Set(Q_CONTROL);
const BUILTINS = new Set(Q_BUILTINS);
/** kdb+'s own namespaces. */
const SYSTEM_NS = /^\.[qQzhjo]\./;

interface QState {
  /** Inside a `/` … `\` comment block. */
  block: boolean;
  /** After a line that is only `\`: the script ended. */
  ended: boolean;
}

// The literals, tried in this order at a number's start.
const TEMPORAL = [
  // timestamp / datetime / date: 2026.11.03, 2026.11.03D12:00:00.000, 2026.11.03T12:00
  /^\d{4}\.\d{2}\.\d{2}(?:[DT](?:\d{2}(?::\d{2}(?::\d{2}(?:\.\d*)?)?)?)?)?[pzd]?/,
  // month: 2026.11m
  /^\d{4}\.\d{2}m/,
  // timespan: 0D12:00:00.000000000, 1D
  /^\d+D(?:\d{2}(?::\d{2}(?::\d{2}(?:\.\d*)?)?)?)?n?/,
  // time / second / minute: 12:00:00.000, 12:00:00, 12:00
  /^\d{2}:\d{2}(?::\d{2}(?:\.\d*)?)?[tvuns]?/,
];
const NUMBER = /^(?:\d+\.?\d*|\.\d+)(?:e[+-]?\d+)?[hijefcpmdznuvtg]?/;
const IDENT = /^\.?[A-Za-z][\w]*(?:\.[A-Za-z_][\w]*)*/;

/** What is before the stream's position on its line is only blanks. */
const atLineStart = (s: StringStream) => /^\s*$/.test(s.string.slice(0, s.pos));
/** The character before the stream's position is a blank (or the line starts). */
const afterBlank = (s: StringStream) => s.pos === 0 || /\s/.test(s.string[s.pos - 1]);

function token(s: StringStream, st: QState): string | null {
  if (st.ended) { s.skipToEnd(); return 'comment'; }
  if (s.sol()) {
    const line = s.string.trimEnd();
    if (st.block) {
      if (line === '\\') st.block = false;
      s.skipToEnd();
      return 'comment';
    }
    if (line === '/') { st.block = true; s.skipToEnd(); return 'comment'; }
    if (line === '\\') { st.ended = true; s.skipToEnd(); return 'comment'; }
    // A system command: `\l file.q`, `\t expr`, `\d .ns`, `\\`.
    if (s.match(/^\\(?:\\|[A-Za-z_]+|\d+)/)) return 'moduleKeyword';
  }
  if (s.eatSpace()) return null;
  const c = s.peek()!;

  // A comment: `/` at the line's start or after a blank.
  if (c === '/' && (afterBlank(s) || atLineStart(s))) { s.skipToEnd(); return 'comment'; }

  if (c === '"') {
    s.next();
    let escaped = false;
    for (let ch = s.next(); ch !== undefined; ch = s.next()) {
      if (escaped) escaped = false;
      else if (ch === '\\') escaped = true;
      else if (ch === '"') break;
    }
    return 'string';
  }

  if (c === '`') {
    s.next();
    // A file or connection handle: `:path/to/file, `:host:5000.
    if (s.eat(':')) { s.match(/^[\w.:/\\-]*/); return 'string.special'; }
    s.match(/^[\w.:]*/);
    return 'variableName.special';
  }

  // Numbers (not inside a name: names are taken whole below).
  if (/\d/.test(c) || (c === '.' && /\d/.test(s.string[s.pos + 1] ?? ''))) {
    if (s.match(/^0x[0-9a-fA-F]*/)) return 'number';
    if (s.match(/^0[NnWw][hijefcpmdznuvtg]?/)) return 'null';
    if (s.match(/^[01]+b/)) return 'bool';
    for (const re of TEMPORAL) if (s.match(re)) return 'number';
    if (s.match(NUMBER)) return 'number';
  }

  const id = s.match(IDENT) as RegExpMatchArray | null;
  if (id) {
    const w = id[0];
    if (KEYWORDS.has(w)) return 'keyword';
    if (CONTROL.has(w) || ITERATORS.has(w)) return 'controlKeyword';
    if (BUILTINS.has(w) || SYSTEM_NS.test(w)) return 'variableName.function';
    return 'variableName';
  }

  // Adverbs: each-prior, each-right, each-left, each, over, scan.
  if (s.match(/^(?:':|\/:|\\:|'|\/|\\)/)) return 'controlKeyword';
  if (s.match(/^[+\-*%!@#$^&|<>=~,?.:_]+/)) return 'operator';
  if (s.match(/^[()[\]{}]/)) return 'bracket';
  if (s.eat(';')) return 'separator';
  s.next();
  return null;
}

export const qParser: StreamParser<QState> = {
  name: 'q',
  startState: () => ({ block: false, ended: false }),
  copyState: (st) => ({ ...st }),
  token,
  languageData: { commentTokens: { line: '/' } },
};

export const qLanguage = StreamLanguage.define(qParser);

// -- completion ----------------------------------------------------------------------------------

/** What completion knows of the connection. */
export interface QNames {
  /** Tables and views (anything with columns) → their loaded columns. */
  tables: Record<string, string[]>;
  /** The connection's other names: functions, variables. */
  others: { label: string; kind: string }[];
}

/** The statement around `pos`: the lines from the last one that doesn't
 *  start with a blank (kdb+'s continuation rule) to the next such line. */
export function statementAround(doc: string, pos: number): string {
  const lines = doc.split('\n');
  let at = 0;
  let i = 0;
  for (; i < lines.length; i++) {
    if (pos <= at + lines[i].length) break;
    at += lines[i].length + 1;
  }
  i = Math.min(i, lines.length - 1);
  const starts = (l: string) => l.length > 0 && !/^\s/.test(l);
  let a = i;
  while (a > 0 && !starts(lines[a])) a--;
  let b = i + 1;
  while (b < lines.length && !starts(lines[b]) && lines[b].trim() !== '') b++;
  return lines.slice(a, b).join('\n');
}

/** A q name as the explorer's path to it: a namespace's object is
 *  `[namespace, name]` (`.ns.t` → `['.ns', 't']`), a root one `[name]`. */
export function qPath(name: string): string[] {
  const m = /^(\.[^.]+)\.(.+)$/.exec(name);
  return m ? [m[1], m[2]] : [name];
}

/** The tables `text` names (whole names: `.u.trade` isn't `trade`). */
export function tablesNamed(text: string, tables: string[]): string[] {
  const words = new Set(text.match(/\.?[A-Za-z][\w]*(?:\.[A-Za-z_][\w]*)*/g) ?? []);
  return tables.filter((t) => words.has(t));
}

const kindType = (kind: string) => (kind === 'function' ? 'function' : 'variable');

/** The completions at the context's position, and the tables of the
 *  statement whose columns aren't loaded yet (`needColumns`). */
export function qCompletion(ctx: CompletionContext, names: QNames): { result: CompletionResult | null; needColumns: string[] } {
  const none = { result: null, needColumns: [] };
  const word = ctx.matchBefore(/`?[\w.]*$/);
  const text = word?.text ?? '';
  const symbol = text.startsWith('`');
  // A file handle (`:path) or a number: nothing to offer.
  if (ctx.matchBefore(/`:[\w.:/\\-]*$/) || /^`?\d/.test(text)) return none;
  if (!text.replace(/^`/, '') && !ctx.explicit && !symbol) return none;
  const from = (word?.from ?? ctx.pos) + (symbol ? 1 : 0);

  const tables = Object.keys(names.tables);
  const named = tablesNamed(statementAround(ctx.state.doc.toString(), ctx.pos), tables);
  const needColumns = named.filter((t) => !names.tables[t].length);
  const options: Completion[] = [];
  for (const t of tables) options.push({ label: t, type: 'class', boost: 2 });
  const seen = new Set<string>();
  for (const t of named) {
    for (const c of names.tables[t]) {
      if (seen.has(c)) continue;
      seen.add(c);
      options.push({ label: c, type: 'property', detail: t, boost: 3 });
    }
  }
  if (!symbol) {
    for (const o of names.others) options.push({ label: o.label, type: kindType(o.kind), boost: 1 });
    for (const k of Q_KEYWORDS) options.push({ label: k, type: 'keyword' });
    for (const k of Q_CONTROL) options.push({ label: k, type: 'keyword' });
    for (const b of Q_BUILTINS) options.push({ label: b, type: 'function', boost: -1 });
    for (const b of Q_NAMESPACED) options.push({ label: b, type: 'function', boost: -2 });
  }
  return { result: { from, options, validFor: /^[\w.]*$/ }, needColumns };
}
