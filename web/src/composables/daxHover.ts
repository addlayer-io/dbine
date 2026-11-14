import type { DbObject, ObjectKindInfo } from '../api/types';
import { daxRefAt, daxTablesNamed, daxTokens, daxUnitAround, type DaxToken } from './daxLanguage';

// Hovering a [column] or a [measure] in the DAX editor (the Power BI driver):
// what the reference under the pointer stands for, lexically and from the
// explorer's objects; tableHoverData.ts reads the details (a column's type,
// key and description, a measure's definition) and CodeEditor.vue shows
// them in the table popup. A table's name is tableHover.ts's `tableAt`.
//
// What a `[name]` stands for, in DAX's order:
// - the unit's own `DEFINE MEASURE` / `COLUMN` of that name (a query measure
//   hides the model's one of the same name);
// - written after its table (`'Sales'[x]`): that table's measure, then its
//   column, then a measure of another table (DAX resolves a measure by its
//   name);
// - alone (`[x]`): the model's measure, a column the query makes itself
//   (`"x", …` in an ADDCOLUMNS, SUMMARIZECOLUMNS, ROW…), then a column of
//   the table that gives it its row context (the table argument of the
//   FILTER, ADDCOLUMNS, SUMX… around it, the closest first), then of the
//   tables its own EVALUATE or DEFINE definition names, then of the rest of
//   the unit's, in their order.

/** A `DEFINE MEASURE` or `DEFINE COLUMN` of the query. */
export interface DaxLocalDef {
  kind: 'measure' | 'column';
  table: string;
  name: string;
  /** Its DAX, as written (the common indentation off). */
  expression: string;
}

/** Where to look for what a reference stands for, in order. */
export type DaxCandidate =
  | { kind: 'local'; def: DaxLocalDef }
  | { kind: 'measure'; object: DbObject }
  /** A column named by a string of the unit (`"Margin"`). */
  | { kind: 'query-column' }
  | { kind: 'columns'; table: DbObject };

/** A `[name]` under the pointer. */
export interface DaxItemRef {
  from: number;
  to: number;
  name: string;
  /** The table written before it, as written; null when alone. */
  written: string | null;
  candidates: DaxCandidate[];
}

/** What a reference turned out to be. */
export type DaxItemHit =
  | { kind: 'local'; def: DaxLocalDef }
  | { kind: 'measure'; object: DbObject }
  | { kind: 'query-column' }
  | { kind: 'column'; table: DbObject; column: string }
  /** A table's columns aren't loaded yet. */
  | { kind: 'loading'; table: DbObject }
  | { kind: 'none' };

const eq = (a: string | null | undefined, b: string) => !!a && a.toLowerCase() === b.toLowerCase();
const upper = (t: DaxToken | undefined) => (t?.type === 'word' ? t.value.toUpperCase() : '');

/** `name` in `list`: the exact spelling first. */
function pick<T>(list: readonly T[], name: string, nameOf: (x: T) => string): T | null {
  return list.find((x) => nameOf(x) === name) ?? list.find((x) => eq(nameOf(x), name)) ?? null;
}

/** `text` with the indentation its lines after the first share taken off. */
function dedent(text: string): string {
  const lines = text.split('\n');
  const rest = lines.slice(1).filter((l) => l.trim());
  const cut = rest.length ? Math.min(...rest.map((l) => /^[ \t]*/.exec(l)![0].length)) : 0;
  return [lines[0], ...lines.slice(1).map((l) => l.slice(Math.min(cut, /^[ \t]*/.exec(l)![0].length)))].join('\n').trimEnd();
}

/** The words that start a definition of a DEFINE, or an EVALUATE. */
const PART_START = new Set(['MEASURE', 'COLUMN', 'TABLE', 'FUNCTION', 'EVALUATE', 'DEFINE']);

/** The token before index `i` that isn't a comment. */
function codeBefore(unit: readonly DaxToken[], i: number): DaxToken | undefined {
  while (--i >= 0) if (unit[i].type !== 'comment') return unit[i];
  return undefined;
}

