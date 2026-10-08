import type { DbObject, Language, ObjectKindInfo } from '../api/types';
import type { LintFinding } from '../api/lint';
import type { ObjectView } from '../stores/tabs';
import { lexRegions, lexSyntax, maskCode, regionAt } from './sqlLexer';

// Names in the editor's text, resolved against the explorer's objects of the
// tab's database (stores/connections, filled from the explorer cache first):
// ⌘/Ctrl+click and "Ir a la definición" open the object; "Mostrar en el
// explorador" selects it; and names that don't exist get a soft warning
// through "Calidad de código" (docs/editor-de-consultas.md).

/** What the editor knows of the tab's database. */
export interface NameIndex {
  language: Language;
  dialect: string;
  database: string;
  objects: DbObject[];
  kinds: ObjectKindInfo[];
  /** Every schema of the database (for "is this schema ours?"). */
  schemas: string[];
  /** The loaded columns of an object, or null when not loaded. */
  columns: (o: DbObject) => string[] | null;
}

/** A name under the pointer and what it stands for. */
export interface NameHit {
  from: number;
  to: number;
  object: DbObject;
  view: ObjectView;
}

/** An identifier, bare or quoted ([x], "x", `x`). */
const ID = String.raw`(?:[\p{L}_#@$][\p{L}\p{N}_$#@]*|\[[^\]\n]+\]|"[^"\n]+"|\x60[^\x60\n]+\x60)`;
const CHAIN = new RegExp(String.raw`${ID}(?:\s*\.\s*${ID})*`, 'gu');
const PART = new RegExp(ID, 'gu');

