// The engine's functions in the editor's completion (issue #97), kept apart
// from CodeEditor.vue so where each one goes can be tested without a
// browser. The catalog comes from the driver (`Session::function_catalog`):
// SQL functions, or what plays their part in the language (Redis and etcd
// commands, MongoDB and CouchDB operators, Solr function queries, Cypher and
// Flux functions). Only the editor's language, its dialect and the entries'
// kinds decide where they are offered, never the engine.
//
// - **SQL-like languages** (SQL, CQL, PartiQL, N1QL…), **Cypher** and **Flux**:
//   - scalar, aggregate and window functions where an expression starts:
//     after SELECT, WHERE, ON, BY, THEN…, an operator, `(` or a comma (not in
//     a column or parameter list being defined, not inside a Cypher map or
//     a Flux call, whose next word is a key). Never in a string or a comment,
//     where a name goes (an alias, after AS, a table's name after INTO), after
//     a whole expression (where a clause keyword goes), nor after a dot,
//     unless the name before it is the functions' schema or package
//     (`strings.`, `sales.`, `apoc.coll.`).
//   - table functions where a table goes: after FROM, JOIN, LATERAL, APPLY,
//     `TABLE(` or a comma in a FROM list.
//   - a function in a schema or package is offered by its whole name too
//     (`strings.toUpper`).
//   - picking one inserts `name()` with the cursor between the parentheses
//     (just the name when a `(` follows already). Keywords that name a
//     function the list offers are left out of it (CodeEditor), and
//     functions rank above keywords.
//   Cypher and Flux have no grammar in the editor: strings and comments are
//   found by a lexical scan with the same rules.
// - **Redis and etcd**: the commands, as the first word of a line, and a
//   container's subcommands after it (`CLIENT ` → `CLIENT KILL`). Picking
//   one inserts `NAME `.
// - **JSON** (MongoDB, CouchDB, Solr…): operators at a key that starts with
//   `$`, quoted or bare (the MongoDB shell's unquoted keys); a `$` value is
//   a field path or a variable (`"$total"`, `"$$ROOT"`), never an operator.
//   The function kinds inside string values when the catalog has no
//   operators (Solr's function queries; with operators, string values are
//   data); and, outside strings, after a dot that follows a name, `)` or `]`
//   (not a number's), the scalar entries (the MongoDB shell's methods:
//   `db.orders.find`).
//
// Each keystroke where nothing fits asks again, so the code before the
// cursor is read from at most LOOK_BACK before it (at a line's start; in SQL,
// and on a long JSON line, never from inside a string or comment, as the
// syntax tree says): the cost is the same in a script of any size.
import { pickedCompletion, type Completion, type CompletionContext, type CompletionResult } from '@codemirror/autocomplete';
import { syntaxTree } from '@codemirror/language';
import type { EditorView } from '@codemirror/view';
import type { FunctionInfo, FunctionKind, Language } from '../api/types';
import { inStringOrComment, isTableSlot, maskSql, parentsBefore, statementAt } from './sqlCompletion';

/** How an editor's language takes functions. */
export type FunctionFlavor = 'sql' | 'cypher' | 'flux' | 'json' | 'commands';
type CodeFlavor = 'sql' | 'cypher' | 'flux';

/** The flavor of an editor's language, or null where the catalog isn't
 *  offered this way (q merges it into its own completion: qLanguage.ts). */
export function functionFlavor(language: Language | undefined, dialect: string | undefined): FunctionFlavor | null {
  if (dialect === 'q') return null;
  switch (language ?? 'sql') {
    case 'sql':
    case 'cql': return 'sql';
    case 'cypher': return 'cypher';
    case 'flux': return 'flux';
    case 'json': return 'json';
    case 'redis': return 'commands';
    default: return null;
  }
}

/** Where functions rank: above keywords (CodeEditor gives those -2), with
 *  the columns of the tables the statement reads. */
export const FUNCTION_BOOST = -1;

/** How far back from the cursor the code is read. */
export const LOOK_BACK = 10_000;