/** Where each part of `unit` starts (a token index): its DEFINE, each
 *  definition (MEASURE, COLUMN, TABLE, FUNCTION, VAR) and each EVALUATE with
 *  its ORDER BY and START AT; a part runs to the next one's start. A `VAR`
 *  at the top starts a definition of the DEFINE unless it is the
 *  expression's own `VAR … RETURN`: right after the `=` (or a function's
 *  `=>`) or a `RETURN`, or after another of the block's VARs. After an
 *  EVALUATE, every VAR is the query's own. */
function daxPartStarts(unit: readonly DaxToken[]): number[] {
  const out: number[] = [];
  let depth = 0;
  let evaluated = false;
  let inVars = false;
  for (let i = 0; i < unit.length; i++) {
    const t = unit[i];
    if (t.type === 'punct') {
      if (t.value === '(' || t.value === '{') depth++;
      else if ((t.value === ')' || t.value === '}') && depth > 0) depth--;
      continue;
    }
    if (depth > 0) continue;
    const k = upper(t);
    if (PART_START.has(k)) {
      out.push(i);
      inVars = false;
      if (k === 'EVALUATE') evaluated = true;
    } else if (k === 'VAR') {
      const prev = codeBefore(unit, i);
      if ((prev?.type === 'punct' && (prev.value === '=' || prev.value === '>')) || upper(prev) === 'RETURN') inVars = true;
      else if (!inVars && !evaluated) out.push(i);
    } else if (k === 'RETURN') inVars = false;
  }
  return out;
}

/** The `DEFINE MEASURE` / `COLUMN` of `unit` named `name` (of `table` when
 *  one is written), with its expression: from after its `=` to the next
 *  definition, a top `VAR` of the DEFINE or the EVALUATE. */
export function daxLocalDefinition(doc: string, unit: readonly DaxToken[], name: string, table: string | null): DaxLocalDef | null {
  const starts = daxPartStarts(unit);
  for (let s = 0; s < starts.length; s++) {
    const i = starts[s];
    const w = upper(unit[i]);
    if (w !== 'MEASURE' && w !== 'COLUMN') continue;
    const owner = unit[i + 1];
    const ref = unit[i + 2];
    if (!owner || (owner.type !== 'name' && owner.type !== 'word') || ref?.type !== 'ref' || ref.open || unit[i + 3]?.value !== '=') continue;
    if (!eq(ref.value, name) || (table !== null && !eq(owner.value, table))) continue;
    const start = i + 4;
    const end = starts[s + 1] ?? unit.length;
    const expression = end > start ? dedent(doc.slice(unit[start].start, unit[end - 1].end)) : '';
    return { kind: w === 'MEASURE' ? 'measure' : 'column', table: owner.value, name: ref.value, expression };
  }
  return null;
}

/** The functions whose string arguments followed by a comma name the
 *  columns they make (`ADDCOLUMNS(T, "Margin", …)`). */
const COLUMN_NAMERS = new Set(['ADDCOLUMNS', 'SUMMARIZECOLUMNS', 'SELECTCOLUMNS', 'ROW', 'SUMMARIZE', 'GROUPBY', 'DATATABLE']);

/** Whether `unit` makes a column `name` itself: a string `"name"` followed
 *  by a comma, as an argument of one of COLUMN_NAMERS (not a filter's
 *  value, as in `[City] = "Rosario"`). */
function namedByString(doc: string, unit: readonly DaxToken[], name: string): boolean {
  const open: number[] = [];
  for (let i = 0; i < unit.length; i++) {
    const t = unit[i];
    if (t.type === 'punct') {
      if (t.value === '(' || t.value === '{') open.push(i);
      else if ((t.value === ')' || t.value === '}') && open.length) open.pop();
      continue;
    }
    if (t.type !== 'string' || t.open || unit[i + 1]?.value !== ',') continue;
    const at = open[open.length - 1];
    if (at === undefined || unit[at].value !== '(' || !COLUMN_NAMERS.has(upper(unit[at - 1]))) continue;
    if (eq(doc.slice(t.start + 1, t.end - 1).replace(/""/g, '"'), name)) return true;
  }
  return false;
}

/** The calls whose table argument gives the arguments after it a row
 *  context, by that argument's index: the iterators and the table
 *  functions that read a table row by row. */
