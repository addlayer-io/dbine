// The SQL editor's name completion (CodeEditor.vue), kept apart from the
// component so it can be tested without a browser: which names go where,
// what an alias stands for, and, in a "DataFrame SQL" cell, the
// DuckDB file readers and the paths inside their quotes.
//
// - **Table positions** (after FROM, JOIN, INTO, UPDATE, TABLE, or a comma
//   in a FROM list, on the same line or the lines above): the tables, and
//   in a DataFrame SQL cell the cells above (each one a table) and the
//   file readers.
// - **After a dot** (`sales.`, `orders.`, an alias): what is under that name.
//   On engines whose queries read other databases (`Driver::cross_database`)
//   that includes `other_db.`: its tables, loaded the first time it's typed.
// - **Anywhere else** (the select list, WHERE, ON…): the names at the top
//   and the columns of what the query under the cursor reads: only that
//   statement (`;` or a `GO` line ends one), its tables, CTEs and derived
//   tables. A column two of them have is offered once per table, qualified.
// - **Inside a reader's quotes** (`read_csv('|')`): "Elegir archivo…", which
//   opens the native file dialog, and the files the notebook was granted
//   (`grantedFiles`), never one in a folder that holds credentials. No
//   folder is ever listed from here: picking or completing a path grants
//   nothing; the backend asks for each file when the cell runs.
import type { Completion, CompletionContext, CompletionResult } from '@codemirror/autocomplete';
import { syntaxTree } from '@codemirror/language';

/** A name completion offers: a schema, a table or a column. */
export interface NameNode {
  label: string;
  type: 'namespace' | 'class' | 'property';
  /** From the top: `[schema, table]`, `[table]`… */
  path: string[];
  children: NameNode[];
  /** The other database it lives in (`path` starts with it): the
   *  database's own node and everything under it. */
  database?: string;
  /** A database whose objects aren't loaded yet. */
  pending?: boolean;
}

/** The schemas, tables and columns of `schema` (table → columns; a
 *  `schema.table` key puts the table under its schema), and under each of
 *  `databases` the same for that database (null: not loaded yet). A name can be a
 *  schema and a table at once (`Person` and `Person.Person`): both stay,
 *  each with its own icon. A table without columns (not loaded yet, or a
 *  notebook cell that hasn't run) is a table all the same. */
export function nameTree(
  schema: Record<string, string[]>,
  databases?: Record<string, Record<string, string[]> | null>,
): NameNode {
  const top: NameNode = { label: '', type: 'namespace', path: [], children: [] };
  const child = (parent: NameNode, label: string, type: NameNode['type'], database = parent.database) => {
    let n = parent.children.find((c) => c.label === label && c.type === type && c.database === database);
    if (!n) {
      n = { label, type, path: [...parent.path, label], children: [] };
      if (database !== undefined) n.database = database;
      parent.children.push(n);
    }
    return n;
  };
  const add = (root: NameNode, entries: Record<string, string[]>) => {
    for (const [key, cols] of Object.entries(entries)) {
      const dot = key.indexOf('.');
      const table = dot < 0
        ? child(root, key, 'class')
        : child(child(root, key.slice(0, dot), 'namespace'), key.slice(dot + 1), 'class');
      for (const c of cols) child(table, c, 'property');
    }
  };
  add(top, schema);
  for (const [db, entries] of Object.entries(databases ?? {})) {
    const node = child(top, db, 'namespace', db);
    if (entries) add(node, entries);
    else node.pending = true;
  }
  return top;
}

/** The children of `node` called `name`: the exact spelling, or else any
 *  case, since most engines don't tell `people` from `People`. */
function childrenNamed(node: NameNode, name: string): NameNode[] {
  const exact = node.children.filter((c) => c.label === name);
  if (exact.length) return exact;
  const lower = name.toLowerCase();
  return node.children.filter((c) => c.label.toLowerCase() === lower);
}

