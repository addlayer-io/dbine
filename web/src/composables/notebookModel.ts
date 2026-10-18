import type {
  CellKind, CellResult, Notebook, NotebookCell, NotebookTarget, QueryOutcome, StatementResult, StoredStatement,
} from '../api/types';

// The notebook's model on the UI side (docs/notebooks.md): new cells, the
// conversion between a run's QueryOutcome and the CellResult the file
// stores, and the outline the assistant gets (never rows). No Vue, no
// stores: plain functions.

export const ROWS_SETTING = 'notebook.rowsPerCell';
export const BYTES_SETTING = 'notebook.bytesPerCell';
export const ROWS_DEFAULT = 100;
export const BYTES_DEFAULT = 1_000_000;
/** The backend clamps to these too. */
export const ROWS_RANGE = [1, 1000] as const;
export const BYTES_RANGE = [64 * 1024, 8 * 1024 * 1024] as const;
/** Rows per table a "DataFrame SQL" cell loads (src-tauri notebook::local). */
export const RESULTS_CAP_SETTING = 'notebook.resultsRowCap';
export const RESULTS_CAP_DEFAULT = 1_000_000;
export const RESULTS_CAP_RANGE = [1_000, 10_000_000] as const;
/** The AWS profile `s3://` reads use in "DataFrame SQL" cells (empty: the
 *  environment's AWS_PROFILE, else `default`). Only its name: the keys stay
 *  in ~/.aws, read by the backend's AWS chain. */
export const AWS_PROFILE_SETTING = 'notebook.awsProfile';
/** "Este archivo incluye una vista previa…" was shown once. */
export const PREVIEW_NOTICE_SETTING = 'notebook.previewNoticeShown';

export const clamp = (n: number, [lo, hi]: readonly [number, number]) => Math.min(hi, Math.max(lo, Math.round(n)));

export function newCellId(): string {
  return crypto.randomUUID().replace(/-/g, '').slice(0, 16);
}

export function newCell(kind: Exclude<CellKind, 'foreign'>, source = ''): NotebookCell {
  return {
    id: newCellId(), kind, source, language: kind === 'sql' ? 'sql' : null, target: null, name: null, keep_result: true,
    chart: null, execution_count: null, result: null, raw: null,
  };
}

// -- cell names: tables for "DataFrame SQL" cells ------------------------------------

/** Words a cell can't be named (DuckDB reads them as SQL); as notebook::local::RESERVED. */
const RESERVED = new Set(('all and any array as asc between both by case cast check collate column constraint create cross current_date '
  + 'current_time current_timestamp current_user default delete desc distinct do drop else end except exists false fetch for foreign '
  + 'from full grant group having in inner insert intersect into is join lateral left like limit natural not null offset on only or '
  + 'order outer pivot primary qualify references returning right select set some table then to true union unique unpivot update '
  + 'user using values when where window with').split(' '));

/** A name usable as a table: a letter or _, then letters, digits or _, up to 63, not a keyword. */
export function validCellName(name: string): boolean {
  return /^[A-Za-z_][A-Za-z0-9_]{0,62}$/.test(name) && !RESERVED.has(name.toLowerCase());
}

/** Why `name` can't be cell `id`'s name: 'invalid', 'taken' (another cell has it, ignoring case), or null. */
export function cellNameProblem(cells: NotebookCell[], id: string, name: string): 'invalid' | 'taken' | null {
  if (!validCellName(name)) return 'invalid';
  const lower = name.toLowerCase();
  return cells.some((c) => c.id !== id && c.kind === 'sql' && c.name?.toLowerCase() === lower) ? 'taken' : null;
}

/** The default name of a SQL cell at `at`: `c<n>`, n its place among the SQL
 *  cells (1-based), the next free one if that's taken. */
export function defaultCellName(cells: NotebookCell[], at: number): string {
  const taken = new Set(cells.flatMap((c) => (c.kind === 'sql' && c.name ? [c.name.toLowerCase()] : [])));
  let n = cells.slice(0, Math.max(0, at)).filter((c) => c.kind === 'sql').length + 1;
  while (taken.has(`c${n}`)) n++;
  return `c${n}`;
}

/** Name the SQL cells that have none (a file from before names, a cell
 *  that lost a repeated one): stable once saved. */
export function assignCellNames(cells: NotebookCell[]): NotebookCell[] {
  cells.forEach((c, i) => {
    if (c.kind === 'sql' && !(c.name && validCellName(c.name))) c.name = defaultCellName(cells.map((x) => (x === c ? { ...x, name: null } : x)), i);
  });
  return cells;
}

/** The target is "DataFrame SQL". */
export const isResultsTarget = (t: NotebookTarget | null | undefined) => !!t?.results;

/** A new notebook: one SQL cell. `gitLinked`: a git project's, saved without results by default. */
export function emptyNotebook(gitLinked: boolean, target: NotebookTarget | null = null): Notebook {
  return { target, save_results: !gitLinked, save_results_set: true, cells: [newCell('sql')], origin: 'dbine', extra_metadata: {}, warnings: [] };
}