// -- the index ----------------------------------------------------------------------------------
/** A name and its overloads, under the schema or package it's reached
 *  through. */
interface Group {
  name: string;
  /** `['sales']`, `['strings']`, `['apoc', 'coll']`; [] for a bare name. */
  qualifier: string[];
  entries: FunctionInfo[];
}

/** A catalog ready for completion: built once per catalog. */
export interface FunctionIndex {
  /** Scalar, aggregate and window functions. */
  expr: Group[];
  table: Group[];
  /** The options where an expression starts, and where a table goes. */
  exprOptions: Completion[];
  tableOptions: Completion[];
  /** JSON, after a dot: the scalar entries by their bare name. */
  methodOptions: Completion[];
  operatorOptions: Completion[];
  commandOptions: Completion[];
  /** The text of "+N overloads". */
  overloads: (n: number) => string;
}

/** What a function is reached through when written in code: its schema
 *  (an import path's last part: Flux's `influxdata/influxdb/schema` is
 *  `schema.`) and the namespaces in a dotted name (`apoc.coll.sum`). */
function split(f: FunctionInfo): { name: string; qualifier: string[] } {
  const schema = f.schema ? f.schema.split('/').pop()!.split('.').filter(Boolean) : [];
  const parts = f.name.split('.');
  // A name that is only dots, or ends with one, stays whole.
  const name = parts.length > 1 && parts[parts.length - 1] ? parts.pop()! : f.name;
  const inName = name === f.name ? [] : parts.filter(Boolean);
  return { name, qualifier: [...schema, ...inName] };
}

/** The entries of `kinds`, a group per name: `qualified` under their
 *  schema or package, `bare` by their last name only (methods), `raw` by
 *  their name as it is (commands such as `JSON.SET`, operators). */
function groupsOf(list: readonly FunctionInfo[], kinds: FunctionKind[], mode: 'qualified' | 'bare' | 'raw'): Group[] {
  const out = new Map<string, Group>();
  for (const f of list) {
    if (!kinds.includes(f.kind) || !f.name) continue;
    const { name, qualifier } = mode === 'raw' ? { name: f.name, qualifier: [] } : mode === 'bare' ? { name: split(f).name, qualifier: [] } : split(f);
    const key = [...qualifier, name].join('\u0000');
    const g = out.get(key);
    if (g) g.entries.push(f);
    else out.set(key, { name, qualifier, entries: [f] });
  }
  return [...out.values()];
}

const detailOf = (f: FunctionInfo) => f.args + (f.returns ? ` → ${f.returns}` : '');

function infoOf(g: Group, overloads: (n: number) => string): string {
  const description = g.entries.find((e) => e.description.trim())?.description.trim() ?? '';
  const more = g.entries.length - 1;
  return [description, more > 0 ? overloads(more) : ''].filter(Boolean).join('\n');
}

/** Picking a function: `text()` with the cursor inside, or just `text`
 *  before a `(` already there. */
export function callApply(text: string) {
  return (view: EditorView, completion: Completion, from: number, to: number) => {
    const open = view.state.sliceDoc(to, to + 1) === '(';
    view.dispatch({
      changes: { from, to, insert: open ? text : `${text}()` },
      selection: { anchor: from + text.length + 1 },
      annotations: pickedCompletion.of(completion),
      userEvent: 'input.complete',
      scrollIntoView: true,
    });
  };
}

/** Picking a command: `NAME ` (no second blank if one follows). */
export function commandApply(text: string) {
  return (view: EditorView, completion: Completion, from: number, to: number) => {
    const blank = /^[ \t]/.test(view.state.sliceDoc(to, to + 1));
    view.dispatch({
      changes: { from, to, insert: blank ? text : `${text} ` },
      selection: { anchor: from + text.length + 1 },
      annotations: pickedCompletion.of(completion),
      userEvent: 'input.complete',
      scrollIntoView: true,
    });
  };
}

