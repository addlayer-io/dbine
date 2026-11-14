import { StreamLanguage, syntaxTree, type StreamParser, type StringStream } from '@codemirror/language';
import type { Completion, CompletionContext, CompletionResult } from '@codemirror/autocomplete';
import type { EditorState } from '@codemirror/state';
import type { DbObject, FunctionInfo } from '../api/types';
import { callApply } from './functionCompletion';

// DAX in the query editor (the Power BI driver says `language: "dax"`): a
// tokenizer for highlighting, the completion of the model's names, and the
// lexical helpers the rest of the editor reads names with (nameRefs.ts,
// tableHover.ts). The engine's functions come from its catalog through
// functionCompletion.ts (the `dax` flavor), not from here.
//
// How DAX writes things (learn.microsoft.com/dax/dax-syntax-reference):
// - comments: `//` and `--` to the line's end, `/* … */` blocks;
// - strings: "…", a quote inside doubled ("");
// - table names: 'Sales Order', a quote inside doubled (''); a plain ASCII
//   name may go bare (Sales);
// - columns and measures: [Amount], a `]` inside doubled (]]), after their
//   table ('Sales'[Amount], Sales[Amount]) or alone ([Total Sales]);
// - functions: a name before `(`, dots included (INFO.VIEW.TABLES, STDEVX.S);
// - a query: DEFINE (MEASURE, VAR, TABLE, COLUMN, FUNCTION) before its first
//   EVALUATE, then EVALUATE <table> [ORDER BY …] [START AT …], as many as
//   wanted. Where a unit starts and ends follows the driver's splitter
//   (crates/drivers/powerbi/src/dax/split.rs): DEFINE starts one; EVALUATE
//   starts one unless the current unit started with DEFINE, whose
//   definitions apply to every EVALUATE after it.
//
// Completion: after `'`, the model's tables; after `'Table'[` or `Table[`,
// that table's columns, then its measures; after a bare `[`, the measures,
// the DEFINE MEASURE / COLUMN names of the unit and the columns of the
// tables the unit names; on a bare word, the tables, the unit's VAR names,
// the measures (inserted with their brackets), the model's functions the
// catalog doesn't list, and the query's keywords. After DEFINE MEASURE or
// COLUMN only the tables (the new name's table comes first), and nothing
// where a name is given: after VAR, TABLE or FUNCTION, and in a
// user-defined function's parameter list.

/** The query's words (ORDER BY and START AT as pairs). TRUE, FALSE, NOT and
 *  BLANK are functions too: the catalog offers those. */
export const DAX_KEYWORDS = [
  'DEFINE', 'EVALUATE', 'ORDER BY', 'START AT', 'ASC', 'DESC', 'MEASURE', 'VAR', 'RETURN', 'TABLE', 'COLUMN', 'FUNCTION', 'IN', 'NOT',
];
/** The words highlighted as keywords. */
const KEYWORD_WORDS = new Set(['DEFINE', 'EVALUATE', 'ORDER', 'BY', 'START', 'AT', 'ASC', 'DESC', 'MEASURE', 'VAR', 'RETURN', 'TABLE', 'COLUMN', 'FUNCTION', 'IN', 'NOT']);
/** Constants written without parentheses. */
const BOOLS = new Set(['TRUE', 'FALSE']);

/** The object kinds of the Power BI driver whose names completion offers
 *  besides the tables (crates/drivers/powerbi: `measure` carries its home
 *  table as `parent`; `function` is a model's DAX user-defined function;
 *  `hierarchy` is named `'Table'[Hierarchy]` and has levels, so the
 *  explorer lists it with the tables: it's left out of them). */
const OTHER_KINDS = new Set(['measure', 'function', 'hierarchy']);

/** A name completion offers besides the tables. */
export interface DaxOther {
  label: string;
  kind: string;
  /** A measure's home table. */
  parent?: string | null;
}

/** The objects of a model completion reads besides `schema`'s tables. */
export function daxOtherNames(objects: readonly DbObject[]): DaxOther[] {
  return objects.filter((o) => OTHER_KINDS.has(o.kind)).map((o) => ({ label: o.name, kind: o.kind, parent: o.parent }));
}

// -- tokens -----------------------------------------------------------------------------------

export type DaxTokenType = 'comment' | 'string' | 'name' | 'ref' | 'word' | 'number' | 'punct';