/** An identifier, bare or quoted ([x], "x", `x`). */
const ID = String.raw`(?:[\w$#]+|\[[^\]\n]+\]|"[^"\n]+"|\x60[^\x60\n]+\x60)`;
const PATH = String.raw`${ID}(?:\s*\.\s*${ID})*`;
const PARENT_BEFORE = new RegExp(String.raw`(${ID})\s*\.\s*$`);
/** `FROM sales.orders o`, `JOIN people AS p`, `, items i`, `UPDATE t`: a
 *  table a statement reads, and its alias if it has one. */
const TABLE_REF = new RegExp(String.raw`(?:\b(?:from|join|update|into)\s+|,\s*)(${PATH})(?:\s+(?:as\s+)?(${ID}))?`, 'gi');
/** `WITH x AS (`, `, y(a, b) AS (`: the text before a CTE's body. */
const CTE_BEFORE = new RegExp(String.raw`(?:\bwith\s+(?:recursive\s+)?|,\s*)(${ID})(?:\s*\(([^()]*)\))?\s+as\s*(?:(?:not\s+)?materialized\s*)?$`, 'i');
/** `) AS t`, `) t(a, b)`: the alias after a derived table. */
const DERIVED_AFTER = new RegExp(String.raw`^\s*(?:as\s+)?(${ID})(?:\s*\(([^()]*)\))?`, 'i');
/** Words that follow a table name without being its alias. */
const NOT_ALIAS = new Set(
  'where on join inner left right full cross outer natural group order having union except intersect minus limit offset fetch set values select from using with as window qualify lateral apply pivot unpivot tablesample returning'.split(' '),
);
/** Select-list items: `t.*`, `x AS a`, `t.a`, `x a`. */
const ITEM_STAR = new RegExp(String.raw`^(${PATH})\s*\.\s*\*$`);
const ITEM_AS = new RegExp(String.raw`\bas\s+(${ID})$`, 'i');
const ITEM_COLUMN = new RegExp(String.raw`^(?:${ID}\s*\.\s*)*(${ID})$`);
const ITEM_ALIAS = new RegExp(String.raw`[\w)\]"'\x60]\s+(${ID})$`);
/** Words that end a select-list expression without being its name. */
const NOT_COLUMN_NAME = new Set('end null true false else then and or not is in like between asc desc'.split(' '));