/** A notebook read from a git project's file that doesn't say whether it
 *  keeps results (a foreign one): the project's default, without results. */
export function applyProjectDefault(n: Notebook, gitLinked: boolean): Notebook {
  if (gitLinked && !n.save_results_set) n.save_results = false;
  return n;
}

export const sameTarget = (a: NotebookTarget | null | undefined, b: NotebookTarget | null | undefined) =>
  (a?.connection ?? null) === (b?.connection ?? null) && (a?.database ?? null) === (b?.database ?? null) && (a?.environment ?? null) === (b?.environment ?? null)
  && !!a?.results === !!b?.results;

export const emptyTarget = (t: NotebookTarget | null | undefined) => !t || (!t.connection && !t.environment && !t.results);

// -- results --------------------------------------------------------------------------------

/** A statement's result as the file keeps it. `statement`: its text, when known. */
function stored(r: StatementResult, statement: string | null): StoredStatement {
  return {
    columns: r.columns.map((c) => ({ name: c.name, type_name: c.type_name })),
    rows: r.rows,
    rows_total: Math.max(r.total_rows, r.rows.length),
    rows_kept: r.rows.length,
    truncated: r.truncated || r.total_rows > r.rows.length,
    rows_affected: r.rows_affected,
    statement: statement?.trim().slice(0, 4000) || null,
  };
}

/** Each result's statement text: between its offset and the next one's
 *  (the whole script when there's a single result and no offsets). */
function statementTexts(results: StatementResult[], script: string): (string | null)[] {
  if (results.length === 1 && results[0].offset == null) return [script];
  return results.map((r, i) => {
    if (r.offset == null) return null;
    const next = results.slice(i + 1).find((x) => x.offset != null && x.offset > r.offset!)?.offset ?? script.length;
    return script.slice(r.offset, next).replace(/;\s*$/, '');
  });
}

/** What a live run leaves on the cell (the UI keeps every row in memory;
 *  the backend cuts the preview when it writes the file). */
export function outcomeToResult(o: QueryOutcome, o2: { script: string; ranAt: Date; engine: string | null; targetLabel: string | null }): CellResult {
  const messages = (o.log?.length ? o.log.filter((m) => m.level !== 'error').map((m) => m.text) : o.messages).map((m) => m.trim()).filter(Boolean);
  const errors = o.errors?.length ? o.errors.map((e) => e.message) : o.error ? [o.error] : [];
  return {
    results: (() => {
      const kept = o.results.filter((r) => r.columns.length || r.rows_affected != null);
      const texts = statementTexts(kept, o2.script);
      return kept.map((r, i) => stored(r, texts[i]));
    })(),
    messages,
    error: errors.length ? errors.join('\n') : null,
    ran_at: o2.ranAt.toISOString().replace(/\.\d{3}Z$/, 'Z'),
    duration_ms: Math.max(0, Math.round(o.elapsed_ms)),
    engine: o2.engine,
    target_label: o2.targetLabel,
  };
}

/** A stored result as the results pane shows it (a preview read from the file). */
export function resultToOutcome(r: CellResult): QueryOutcome {
  return {
    results: r.results.map((s) => ({
      columns: s.columns.map((c) => ({ name: c.name, type_name: c.type_name })),
      rows: s.rows,
      total_rows: s.rows_total,
      truncated: s.truncated,
      rows_affected: s.rows_affected,
    })),
    messages: r.messages,
    error: r.error,
    elapsed_ms: r.duration_ms,
    plans: [],
    log: [
      ...r.messages.map((text) => ({ level: 'info' as const, text, statement: null, code: null, line: null })),
      ...(r.error ? [{ level: 'error' as const, text: r.error, statement: null, code: null, line: null }] : []),
    ],
    errors: r.error ? [{ message: r.error, code: null, sqlstate: null, statement: null, offset: null, line: null, fatal: true }] : [],
  };
}

/** A run's first result set, when it is complete (no row cut it, no error):
 *  what a "DataFrame SQL" cell can load as that cell's table
 *  without running its query again. */
export function completeRows(o: QueryOutcome | null | undefined): { columns: StatementResult['columns']; rows: StatementResult['rows'] } | null {
  if (!o || o.error || o.errors?.length) return null;
  const r = o.results.find((x) => x.columns.length);
  if (!r || r.truncated || r.total_rows > r.rows.length) return null;
  return { columns: r.columns, rows: r.rows };
}

/** The rows shown and returned, over the result sets with columns. */
export function rowCounts(r: CellResult): { kept: number; total: number; truncated: boolean; sets: number } {
  const sets = r.results.filter((s) => s.columns.length);
  return {
    kept: sets.reduce((n, s) => n + s.rows_kept, 0),
    total: sets.reduce((n, s) => n + s.rows_total, 0),
    truncated: sets.some((s) => s.truncated),
    sets: sets.length,
  };
}

// -- the assistant (never row values) --------------------------------------------------------

