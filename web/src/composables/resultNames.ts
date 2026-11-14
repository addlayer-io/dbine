import type { Message, QueryOutcome, ScriptError, StatementResult } from '../api/types';

// Named results (docs/query-editor.md#named-results): the line comment right
// above a statement names its result, and a named result has a result tab
// of its own in the editor. This file is the pure part: which comments count,
// the name above a position, and a run's outcome split by statement.

/** "Mostrar «▶ Ejecutar» sobre cada sentencia" (Configuración › General, on by default). */
export const RUN_LENS_SETTING = 'editor.runLens';
/** No lens past these: a script this long isn't read statement by statement. */
export const LENS_MAX_STATEMENTS = 500;
export const LENS_MAX_CHARS = 1_000_000;

/** A lens over a statement: where it starts in the editor and its name. */
export interface RunLens {
  start: number;
  name: string | null;
}

/** The line-comment markers of an engine, longest first: `--` in the SQL
 *  family, `#` where its dialect has hash comments (the MySQL family,
 *  ClickHouse, BigQuery, Spanner; Redis's and the JSON consoles' `#`
 *  lines), `//` where it has slash comments (Snowflake, Dremio, Drill, CQL,
 *  the JSON consoles and MongoDB's JavaScript, Cypher, Flux), `/` in
 *  KDB+'s q, and both `//` and `--` in DAX. */
export function commentMarkers(language: string | undefined, dialect: string | undefined): string[] {
  if (dialect === 'q') return ['/'];
  if (language === 'dax') return ['//', '--'];
  const out: string[] = [];
  if (language === 'json' || language === 'cypher' || language === 'flux' || language === 'cql'
    || dialect === 'snowflake' || dialect === 'dremio' || dialect === 'drill') out.push('//');
  if (language === 'sql' || language === 'cql' || language === undefined) out.push('--');
  if (language === 'redis' || language === 'json' || dialect === 'mysql' || dialect === 'mariadb' || dialect === 'clickhouse'
    || dialect === 'bigquery' || dialect === 'spanner') out.push('#');
  return out;
}

/** A line that is a line comment, with the name it gives (null: none), or
 *  null when the line is something else. Db2's client directives
 *  (`--#SET TERMINATOR @`) are comments to the server but not names. */