function option(g: Group, label: string, overloads: (n: number) => string, apply: (text: string) => Completion['apply'], type: string, boost: number): Completion {
  const detail = detailOf(g.entries[0]);
  const info = infoOf(g, overloads);
  return { label, type, boost, ...(detail ? { detail } : {}), ...(info ? { info } : {}), apply: apply(label) };
}

const callOption = (g: Group, label: string, overloads: (n: number) => string) =>
  option(g, label, overloads, callApply, 'function', FUNCTION_BOOST);

/** Where an expression or a table starts, a function in a schema or package
 *  matches by its name (`toUp`, `st_dist`), shows whole (`strings.toUpper`)
 *  and goes in whole. */
function plainOption(g: Group, overloads: (n: number) => string): Completion {
  if (!g.qualifier.length) return callOption(g, g.name, overloads);
  const whole = [...g.qualifier, g.name].join('.');
  return { ...callOption(g, g.name, overloads), displayLabel: whole, apply: callApply(whole) };
}

/** The matched characters of an option shown whole (`displayLabel`): its
 *  name is the end of what's shown. */
export function matchShown(c: Completion, matched?: readonly number[]): readonly number[] {
  if (!matched) return [];
  const shift = c.displayLabel ? c.displayLabel.length - c.label.length : 0;
  return shift ? matched.map((p) => p + shift) : matched;
}

export function functionIndex(list: readonly FunctionInfo[], overloads: (n: number) => string): FunctionIndex {
  const expr = groupsOf(list, ['scalar', 'aggregate', 'window'], 'qualified');
  const table = groupsOf(list, ['table'], 'qualified');
  return {
    expr,
    table,
    exprOptions: expr.map((g) => plainOption(g, overloads)),
    tableOptions: table.map((g) => plainOption(g, overloads)),
    methodOptions: groupsOf(list, ['scalar'], 'bare').map((g) => callOption(g, g.name, overloads)),
    operatorOptions: groupsOf(list, ['operator'], 'raw').map((g) => option(g, g.name, overloads, (t) => t, 'keyword', 0)),
    commandOptions: groupsOf(list, ['command'], 'raw').map((g) => option(g, g.name, overloads, commandApply, 'function', 0)),
    overloads,
  };
}

/** The catalog has something the language takes: one without (an engine
 *  without a catalog, an Elasticsearch body) reads nothing at all. */
export function offersAny(index: FunctionIndex, flavor: FunctionFlavor): boolean {
  switch (flavor) {
    case 'commands': return index.commandOptions.length > 0;
    case 'json': return index.operatorOptions.length > 0 || index.exprOptions.length > 0;
    default: return index.exprOptions.length > 0 || index.tableOptions.length > 0;
  }
}

/** The groups reached through `parents` (`a.b.` before the cursor): those
 *  whose qualifier ends the path (`db.sales.` → `sales`), labeled by name,
 *  and, when `deeper`, those whose qualifier goes on from it (`apoc.` →
 *  `coll.sum`). */
function qualifiedOptions(groups: Group[], parents: string[], overloads: (n: number) => string, deeper = true): Completion[] {
  const typed = parents.map((p) => p.toLowerCase());
  const out: Completion[] = [];
  for (const g of groups) {
    if (!g.qualifier.length) continue;
    const q = g.qualifier.map((p) => p.toLowerCase());
    let rest: string[] | null = null;
    if (typed.length >= q.length && q.every((p, i) => p === typed[typed.length - q.length + i])) rest = [];
    else if (deeper && typed.length < q.length && typed.every((p, i) => p === q[i])) rest = g.qualifier.slice(typed.length);
    if (rest) out.push(callOption(g, [...rest, g.name].join('.'), overloads));
  }
  return out;
}

// -- strings and comments without a grammar (Cypher, Flux, JSON) -------------------------------
interface LexRules {
  /** `/* … *\/` comments (`//` ones are in every language here). */
  block: boolean;
  quotes: string;
  /** Strings end at the line's end (JSON). */
  oneLine: boolean;
  /** `/…/` regex literals (the MongoDB shell's, Flux's). */
  regex: boolean;
}
const LEX: Record<'cypher' | 'flux' | 'json', LexRules> = {
  cypher: { block: true, quotes: `'"\``, oneLine: false, regex: false },
  flux: { block: false, quotes: '"', oneLine: false, regex: true },
  json: { block: true, quotes: `"'`, oneLine: true, regex: true },
};