function unquote(id: string): string {
  return /^[[\"`]/.test(id) ? id.slice(1, -1) : id;
}

const eq = (a: string | null | undefined, b: string) => (a ?? '').toLowerCase() === b.toLowerCase();

/** Objects named `path` (`[name]`, `[schema, name]`, `[db, schema, name]`):
 *  the exact spelling first, then any case. */
export function findObjects(idx: NameIndex, path: string[]): DbObject[] {
  let p = path;
  if (p.length > 2 && eq(p[0], idx.database)) p = p.slice(1);
  if (p.length > 2 || !p.length) return [];
  const [schema, name] = p.length === 2 ? p : [null, p[0]];
  const match = (exact: boolean) => idx.objects.filter((o) => {
    const same = (a: string | null, b: string) => (exact ? a === b : eq(a, b));
    return same(o.name, name) && (schema === null || same(o.schema, schema));
  });
  const exact = match(true);
  const all = exact.length ? exact : match(false);
  // Tables, views and collections before indexes or triggers of the same name.
  const kindOf = (o: DbObject) => idx.kinds.find((k) => k.id === o.kind);
  return [...all].sort((a, b) => Number(!!a.parent) - Number(!!b.parent) || Number(!kindOf(a)?.has_columns) - Number(!kindOf(b)?.has_columns));
}

/** The tab "Ir a la definición" opens: views and routines show their code,
 *  tables and collections their structure, the rest their data. */
export function viewFor(idx: NameIndex, o: DbObject): ObjectView {
  const k = idx.kinds.find((x) => x.id === o.kind);
  if (!k) return 'data';
  const viewLike = /view/.test(k.id) || !k.browsable;
  if (viewLike) return k.has_definition ? 'definition' : k.has_columns ? 'structure' : 'data';
  return k.has_columns ? 'structure' : k.browsable ? 'data' : 'definition';
}

/** alias (lower case) → the name it stands for, per statement of `masked`;
 *  an alias used for two different tables in the same text is left out. */
function aliasesIn(masked: string): Map<string, string[]> {
  const out = new Map<string, string[]>();
  const twice = new Set<string>();
  const re = new RegExp(String.raw`(?:\b(?:from|join|update|into|using)|,)\s*(${ID}(?:\s*\.\s*${ID})*)\s+(?:as\s+)?(${ID})`, 'giu');
  for (let m = re.exec(masked); m; m = re.exec(masked)) {
    const alias = unquote(m[2]).toLowerCase();
    if (KEYWORDS.has(alias)) {
      // `…, p.total FROM t x`: the keyword starts the next match.
      re.lastIndex = m.index + m[0].length - m[2].length;
      continue;
    }
    const path = splitChain(m[1]).map((p) => p.text);
    const had = out.get(alias);
    if (had && had.join('.').toLowerCase() !== path.join('.').toLowerCase()) twice.add(alias);
    out.set(alias, path);
  }
  for (const a of twice) out.delete(a);
  return out;
}

function splitChain(chain: string, base = 0): { text: string; from: number; to: number }[] {
  return [...chain.matchAll(PART)].map((m) => ({ text: unquote(m[0]), from: base + m.index!, to: base + m.index! + m[0].length }));
}

/** The name at `pos` and the object it stands for, or null. */
export function nameAt(idx: NameIndex, doc: string, pos: number): NameHit | null {
  if (!idx.objects.length) return null;
  const sqlLike = idx.language === 'sql' || idx.language === 'cql';
  const syn = lexSyntax(idx.language, idx.dialect);
  if (sqlLike) {
    const kind = regionAt(lexRegions(doc, syn), pos);
    if (kind === 'string' || kind === 'comment') return null;
    const line = lineAround(doc, pos);
    for (const m of line.text.matchAll(CHAIN)) {
      const from = line.from + m.index!;
      if (pos < from || pos > from + m[0].length) continue;
      const parts = splitChain(m[0], from);
      const k = parts.findIndex((p) => pos >= p.from && pos <= p.to);
      if (k < 0) return null;
      const hit = resolveParts(idx, doc, syn, parts.map((p) => p.text), k);
      return hit ? { from: parts[k].from, to: parts[k].to, object: hit, view: viewFor(idx, hit) } : null;
    }
    return null;
  }
  // mongosh (`db.pedidos.find`), HTTP consoles (`GET /pedidos/_search`),
  // Cypher labels (`:Persona`), Redis keys: a word, or the whole token.
  const line = lineAround(doc, pos);
  const at = pos - line.from;
  const around = (re: RegExp) => {
    let s = at;
    let e = at;
    while (s > 0 && re.test(line.text[s - 1])) s--;
    while (e < line.text.length && re.test(line.text[e])) e++;
    return { from: line.from + s, to: line.from + e, text: line.text.slice(s, e) };
  };
  for (const w of [around(/[\p{L}\p{N}_\-$]/u), around(/[^\s"'(){}[\],;]/u)]) {
    if (!w.text) continue;
    const found = idx.objects.find((o) => o.name === w.text) ?? idx.objects.find((o) => eq(o.name, w.text));
    if (found) return { from: w.from, to: w.to, object: found, view: viewFor(idx, found) };
  }
  return null;
}

function lineAround(doc: string, pos: number) {
  const from = doc.lastIndexOf('\n', Math.max(0, pos - 1)) + 1;
  const end = doc.indexOf('\n', pos);
  return { from, text: doc.slice(from, end < 0 ? doc.length : end) };
}

/** The object `parts[0..k]` names: an alias's table, a table, or the table
 *  of a column (`o.total` opens `orders`). */
function resolveParts(idx: NameIndex, doc: string, syn: ReturnType<typeof lexSyntax>, parts: string[], k: number): DbObject | null {
  const aliases = aliasesIn(maskCode(doc, syn));
  let path = parts.slice(0, k + 1);
  const viaAlias = aliases.get(parts[0].toLowerCase());
  if (viaAlias && (parts.length > 1 || !findObjects(idx, path).length)) path = [...viaAlias, ...path.slice(1)];
  for (let n = path.length; n >= 1; n--) {
    const found = findObjects(idx, path.slice(0, n));
    if (found.length) return found[0];
  }
  return null;
}

// -- unknown names ---------------------------------------------------------------------------

/** Words that never are a table's alias, nor a table after FROM. */
const KEYWORDS = new Set((
  'select from where join inner left right full cross outer natural on using group order by having union all except intersect minus ' +
  'limit offset fetch set values as with into update delete insert merge when then else end case and or not in exists is null like ' +
  'between window qualify pivot unpivot lateral only for returning top distinct apply option tablesample partition sample final prewhere format settings'
).split(' '));

/** Names that exist without being in the explorer: engine catalogs and
 *  pseudo tables (`dual`, a trigger's `inserted`, PostgreSQL's `excluded`). */
const BUILTIN_NAME = /^(dual|inserted|deleted|new|old|excluded|sysdummy1)$|^(pg_|sys|sqlite_|information_schema|all_|user_|dba_|v\$|gv\$|cdb_|duckdb_|system\.)/i;
/** Columns every row has without being listed. */
const PSEUDO_COLUMN = /^(rowid|oid|ctid|xmin|xmax|cmin|cmax|tableoid|rownum|_rowid_|ora_rowscn|rowversion|\$action|_id|\*)$/i;

/** Engines whose names the explorer can't vouch for: paths of a file system
 *  (Drill, IoTDB), containers that are an alias (Cosmos), keyspaces of
 *  buckets (Couchbase), child tables created on write (TDengine). */
const NO_NAME_CHECK = new Set(['cosmos', 'n1ql', 'iotdb', 'tdengine', 'drill', 'dremio']);
/** Engines whose rows have free-form fields: tables are checked, columns never. */
const NO_COLUMN_CHECK = new Set(['partiql', 'orientdb', 'influxql', 'influxdb3', 'ksql', 'spanner', 'bigquery', 'snowflake', 'databricks', 'sparksql', 'hive']);

interface Tok { text: string; start: number; end: number; word: boolean }

const TOKEN = /\[[^\]\n]*\]|"[^"\n]*"|`[^`\n]*`|'[^'\n]*'|[\p{L}_#@$][\p{L}\p{N}_$#@]*|\d[\w.]*|::|:[\p{L}_][\p{L}\p{N}_]*|[.,();=*?]|\S/gu;

function tokens(masked: string): Tok[] {
  return [...masked.matchAll(TOKEN)].map((m) => ({
    text: m[0], start: m.index!, end: m.index! + m[0].length,
    word: /^([\p{L}_#@$]|\[|"|`)/u.test(m[0]),
  }));
}

/** A name read at token `i`: its parts, where it ends. */
function readChain(t: Tok[], i: number): { parts: Tok[]; next: number } | null {
  if (!t[i]?.word) return null;
  const parts = [t[i]];
  let j = i + 1;
  while (t[j]?.text === '.' && t[j + 1]?.word) {
    parts.push(t[j + 1]);
    j += 2;
  }
  return { parts, next: j };
}

const lc = (t: Tok | undefined) => (t ? t.text.toLowerCase() : '');

/** Warnings for tables and columns that don't exist in the tab's database,
 *  or [] when that can't be told (objects not loaded, an engine whose names
 *  aren't all listed). Never errors: running is never blocked. */
export function unknownNames(idx: NameIndex, doc: string): LintFinding[] {
  if ((idx.language !== 'sql' && idx.language !== 'cql') || NO_NAME_CHECK.has(idx.dialect) || !idx.objects.length) return [];
  const masked = maskCode(doc, lexSyntax(idx.language, idx.dialect));
  const t = tokens(masked);
  const checkColumns = !NO_COLUMN_CHECK.has(idx.dialect);

  // Names the script makes itself: CTEs, CREATE …, SELECT … INTO, DECLARE @t TABLE.
  const local = new Set<string>();
  const createRe = new RegExp(String.raw`\bcreate\s+(?:or\s+(?:replace|alter)\s+)?(?:(?:global|local|temp|temporary|unlogged|volatile|transient|external|materialized|multiset)\s+)*(?:table|view|function|procedure|synonym|type|sequence|stream|collection)\s+(?:if\s+not\s+exists\s+)?(${ID}(?:\s*\.\s*${ID})*)`, 'giu');
  for (const m of masked.matchAll(createRe)) local.add(splitChain(m[1]).at(-1)!.text.toLowerCase());
  for (let i = 0; i < t.length; i++) {
    // `name AS (` and `name (a, b) AS (`: a CTE.
    if (t[i].word && lc(t[i + 1]) === 'as' && t[i + 2]?.text === '(') local.add(unquote(t[i].text).toLowerCase());
    if (t[i].word && t[i + 1]?.text === '(') {
      let j = i + 2;
      let depth = 1;
      while (j < t.length && depth) { if (t[j].text === '(') depth++; else if (t[j].text === ')') depth--; j++; }
      if (lc(t[j]) === 'as' && t[j + 1]?.text === '(') local.add(unquote(t[i].text).toLowerCase());
    }
    // T-SQL's SELECT … INTO new_table (not INSERT INTO).
    if (lc(t[i]) === 'into' && !['insert', 'replace', 'merge', 'ignore'].includes(lc(t[i - 1]))) {
      const c = readChain(t, i + 1);
      if (c) local.add(unquote(c.parts.at(-1)!.text).toLowerCase());
    }
  }

  const schemas = new Set([...idx.schemas, ...idx.objects.map((o) => o.schema ?? '')].filter(Boolean).map((s) => s.toLowerCase()));
  const out: LintFinding[] = [];
  const lineOf = (pos: number) => doc.slice(0, pos).split('\n').length;
  /** table (lower-case path) → its object, per statement. */
  let stmtTables = new Map<string, DbObject>();
  let stmtAliases = new Map<string, DbObject | null>();
  let stmtStart = 0;
  /** Parentheses: true for a function's (`EXTRACT(… FROM x)`), false for a subquery's. */
  const parens: boolean[] = [];
  /** Parenthesis depths whose FROM list is being read (`FROM a, b`). */
  const fromAt = new Set<number>();
  /** Column references to check once the statement's tables are known. */
  let pendingCols: { owner: string[]; col: Tok }[] = [];
  let insertCols: { table: DbObject; cols: Tok[] } | null = null;
  let pendingInsert: { table: DbObject; cols: Tok[] }[] = [];

  const flush = () => {
    for (const p of pendingCols) {
      let table: DbObject | null | undefined;
      if (p.owner.length === 1) {
        const key = p.owner[0].toLowerCase();
        table = stmtAliases.has(key) ? stmtAliases.get(key) : stmtTables.get(key);
      } else {
        table = stmtTables.get(p.owner.map((x) => x.toLowerCase()).join('.'));
      }
      if (table) checkColumn(table, p.col);
    }
    for (const ins of pendingInsert) for (const c of ins.cols) checkColumn(ins.table, c);
    pendingCols = [];
    pendingInsert = [];
    stmtTables = new Map();
    stmtAliases = new Map();
  };
  const checkColumn = (table: DbObject, col: Tok) => {
    if (!checkColumns) return;
    const name = unquote(col.text);
    if (PSEUDO_COLUMN.test(name) || /^[@#:$?]/.test(name)) return;
    const cols = idx.columns(table);
    if (!cols?.length) return;
    if (cols.some((c) => eq(c, name))) return;
    out.push({
      rule: 'unknown-column', severity: 'warning', start: col.start, end: col.end, line: lineOf(col.start),
      params: { column: name, table: table.schema ? `${table.schema}.${table.name}` : table.name },
    });
  };
  /** A table reference at token `i`: checked, remembered with its alias. */
  const tableRef = (i: number, inInsert = false): number => {
    while (['only', 'lateral'].includes(lc(t[i]))) i++;
    const c = readChain(t, i);
    if (!c) return i;
    // A table function (`generate_series(…)`, `OPENJSON(…)`): not a table.
    if (t[c.next]?.text === '(' && !inInsert) return c.next;
    const path = c.parts.map((p) => unquote(p.text));
    const last = c.parts.at(-1)!;
    let obj: DbObject | null = null;
    const quietName = /^[@#:$?]/.test(c.parts[0].text) || path.some((p) => p.includes('@')) || BUILTIN_NAME.test(path.join('.')) || BUILTIN_NAME.test(path.at(-1)!)
      || local.has(path.at(-1)!.toLowerCase()) || KEYWORDS.has(path.at(-1)!.toLowerCase());
    if (!quietName) {
      const found = findObjects(idx, path);
      obj = found[0] ?? null;
      const ownSchema = path.length === 1 || (path.length === 2 && schemas.has(path[0].toLowerCase()))
        || (path.length === 3 && eq(path[0], idx.database) && schemas.has(path[1].toLowerCase()));
      if (!obj && ownSchema) {
        out.push({ rule: 'unknown-table', severity: 'warning', start: c.parts[0].start, end: last.end, line: lineOf(c.parts[0].start), params: { name: path.join('.') } });
      }
    }
    if (obj) {
      stmtTables.set(path.map((p) => p.toLowerCase()).join('.'), obj);
      stmtTables.set(obj.name.toLowerCase(), obj);
    }
    let j = c.next;
    if (lc(t[j]) === 'as') j++;
    if (t[j]?.word && !KEYWORDS.has(lc(t[j]))) {
      const alias = unquote(t[j].text).toLowerCase();
      // The same alias for two tables in one statement: ambiguous, not checked.
      if (stmtAliases.has(alias) && stmtAliases.get(alias) !== obj) stmtAliases.set(alias, null);
      else stmtAliases.set(alias, obj);
      j++;
    }
    if (inInsert && obj && t[j]?.text === '(') {
      insertCols = { table: obj, cols: [] };
      let k = j + 1;
      while (k < t.length && t[k].text !== ')') {
        if (t[k].word && (t[k + 1]?.text === ',' || t[k + 1]?.text === ')')) insertCols.cols.push(t[k]);
        k++;
      }
      pendingInsert.push(insertCols);
      insertCols = null;
      return k + 1;
    }
    return j;
  };

  for (let i = 0; i < t.length; i++) {
    const tok = t[i];
    const w = lc(tok);
    if (tok.text === ';' || (w === 'go' && /^\s*go\s*$/im.test(masked.slice(masked.lastIndexOf('\n', tok.start) + 1, masked.indexOf('\n', tok.end) < 0 ? masked.length : masked.indexOf('\n', tok.end))))) {
      flush();
      stmtStart = i + 1;
      fromAt.clear();
      parens.length = 0;
      continue;
    }
    if (tok.text === '(') {
      const prev = t[i - 1];
      parens.push(!!prev?.word && !KEYWORDS.has(lc(prev)) && !['exists', 'in', 'any', 'some'].includes(lc(prev)));
      continue;
    }
    if (tok.text === ')') {
      fromAt.delete(parens.length);
      parens.pop();
      continue;
    }
    const inFunction = parens.some(Boolean);
    const first = lc(t[stmtStart]);
    if (w === 'from' && !inFunction) {
      if (lc(t[i - 1]) === 'distinct' || ['revoke', 'deny', 'fetch', 'move', 'close', 'copy', 'load', 'unload'].includes(first)) continue;
      fromAt.add(parens.length);
      i = tableRef(i + 1) - 1;
    } else if (w === 'join' && !inFunction) {
      fromAt.add(parens.length);
      i = tableRef(i + 1) - 1;
    } else if (tok.text === ',' && fromAt.has(parens.length) && !inFunction) {
      i = tableRef(i + 1) - 1;
    } else if (['where', 'group', 'order', 'having', 'limit', 'union', 'except', 'intersect', 'set', 'values', 'returning', 'window', 'on', 'select'].includes(w)) {
      fromAt.delete(parens.length);
    } else if (w === 'update' && !inFunction && !['for', 'key', 'do', 'then', 'on', 'no', 'and'].includes(lc(t[i - 1]))) {
      let j = i + 1;
      if (['statistics', 'stats'].includes(lc(t[j]))) continue;
      if (lc(t[j]) === 'top' && t[j + 1]?.text === '(') { while (j < t.length && t[j].text !== ')') j++; j++; }
      i = tableRef(j) - 1;
    } else if (w === 'into' && ['insert', 'replace', 'merge', 'ignore'].includes(lc(t[i - 1])) && !inFunction) {
      i = tableRef(i + 1, true) - 1;
    } else if (tok.word && t[i + 1]?.text === '.' && t[i + 2] && !inFunction) {
      // `alias.column`, `table.column`, `schema.table.column`.
      const c = readChain(t, i);
      if (c && c.parts.length >= 2 && t[c.next]?.text !== '(') {
        const owner = c.parts.slice(0, -1).map((p) => unquote(p.text));
        if (owner.length <= 2 && !/^[@#:$]/.test(owner[0])) pendingCols.push({ owner, col: c.parts.at(-1)! });
        i = c.next - 1;
      } else if (c) {
        i = c.next - 1;
      }
    }
  }
  flush();
  return out;
}
