import type { Language } from '../api/types';
import { lexRegions, lexSyntax } from './sqlLexer';

// Parameters in editor queries (docs/editor-de-consultas.md): `:name` outside
// strings and comments (never a `::` cast), plus the engine's own markers
// where they can't mean anything else: `?` on engines that take JDBC-style
// placeholders and `@name` on engines where `@` isn't a variable. Before a run
// the editor asks for each value and puts it in the text as a literal of the
// engine's dialect.

export type ParamType = 'text' | 'number' | 'date' | 'null';

export interface ParamValue {
  type: ParamType;
  value: string;
}

/** What a run sends to the engine: where each marker is. */
export interface ParamSpot {
  /** `name` for `:name` / `@name`; `?1`, `?2`… for positional `?`. */
  key: string;
  start: number;
  end: number;
}

/** A parameter to ask for (several spots may share it). */
export interface ParamItem {
  key: string;
  /** As written: `:cliente`, `@desde`, `? (1)`. */
  label: string;
}

/** Where the engine runs the text (the tab's driver). */
export interface ParamTarget {
  language: Language;
  dialect: string;
  /** The driver id (MongoDB's literals differ from other JSON consoles'). */
  driverId: string;
}

/** `?` is a placeholder there (and never an operator: not PostgreSQL's jsonb
 *  `?`, not ClickHouse's ternary). */
const QUESTION_DIALECTS = new Set([
  'mysql', 'mariadb', 'sqlite', 'db2', 'access', 'informix', 'standard', 'trino', 'snowflake', 'partiql',
  'n1ql', 'orientdb', 'hive', 'sparksql', 'databricks', 'phoenix', 'drill', 'exasol', 'vertica', 'cubrid', 'altibase',
]);
/** `@name` is a query parameter there; elsewhere (SQL Server, MySQL…) it's a variable. */
const AT_DIALECTS = new Set(['bigquery', 'spanner', 'cosmos']);

/** Which markers a target has; null when the editor doesn't look for any
 *  (Cypher has its own `$name` with `:param`, and `:Label` is syntax). */
export function paramMarkers(t: ParamTarget): { colon: boolean; question: boolean; at: boolean } | null {
  if (t.language === 'cypher') return null;
  if (t.language === 'sql') return { colon: true, question: QUESTION_DIALECTS.has(t.dialect), at: AT_DIALECTS.has(t.dialect) };
  if (t.language === 'cql') return { colon: true, question: true, at: false };
  return { colon: true, question: false, at: false };
}

/** A script that defines code (a trigger, a procedure, Firebird's EXECUTE
 *  BLOCK): its `:name` are the code's own variables, never asked for. */
const ROUTINE = /\b(?:(?:create|alter|recreate)\s+(?:or\s+(?:replace|alter)\s+)?(?:definer\s*=\s*\S+\s+)?(?:editionable\s+|noneditionable\s+)?(?:trigger|procedure|function|package|type\s+body)|execute\s+block)\b/i;

const NAME_START = /[\p{L}_]/u;
const NAME = /[\p{L}\p{N}_]+/uy;
/** A `:` right after these is not a parameter (`a::int`, `arr[i:j]`, `x:y`, `(…):x`). */
const NOT_BEFORE_COLON = /[\p{L}\p{N}_:\])'"`$.@#]/u;

/** The parameter markers of `sql`, in order. */
export function findParams(sql: string, t: ParamTarget): ParamSpot[] {
  const marks = paramMarkers(t);
  if (!marks) return [];
  const regions = lexRegions(sql, lexSyntax(t.language, t.dialect));
  const isSql = t.language === 'sql' || t.language === 'cql';
  if (isSql) {
    const code = regions.filter((r) => r.kind === 'code').map((r) => sql.slice(r.start, r.end)).join(' ');
    if (ROUTINE.test(code)) return [];
  }
  const out: ParamSpot[] = [];
  let positional = 0;
  /** `[`…`]` depth in code (array slices: `a[1:n]`). */
  let brackets = 0;
  for (const r of regions) {
    if (r.kind !== 'code') continue;
    for (let i = r.start; i < r.end; i++) {
      const c = sql[i];
      if (c === '[') { brackets++; continue; }
      if (c === ']') { brackets = Math.max(0, brackets - 1); continue; }
      const prev = i > 0 ? sql[i - 1] : '';
      if (c === ':' && marks.colon) {
        if (sql[i + 1] === ':' || prev === ':' || NOT_BEFORE_COLON.test(prev)) continue;
        if (isSql && brackets > 0) continue;
        if (!NAME_START.test(sql[i + 1] ?? '')) continue;
        NAME.lastIndex = i + 1;
        const m = NAME.exec(sql);
        if (!m) continue;
        const end = i + 1 + m[0].length;
        // Oracle trigger rows (`:new.col`, `:old.col`) are the trigger's.
        if (sql[end] === '.' && /^(new|old|parent)$/i.test(m[0])) continue;
        // `:=` assignment never matches (the name must follow), `a := :b` does.
        out.push({ key: m[0], start: i, end });
        i = end - 1;
      } else if (c === '?' && marks.question) {
        if (prev === '?' || sql[i + 1] === '?' || /[|&-]/.test(sql[i + 1] ?? '')) continue;
        const digits = /\d+/y;
        digits.lastIndex = i + 1;
        const d = digits.exec(sql);
        if (d) {
          out.push({ key: `?${d[0]}`, start: i, end: i + 1 + d[0].length });
          i += d[0].length;
        } else {
          positional++;
          out.push({ key: `?#${positional}`, start: i, end: i + 1 });
        }
      } else if (c === '@' && marks.at) {
        if (sql[i + 1] === '@' || prev === '@' || /[\p{L}\p{N}_]/u.test(prev)) continue;
        if (!NAME_START.test(sql[i + 1] ?? '')) continue;
        NAME.lastIndex = i + 1;
        const m = NAME.exec(sql);
        if (!m) continue;
        out.push({ key: `@${m[0]}`, start: i, end: i + 1 + m[0].length });
        i += m[0].length;
      }
    }
  }
  return out;
}