const ROW_CONTEXT = new Map<string, number>([
  ...['FILTER', 'ADDCOLUMNS', 'SELECTCOLUMNS', 'CALCULATETABLE', 'GENERATE', 'GENERATEALL', 'SUMMARIZE', 'GROUPBY'].map((f) => [f, 0] as const),
  ...['SUMX', 'AVERAGEX', 'MINX', 'MAXX', 'COUNTX', 'COUNTAX', 'PRODUCTX', 'CONCATENATEX', 'RANKX', 'MEDIANX', 'GEOMEANX'].map((f) => [f, 0] as const),
  ...['PERCENTILEX.INC', 'PERCENTILEX.EXC', 'STDEVX.S', 'STDEVX.P', 'VARX.S', 'VARX.P'].map((f) => [f, 0] as const),
  ['TOPN', 1],
  ['SAMPLE', 1],
]);

/** The model tables that give token index `at` of `unit` its row context:
 *  walking the parentheses around it outward, the first table named in the
 *  table argument of each ROW_CONTEXT call it's an argument after (the
 *  closest first). */
function rowContextTables(unit: readonly DaxToken[], at: number, tables: readonly string[]): string[] {
  /** Each open bracket around `at`: its index and where each argument starts. */
  const open: { at: number; args: number[] }[] = [];
  for (let i = 0; i < at; i++) {
    const t = unit[i];
    if (t.type !== 'punct') continue;
    if (t.value === '(' || t.value === '{') open.push({ at: i, args: [i + 1] });
    else if ((t.value === ')' || t.value === '}') && open.length) open.pop();
    else if (t.value === ',' && open.length) open[open.length - 1].args.push(i + 1);
  }
  const out: string[] = [];
  for (let k = open.length - 1; k >= 0; k--) {
    const { at: p, args } = open[k];
    const n = unit[p].value === '(' ? ROW_CONTEXT.get(upper(unit[p - 1])) : undefined;
    // Only the arguments after the table have its row context.
    if (n === undefined || args.length - 1 <= n) continue;
    const first = daxTablesNamed(unit.slice(args[n], args[n + 1] - 1), tables)[0];
    if (first && !out.includes(first)) out.push(first);
  }
  return out;
}

/** The `[column]` or `[measure]` at `pos` and where to look for it; null
 *  when it's no reference, or nothing in the query or the explorer could
 *  be it. */
export function daxItemAt(idx: { objects: readonly DbObject[]; kinds: readonly ObjectKindInfo[] }, doc: string, pos: number): DaxItemRef | null {
  const r = daxRefAt(doc, pos);
  if (r?.kind !== 'ref') return null;
  const tokens = daxTokens(doc);
  const unit = daxUnitAround(tokens, tokens.findIndex((t) => t.start === r.from));
  const at = unit.findIndex((t) => t.start === r.from);
  const kindOf = (o: DbObject) => idx.kinds.find((k) => k.id === o.kind);
  const tables = idx.objects.filter((o) => !o.parent && o.kind !== 'hierarchy' && kindOf(o)?.has_columns);
  const measures = idx.objects.filter((o) => o.kind === 'measure');
  const byName = (o: DbObject) => o.name;
  const out: DaxCandidate[] = [];
  const local = daxLocalDefinition(doc, unit, r.name, r.table);
  if (local) out.push({ kind: 'local', def: local });
  const anyMeasure = pick(measures, r.name, byName);
  if (r.table !== null) {
    const own = pick(measures.filter((m) => eq(m.parent, r.table!)), r.name, byName);
    if (own) out.push({ kind: 'measure', object: own });
    const t = pick(tables, r.table, byName);
    if (t) out.push({ kind: 'columns', table: t });
    if (anyMeasure && anyMeasure !== own) out.push({ kind: 'measure', object: anyMeasure });
  } else {
    if (anyMeasure) out.push({ kind: 'measure', object: anyMeasure });
    if (namedByString(doc, unit, r.name)) out.push({ kind: 'query-column' });
    // The row context's tables, then its own EVALUATE's or definition's,
    // then the rest of the unit's: in a star schema, keys and dates share
    // names across tables.
    const names = tables.map(byName);
    const starts = daxPartStarts(unit);
    let s = starts.length - 1;
    while (s >= 0 && starts[s] > at) s--;
    const part = s < 0 ? unit : unit.slice(starts[s], starts[s + 1] ?? unit.length);
    const named = new Set([...rowContextTables(unit, at, names), ...daxTablesNamed(part, names), ...daxTablesNamed(unit, names)]);
    for (const n of named) {
      const t = tables.find((o) => o.name === n);
      if (t) out.push({ kind: 'columns', table: t });
    }
  }
  if (!out.length) return null;
  return { from: r.from, to: r.to, name: r.name, written: r.table, candidates: out };
}