export interface DaxToken {
  type: DaxTokenType;
  start: number;
  end: number;
  /** A name's or a ref's name (quotes and brackets off, doubled ones made
   *  single); a word's or a punctuation's text; '' for the rest. */
  value: string;
  /** A comment, string, name or ref that reached the text's end (or, for a
   *  name or a ref, the line's end) without its closing character. */
  open?: boolean;
}

const WORD_START = /[\p{L}_]/u;
const WORD_PART = /[\p{L}\p{N}_]/u;
const DIGIT = /\d/;

/** `text` cut into DAX tokens (blanks left out). */
export function daxTokens(text: string): DaxToken[] {
  const out: DaxToken[] = [];
  const n = text.length;
  let i = 0;
  /** The end of a run quoted by `close`, which a doubled `close` doesn't end. */
  const quoted = (from: number, close: string, oneLine: boolean): { end: number; open: boolean } => {
    let j = from + 1;
    while (j < n) {
      const c = text[j];
      if (c === close) {
        if (text[j + 1] === close) { j += 2; continue; }
        return { end: j + 1, open: false };
      }
      if (oneLine && c === '\n') return { end: j, open: true };
      j++;
    }
    return { end: n, open: true };
  };
  while (i < n) {
    const c = text[i];
    const two = text.slice(i, i + 2);
    if (c === ' ' || c === '\t' || c === '\n' || c === '\r') { i++; continue; }
    if (two === '//' || two === '--') {
      const e = text.indexOf('\n', i);
      const end = e < 0 ? n : e;
      out.push({ type: 'comment', start: i, end, value: '', ...(e < 0 ? { open: true } : {}) });
      i = end;
    } else if (two === '/*') {
      const e = text.indexOf('*/', i + 2);
      const end = e < 0 ? n : e + 2;
      out.push({ type: 'comment', start: i, end, value: '', ...(e < 0 ? { open: true } : {}) });
      i = end;
    } else if (c === '"') {
      const q = quoted(i, '"', false);
      out.push({ type: 'string', start: i, end: q.end, value: '', ...(q.open ? { open: true } : {}) });
      i = q.end;
    } else if (c === "'") {
      const q = quoted(i, "'", true);
      const inner = text.slice(i + 1, q.open ? q.end : q.end - 1);
      out.push({ type: 'name', start: i, end: q.end, value: inner.replace(/''/g, "'"), ...(q.open ? { open: true } : {}) });
      i = q.end;
    } else if (c === '[') {
      const q = quoted(i, ']', true);
      const inner = text.slice(i + 1, q.open ? q.end : q.end - 1);
      out.push({ type: 'ref', start: i, end: q.end, value: inner.replace(/]]/g, ']'), ...(q.open ? { open: true } : {}) });
      i = q.end;
    } else if (DIGIT.test(c) || (c === '.' && DIGIT.test(text[i + 1] ?? ''))) {
      const m = /^(?:\d+\.?\d*|\.\d+)(?:[eE][-+]?\d+)?/.exec(text.slice(i, i + 64))!;
      out.push({ type: 'number', start: i, end: i + m[0].length, value: m[0] });
      i += m[0].length;
    } else if (WORD_START.test(c)) {
      let j = i + 1;
      for (;;) {
        while (j < n && WORD_PART.test(text[j])) j++;
        // INFO.VIEW.TABLES, STDEVX.S: the dot joins two parts of a name.
        if (text[j] === '.' && WORD_START.test(text[j + 1] ?? '')) { j++; continue; }
        break;
      }
      out.push({ type: 'word', start: i, end: j, value: text.slice(i, j) });
      i = j;
    } else {
      const op = /^(?:&&|\|\||==|<>|<=|>=)/.exec(text.slice(i, i + 2));
      const len = op ? 2 : 1;
      out.push({ type: 'punct', start: i, end: i + len, value: text.slice(i, i + len) });
      i += len;
    }
  }
  return out;
}

const upper = (t: DaxToken | undefined) => (t?.type === 'word' ? t.value.toUpperCase() : '');

/** Where each unit of `tokens` starts (the index of its DEFINE or EVALUATE
 *  token), as the driver's splitter cuts the script; a script's leading
 *  text before the first keyword is no unit. */
export function daxUnitStarts(tokens: readonly DaxToken[]): number[] {
  const out: number[] = [];
  let depth = 0;
  let defined = false;
  for (let i = 0; i < tokens.length; i++) {
    const t = tokens[i];
    if (t.type === 'punct') {
      if (t.value === '(' || t.value === '{') depth++;
      else if ((t.value === ')' || t.value === '}') && depth > 0) depth--;
      continue;
    }
    if (depth > 0 || t.type !== 'word') continue;
    const w = upper(t);
    if (w === 'DEFINE') {
      out.push(i);
      defined = true;
    } else if (w === 'EVALUATE' && !defined) {
      // After a DEFINE, an EVALUATE stays in its unit: the definitions hold for it.
      out.push(i);
    }
  }
  return out;
}