function lineComment(line: string, markers: string[], dialect?: string): { name: string | null } | null {
  const s = line.replace(/\r$/, '').trim();
  const marker = markers.find((m) => s.startsWith(m));
  if (!marker) return null;
  if (dialect === 'db2' && /^--#SET\b/i.test(s)) return { name: null };
  // `----- Totales` and `## Totales` too: the marker's character repeated.
  const ch = marker[0].replace(/[.*+?^${}()|[\]\\/]/g, '\\$&');
  const name = s.slice(marker.length).replace(new RegExp(`^${ch}*`), '').trim().replace(/\s+/g, ' ');
  return { name: name || null };
}

/** The name the comment right above `offset` gives it: the line comment on
 *  the line just before the one `offset` is on (no blank line between),
 *  its marker and spaces stripped. Null when `offset` isn't the first thing
 *  on its line, that line isn't a line comment, or nothing is left. */
export function nameAbove(text: string, offset: number, markers: string[], dialect?: string): string | null {
  if (!markers.length || offset < 0 || offset > text.length) return null;
  const lineStart = text.lastIndexOf('\n', offset - 1) + 1;
  // Code before it on its line (`SELECT 1; SELECT 2;`): the comment above
  // names what starts that line, not this.
  if (text.slice(lineStart, offset).trim()) return null;
  if (lineStart === 0) return null;
  const aboveStart = text.lastIndexOf('\n', lineStart - 2) + 1;
  return lineComment(text.slice(aboveStart, lineStart - 1), markers, dialect)?.name ?? null;
}

/** Where the code of a statement that starts at `at` in `text` (and ends
 *  at `end`) begins, and its name. Where the engine's splitter doesn't take
 *  the marker for a comment (MongoDB's and Cypher's `//`, Redis's and the
 *  JSON consoles' `#`), a statement starts on the comment lines above it:
 *  they're skipped, and the last one before the code, with no blank line
 *  between, names it. A statement that doesn't start with a comment line
 *  is named by the line above it ({@link nameAbove}). A comment after code
 *  on the same line (the statement before) is neither. `start` null: there
 *  is nothing but comments. */
export function statementHead(text: string, at: number, end: number, markers: string[], dialect?: string): { start: number | null; name: string | null } {
  const stop = Math.min(end, text.length);
  let i = Math.max(0, at);
  let name: string | null = null;
  let commented = false;
  while (i < stop) {
    // The next thing written, and whether a blank line comes before it.
    let j = i;
    let newlines = 0;
    for (; j < stop && /\s/.test(text[j]); j++) if (text[j] === '\n') newlines++;
    if (j >= stop) break;
    if (newlines > 1) name = null;
    const lineStart = text.lastIndexOf('\n', j - 1) + 1;
    const nl = text.indexOf('\n', j);
    const lineEnd = nl < 0 || nl > stop ? stop : nl;
    const after = !!text.slice(lineStart, j).trim();
    const comment = lineComment(text.slice(j, lineEnd), markers, dialect);
    if (!comment) return { start: j, name: after ? null : commented ? name : nameAbove(text, j, markers, dialect) };
    if (!after) {
      name = comment.name;
      commented = true;
    }
    i = lineEnd;
  }
  return { start: null, name: null };
}

/** The end of a statement's text without the blank space and whole comment
 *  lines at its end (they're the next statement's). */
function trimTail(text: string, start: number, end: number, markers: string[]): number {
  for (;;) {
    while (end > start && /\s/.test(text[end - 1])) end--;
    const lineStart = text.lastIndexOf('\n', end - 1) + 1;
    if (lineStart <= start || !lineComment(text.slice(lineStart, end), markers)) return end;
    end = lineStart;
  }
}

/** For a script the engine got whole and numbered itself (MongoDB, Redis,
 *  Neo4j…): each statement its outcome numbers, where it is in `sent`: from
 *  where its results (or its error) say it starts up to where the next one
 *  starts, without the blank space and comment lines at its end. In the
 *  order of the text; none when the outcome numbers nothing. */
export function numberedStatements(o: QueryOutcome, sent: string, markers: string[]): { statement: number; start: number; end: number }[] {
  const at = new Map<number, number>();
  const note = (s: number | null | undefined, offset: number | null | undefined) => {
    if (s == null || offset == null) return;
    const k = Math.min(Math.max(0, offset), sent.length);
    if (!at.has(s) || k < at.get(s)!) at.set(s, k);
  };
  for (const r of o.results) note(r.statement, r.offset);
  for (const e of o.errors ?? []) note(e.statement, e.offset);
  const list = [...at].map(([statement, start]) => ({ statement, start })).sort((a, b) => a.start - b.start || a.statement - b.statement);
  return list.map((s, i) => {
    const next = list[i + 1]?.start ?? sent.length;
    let start = s.start;
    while (start < next && /\s/.test(sent[start])) start++;
    return { statement: s.statement, start, end: Math.max(start, trimTail(sent, start, next, markers)) };
  });
}

/** The units with more than one statement in them (a SQL Server batch with
 *  no GO between its statements) left unnamed: a name is a statement's.
 *  `starts`: where the text's statements start. */
export function oneStatementEach<T extends { start: number; end: number; name: string | null }>(units: T[], starts: number[]): T[] {
  const sorted = [...starts].sort((a, b) => a - b);
  const from = (x: number) => {
    let lo = 0;
    let hi = sorted.length;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (sorted[mid] < x) lo = mid + 1;
      else hi = mid;
    }
    return lo;
  };
  return units.map((u) => (u.name && from(Math.max(u.end, u.start + 1)) - from(u.start) > 1 ? { ...u, name: null } : u));
}

/** A unit of a run (a statement or a batch, as the engine runs them) with
 *  what naming needs: where it is in the text sent and in the editor. */
export interface RunUnit {
  /** Where it starts and ends in the text sent (JS string indices), and its 1-based line there. */
  start: number;
  end: number;
  line: number;
  /** Its text (as sent: parameters filled). */
  text: string;
  /** Where it starts in the editor, and that 1-based line. */
  editorStart: number;
  editorLine: number;
  name: string | null;
  /** A client command (DELIMITER, SET TERM), or nothing but comments:
   *  never named, never shown. */
  client: boolean;
}

/** The names of a run's units with each name kept once: a later statement
 *  of the same run with a name already taken stays unnamed. */