/** The parameters to ask for, once each, in the order they first appear. */
export function paramItems(spots: ParamSpot[]): ParamItem[] {
  const seen = new Set<string>();
  const out: ParamItem[] = [];
  for (const s of spots) {
    if (seen.has(s.key)) continue;
    seen.add(s.key);
    out.push({ key: s.key, label: s.key.startsWith('?#') ? `? (${s.key.slice(2)})` : s.key.startsWith('?') || s.key.startsWith('@') ? s.key : `:${s.key}` });
  }
  return out;
}

const NUMBER = /^[-+]?(\d+(\.\d*)?|\.\d+)([eE][-+]?\d+)?$/;
const DATE = /^(\d{4}-\d{2}-\d{2})(?:[ T](\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?))?$/;

/** Why a value can't be used, or null. */
export function paramProblem(v: ParamValue): 'number' | 'date' | null {
  if (v.type === 'number' && !NUMBER.test(v.value.trim())) return 'number';
  if (v.type === 'date' && !DATE.test(v.value.trim())) return 'date';
  return null;
}

/** Whether the target has a NULL literal (Redis and Flux don't). */
export function hasNull(t: ParamTarget): boolean {
  return t.language === 'sql' || t.language === 'cql' || t.language === 'json';
}

const isMongo = (t: ParamTarget) => ['mongodb', 'ferretdb', 'documentdb'].includes(t.driverId);

/** The value as a literal of the target's language. */
export function paramLiteral(v: ParamValue, t: ParamTarget): string {
  const raw = v.type === 'text' ? v.value : v.value.trim();
  const d = t.dialect;
  if (t.language !== 'sql' && t.language !== 'cql') {
    // mongosh, JSON consoles, Redis, Flux.
    if (v.type === 'null') return t.language === 'json' ? 'null' : '""';
    if (v.type === 'number') return raw;
    if (v.type === 'date') {
      const m = DATE.exec(raw)!;
      const iso = `${m[1]}T${m[2] ?? '00:00:00'}`;
      if (t.language === 'flux') return `${iso}Z`;
      if (t.language === 'json' && isMongo(t)) return `ISODate(${JSON.stringify(`${iso}Z`)})`;
      return JSON.stringify(raw);
    }
    return JSON.stringify(raw);
  }
  if (v.type === 'null') return 'NULL';
  if (v.type === 'number') return raw;
  if (v.type === 'date') {
    const m = DATE.exec(raw)!;
    const time = m[2] ?? null;
    const text = time ? `${m[1]} ${time}` : m[1];
    if (t.language === 'cql') return `'${text}'`;
    if (d === 'mssql') return time ? `CAST('${m[1]}T${time}' AS datetime2)` : `CAST('${m[1]}' AS date)`;
    if (d === 'sybase') return `'${m[1].replace(/-/g, '')}${time ? ` ${time}` : ''}'`;
    if (d === 'access') return `#${text}#`;
    if (['sqlite', 'clickhouse', 'informix', 'altibase', 'cosmos', 'n1ql', 'partiql', 'iotdb', 'tdengine', 'influxql', 'influxdb3', 'ksql', 'orientdb', 'phoenix', 'etcd'].includes(d)) return `'${text}'`;
    return time ? `TIMESTAMP '${text}'` : `DATE '${text}'`;
  }
  // Text.
  if (['bigquery', 'spanner', 'cosmos', 'n1ql'].includes(d)) return `'${raw.replace(/\\/g, '\\\\').replace(/'/g, "\\'")}'`;
  if (['mysql', 'mariadb', 'clickhouse', 'hive', 'sparksql', 'databricks'].includes(d)) return `'${raw.replace(/\\/g, '\\\\').replace(/'/g, "''")}'`;
  const quoted = `'${raw.replace(/'/g, "''")}'`;
  return d === 'mssql' || d === 'sybase' ? `N${quoted}` : quoted;
}

/** `sql` with every marker replaced by its value's literal. */
export function fillParams(sql: string, spots: ParamSpot[], values: Record<string, ParamValue>, t: ParamTarget): string {
  let out = '';
  let at = 0;
  for (const s of spots) {
    const v = values[s.key];
    if (!v) continue;
    out += sql.slice(at, s.start) + paramLiteral(v, t);
    at = s.end;
  }
  return out + sql.slice(at);
}

/** A first guess for a parameter never answered: number for `id`-like names. */
export function guessType(key: string): ParamType {
  return /(^|_)(id|num|count|qty|cantidad|nro|numero|limit|limite|top)$/i.test(key) || /^\?/.test(key) ? 'number'
    : /(fecha|date|desde|hasta|since|until)$/i.test(key) ? 'date' : 'text';
}