function unquote(id: string): string {
  return /^[["`]/.test(id) ? id.slice(1, -1) : id;
}
const splitPath = (path: string) => path.split(/\s*\.\s*/).map(unquote);

/** The `a.b.` at the end of `text`, as `['a', 'b']`. */
export function parentsBefore(text: string): string[] {
  const parents: string[] = [];
  for (let rest = text; ;) {
    const m = PARENT_BEFORE.exec(rest);
    if (!m) return parents;
    parents.unshift(unquote(m[1]));
    rest = rest.slice(0, m.index);
  }
}

/** Where a table name goes, from the text before the word being typed:
 *  right after FROM, JOIN, INTO, UPDATE or TABLE (on this line or with only
 *  blanks since), or after a comma in a FROM list (`FROM a x, |`). */
export function isTableSlot(before: string): boolean {
  if (/\b(from|join|into|update|table)\s+$/i.test(before)) return true;
  if (!/,\s*$/.test(before)) return false;
  // The clause the comma is in: the last clause word at the comma's depth.
  const outer: string[] = [];
  const words = /\(|\)|\b(select|from|where|join|on|group|order|having|limit|set|values|union|except|intersect|returning|window|qualify)\b/gi;
  let clause = '';
  for (let m = words.exec(before); m; m = words.exec(before)) {
    if (m[0] === '(') { outer.push(clause); clause = ''; } else if (m[0] === ')') clause = outer.pop() ?? '';
    else clause = m[1].toLowerCase();
  }
  return clause === 'from' || clause === 'join';
}

/** How far back the table position and parents are looked for. */
const LOOK_BACK = 2000;

export function tableSlotAt(ctx: CompletionContext): boolean {
  const word = ctx.matchBefore(/[\w$#]*/);
  const end = word ? word.from : ctx.pos;
  return isTableSlot(ctx.state.sliceDoc(Math.max(0, end - LOOK_BACK), end));
}

export function inStringOrComment(ctx: CompletionContext): boolean {
  return /String|Comment|QuotedIdentifier/.test(syntaxTree(ctx.state).resolveInner(ctx.pos, -1).name);
}

// -- the statement under the cursor ----------------------------------------------------------
/** `doc` with comments and the insides of string literals blanked, same
 *  length (newlines, quotes and quoted names stay): the regexes below only
 *  see code. */
export function maskSql(doc: string): string {
  const blank = (s: string) => s.replace(/[^\n]/g, ' ');
  const n = doc.length;
  let out = '';
  let i = 0;
  while (i < n) {
    const c = doc[i];
    const two = doc.slice(i, i + 2);
    if (two === '--' || two === '/*') {
      let end = two === '--' ? doc.indexOf('\n', i) : doc.indexOf('*/', i + 2);
      end = end < 0 ? n : two === '--' ? end : end + 2;
      out += blank(doc.slice(i, end));
      i = end;
    } else if (c === "'") {
      let j = i + 1;
      while (j < n && !(doc[j] === "'" && doc[j + 1] !== "'")) j += doc[j] === "'" ? 2 : 1;
      out += "'" + blank(doc.slice(i + 1, Math.min(j, n))) + (j < n ? "'" : '');
      i = Math.min(j + 1, n);
    } else if (c === '"' || c === '`') {
      const j = doc.indexOf(c, i + 1);
      const end = j < 0 ? n : j + 1;
      out += doc.slice(i, end);
      i = end;
    } else if (c === '$' && !/[\w$]/.test(doc[i - 1] ?? '')) {
      const tag = /^\$(?:[A-Za-z_]\w*)?\$/.exec(doc.slice(i, i + 64));
      const j = tag ? doc.indexOf(tag[0], i + tag[0].length) : -1;
      if (!tag) { out += c; i++; continue; }
      const end = j < 0 ? n : j + tag[0].length;
      out += tag[0] + blank(doc.slice(i + tag[0].length, Math.max(i + tag[0].length, end - tag[0].length))) + (j < 0 ? '' : tag[0]);
      i = end;
    } else {
      out += c;
      i++;
    }
  }
  return out;
}

/** A line with only SQL Server's batch separator (`GO`, `GO 5`). */
const GO_LINE = /^[ \t]*go(?:[ \t]+\d+)?[ \t]*$/gim;

/** The statement `pos` is in, in `masked` (maskSql): between the `;` (or
 *  `GO` line) before it and the one after it. */
export function statementAt(masked: string, pos: number): { from: number; to: number } {
  let from = 0;
  let to = masked.length;
  const cut = (start: number, end: number) => {
    if (end <= pos) from = Math.max(from, end);
    else if (start >= pos) to = Math.min(to, start);
  };
  for (let i = masked.indexOf(';'); i >= 0; i = masked.indexOf(';', i + 1)) cut(i, i + 1);
  const go = new RegExp(GO_LINE);
  for (let m = go.exec(masked); m; m = go.exec(masked)) {
    cut(m.index, m.index + m[0].length);
    if (!m[0].length) go.lastIndex++;
  }
  return { from, to };
}

/** A parenthesis in a statement: a CTE's body, a derived table, a
 *  subquery, or anything else (a call, a list). */
interface Group {
  open: number;
  /** The `)`, or the end of the statement while it isn't typed yet. */
  close: number;
  parent: Group | null;
  children: Group[];
  /** It holds a query (SELECT, WITH, VALUES). */
  query: boolean;
  kind: 'root' | 'cte' | 'derived' | 'other';
  /** The CTE's or the derived table's name. */
  name?: string;
  /** `x(a, b) AS (…)`, `(…) t(a, b)`: the columns named outright. */
  columnList?: string[];
}

/** What a name in FROM/JOIN stands for: a table of the tree, a CTE or a
 *  derived table. */
interface Source {
  /** The alias, else the table's own name. */
  name: string;
  aliased: boolean;
  /** What the list shows next to its columns: `c (customers)`. */
  label: string;
  columns: NameNode[];
  /** A table of the tree: its path (after a dot, resolved there). */
  path?: string[];
}

const property = (label: string): NameNode => ({ label, type: 'property', path: [], children: [] });

function uniqueColumns(columns: NameNode[]): NameNode[] {
  const seen = new Set<string>();
  return columns.filter((c) => !seen.has(c.label.toLowerCase()) && !!seen.add(c.label.toLowerCase()));
}

/** The parentheses of a statement, nested, with what each one is. */
function groupsOf(s: string): Group {
  const root: Group = { open: -1, close: s.length, parent: null, children: [], query: true, kind: 'root' };
  const stack = [root];
  const all: Group[] = [];
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    if (c === '"' || c === '`') {
      const j = s.indexOf(c, i + 1);
      i = j < 0 ? s.length : j;
    } else if (c === '(') {
      const parent = stack[stack.length - 1];
      const g: Group = { open: i, close: s.length, parent, children: [], query: false, kind: 'other' };
      parent.children.push(g);
      stack.push(g);
      all.push(g);
    } else if (c === ')' && stack.length > 1) {
      stack.pop()!.close = i;
    }
  }
  const columnList = (list: string | undefined) => (list?.trim() ? list.split(',').map((c) => unquote(c.trim())).filter(Boolean) : undefined);
  for (const g of all) {
    g.query = /^\s*(?:select|with|values)\b/i.test(s.slice(g.open + 1, g.close));
    if (!g.query) continue;
    const before = s.slice(0, g.open);
    const cte = CTE_BEFORE.exec(before);
    if (cte) {
      g.kind = 'cte';
      g.name = unquote(cte[1]);
      g.columnList = columnList(cte[2]);
      continue;
    }
    if (g.close >= s.length) continue;
    const after = DERIVED_AFTER.exec(s.slice(g.close + 1));
    const inFrom = /\b(?:from|join|lateral|apply)\s*$/i.test(before) || (/,\s*$/.test(before) && isTableSlot(before));
    if (after && inFrom && !NOT_ALIAS.has(unquote(after[1]).toLowerCase())) {
      g.kind = 'derived';
      g.name = unquote(after[1]);
      g.columnList = columnList(after[2]);
    }
  }
  return root;
}

/** The names a statement's queries read, CTEs and derived tables
 *  included, and the columns each one gives. */
class StatementScope {
  private sourcesMemo = new Map<Group, Source[]>();
  private columnsMemo = new Map<Group, NameNode[]>();
  private busy = new Set<Group>();
  private s: string;
  private tree: NameNode;
  readonly root: Group;

  constructor(s: string, tree: NameNode) {
    this.s = s;
    this.tree = tree;
    this.root = groupsOf(s);
  }

  /** The text of `g`, with what's inside its own parentheses blanked. */
  private bodyOf(g: Group): string {
    const start = g.open + 1;
    let out = this.s.slice(start, g.close);
    for (const c of g.children) {
      const a = c.open + 1 - start;
      const b = c.close - start;
      out = out.slice(0, a) + out.slice(a, b).replace(/[^\n]/g, ' ') + out.slice(b);
    }
    return out;
  }

  private queryAncestor(g: Group | null): Group | null {
    while (g && !g.query) g = g.parent;
    return g;
  }

  /** The CTEs `g` can read: those of its own WITH and of the queries
   *  around it. */
  ctesFor(g: Group): Group[] {
    const out: Group[] = [];
    for (let a: Group | null = g; a; a = a.parent) {
      for (const c of a.children) if (c.kind === 'cte' && c !== g) out.push(c);
    }
    return out;
  }

  /** The tables, CTEs and derived tables query `g` reads. */
  sources(g: Group): Source[] {
    const memo = this.sourcesMemo.get(g);
    if (memo) return memo;
    const body = this.bodyOf(g);
    const ctes = this.ctesFor(g);
    const out: Source[] = [];
    const add = (src: Source) => {
      if (!out.some((o) => o.name.toLowerCase() === src.name.toLowerCase())) out.push(src);
    };
    const re = new RegExp(TABLE_REF);
    for (let m = re.exec(body); m; m = re.exec(body)) {
      let alias = m[2] ? unquote(m[2]) : undefined;
      if (alias && NOT_ALIAS.has(alias.toLowerCase())) {
        // `FROM t JOIN u`: the keyword starts the next match.
        re.lastIndex = m.index + m[0].length - m[2].length;
        alias = undefined;
      }
      // A comma outside a FROM list (`SELECT a, b c`) isn't a table.
      if (m[0].startsWith(',') && !isTableSlot(body.slice(0, m.index + 1))) {
        re.lastIndex = m.index + 1;
        continue;
      }
      const path = splitPath(m[1]);
      const cte = path.length === 1 ? ctes.find((c) => c.name!.toLowerCase() === path[0].toLowerCase()) : undefined;
      if (cte) {
        add({ name: alias ?? cte.name!, aliased: !!alias, label: alias ? `${alias} (${cte.name})` : cte.name!, columns: this.columns(cte) });
        continue;
      }
      let level = [this.tree];
      for (const name of path) level = level.flatMap((n) => childrenNamed(n, name));
      const columns = uniqueColumns(level.filter((n) => n.type === 'class').flatMap((t) => t.children));
      const shown = path.join('.');
      add({ name: alias ?? path[path.length - 1], aliased: !!alias, label: alias ? `${alias} (${shown})` : shown, columns, path });
    }
    for (const c of g.children) {
      if (c.kind === 'derived') add({ name: c.name!, aliased: true, label: c.name!, columns: this.columns(c) });
    }
    this.sourcesMemo.set(g, out);
    return out;
  }

  /** The columns a CTE or a derived table gives: those named outright,
   *  else its select list's names (`a`, `t.a`, `x AS a`, `x a`) and the
   *  columns behind `*` and `t.*`. */
  columns(g: Group): NameNode[] {
    if (g.columnList) return g.columnList.map(property);
    const memo = this.columnsMemo.get(g);
    if (memo) return memo;
    // A recursive CTE reading itself.
    if (this.busy.has(g)) return [];
    this.busy.add(g);
    const out: NameNode[] = [];
    const body = this.bodyOf(g);
    const select = /\bselect\b/i.exec(body);
    if (select) {
      let list = body.slice(select.index + select[0].length);
      const end = /\b(?:from|into|where|group|order|having|limit|union|except|intersect|minus|window|qualify|fetch|offset)\b/i.exec(list);
      if (end) list = list.slice(0, end.index);
      list = list
        .replace(/^\s*(?:distinct\s+on\s*\([^)]*\)|distinct|all)\b/i, '')
        .replace(/^\s*top\s*(?:\d+|\([^)]*\))(?:\s+percent)?(?:\s+with\s+ties)?/i, '');
      const sources = () => this.sources(g);
      for (const raw of list.split(',')) {
        const item = raw.trim();
        let m: RegExpExecArray | null;
        if (item === '*') {
          for (const s of sources()) out.push(...s.columns);
        } else if ((m = ITEM_STAR.exec(item))) {
          const name = splitPath(m[1]).pop()!.toLowerCase();
          out.push(...(sources().find((s) => s.name.toLowerCase() === name)?.columns ?? []));
        } else if ((m = ITEM_AS.exec(item))) {
          out.push(property(unquote(m[1])));
        } else if ((m = ITEM_COLUMN.exec(item))) {
          const name = unquote(m[1]);
          if (!/^\d/.test(name) && !NOT_COLUMN_NAME.has(name.toLowerCase())) out.push(property(name));
        } else if ((m = ITEM_ALIAS.exec(item))) {
          const name = unquote(m[1]);
          if (!NOT_COLUMN_NAME.has(name.toLowerCase()) && !NOT_ALIAS.has(name.toLowerCase())) out.push(property(name));
        }
      }
    }
    this.busy.delete(g);
    const unique = uniqueColumns(out);
    this.columnsMemo.set(g, unique);
    return unique;
  }

  /** The innermost query around `pos`, and the queries whose names it can
   *  also use (a subquery sees the query around it; a CTE or a derived
   *  table doesn't). */
  at(pos: number): { query: Group; chain: Group[] } {
    let g = this.root;
    let query = this.root;
    descend: for (;;) {
      for (const c of g.children) {
        if (c.open < pos && pos <= c.close) {
          g = c;
          if (c.query) query = c;
          continue descend;
        }
      }
      break;
    }
    const chain = [query];
    for (let q = query; q.kind === 'other';) {
      const up = this.queryAncestor(q.parent);
      if (!up) break;
      chain.push(up);
      q = up;
    }
    return { query, chain };
  }
}

/** What the name completion found: options, or a request (the names, or a
 *  table's columns, aren't loaded yet). */
export interface NameAnswer {
  result: CompletionResult | null;
  /** Nothing to offer yet: ask for the names. */
  needNames?: boolean;
  /** The table after the dot has no columns loaded: ask for them (the
   *  path within `columnsIn`, else within the tab's database). */
  needColumns?: string[];
  columnsIn?: string;
  /** The other database after the dot isn't loaded: ask for its objects. */
  needDatabase?: string;
}

/** Schemas, tables and columns, with what comes after a dot resolved by
 *  name (`sales.`, `orders.`, an alias, a CTE). Aliases, CTEs and the
 *  columns offered come from the statement under the cursor only, and from
 *  the query (or subquery) it is in. `quote` wraps a name that needs it
 *  (`[x]`, `"x"`). */
export function nameCompletion(ctx: CompletionContext, tree: NameNode, quote: (label: string) => string): NameAnswer {
  if (inStringOrComment(ctx)) return { result: null };
  const word = ctx.matchBefore(/[\w$#]*/) ?? { from: ctx.pos, text: '' };
  let parents = parentsBefore(ctx.state.sliceDoc(Math.max(0, word.from - 300), word.from));
  const slot = !parents.length && isTableSlot(ctx.state.sliceDoc(Math.max(0, word.from - LOOK_BACK), word.from));
  // A table position opens the list by itself (CodeEditor's openTableList):
  // then it is an explicit request, and so is ⌃Space.
  if (!parents.length && !word.text && !ctx.explicit) return { result: null };
  if (!tree.children.length) return { result: null, needNames: true };
  const bare = (label: string) => /^[a-z_][\w$#]*$/i.test(label);
  const option = (n: NameNode, boost?: number): Completion => {
    const apply = bare(n.label) ? undefined : quote(n.label);
    return { label: n.label, type: n.type, ...(apply ? { apply } : {}), ...(boost ? { boost } : {}) };
  };
  const done = (options: Completion[]): NameAnswer => ({ result: { from: word.from, options, validFor: /^[\w$#]*$/ } });

  const masked = maskSql(ctx.state.doc.toString());
  const stmt = statementAt(masked, word.from);
  const scope = new StatementScope(masked.slice(stmt.from, stmt.to), tree);
  const { query, chain } = scope.at(word.from - stmt.from);
  const ctes = scope.ctesFor(query);

  if (parents.length === 1) {
    const key = parents[0].toLowerCase();
    const src = chain.flatMap((q) => scope.sources(q)).find((s) => s.name.toLowerCase() === key);
    const cte = src ? undefined : ctes.find((c) => c.name!.toLowerCase() === key);
    if (src?.path) parents = src.path;
    else if (src || cte) {
      // A CTE or a derived table: the columns of its select list.
      const columns = src ? src.columns : scope.columns(cte!);
      return columns.length ? done(columns.map((c) => option(c))) : { result: null };
    }
  }

  let level = [tree];
  for (const name of parents) {
    level = level.flatMap((n) => childrenNamed(n, name));
    if (!level.length) return { result: null };
  }
  const pending = parents.length ? level.find((n) => n.pending) : undefined;
  if (pending) return { result: null, needDatabase: pending.database };
  if (parents.length && level.every((n) => !n.children.length)) {
    const table = level.find((n) => n.type === 'class');
    if (table?.database !== undefined) return { result: null, needColumns: table.path.slice(1), columnsIn: table.database };
    return { result: null, needColumns: table?.path };
  }

  const options = level.flatMap((n) => n.children).map((c) => option(c));
  if (!parents.length && slot) {
    // The statement's CTEs are tables too.
    for (const c of ctes) options.push({ label: c.name!, type: 'class', ...(bare(c.name!) ? {} : { apply: quote(c.name!) }) });
  }
  if (!parents.length && !slot) {
    // The select list, WHERE, ON…: the aliases, and the columns of what the
    // query reads (a little below the names themselves). A column that more
    // than one of them has is offered once per table, qualified.
    const sources = scope.sources(query);
    for (const s of sources) if (s.aliased || !s.path) options.push({ label: s.name, type: 'variable' });
    const count = new Map<string, number>();
    for (const s of sources) for (const c of s.columns) count.set(c.label.toLowerCase(), (count.get(c.label.toLowerCase()) ?? 0) + 1);
    for (const s of sources) {
      for (const c of s.columns) {
        const o: Completion = { ...option(c, -1), detail: s.label };
        if ((count.get(c.label.toLowerCase()) ?? 0) > 1) {
          o.apply = `${bare(s.name) ? s.name : quote(s.name)}.${bare(c.label) ? c.label : quote(c.label)}`;
        }
        options.push(o);
      }
    }
  }
  return done(options);
}

// -- DuckDB's file readers ("DataFrame SQL" cells) ----------------------------------------
/** The readers a results cell may call, as src-tauri/src/notebook/files.rs
 *  READERS; `format` is what the list shows next to each; `table`: it
 *  reads a table of a database file (`sqlite_scan('x.db', 'tabla')`). */
export const FILE_READERS: { name: string; format: string; table?: boolean }[] = [
  { name: 'read_csv', format: 'CSV' },
  { name: 'read_csv_auto', format: 'CSV' },
  { name: 'read_parquet', format: 'Parquet' },
  { name: 'parquet_scan', format: 'Parquet' },
  { name: 'read_json', format: 'JSON' },
  { name: 'read_json_auto', format: 'JSON' },
  { name: 'read_ndjson', format: 'NDJSON' },
  { name: 'read_ndjson_auto', format: 'NDJSON' },
  { name: 'read_xlsx', format: 'Excel' },
  { name: 'st_read', format: 'GeoJSON, Shapefile, GeoPackage…' },
  { name: 'sqlite_scan', format: 'SQLite', table: true },
  { name: 'read_duckdb', format: 'DuckDB', table: true },
];

/** What picking a reader inserts: a snippet with the cursor between the
 *  quotes; a database reader also gets the table's quotes, the next field
 *  (Tab). */
export const readerSnippet = (name: string) =>
  FILE_READERS.find((r) => r.name === name)?.table ? `${name}('\${1}', '\${2}')` : `${name}('\${}')`;

/** The readers, in a table position of a results cell (or ⌃Space there). */
export function readerCompletion(ctx: CompletionContext, make: (name: string, format: string) => Completion): CompletionResult | null {
  if (inStringOrComment(ctx)) return null;
  const word = ctx.matchBefore(/[\w$#]*/) ?? { from: ctx.pos, text: '' };
  if (!isTableSlot(ctx.state.sliceDoc(Math.max(0, word.from - LOOK_BACK), word.from))) return null;
  if (!word.text && !ctx.explicit) return null;
  return { from: word.from, options: FILE_READERS.map((r) => make(r.name, r.format)), validFor: /^[\w$#]*$/ };
}

const READER_NAMES = FILE_READERS.map((r) => r.name).join('|');
/** Inside the first argument of a reader, a string or a list of them:
 *  `read_csv('~/Down|`, `read_csv(['a.csv', 'b|`. Group 1: what is typed. */
const IN_READER_PATH = new RegExp(String.raw`\b(?:${READER_NAMES})\s*\(\s*(?:\[\s*(?:'(?:[^'\n]|'')*'\s*,\s*)*)?'((?:[^'\n]|'')*)$`, 'i');

/** The path being typed inside a reader's quotes, or null when the cursor
 *  isn't there. `from`/`to`: the text between the quotes. */
export function readerPathAt(ctx: CompletionContext): { from: number; to: number; typed: string } | null {
  const line = ctx.state.doc.lineAt(ctx.pos);
  const m = IN_READER_PATH.exec(ctx.state.sliceDoc(line.from, ctx.pos));
  if (!m) return null;
  const rest = ctx.state.sliceDoc(ctx.pos, line.to);
  const close = /^(?:[^'\n]|'')*'/.exec(rest);
  return { from: ctx.pos - m[1].length, to: close ? ctx.pos + close[0].length - 1 : ctx.pos, typed: m[1] };
}

/** A path as it goes between single quotes. */
export const quotePath = (path: string) => path.replace(/'/g, "''");

/** Folders and files that hold credentials or keys: never suggested, even
 *  when the notebook was granted them. */
const SENSITIVE_DIRS = new Set([
  '.ssh', '.aws', '.azure', '.gnupg', '.gpg', '.kube', '.docker', '.password-store', '.vault', '.oci', '.terraform.d',
  'keychains', '.1password', '.op', '.gcloud',
]);
const SENSITIVE_FILE = /^(?:\.env(?:\..*)?|\.netrc|_netrc|\.pgpass|\.my\.cnf|\.npmrc|\.pypirc|\.git-credentials|\.vault-token|credentials(?:\.\w+)?|id_[a-z0-9]+(?:\.pub)?|.*\.(?:pem|key|p12|pfx|jks|keystore|kdbx|ppk|asc|gpg))$/i;

/** The path is in (or is) a place that holds secrets: `~/.ssh/…`,
 *  `~/.aws/credentials`, `.env`, `~/.config/gcloud/…`, a private key… */
export function isSensitivePath(path: string): boolean {
  const parts = path.split(/[\\/]+/).filter(Boolean).map((p) => p.toLowerCase());
  if (!parts.length) return false;
  for (let i = 0; i < parts.length - 1; i++) {
    if (SENSITIVE_DIRS.has(parts[i])) return true;
    if (parts[i] === '.config' && ['gcloud', 'gh', 'op', 'doctl', 'hub'].includes(parts[i + 1])) return true;
  }
  return SENSITIVE_DIRS.has(parts[parts.length - 1]) || SENSITIVE_FILE.test(parts[parts.length - 1]);
}

/** Inside a reader's quotes: `pick` ("Elegir archivo…", first) and the
 *  granted files that match what is typed, minus the sensitive ones. */
export function readerPathCompletion(
  ctx: CompletionContext,
  grantedFiles: readonly string[],
  pick: ((at: { from: number; to: number }) => Completion) | null,
): CompletionResult | null {
  const at = readerPathAt(ctx);
  if (!at) return null;
  const typed = at.typed.replace(/''/g, "'").toLowerCase();
  const files = grantedFiles.filter((f) => !isSensitivePath(f) && (!typed || f.toLowerCase().includes(typed)));
  const options: Completion[] = files.map((f) => ({ label: f, type: 'text', apply: quotePath(f) }));
  if (pick) options.unshift(pick(at));
  if (!options.length) return null;
  // The whole text between the quotes is replaced; `filter: false` keeps
  // "Elegir archivo…" whatever was typed.
  return { from: at.from, to: at.to, options, filter: false };
}

/** What a cell's editor knows about local files: `enabled` only in a
 *  "DataFrame SQL" cell (a cell on a real database never gets the
 *  readers); `pick` makes the "Elegir archivo…" entry. */
export interface FileCompletion {
  enabled: boolean;
  grantedFiles: readonly string[];
  pick: ((at: { from: number; to: number }) => Completion) | null;
}

/** The readers in a table position, or the paths inside their quotes. */
export function fileCompletion(ctx: CompletionContext, files: FileCompletion, make: (name: string, format: string) => Completion): CompletionResult | null {
  if (!files.enabled) return null;
  return readerPathCompletion(ctx, files.grantedFiles, files.pick) ?? readerCompletion(ctx, make);
}