/** A result described without its rows: columns and types, counts, the error. */
export function describeResult(r: CellResult | null): string {
  if (!r) return '';
  const out: string[] = [];
  r.results.forEach((s, i) => {
    if (s.columns.length) {
      out.push(`#${i + 1}: ${s.columns.map((c) => `${c.name} ${c.type_name}`.trim()).join(', ')}`);
      out.push(`  rows_total=${s.rows_total}${s.truncated ? ' (truncated preview)' : ''}`);
    } else if (s.rows_affected != null) out.push(`#${i + 1}: rows_affected=${s.rows_affected}`);
  });
  if (r.error) out.push(`error: ${r.error}`);
  return out.join('\n');
}

/** The notebook so far, up to cell `upTo` (included): text cells' headings and
 *  the SQL of the code cells. No results. */
export function outline(cells: NotebookCell[], upTo: number): string {
  const out: string[] = [];
  for (const c of cells.slice(0, upTo + 1)) {
    if (c.kind === 'markdown') {
      const heads = c.source.split('\n').filter((l) => /^\s{0,3}#{1,6}\s/.test(l)).map((l) => l.trim());
      if (heads.length) out.push(heads.join('\n'));
    } else if (c.kind === 'sql' && c.source.trim()) {
      out.push('```' + (c.language || 'sql') + '\n' + c.source.trim() + '\n```');
    }
  }
  return out.join('\n\n');
}

// -- connections by name ----------------------------------------------------------------------

/** What a file's connection name is on this machine. */
export type NameMatch =
  | { kind: 'one'; id: string }
  /** More than one local connection has that name and nothing says which. */
  | { kind: 'ambiguous'; ids: string[] }
  | { kind: 'none' };

/** A name in a file → a local connection. A unique exact name wins; a name
 *  several connections share uses the choice remembered for this file
 *  (`fileAlias`); a name nobody has uses the machine's alias, else a unique
 *  case-insensitive match. */
export function matchConnection(list: { id: string; name: string }[], name: string, fileAlias?: string | null, globalAlias?: string | null): NameMatch {
  const exact = list.filter((c) => c.name === name);
  if (exact.length === 1) return { kind: 'one', id: exact[0].id };
  if (exact.length > 1) {
    return fileAlias && exact.some((c) => c.id === fileAlias) ? { kind: 'one', id: fileAlias } : { kind: 'ambiguous', ids: exact.map((c) => c.id) };
  }
  if (fileAlias && list.some((c) => c.id === fileAlias)) return { kind: 'one', id: fileAlias };
  if (globalAlias && list.some((c) => c.id === globalAlias)) return { kind: 'one', id: globalAlias };
  const loose = list.filter((c) => c.name.trim().toLowerCase() === name.trim().toLowerCase());
  return loose.length === 1 ? { kind: 'one', id: loose[0].id } : loose.length > 1 ? { kind: 'ambiguous', ids: loose.map((c) => c.id) } : { kind: 'none' };
}

/** A path inside a linked project's folder: that project and the path
 *  relative to its root (with '/'). Null when it's in none. */
export function projectFileOf(projects: { id: string; path: string }[], path: string): { projectId: string; projectPath: string } | null {
  const norm = (p: string) => p.replace(/\\/g, '/').replace(/\/+$/, '');
  const file = norm(path);
  const win = /^[a-z]:\//i.test(file);
  const key = (p: string) => (win ? p.toLowerCase() : p);
  let best: { projectId: string; projectPath: string; len: number } | null = null;
  for (const p of projects) {
    const root = norm(p.path);
    if (!root || !key(file).startsWith(`${key(root)}/`)) continue;
    if (!best || root.length > best.len) best = { projectId: p.id, projectPath: file.slice(root.length + 1), len: root.length };
  }
  return best && { projectId: best.projectId, projectPath: best.projectPath };
}

// -- saving: edits made while a save is in flight stay unsaved --------------------------------

/** Counts edits; a save that started at version `v` ends 'saved' only if
 *  nothing changed since. */
export class EditVersion {
  version = 0;
  bump() { this.version++; }
  /** What a save serializes (call before reading the notebook). */
  start(): number { return this.version; }
  /** After the write: 'saved' if no edit came in meanwhile, else 'dirty'. */
  end(startedAt: number): 'saved' | 'dirty' { return this.version === startedAt ? 'saved' : 'dirty'; }
}

// -- running: one cell at a time --------------------------------------------------------------

/** Taken synchronously before any await: a second run is refused at once. */
export class RunLock {
  held = false;
  take(): boolean {
    if (this.held) return false;
    this.held = true;
    return true;
  }
  release() { this.held = false; }
}

// -- the assistant's prompts per target language ----------------------------------------------

/** The prompts' variant: SQL engines get "SQL" and GROUP BY; the others
 *  (Redis, MongoDB, Cypher…) are asked in their own language. */
export function promptVariant(language: string | null | undefined): 'sql' | 'native' {
  return !language || language === 'sql' ? 'sql' : 'native';
}