/** The tokens of the unit around token index `at`: from its DEFINE or
 *  EVALUATE to the next unit's. */
export function daxUnitAround(tokens: readonly DaxToken[], at: number): DaxToken[] {
  const starts = daxUnitStarts(tokens);
  let from = 0;
  let to = tokens.length;
  for (const s of starts) {
    if (s <= at) from = s;
    else { to = s; break; }
  }
  return tokens.slice(from, to);
}

/** The names a unit makes itself: VAR names, DEFINE MEASURE / COLUMN names
 *  (written `[x]`), DEFINE TABLE names. */
export function daxDefinedNames(unit: readonly DaxToken[]): { vars: string[]; refs: string[]; tables: string[] } {
  const vars: string[] = [];
  const refs: string[] = [];
  const tables: string[] = [];
  const add = (list: string[], v: string) => { if (v && !list.includes(v)) list.push(v); };
  for (let i = 0; i < unit.length; i++) {
    const w = upper(unit[i]);
    const next = unit[i + 1];
    if (w === 'VAR' && next?.type === 'word') add(vars, next.value);
    else if (w === 'MEASURE' || w === 'COLUMN') {
      // MEASURE 'T'[M] = …, MEASURE T[M] = … (the table is required).
      const ref = (next?.type === 'name' || next?.type === 'word') && unit[i + 2]?.type === 'ref' ? unit[i + 2] : null;
      if (ref && !ref.open) add(refs, ref.value);
    } else if (w === 'TABLE' && (next?.type === 'name' || next?.type === 'word') && unit[i + 2]?.value === '=') {
      add(tables, next.value);
    }
  }
  return { vars, refs, tables };
}

/** The tables of `tables` that `unit` names, quoted or bare, in their own
 *  spelling. */
export function daxTablesNamed(unit: readonly DaxToken[], tables: readonly string[]): string[] {
  const byLower = new Map(tables.map((t) => [t.toLowerCase(), t] as const));
  const out: string[] = [];
  for (let i = 0; i < unit.length; i++) {
    const t = unit[i];
    // A bare word is a table where it isn't a function's name.
    const isName = t.type === 'name' || (t.type === 'word' && unit[i + 1]?.value !== '(');
    const found = isName ? byLower.get(t.value.toLowerCase()) : undefined;
    if (found && !out.includes(found)) out.push(found);
  }
  return out;
}

/** The table name right before token index `i` (a `[ref]`'s owner):
 *  `'Sales'` or a bare `Sales`, with nothing else between. */
function ownerBefore(tokens: readonly DaxToken[], i: number, text: string): DaxToken | null {
  const prev = tokens[i - 1];
  if (!prev || (prev.type !== 'name' && prev.type !== 'word') || prev.open) return null;
  // Only blanks between (DAX takes `'Sales' [Amount]`), never a line break.
  if (!/^[ \t]*$/.test(text.slice(prev.end, tokens[i].start))) return null;
  if (prev.type === 'word' && (KEYWORD_WORDS.has(prev.value.toUpperCase()) || prev.value.includes('.'))) return null;
  return prev;
}

/** Whether the end of `tokens` is in a user-defined function's parameter
 *  list (`FUNCTION f = (a : SCALAR, b`): parameter names and their types
 *  go there, not the model's names. */
function inFunctionParams(tokens: readonly DaxToken[]): boolean {
  const open: number[] = [];
  for (let i = 0; i < tokens.length; i++) {
    const t = tokens[i];
    if (t.type !== 'punct') continue;
    if (t.value === '(' || t.value === '{') open.push(i);
    else if ((t.value === ')' || t.value === '}') && open.length) open.pop();
  }
  const at = open[open.length - 1];
  if (at === undefined || tokens[at].value !== '(') return false;
  return tokens[at - 1]?.value === '=' && tokens[at - 2]?.type === 'word' && upper(tokens[at - 3]) === 'FUNCTION';
}

/** What a position of `doc` names, lexically: a table (`'Sales'`, or a bare
 *  word), or a column/measure reference (`[x]`) with its table when one is
 *  written before it. Null in strings, comments, numbers and punctuation. */