export function uniqueNames<T extends { name: string | null }>(units: T[]): T[] {
  const seen = new Set<string>();
  return units.map((u) => {
    if (!u.name) return u;
    if (seen.has(u.name)) return { ...u, name: null };
    seen.add(u.name);
    return u;
  });
}

/** 1-based line of each offset of `text` (offsets in any order). */
export function linesAt(text: string, offsets: number[]): number[] {
  const order = offsets.map((o, i) => [o, i] as const).sort((a, b) => a[0] - b[0]);
  const out: number[] = new Array(offsets.length);
  let line = 1;
  let at = 0;
  for (const [o, i] of order) {
    const end = Math.min(Math.max(0, o), text.length);
    for (; at < end; at++) if (text.charCodeAt(at) === 10) line++;
    out[i] = line;
  }
  return out;
}

export interface NamedPart {
  name: string;
  /** Its statement index in the outcome (the index of its unit in `units`). */
  unit: number;
  /** Named by the comment above where its results start in their unit
   *  ({@link ByPlace}), not by its unit's: only some of the unit's results. */
  placed?: boolean;
  /** Its results, messages and errors, with offsets and lines from the
   *  unit's own start (the tab's script is the unit's text). */
  outcome: QueryOutcome;
}

const empty = (): QueryOutcome => ({ results: [], messages: [], error: null, elapsed_ms: 0, plans: [], log: [], errors: [] });

/** The statements of a run whose results start in different places (a DAX
 *  `DEFINE` and its `EVALUATE`s, one result each), with those places: where
 *  in the text sent, sorted. A statement whose results all start in one
 *  place (every SQL statement, where they start where it does) isn't one. */
export function resultPlaces(o: QueryOutcome): Map<number, number[]> {
  const at = new Map<number, Set<number>>();
  for (const r of o.results) {
    if (r.statement == null || r.offset == null) continue;
    let s = at.get(r.statement);
    if (!s) at.set(r.statement, (s = new Set()));
    s.add(r.offset);
  }
  const out = new Map<number, number[]>();
  for (const [s, places] of at) if (places.size > 1) out.set(s, [...places].sort((a, b) => a - b));
  return out;
}

/** Results named by where they start: in a statement whose results start in
 *  different places ({@link resultPlaces}), the comment above a result's
 *  place names it, and it goes to a tab of its own; one without such a
 *  comment stays with its statement. */
export interface ByPlace {
  /** The name above `offset` of the text sent, in statement `unit`. */
  nameAt: (offset: number, unit: number) => string | null;
  /** The name of the tab the run is in: a result with it stays there. */
  kept?: string | null;
}

/** Each result's own name by its place ({@link ByPlace}), by result index:
 *  null where it has none, or where its statement's results all start in
 *  one place. A name a statement of the run has, or an earlier result
 *  started elsewhere, is that one's: the result stays with its statement. */
function placeNames(o: QueryOutcome, units: ({ name: string | null } | undefined)[], nameAt: ByPlace['nameAt']): (string | null)[] {
  const places = resultPlaces(o);
  if (!places.size) return o.results.map(() => null);
  const taken = new Set(units.map((u) => u?.name).filter((n): n is string => !!n));
  const where = new Map<string, string>();
  return o.results.map((r) => {
    if (r.statement == null || r.offset == null || !places.has(r.statement) || !units[r.statement]) return null;
    const name = nameAt(r.offset, r.statement);
    if (!name || taken.has(name)) return null;
    const place = `${r.statement}:${r.offset}`;
    if ((where.get(name) ?? place) !== place) return null;
    where.set(name, place);
    return name;
  });
}

/** In a statement whose results start in different places (`places`: where
 *  in its `text`, sorted), the text that gives the one at `place` by itself
 *  (its export and its chart run it, since a tab holds only some of them):
 *  what comes before the first place, which they share (a DAX `DEFINE`),
 *  and its own text up to the next place, without the comment lines at its
 *  end (the next one's). */
export function placeText(text: string, places: number[], place: number, markers: string[]): string {
  const next = places.find((p) => p > place) ?? text.length;
  const own = text.slice(place, trimTail(text, place, next, markers));
  const shared = text.slice(0, trimTail(text, 0, Math.min(places[0] ?? place, place), markers));
  return shared ? `${shared}\n${own}` : own;
}