/** What `ref` stands for, given each table's loaded column names (null:
 *  not loaded yet; an error counts as none). The first candidate that has
 *  it wins; a table not loaded holds the answer until it is. */
export function resolveDaxItem(ref: DaxItemRef, columnsOf: (t: DbObject) => readonly string[] | null): DaxItemHit {
  for (const c of ref.candidates) {
    if (c.kind !== 'columns') return c;
    const cols = columnsOf(c.table);
    if (cols === null) return { kind: 'loading', table: c.table };
    const col = pick(cols, ref.name, (x) => x);
    if (col !== null) return { kind: 'column', table: c.table, column: col };
  }
  return { kind: 'none' };
}

// -- a measure's definition ---------------------------------------------------------------------

/** What the hover shows of a model measure, read from its definition (the
 *  driver's `definition_of`, crates/drivers/powerbi/src/objects.rs): its
 *  description as `//` lines, then `MEASURE 'T'[M] = …` (the expression on
 *  the same line, or on the next ones indented four spaces), or
 *  `// MEASURE 'T'[M]: …` when the user may not see it, then the type and
 *  the format string on lines of their own. */
export interface MeasureDetails {
  description: string | null;
  /** Null: the user may not see it. */
  expression: string | null;
  dataType: string | null;
  format: string | null;
}

/** The driver's lines for a measure's type and format, and the end of its
 *  `// MEASURE 'T'[M]` line when the expression is hidden (objects.rs). */
const TYPE_LINE = '// Tipo de dato: ';
const FORMAT_LINE = '// Formato: ';
const HIDDEN_HEAD_END = ': sin permiso para ver la expresión';

/** Whether a definition's line is the measure's head: `MEASURE …`, or the
 *  hidden one's comment (a description line may start with `MEASURE` too,
 *  after its `//`). */
const isHead = (l: string) => /^MEASURE\s/.test(l) || (/^\/\/ MEASURE\s/.test(l) && l.trimEnd().endsWith(HIDDEN_HEAD_END));

export function measureDetails(definition: string): MeasureDetails {
  const lines = definition.replace(/\r\n?/g, '\n').split('\n');
  const description: string[] = [];
  let expression: string | null = null;
  let dataType: string | null = null;
  let format: string | null = null;
  let i = 0;
  for (; i < lines.length; i++) {
    const l = lines[i];
    if (isHead(l)) break;
    if (l.startsWith('//')) description.push(l.replace(/^\/\/ ?/, ''));
  }
  const head = lines[i];
  if (head?.startsWith('MEASURE')) {
    // The `=` after the measure's [name] (a name may have its own `=`).
    const toks = daxTokens(head);
    const k = toks.findIndex((t, j) => t.type === 'punct' && t.value === '=' && toks[j - 1]?.type === 'ref');
    const same = k >= 0 ? head.slice(toks[k].end).trim() : '';
    const body: string[] = [];
    while (i + 1 < lines.length && lines[i + 1].startsWith('    ')) body.push(lines[++i].slice(4));
    expression = [same, ...body].filter((l, j) => j > 0 || l).join('\n').trim() || null;
  }
  for (i++; i < lines.length; i++) {
    const l = lines[i];
    if (l.startsWith(TYPE_LINE)) dataType = l.slice(TYPE_LINE.length).trim() || null;
    else if (l.startsWith(FORMAT_LINE)) format = l.slice(FORMAT_LINE.length).trim() || null;
  }
  const text = description.join('\n').trim();
  return { description: text || null, expression, dataType, format };
}
