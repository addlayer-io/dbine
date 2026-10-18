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
// - **Anywhere else** (the select list, WHERE, ON…): the names at the top
//   and the columns of the tables the statement reads.
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
}

/** The schemas, tables and columns of `schema` (table → columns; a
 *  `schema.table` key puts the table under its schema). A name can be a
 *  schema and a table at once (`Person` and `Person.Person`): both stay,
 *  each with its own icon. A table without columns (not loaded yet, or a
 *  notebook cell that hasn't run) is a table all the same. */
export function nameTree(schema: Record<string, string[]>): NameNode {
  const top: NameNode = { label: '', type: 'namespace', path: [], children: [] };
  const child = (parent: NameNode, label: string, type: NameNode['type']) => {
    let n = parent.children.find((c) => c.label === label && c.type === type);
    if (!n) parent.children.push((n = { label, type, path: [...parent.path, label], children: [] }));
    return n;
  };
  for (const [key, cols] of Object.entries(schema)) {
    const dot = key.indexOf('.');
    const table = dot < 0
      ? child(top, key, 'class')
      : child(child(top, key.slice(0, dot), 'namespace'), key.slice(dot + 1), 'class');
    for (const c of cols) child(table, c, 'property');
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
const PARENT_BEFORE = new RegExp(String.raw`(${ID})\s*\.\s*$`);
/** `FROM sales.orders o`, `JOIN people AS p`, `, items i`. */
const ALIAS = new RegExp(String.raw`(?:\b(?:from|join|update|into)|,)\s*(${ID}(?:\s*\.\s*${ID})*)\s+(?:as\s+)?(${ID})`, 'gi');
/** `FROM sales.orders`, `JOIN people`: the tables a statement reads. */
const TABLE_REF = new RegExp(String.raw`\b(?:from|join|update|into)\s+(${ID}(?:\s*\.\s*${ID})*)`, 'gi');
/** Words that follow a table name without being its alias. */
const NOT_ALIAS = new Set(
  'where on join inner left right full cross outer natural group order having union except intersect limit offset set values select from using with as'.split(' '),
);

function unquote(id: string): string {
  return /^[[\"`]/.test(id) ? id.slice(1, -1) : id;
}

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

/** alias (lower case) → the path it stands for. */
export function aliasesIn(doc: string): Map<string, string[]> {
  const out = new Map<string, string[]>();
  const re = new RegExp(ALIAS);
  for (let m = re.exec(doc); m; m = re.exec(doc)) {
    const alias = unquote(m[2]);
    if (NOT_ALIAS.has(alias.toLowerCase())) {
      // `…, p.total FROM t x`: the keyword starts the next match.
      re.lastIndex = m.index + m[0].length - m[2].length;
      continue;
    }
    out.set(alias.toLowerCase(), m[1].split(/\s*\.\s*/).map(unquote));
  }
  return out;
}

/** Where a table name goes, from the text before the word being typed:
 *  right after FROM, JOIN, INTO, UPDATE or TABLE (on this line or with only
 *  blanks since), or after a comma in a FROM list (`FROM a x, |`). */
export function isTableSlot(before: string): boolean {
  if (/\b(from|join|into|update|table)\s+$/i.test(before)) return true;
  if (!/,\s*$/.test(before)) return false;
  // The clause the comma is in: the last clause word outside parentheses.
  let depth = 0;
  const words = /\(|\)|\b(select|from|where|join|on|group|order|having|limit|set|values|union|except|intersect|returning|window|qualify)\b/gi;
  let clause = '';
  for (let m = words.exec(before); m; m = words.exec(before)) {
    if (m[0] === '(') { depth++; clause = ''; } else if (m[0] === ')') depth = Math.max(0, depth - 1);
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

/** What the name completion found: options, or a request (the names, or a
 *  table's columns, aren't loaded yet). */
export interface NameAnswer {
  result: CompletionResult | null;
  /** Nothing to offer yet: ask for the names. */
  needNames?: boolean;
  /** The table after the dot has no columns loaded: ask for them. */
  needColumns?: string[];
}

/** The columns of the tables `doc` reads (FROM, JOIN, an alias), once each. */
function columnsInScope(doc: string, tree: NameNode, aliases: Map<string, string[]>): NameNode[] {
  const paths: string[][] = [...aliases.values()];
  const re = new RegExp(TABLE_REF);
  for (let m = re.exec(doc); m; m = re.exec(doc)) paths.push(m[1].split(/\s*\.\s*/).map(unquote));
  const seen = new Set<string>();
  const out: NameNode[] = [];
  for (const path of paths) {
    let level = [tree];
    for (const name of path) level = level.flatMap((n) => childrenNamed(n, name));
    for (const table of level.filter((n) => n.type === 'class')) {
      for (const c of table.children) {
        if (seen.has(c.label.toLowerCase())) continue;
        seen.add(c.label.toLowerCase());
        out.push(c);
      }
    }
  }
  return out;
}

/** Schemas, tables and columns, with what comes after a dot resolved by
 *  name (`sales.`, `orders.`, an alias). `quote` wraps a name that needs it
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
  const option = (n: NameNode, boost?: number): Completion => {
    const apply = /^[a-z_][\w$#]*$/i.test(n.label) ? undefined : quote(n.label);
    return { label: n.label, type: n.type, ...(apply ? { apply } : {}), ...(boost ? { boost } : {}) };
  };
  const doc = ctx.state.doc.toString();
  const aliases = aliasesIn(doc);
  if (parents.length === 1) parents = aliases.get(parents[0].toLowerCase()) ?? parents;

  let level = [tree];
  for (const name of parents) {
    level = level.flatMap((n) => childrenNamed(n, name));
    if (!level.length) return { result: null };
  }
  if (parents.length && level.every((n) => !n.children.length)) {
    const table = level.find((n) => n.type === 'class');
    return { result: null, needColumns: table?.path };
  }

  const options = level.flatMap((n) => n.children).map((c) => option(c));
  if (!parents.length && !slot) {
    // The select list, WHERE, ON…: the aliases, and the columns of the
    // tables the statement reads (a little below the names themselves).
    for (const [alias] of aliases) options.push({ label: alias, type: 'variable' });
    for (const c of columnsInScope(doc, tree, aliases)) options.push(option(c, -1));
  }
  return { result: { from: word.from, options, validFor: /^[\w$#]*$/ } };
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