export type DaxRef =
  | { kind: 'table'; from: number; to: number; name: string; quoted: boolean }
  | { kind: 'ref'; from: number; to: number; name: string; table: string | null };

export function daxRefAt(doc: string, pos: number): DaxRef | null {
  const tokens = daxTokens(doc);
  // The token under `pos` (its end counts: the pointer right after a word).
  let i = tokens.findIndex((t) => pos >= t.start && pos < t.end);
  if (i < 0) i = tokens.findIndex((t) => pos === t.end && (t.type === 'word' || t.type === 'name' || t.type === 'ref'));
  if (i < 0) return null;
  const t = tokens[i];
  if (t.type === 'name') return t.value ? { kind: 'table', from: t.start, to: t.end, name: t.value, quoted: true } : null;
  if (t.type === 'word') {
    // A function's name, a keyword, a dotted name: no table.
    if (tokens[i + 1]?.value === '(' || KEYWORD_WORDS.has(t.value.toUpperCase()) || BOOLS.has(t.value.toUpperCase()) || t.value.includes('.')) return null;
    return { kind: 'table', from: t.start, to: t.end, name: t.value, quoted: false };
  }
  if (t.type === 'ref' && t.value) {
    const owner = ownerBefore(tokens, i, doc);
    return { kind: 'ref', from: t.start, to: t.end, name: t.value, table: owner?.value ?? null };
  }
  return null;
}

// -- highlighting -------------------------------------------------------------------------------

interface DaxState {
  /** Inside a `/* … *\/` comment that goes on from an earlier line. */
  block: boolean;
  /** Inside a "string" that goes on from an earlier line. */
  string: boolean;
}

/** The rest of a "string" from the stream's position: true when it closed. */
function readString(s: StringStream): boolean {
  for (let ch = s.next(); ch !== undefined; ch = s.next()) {
    if (ch === '"') {
      if (s.peek() === '"') { s.next(); continue; }
      return true;
    }
  }
  return false;
}

