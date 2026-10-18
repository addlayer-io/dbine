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
/** "Este archivo incluye una vista previa…" was shown once. */
export const PREVIEW_NOTICE_SETTING = 'notebook.previewNoticeShown';

export const clamp = (n: number, [lo, hi]: readonly [number, number]) => Math.min(hi, Math.max(lo, Math.round(n)));

export function newCellId(): string {
  return crypto.randomUUID().replace(/-/g, '').slice(0, 16);
}

export function newCell(kind: Exclude<CellKind, 'foreign'>, source = ''): NotebookCell {
  return {
    id: newCellId(), kind, source, language: kind === 'sql' ? 'sql' : null, target: null, keep_result: true,
    chart: null, execution_count: null, result: null, raw: null,
  };
}

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
  (a?.connection ?? null) === (b?.connection ?? null) && (a?.database ?? null) === (b?.database ?? null) && (a?.environment ?? null) === (b?.environment ?? null);

export const emptyTarget = (t: NotebookTarget | null | undefined) => !t || (!t.connection && !t.environment);

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