/** A run's outcome split by statement: the parts of each named unit (its
 *  results, log entries and errors, by statement index), in the order the
 *  units ran, and what's left (unnamed statements, what has no statement,
 *  plans and plain messages), or null when nothing is. With `byPlace`, a
 *  result named by its place has a part of its own, after its unit's. */
export function splitOutcome(
  o: QueryOutcome,
  units: (Pick<RunUnit, 'start' | 'line' | 'name'> | undefined)[],
  byPlace?: ByPlace,
): { rest: QueryOutcome | null; parts: NamedPart[] } {
  const named = (s: number | null | undefined) => (s != null && units[s]?.name ? s : null);
  // By its unit's index, or by the name a result gives itself.
  const parts = new Map<number | string, { unit: number; outcome: QueryOutcome }>();
  const partOf = (key: number | string, s: number) => {
    let p = parts.get(key);
    if (!p) parts.set(key, (p = { unit: s, outcome: empty() }));
    return p.outcome;
  };
  const rest = empty();
  const shiftLine = (line: number | null | undefined, s: number) => (line == null ? line ?? null : Math.max(1, line - units[s]!.line + 1));
  const shiftOffset = (offset: number | null | undefined, s: number) => (offset == null ? offset ?? null : Math.max(0, offset - units[s]!.start));

  const own = byPlace ? placeNames(o, units, byPlace.nameAt) : [];
  o.results.forEach((r, i) => {
    const name = own[i] ?? null;
    // Named as the tab the run is in: it stays there.
    if (name !== null && name === byPlace?.kept) { rest.results.push(r); return; }
    const s = name !== null ? r.statement! : named(r.statement);
    if (s === null) { rest.results.push(r); return; }
    const moved: StatementResult = { ...r, offset: shiftOffset(r.offset, s), line: shiftLine(r.line, s) };
    partOf(name ?? s, s).results.push(moved);
  });
  for (const m of o.log ?? []) {
    const s = named(m.statement);
    if (s === null) { rest.log!.push(m); continue; }
    const moved: Message = { ...m, line: shiftLine(m.line, s) };
    partOf(s, s).log!.push(moved);
  }
  const movedErrors = new Set<string>();
  for (const e of o.errors ?? []) {
    const s = named(e.statement);
    if (s === null) { rest.errors!.push(e); continue; }
    const moved: ScriptError = { ...e, offset: shiftOffset(e.offset, s), line: shiftLine(e.line, s) };
    partOf(s, s).errors!.push(moved);
    movedErrors.add(e.message);
  }
  if (!parts.size) return { rest: o, parts: [] };

  // Plain messages: each part's are its log's (the pane shows only those
  // past the logged ones); the rest keeps the plain ones nobody logged.
  const texts = (log: Message[]) => log.filter((m) => m.level !== 'error').map((m) => m.text);
  const logged = texts(o.log ?? []).length;
  let named_ms = 0;
  // In the order the units ran; a unit's results named by their place after
  // its own part, in the order they came (a stable sort keeps it).
  const ordered = [...parts.entries()].sort((a, b) => a[1].unit - b[1].unit || Number(typeof a[0] === 'string') - Number(typeof b[0] === 'string'));
  const out: NamedPart[] = ordered.map(([key, { unit, outcome: p }]) => {
    p.messages = texts(p.log!);
    p.error = p.errors![0]?.message ?? null;
    p.elapsed_ms = p.results.reduce((n, r) => n + (r.elapsed_ms ?? 0), 0);
    named_ms += p.elapsed_ms;
    return typeof key === 'string' ? { name: key, unit, placed: true, outcome: p } : { name: units[unit]!.name!, unit, outcome: p };
  });
  rest.messages = [...texts(rest.log!), ...o.messages.slice(logged)];
  rest.plans = o.plans;
  rest.error = rest.errors![0]?.message ?? (o.error && !movedErrors.has(o.error) ? o.error : null);
  rest.elapsed_ms = Math.max(0, o.elapsed_ms - named_ms);
  rest.transaction = o.transaction;
  rest.database = o.database;
  const left = rest.results.length || rest.log!.length || rest.errors!.length || rest.plans.length || rest.messages.length || rest.error;
  return { rest: left ? rest : null, parts: out };
}