function token(s: StringStream, st: DaxState): string | null {
  if (st.block) {
    if (s.skipTo('*/')) { s.match('*/'); st.block = false; } else s.skipToEnd();
    return 'comment';
  }
  if (st.string) {
    st.string = !readString(s);
    return 'string';
  }
  if (s.eatSpace()) return null;
  if (s.match('//') || s.match('--')) { s.skipToEnd(); return 'comment'; }
  if (s.match('/*')) {
    if (s.skipTo('*/')) s.match('*/'); else { s.skipToEnd(); st.block = true; }
    return 'comment';
  }
  const c = s.peek()!;
  if (c === '"') {
    s.next();
    st.string = !readString(s);
    return 'string';
  }
  if (c === "'" || c === '[') {
    const close = c === "'" ? "'" : ']';
    s.next();
    for (let ch = s.next(); ch !== undefined; ch = s.next()) {
      if (ch === close) {
        if (s.peek() === close) { s.next(); continue; }
        break;
      }
    }
    // Table names as types, columns and measures as properties: not
    // strings, so completion runs inside them.
    return c === "'" ? 'typeName' : 'propertyName';
  }
  if (s.match(/^(?:\d+\.?\d*|\.\d+)(?:[eE][-+]?\d+)?/)) return 'number';
  const id = s.match(/^[\p{L}_][\p{L}\p{N}_]*(?:\.[\p{L}_][\p{L}\p{N}_]*)*/u) as RegExpMatchArray | null;
  if (id) {
    const w = id[0].toUpperCase();
    if (s.match(/^\s*\(/, false)) return 'variableName.function';
    if (KEYWORD_WORDS.has(w)) return 'keyword';
    if (BOOLS.has(w)) return 'bool';
    return 'variableName';
  }
  if (s.match(/^(?:&&|\|\||==|<>|<=|>=|[=<>+\-*/^&])/)) return 'operator';
  if (s.match(/^[(){}]/)) return 'bracket';
  if (s.eat(',')) return 'separator';
  s.next();
  return null;
}

export const daxParser: StreamParser<DaxState> = {
  name: 'dax',
  startState: () => ({ block: false, string: false }),
  copyState: (st) => ({ ...st }),
  token,
  languageData: { commentTokens: { line: '//', block: { open: '/*', close: '*/' } } },
};

export const daxLanguage = StreamLanguage.define(daxParser);

/** Whether `pos` is in a 'table name' or a [column or measure] reference
 *  (or right at its end) of a DAX editor: no snippet or parameter goes
 *  there. The tokenizer tags them as types and properties, not strings. */
export function daxInName(state: EditorState, pos: number): boolean {
  if (!daxLanguage.isActiveAt(state, pos)) return false;
  return /^(?:typeName|propertyName)$/.test(syntaxTree(state).resolveInner(pos, -1).name);
}

// -- completion ---------------------------------------------------------------------------------

/** What completion knows of the model. */
export interface DaxNames {
  /** Tables (and calculation groups) → their loaded columns. */
  tables: Record<string, string[]>;
  /** Measures (with their home table), the model's functions, hierarchies. */
  others: readonly DaxOther[];
  /** The function catalog, when loaded: a table named like a function is
   *  written quoted, and the model's functions it lists aren't offered
   *  twice. */
  functions?: readonly FunctionInfo[] | null;
}

/** How far around the cursor the text is read. */
const LOOK_AROUND = 10_000;

/** A table's name as DAX writes it bare where it can, quoted where it must
 *  (blanks or other characters, a keyword or a function's name). */
export function daxTableName(name: string, reserved: ReadonlySet<string> = new Set()): string {
  const plain = /^[A-Za-z_][A-Za-z0-9_]*$/.test(name) && !KEYWORD_WORDS.has(name.toUpperCase()) && !BOOLS.has(name.toUpperCase())
    && !reserved.has(name.toUpperCase());
  return plain ? name : `'${name.replace(/'/g, "''")}'`;
}

const refText = (name: string) => `[${name.replace(/]/g, ']]')}]`;

/** The matched characters of an option shown with brackets or quotes
 *  around its label. */
function shiftedMatch(c: Completion, matched?: readonly number[]): readonly number[] {
  if (!matched) return [];
  return c.displayLabel && c.displayLabel !== c.label ? matched.map((p) => p + 1) : matched;
}

/** The completions at the context's position, and the tables whose columns
 *  they'd need and aren't loaded yet (`needColumns`). */
export function daxCompletion(ctx: CompletionContext, names: DaxNames): { result: CompletionResult | null; needColumns: string[] } {
  const none = { result: null, needColumns: [] };
  const { doc } = ctx.state;
  const pos = ctx.pos;
  const start = doc.lineAt(Math.max(0, pos - LOOK_AROUND)).from;
  const end = doc.lineAt(Math.min(doc.length, pos + LOOK_AROUND)).to;
  const text = doc.sliceString(start, end);
  const at = pos - start;
  const before = daxTokens(text.slice(0, at));
  const last = before[before.length - 1];
  const inside = last?.open && last.end === at ? last : null;
  if (inside && (inside.type === 'comment' || inside.type === 'string')) return none;

  const hierarchies = new Set(names.others.filter((o) => o.kind === 'hierarchy').map((o) => o.label));
  const tables = Object.keys(names.tables).filter((t) => !hierarchies.has(t));
  const measures = names.others.filter((o) => o.kind === 'measure');
  const lineRest = doc.sliceString(pos, doc.lineAt(pos).to);

  // The unit around the cursor: what it names and defines.
  const all = daxTokens(text);
  const index = all.findIndex((t) => t.end >= at);
  const unit = daxUnitAround(all, index < 0 ? all.length - 1 : index);
  const defined = daxDefinedNames(unit);

  // Inside 'a table name. The pick replaces the rest of the name up to its
  // closing quote too (the one the editor added, or the name's end when
  // editing its middle), unless that quote opens the next name: a `'` typed
  // before `FILTER('X', 1)` isn't paired, and FILTER isn't this name.
  if (inside?.type === 'name') {
    const close = /^[^'\n]*'/.exec(lineRest);
    const ownClose = close && !/^[\p{L}\p{N}_]/u.test(lineRest.slice(close[0].length));
    const options: Completion[] = [...tables, ...defined.tables].map((t) => ({
      label: t, type: 'class', apply: `${t.replace(/'/g, "''")}'`,
    }));
    if (!options.length) return none;
    return { result: { from: start + inside.start + 1, to: ownClose ? pos + close[0].length : pos, options, validFor: /^[^'\n]*$/ }, needColumns: [] };
  }

  // Inside [a column or a measure. The same with its closing `]`, unless
  // the run up to it is other code: another reference (`[`) or a table's
  // name right after the bracket (`[‸'Sales'[Amount]`). A `'` further in
  // is the name's own (`[Last Year's Sales]`).
  if (inside?.type === 'ref') {
    const run = /^[^\]\n]*\]/.exec(lineRest)?.[0];
    const close = run !== undefined && !run.includes('[') && !/^\s*'/.test(run) ? run : null;
    const owner = ownerBefore(before, before.length - 1, text);
    const options: Completion[] = [];
    const seen = new Set<string>();
    const push = (o: Completion) => {
      if (seen.has(o.label)) return;
      seen.add(o.label);
      options.push({ ...o, apply: `${o.label.replace(/]/g, ']]')}]` });
    };
    let needColumns: string[] = [];
    const table = owner ? tables.find((t) => t === owner.value) ?? tables.find((t) => t.toLowerCase() === owner.value.toLowerCase()) : undefined;
    // A quoted name the model doesn't have: nothing known to offer. A bare
    // word that isn't a table (`RETURN [`, a VAR): a reference on its own.
    if (owner?.type === 'name' && !table) return none;
    if (table) {
      if (!names.tables[table].length) needColumns = [table];
      for (const c of names.tables[table]) push({ label: c, type: 'property', boost: 2 });
      for (const m of measures) if (m.parent && m.parent.toLowerCase() === table.toLowerCase()) push({ label: m.label, type: 'variable', boost: 1 });
    } else {
      for (const r of defined.refs) push({ label: r, type: 'variable', boost: 3 });
      for (const m of measures) push({ label: m.label, type: 'variable', detail: m.parent ?? undefined, boost: 2 });
      const named = daxTablesNamed(unit, tables);
      needColumns = named.filter((t) => !names.tables[t].length);
      for (const t of named) for (const c of names.tables[t]) push({ label: c, type: 'property', detail: t, boost: 1 });
    }
    const result = options.length ? { from: start + inside.start + 1, to: close ? pos + close.length : pos, options, validFor: /^[^\]\n]*$/ } : null;
    return { result, needColumns };
  }

  // A bare word.
  const word = ctx.matchBefore(/[\p{L}\p{N}_.]*$/u);
  const typed = word?.text ?? '';
  if (typed.includes('.') || /^\d/.test(typed)) return none;
  if (!typed && !ctx.explicit) return none;
  // Right after a name or a reference (`'T'x`, `[M]x`): nothing goes there.
  if (last && !typed && (last.type === 'name' || last.type === 'ref') && last.end === at) return none;
  // Where a VAR, a DEFINE TABLE or a DEFINE FUNCTION is named, or a
  // function's parameter (`FUNCTION f = (a : SCALAR, b`): a new name.
  const prev = typed ? before[before.length - 2] : last;
  if (['VAR', 'TABLE', 'FUNCTION'].includes(upper(prev)) || inFunctionParams(before)) return none;

  const reserved = new Set((names.functions ?? []).map((f) => f.name.toUpperCase()));
  const options: Completion[] = [];
  for (const t of tables) {
    const written = daxTableName(t, reserved);
    options.push({ label: t, type: 'class', boost: 1, ...(written !== t ? { displayLabel: written, apply: written } : {}) });
  }
  for (const t of defined.tables) options.push({ label: t, type: 'class', boost: 1, apply: daxTableName(t, reserved) });
  const result = (): CompletionResult => ({ from: word?.from ?? pos, options, validFor: /^[\p{L}\p{N}_]*$/u, getMatch: shiftedMatch });
  // DEFINE MEASURE / COLUMN: the new name's table comes first
  // (`MEASURE 'Sales'[Margin] = …`), so only the tables.
  if (['MEASURE', 'COLUMN'].includes(upper(prev))) return options.length ? { result: result(), needColumns: [] } : none;
  for (const v of defined.vars) options.push({ label: v, type: 'variable', boost: 2 });
  for (const m of measures) options.push({ label: m.label, displayLabel: refText(m.label), apply: refText(m.label), type: 'variable', detail: m.parent ?? undefined });
  for (const r of defined.refs) options.push({ label: r, displayLabel: refText(r), apply: refText(r), type: 'variable', boost: 1 });
  // The model's functions the catalog doesn't list (a static catalog).
  for (const f of names.others) {
    if (f.kind === 'function' && !reserved.has(f.label.toUpperCase())) options.push({ label: f.label, type: 'function', apply: callApply(f.label) });
  }
  // A keyword that's a function too (NOT): the catalog's entry stands for it.
  for (const k of DAX_KEYWORDS) if (!reserved.has(k)) options.push({ label: k, type: 'keyword', boost: -2 });
  return { result: result(), needColumns: [] };
}
