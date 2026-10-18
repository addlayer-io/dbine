import type { NotebookCell, NotebookParameter, NotebookParamType, NotebookParamValue } from '../api/types';

// Notebook parameters (docs/notebooks.md#parameters): values asked for when
// running, used by the cells as `{{name}}`. The backend finds them in the
// text, refuses them where they can't go and binds or quotes each value
// (src-tauri/src/notebook/params.rs); here: the definitions' checks, the
// values a run uses, which cells use which (for the stale marks) and how
// a result says what produced it.

export const PARAM_TYPES: NotebookParamType[] = ['text', 'number', 'date', 'datetime', 'boolean', 'list'];

/** A parameter name: a letter or `_`, then letters, digits or `_` (ASCII), at most 63. */
export function validParamName(name: string): boolean {
  return /^[A-Za-z_][A-Za-z0-9_]{0,62}$/.test(name);
}

/** Why `name` can't be the name of parameter `index`, or null. */
export function paramNameProblem(defs: NotebookParameter[], index: number, name: string): 'invalid' | 'taken' | null {
  if (!validParamName(name)) return 'invalid';
  if (defs.some((d, i) => i !== index && d.name.toLowerCase() === name.toLowerCase())) return 'taken';
  return null;
}

/** A new parameter's name: `p1`, `p2`… not taken. */
export function newParamName(defs: NotebookParameter[]): string {
  for (let i = 1; ; i++) {
    const n = `p${i}`;
    if (!defs.some((d) => d.name.toLowerCase() === n)) return n;
  }
}

const MARKER = /\{\{[ \t]*([A-Za-z_][A-Za-z0-9_]*)[ \t]*\}\}/g;

/** The defined parameters `text` mentions as `{{name}}` (each once, in
 *  order, as defined), wherever they are: the backend decides which ones
 *  run; these are the ones whose change makes the cell's result stale. */
export function mentionedParams(text: string, defs: NotebookParameter[]): string[] {
  const out: string[] = [];
  for (const m of text.matchAll(MARKER)) {
    if (m.index > 0 && text[m.index - 1] === '{') continue;
    const d = defs.find((p) => p.name.toLowerCase() === m[1].toLowerCase());
    if (d && !out.includes(d.name)) out.push(d.name);
  }
  return out;
}

const empty = (v: NotebookParamValue | undefined): boolean => v === null || v === undefined || (typeof v === 'string' && !v.trim());

const NUMBER = /^[-+]?(\d+(\.\d*)?|\.\d+)([eE][-+]?\d+)?$/;
const DATE = /^\d{4}-(0[1-9]|1[0-2])-(0[1-9]|[12]\d|3[01])$/;
const DATETIME = /^\d{4}-(0[1-9]|1[0-2])-(0[1-9]|[12]\d|3[01])[T ]([01]\d|2[0-3]):[0-5]\d(:[0-5]\d(\.\d{1,9})?)?$/;

/** Why `v` isn't a value of `p`'s type, or null (an empty one is fine: the default or NULL). */
export function paramValueProblem(p: NotebookParameter, v: NotebookParamValue | undefined): 'number' | 'date' | 'datetime' | 'boolean' | 'list' | null {
  if (empty(v)) return null;
  const s = String(v).trim();
  switch (p.type) {
    case 'number': return NUMBER.test(s) ? null : 'number';
    case 'date': return DATE.test(s) ? null : 'date';
    case 'datetime': return DATETIME.test(s) ? null : 'datetime';
    case 'boolean': return typeof v === 'boolean' || ['true', 'false', '1', '0', 'sí', 'si', 'no', 'yes'].includes(s.toLowerCase()) ? null : 'boolean';
    case 'list': return (p.options ?? []).includes(String(v)) ? null : 'list';
    default: return null;
  }
}

/** The value a run uses for `p`: the one given (this session's), else its default, else null. */
export function effectiveValue(p: NotebookParameter, given: Record<string, NotebookParamValue>): NotebookParamValue {
  const g = given[p.name];
  if (!empty(g)) return g;
  return empty(p.default) ? null : (p.default ?? null);
}

/** The values a run uses, by name (given ones, else defaults). */
export function effectiveValues(defs: NotebookParameter[], given: Record<string, NotebookParamValue>): Record<string, NotebookParamValue> {
  return Object.fromEntries(defs.map((p) => [p.name, effectiveValue(p, given)]));
}

/** The required parameters among `names` (all, when null) without a value
 *  (given or default): what the run has to ask for. */
export function missingRequired(defs: NotebookParameter[], given: Record<string, NotebookParamValue>, names: string[] | null = null): string[] {
  return defs.filter((p) => p.required && (names === null || names.includes(p.name)) && empty(effectiveValue(p, given))).map((p) => p.name);
}

/** The values a cell's result records: those of the parameters it used. */
export function usedValues(defs: NotebookParameter[], given: Record<string, NotebookParamValue>, used: string[]): Record<string, NotebookParamValue> | null {
  if (!used.length) return null;
  const all = effectiveValues(defs, given);
  return Object.fromEntries(used.map((n) => [n, normalize(defs.find((d) => d.name === n), all[n])]));
}

/** As the backend records it: numbers as numbers, booleans as booleans,
 *  text trimmed for dates. */
function normalize(p: NotebookParameter | undefined, v: NotebookParamValue): NotebookParamValue {
  if (v === null || !p) return v;
  if (p.type === 'number' && typeof v === 'string' && NUMBER.test(v.trim())) return Number(v.trim().replace(/^\+/, ''));
  if (p.type === 'boolean' && typeof v === 'string') return ['true', '1', 'sí', 'si', 'yes'].includes(v.trim().toLowerCase());
  if ((p.type === 'date' || p.type === 'datetime') && typeof v === 'string') {
    const s = v.trim().replace(' ', 'T');
    return p.type === 'datetime' && /T\d\d:\d\d$/.test(s) ? `${s}:00` : s;
  }
  return v;
}

const same = (a: NotebookParamValue | undefined, b: NotebookParamValue | undefined) => (a ?? null) === (b ?? null) || String(a ?? '') === String(b ?? '');

/** The result was produced with other values than the ones in use now (for
 *  the parameters its cell uses): its header says so. A result without
 *  recorded values, of a cell that now uses parameters, is stale too only
 *  when it ran in this session (a file from before parameters isn't). */
export function resultIsStale(cell: NotebookCell, defs: NotebookParameter[], given: Record<string, NotebookParamValue>): boolean {
  const r = cell.result;
  if (!r || cell.kind !== 'sql') return false;
  const used = mentionedParams(cell.source, defs);
  const now = usedValues(defs, given, used) ?? {};
  const then = r.params ?? null;
  if (!then) return false;
  const names = new Set([...used, ...Object.keys(then).filter((k) => defs.some((d) => d.name === k))]);
  for (const n of names) if (!same(then[n], now[n])) return true;
  return false;
}

/** "mes = 2026-10 · activo = sí": a result's values as its header shows them. */
export function formatParamValues(values: Record<string, NotebookParamValue> | null | undefined, yes = 'true', no = 'false'): string {
  if (!values) return '';
  return Object.entries(values).map(([k, v]) => `${k} = ${v === null ? 'NULL' : v === true ? yes : v === false ? no : String(v)}`).join(' · ');
}

/** A key for "this run's values": a source's rows are reused by a
 *  DataFrame SQL cell only when they came from the same values. */
export function valuesKey(values: Record<string, NotebookParamValue> | null | undefined): string {
  if (!values) return '';
  return JSON.stringify(Object.keys(values).sort().map((k) => [k, values[k]]));
}