/** Where a lexical scan of a text ended: `masked` is the text with the
 *  insides of strings and comments blanked (same length, quotes kept). */
export interface Scan {
  masked: string;
  inside: 'code' | 'string' | 'comment';
  /** Inside a string: its quote (`/` for a regex) and where it opened. */
  quote: string;
  stringStart: number;
}

const blank = (s: string) => s.replace(/[^\n]/g, ' ');

export function scanCode(text: string, rules: LexRules): Scan {
  const out: string[] = [];
  const n = text.length;
  let lastCode = '';
  let i = 0;
  const end = (inside: Scan['inside'], quote = '', stringStart = -1): Scan => ({ masked: out.join(''), inside, quote, stringStart });
  while (i < n) {
    const c = text[i];
    const two = text.slice(i, i + 2);
    if (two === '//' || (rules.block && two === '/*')) {
      const close = two === '//' ? text.indexOf('\n', i) : text.indexOf('*/', i + 2);
      if (close < 0) { out.push(blank(text.slice(i))); return end('comment'); }
      const to = two === '//' ? close : close + 2;
      out.push(blank(text.slice(i, to)));
      i = to;
      continue;
    }
    const regex = rules.regex && c === '/' && (lastCode === '' || /[(,:=[!&|?{};~]/.test(lastCode));
    if (rules.quotes.includes(c) || regex) {
      let j = i + 1;
      while (j < n && text[j] !== c && !((rules.oneLine || regex) && text[j] === '\n')) j += text[j] === '\\' ? 2 : 1;
      if (j >= n) { out.push(c + blank(text.slice(i + 1))); return end('string', c, i); }
      if (text[j] === '\n') {
        // An unclosed string ends with its line; a `/` that opens no regex is a `/`.
        out.push(regex ? c : c + blank(text.slice(i + 1, j)));
        if (regex) lastCode = c;
        i = regex ? i + 1 : j;
        continue;
      }
      out.push(c + blank(text.slice(i + 1, j)) + c);
      lastCode = c;
      i = j + 1;
      continue;
    }
    out.push(c);
    if (!/\s/.test(c)) lastCode = c;
    i++;
  }
  return end('code');
}

// -- what's read before the cursor ---------------------------------------------------------------
/** `from`, or where the token of the syntax tree it falls inside ends, if
 *  it's one of `tokens` (a string, a comment); null while the tree isn't
 *  parsed that far (the editor parses up to what it shows, past the cursor). */
function outOf(ctx: CompletionContext, from: number, end: number, tokens: RegExp): number | null {
  const tree = syntaxTree(ctx.state);
  if (tree.length < end) return null;
  const node = tree.resolveInner(from, 1);
  return tokens.test(node.name) && node.from < from ? Math.min(node.to, end) : from;
}
const SQL_TOKENS = /String|Comment|QuotedIdentifier/;
/** JSON's strings, as values and as keys. */
const JSON_TOKENS = /String|PropertyName/;

/** Where reading the code before `end` starts: at the start of the line
 *  LOOK_BACK before it (a one-line string or a `//` comment is read whole;
 *  a string or block comment spanning lines that opened before that line
 *  isn't seen). On a line longer than that (minified JSON), from LOOK_BACK
 *  before, out of the string `tokens` of the syntax tree say it's in. */
function readFrom(ctx: CompletionContext, end: number, tokens?: RegExp): number {
  if (end <= LOOK_BACK) return 0;
  const from = end - LOOK_BACK;
  const line = ctx.state.doc.lineAt(from);
  return (tokens && from - line.from > LOOK_BACK ? outOf(ctx, from, end, tokens) : null) ?? line.from;
}

/** SQL: from LOOK_BACK before `end`, never from inside a string, a comment
 *  or a quoted name (the syntax tree says where they end; without it, from
 *  the start). */
function sqlReadFrom(ctx: CompletionContext, end: number): number {
  return end <= LOOK_BACK ? 0 : outOf(ctx, end - LOOK_BACK, end, SQL_TOKENS) ?? 0;
}

/** Nothing typed and not asked for: only right after a dot (`sales.`) is
 *  there something to offer. */
function idle(ctx: CompletionContext, from: number, typed: string): boolean {
  return !typed && !ctx.explicit && !/\.\s*$/.test(ctx.state.sliceDoc(Math.max(0, from - 100), from));
}

// -- SQL, Cypher, Flux ---------------------------------------------------------------------------
/** An identifier, bare or quoted ([x], "x", `x`). */
const ID = String.raw`(?:[\w$#]+|\[[^\]\n]+\]|"[^"\n]+"|\x60[^\x60\n]+\x60)`;
/** `a.b.` at the end of a text. */
const QUALIFIER_BEFORE = new RegExp(String.raw`(?:${ID}\s*\.\s*)+$`);
const PATH = String.raw`${ID}(?:\s*\.\s*${ID})*`;

/** The words after which an expression starts. */
const OPENERS: Record<CodeFlavor, Set<string>> = {
  sql: new Set(
    'select distinct where and or not xor on having qualify by when then else case like ilike rlike regexp between in returning return if elseif elsif while print default div mod'.split(' '),
  ),
  cypher: new Set('return with where and or xor not when then else case in unwind distinct by contains'.split(' ')),
  flux: new Set('return and or not if then else'.split(' ')),
};

/** A word right after these is a parameter, a variable or a label, never a
 *  function: T-SQL `@x`, `:bind`, `::type`, Cypher `$param`, `(n:Label)`. */
const ADJACENT: Record<CodeFlavor, RegExp> = { sql: /[@:]$/, cypher: /[$:]$/, flux: /(?!)/ };

/** Before a `(` in SQL, a list of names being defined or listed, not
 *  expressions: `CREATE TABLE t (`, `INSERT INTO t (`, `USING (`,
 *  `PRIMARY KEY (`. */
const NAME_LIST_HEAD = String.raw`\b(?:table|into|view|index|type|function|procedure|trigger)\s+(?:if\s+(?:not\s+)?exists\s+)?${PATH}|\breferences\s+${PATH}|\b(?:using|key|unique)`;
/** The items after a comma in those lists are names too. */
const SQL_NAME_LIST = new RegExp(String.raw`(?:${NAME_LIST_HEAD})\s*$`, 'i');
/** Right after `(`, a window's `OVER (` too (PARTITION BY, ORDER BY or a
 *  window's name come first); the items of its lists are expressions. */
const SQL_NAME_LIST_OPEN = new RegExp(String.raw`(?:${NAME_LIST_HEAD}|\bover)\s*$`, 'i');

/** The innermost bracket still open at the end of `text`: its index and
 *  character, or null. */
function openBracket(text: string): { at: number; ch: string } | null {
  const stack: { at: number; ch: string }[] = [];
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (c === '(' || c === '[' || c === '{') stack.push({ at: i, ch: c });
    else if (c === ')' || c === ']' || c === '}') stack.pop();
  }
  return stack[stack.length - 1] ?? null;
}

/** Where a table goes in SQL: `function` (a table function fits: FROM,
 *  JOIN, LATERAL, APPLY, `TABLE(`, a comma in a FROM list), `name` (only a
 *  table's name: INTO, UPDATE, TABLE), or null. */
export function tableSlotKind(before: string): 'function' | 'name' | null {
  if (/\b(?:into|update|table)\s+$/i.test(before)) return 'name';
  if (isTableSlot(before) || /\b(?:lateral|apply)\s+$/i.test(before) || /\btable\s*\(\s*$/i.test(before)) return 'function';
  return null;
}

/** An expression starts after `before` (the code before the word, strings
 *  and comments blanked, from the statement's start). */
export function exprSlot(before: string, flavor: CodeFlavor): boolean {
  // trimEnd, not `/\s+$/`: that one backtracks over every run of blanks
  // (a blanked string) and costs the square of its length.
  const trimmed = before.trimEnd();
  if (!trimmed) return flavor === 'flux';
  // T-SQL's `SELECT TOP 10 |`, `SELECT TOP (5) PERCENT |`.
  if (flavor === 'sql' && /\btop\s*(?:\d+|\([^()]*\))(?:\s+percent)?(?:\s+with\s+ties)?$/i.test(trimmed)) return true;
  const last = trimmed[trimmed.length - 1];
  // Flux: a new line starts a new statement (a call, a pipe), at the top
  // level or in a function's body (`=> {`); inside a call's parameters, a
  // record or a list it's the next parameter, key or item, as on one line.
  if (flavor === 'flux' && before.slice(trimmed.length).includes('\n')) {
    const open = openBracket(trimmed);
    if (!open || (open.ch === '{' && /=>\s*$/.test(trimmed.slice(0, open.at)))) return true;
  }
  if (last === '(') {
    const head = trimmed.slice(0, -1);
    if (flavor === 'flux') return false; // a call's first parameter name, a lambda's parameters
    if (flavor === 'cypher') {
      // A node pattern, and a path's (`MATCH p = (`): a variable or a label goes there.
      return head.trim() !== ''
        && !/(?:[-<>:,]|\b(?:match|merge|create|optional|delete|detach|remove|foreach))\s*$/i.test(head)
        && !/\b(?:match|merge|create)\s+\w+\s*=\s*$/i.test(head);
    }
    return !SQL_NAME_LIST_OPEN.test(head);
  }
  // Cypher: a relationship pattern (`-[`, `<-[`): its variable.
  if (last === '[' && flavor === 'cypher' && /[-<]\s*$/.test(trimmed.slice(0, -1))) return false;
  if (last === ',') {
    const open = openBracket(trimmed);
    if (flavor === 'flux') return open?.ch === '[';
    if (open?.ch === '{') return false; // a map's next key
    if (flavor === 'sql' && open?.ch === '(') return !SQL_NAME_LIST.test(trimmed.slice(0, open.at));
    return true;
  }
  if (last === '*') return !/(?:^|\bselect|\bdistinct|\breturn|\bwith|,|\(|\.)\s*\*$/i.test(trimmed);
  if (last === ':') return flavor !== 'sql';
  if ('=<>+-/%|&^~![?'.includes(last)) return true;
  const word = /[\w$#@]+$/.exec(trimmed);
  if (!word) return false;
  if (trimmed[word.index - 1] === '.') return false;
  const w = word[0].toLowerCase();
  if (/^\d/.test(w)) return false;
  if (w === 'not' && /\bis\s+not$/i.test(trimmed)) return false;
  return OPENERS[flavor].has(w);
}

const WORD: Record<CodeFlavor, RegExp> = { sql: /[\w$#]*$/, cypher: /\w*$/, flux: /\w*$/ };
const VALID: Record<CodeFlavor, RegExp> = { sql: /^[\w$#]*$/, cypher: /^\w*$/, flux: /^\w*$/ };

function codeCompletion(ctx: CompletionContext, index: FunctionIndex, flavor: CodeFlavor): CompletionResult | null {
  const word = ctx.matchBefore(WORD[flavor]) ?? { from: ctx.pos, text: '' };
  // The cheap checks first: a number, or nothing typed where nothing was asked for.
  if (/^\d/.test(word.text) || idle(ctx, word.from, word.text)) return null;
  let before: string;
  if (flavor === 'sql') {
    if (inStringOrComment(ctx)) return null;
    const masked = maskSql(ctx.state.sliceDoc(sqlReadFrom(ctx, word.from), word.from));
    before = masked.slice(statementAt(masked, masked.length).from);
  } else {
    const scan = scanCode(ctx.state.sliceDoc(readFrom(ctx, word.from), word.from), LEX[flavor]);
    if (scan.inside !== 'code') return null;
    before = flavor === 'cypher' ? scan.masked.slice(scan.masked.lastIndexOf(';') + 1) : scan.masked;
  }
  const done = (options: Completion[]): CompletionResult | null =>
    (options.length ? { from: word.from, options, validFor: VALID[flavor], getMatch: matchShown } : null);

  // After a dot: only what the name before it is the schema or package of.
  const q = QUALIFIER_BEFORE.exec(before);
  if (q) {
    const lead = before.slice(0, q.index);
    const slot = flavor === 'sql' ? tableSlotKind(lead) : null;
    if (slot === 'name') return null;
    return done(qualifiedOptions(slot === 'function' ? index.table : index.expr, parentsBefore(q[0]), index.overloads));
  }
  if (/\.\s*$/.test(before) || (!word.text && !ctx.explicit)) return null;
  if (ADJACENT[flavor].test(before)) return null;
  if (flavor === 'sql') {
    const slot = tableSlotKind(before);
    if (slot === 'name') return null;
    if (slot === 'function') return done(index.tableOptions);
  }
  return exprSlot(before, flavor) ? done(index.exprOptions) : null;
}

// -- Redis, etcd -----------------------------------------------------------------------------------
const COMMAND_VALID = /^[^\s"'#]*(?: [^\s"'#]*)?$/;

function commandCompletion(ctx: CompletionContext, index: FunctionIndex): CompletionResult | null {
  if (!index.commandOptions.length) return null;
  const line = ctx.state.doc.lineAt(ctx.pos);
  const before = ctx.state.sliceDoc(line.from, ctx.pos);
  const first = /^(\s*)([^\s"'#]*)$/.exec(before);
  if (first) {
    if (!first[2] && !ctx.explicit) return null;
    return { from: line.from + first[1].length, options: index.commandOptions, validFor: COMMAND_VALID };
  }
  // A container's subcommand: `CLIENT K` → `CLIENT KILL`.
  const second = /^(\s*)([^\s"'#]+) ([^\s"'#]*)$/.exec(before);
  if (!second) return null;
  const head = `${second[2].toLowerCase()} `;
  const subs = index.commandOptions.filter((o) => o.label.toLowerCase().startsWith(head));
  return subs.length ? { from: line.from + second[1].length, options: subs, validFor: COMMAND_VALID } : null;
}

// -- JSON ------------------------------------------------------------------------------------------
const OPERATOR_VALID = /^\$[\w$.]*$/;

/** `rest` (after the cursor, to the line's end) closes the string being
 *  typed before a `:`. */
const CLOSES_KEY: Record<string, RegExp> = { '"': /^(?:[^"\\]|\\.)*"\s*:/, "'": /^(?:[^'\\]|\\.)*'\s*:/ };

/** A key goes after `before` (masked): it follows `{`, or a comma inside an
 *  object. */
function keySlot(before: string): boolean {
  const trimmed = before.trimEnd();
  const last = trimmed[trimmed.length - 1];
  return last === '{' || (last === ',' && openBracket(trimmed)?.ch === '{');
}

/** The string opened at `start` (in `masked`) with `quote` is an object's
 *  key: it's where a key goes, or `rest` closes it before a `:`. */
function jsonKeyAt(masked: string, start: number, rest: string, quote: string): boolean {
  return !!CLOSES_KEY[quote]?.test(rest) || keySlot(masked.slice(0, start));
}

function jsonCompletion(ctx: CompletionContext, index: FunctionIndex): CompletionResult | null {
  // The cheap checks first: a number, or nothing typed where nothing was asked for.
  const typed = ctx.matchBefore(/[\w$]*$/) ?? { from: ctx.pos, text: '' };
  if (/^\d/.test(typed.text) || idle(ctx, typed.from, typed.text)) return null;
  const from = readFrom(ctx, ctx.pos, JSON_TOKENS);
  const text = ctx.state.sliceDoc(from, ctx.pos);
  const scan = scanCode(text, LEX.json);
  if (scan.inside === 'comment') return null;
  const rest = () => ctx.state.sliceDoc(ctx.pos, ctx.state.doc.lineAt(ctx.pos).to);
  if (scan.inside === 'string') {
    if (scan.quote === '/') return null;
    const start = from + scan.stringStart + 1;
    const content = text.slice(scan.stringStart + 1);
    // `"$ma|"`: an operator, at a key only (a `$` value is a field path or a variable).
    if (content.startsWith('$')) {
      return OPERATOR_VALID.test(content) && index.operatorOptions.length && jsonKeyAt(scan.masked, scan.stringStart, rest(), scan.quote)
        ? { from: start, options: index.operatorOptions, validFor: OPERATOR_VALID }
        : null;
    }
    // A string value of a catalog without operators: function queries.
    if (scan.quote === "'" || !index.exprOptions.length || index.operatorOptions.length) return null;
    if (jsonKeyAt(scan.masked, scan.stringStart, rest(), scan.quote)) return null;
    const word = ctx.matchBefore(/\w*$/);
    const wordFrom = Math.max(word?.from ?? ctx.pos, start);
    const typedHere = ctx.state.sliceDoc(wordFrom, ctx.pos);
    if ((!typedHere && !ctx.explicit) || /^\d/.test(typedHere) || ctx.state.sliceDoc(wordFrom - 1, wordFrom) === '.') return null;
    return { from: wordFrom, options: index.exprOptions, validFor: /^\w*$/, getMatch: matchShown };
  }
  // A bare `$word`: an unquoted key (the MongoDB shell's relaxed JSON).
  const op = ctx.matchBefore(/\$[\w$.]*$/);
  if (op && !/[\w$.]/.test(ctx.state.sliceDoc(op.from - 1, op.from))) {
    const key = /^[\w$.]*\s*:/.test(rest()) || keySlot(scan.masked.slice(0, op.from - from));
    return key && index.operatorOptions.length ? { from: op.from, options: index.operatorOptions, validFor: OPERATOR_VALID } : null;
  }
  // After a dot that follows a name, `)` or `]`: a method (`db.orders.fi|`,
  // `).so|`); never after a number's decimal point (`1.`).
  const word = ctx.matchBefore(/\w*$/) ?? { from: ctx.pos, text: '' };
  const before = scan.masked.slice(0, word.from - from);
  if (/^\d/.test(word.text) || !index.methodOptions.length) return null;
  if (!/[\w$)\]]\s*\.\s*$/.test(before) || /(?:^|[^\w$.])\d+\s*\.\s*$/.test(before)) return null;
  const q = QUALIFIER_BEFORE.exec(before);
  const own = q ? qualifiedOptions(index.expr.filter((g) => g.entries.some((e) => e.kind === 'scalar')), parentsBefore(q[0]), index.overloads, false) : [];
  return { from: word.from, options: own.length ? own : index.methodOptions, validFor: /^\w*$/ };
}

// -- the source -----------------------------------------------------------------------------------
/** The catalog's entries that fit where the cursor is, or null. */
export function functionCompletion(ctx: CompletionContext, index: FunctionIndex, flavor: FunctionFlavor): CompletionResult | null {
  if (!offersAny(index, flavor)) return null;
  switch (flavor) {
    case 'commands': return commandCompletion(ctx, index);
    case 'json': return jsonCompletion(ctx, index);
    default: return codeCompletion(ctx, index, flavor);
  }
}

/** The keywords without those the functions offered here name (`LEFT`,
 *  `REPLACE` in MySQL): the function stands for them. */
export function withoutFunctionNames(keywords: CompletionResult, functions: CompletionResult | null): CompletionResult {
  // Only the names written bare: `sales.left` doesn't stand for LEFT.
  const names = new Set((functions?.options ?? []).filter((o) => !o.displayLabel).map((o) => o.label.toLowerCase()));
  if (!names.size) return keywords;
  const options = keywords.options.filter((o) => !names.has(o.label.toLowerCase()));
  return options.length === keywords.options.length ? keywords : { ...keywords, options };
}
