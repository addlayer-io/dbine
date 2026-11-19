import type { Cell, VectorElement, VectorMeta } from '../api/types';
import type { VectorIndex } from '../api/schema-types';

// Vectors (embeddings) on the UI side: which result columns hold them, how a
// cell shows (a short preview and its dimensions, never the whole text cut
// at 500 characters), what an edit must be, and the values edits and copies
// send to the driver's scripts. A vector cell is text with a JSON array of
// numbers (`[0.1,-2,3.5]`), whole; sparse vectors keep the engine's text
// (dbine_driver::vector). No runtime imports, so `node --test` loads it
// directly (vector.test.mjs).

/** The standard distance names (`dbine_driver::vector::metric`): the UI
 *  shows these translated (`vectors:metric.<name>`), any other as is. */
export const STANDARD_METRICS = ['cosine', 'l2', 'ip', 'l1', 'hamming', 'jaccard'] as const;
/** The standard build-parameter names (`dbine_driver::vector::param`),
 *  with a translated label (`vectors:param.<name>`). */
export const STANDARD_PARAMS = ['m', 'ef_construction', 'lists', 'quantization'] as const;

export const isStandardMetric = (m: string) => (STANDARD_METRICS as readonly string[]).includes(m);
export const isStandardParam = (p: string) => (STANDARD_PARAMS as readonly string[]).includes(p);

/** The element type as engines and docs write it. */
export const ELEMENT_NAMES: Record<VectorElement, string> = {
  f32: 'float32', f64: 'float64', f16: 'float16', bf16: 'bfloat16', i8: 'int8', u8: 'uint8', bit: 'bit', other: '',
};

const ELEMENT_TOKENS: [RegExp, VectorElement][] = [
  [/^(float32|f32|float4|real|float)$/i, 'f32'],
  [/^(float64|f64|float8|double)$/i, 'f64'],
  [/^(float16|f16|half)$/i, 'f16'],
  [/^(bfloat16|bf16)$/i, 'bf16'],
  [/^(int8|i8|tinyint)$/i, 'i8'],
  [/^(uint8|u8)$/i, 'u8'],
  [/^(binary|bit|f1bit)$/i, 'bit'],
  [/^(int|integer|i16|i32|i64|int16|int32|int64)$/i, 'other'],
];

/** The vector type names DBine recognizes in a column's type, with what
 *  the name itself says about the elements. */
const VECTOR_HEADS: Record<string, { element: VectorElement; sparse?: boolean }> = {
  vector: { element: 'f32' },
  halfvec: { element: 'f16' },
  sparsevec: { element: 'f32', sparse: true },
  real_vector: { element: 'f32' },
  half_vector: { element: 'f16' },
  f32_blob: { element: 'f32' },
  f64_blob: { element: 'f64' },
  f16_blob: { element: 'f16' },
  fb16_blob: { element: 'bf16' },
  f8_blob: { element: 'other' },
  f1bit_blob: { element: 'bit' },
  dense_vector: { element: 'f32' },
  knn_vector: { element: 'f32' },
  sparse_vector: { element: 'f32', sparse: true },
};

/**
 * What a column's type name says about vectors in it, for results and
 * schemas without the driver's mark (`ResultColumn.vector`): pgvector's
 * `vector(3)`, `halfvec(3)`, `sparsevec(5)`; Oracle's `VECTOR(3, FLOAT32)`;
 * SQL Server's `vector(3, float16)`; Snowflake's `VECTOR(FLOAT, 3)`;
 * Cassandra's `vector<float, 3>`; libSQL's `F32_BLOB(3)`; HANA's
 * `REAL_VECTOR(3)`; Elasticsearch's `dense_vector`; Spanner's
 * `ARRAY<FLOAT32>(vector_length=>3)`. Null for any other type.
 */
export function vectorOfType(typeName: string | null | undefined): VectorMeta | null {
  const ty = (typeName ?? '').trim();
  const head = /^([a-z_][a-z0-9_]*)/i.exec(ty)?.[1].toLowerCase();
  if (!head) return null;
  const rest = ty.slice(head.length).trim();
  if (head === 'array') {
    const m = /^<\s*(float32|float64)\s*>\s*\(\s*vector_length\s*=>\s*(\d+)\s*\)$/i.exec(rest);
    return m ? { dims: Number(m[2]), element: /64/.test(m[1]) ? 'f64' : 'f32' } : null;
  }
  const known = VECTOR_HEADS[head];
  // Only the name with its arguments: `vector_id`, `vectors` aren't vector types.
  if (!known || (rest && !/^[(<]/.test(rest))) return null;
  const meta: VectorMeta = { element: known.element };
  if (known.sparse) meta.sparse = true;
  // The arguments in `(…)` and `<…>`, in order: dims and element type.
  const args = [...rest.matchAll(/[(<]([^()<>]*)[)>]/g)].flatMap((m) => m[1].split(',')).map((a) => a.trim()).filter(Boolean);
  for (const a of args) {
    if (/^\d+$/.test(a)) { meta.dims ??= Number(a); continue; }
    if (/^sparse$/i.test(a)) { meta.sparse = true; continue; }
    const el = ELEMENT_TOKENS.find(([re]) => re.test(a))?.[1];
    if (el) meta.element = el;
  }
  return meta;
}

/** The column's vectors: the driver's mark, else what its type name says. */
export function vectorMeta(col: { type_name?: string | null; vector?: VectorMeta | null } | null | undefined): VectorMeta | null {
  if (!col) return null;
  return col.vector ?? vectorOfType(col.type_name);
}

/** A dense vector column (its cells are JSON arrays of numbers). */
export const isDense = (meta: VectorMeta | null | undefined): meta is VectorMeta => !!meta && !meta.sparse;

/** The numbers of a vector written as a JSON array (`[1, 2.5, -3e-2]`),
 *  or null when the value isn't one. Every number must be finite. An array
 *  of numbers is taken as is. */
export function parseVector(v: unknown): number[] | null {
  let arr: unknown = v;
  if (typeof v === 'string') {
    const t = v.trim();
    if (!t.startsWith('[') || !t.endsWith(']')) return null;
    try { arr = JSON.parse(t); } catch { return null; }
  }
  if (!Array.isArray(arr)) return null;
  for (const x of arr) if (typeof x !== 'number' || !Number.isFinite(x)) return null;
  return arr as number[];
}

/** A number short, for the preview: integers whole, others with three
 *  significant digits (more when the integer part needs them), very small
 *  or big ones with an exponent. */
export function shortNumber(n: number): string {
  if (!Number.isFinite(n)) return 'null';
  if (n === 0) return '0';
  const a = Math.abs(n);
  if (Number.isInteger(n) && a < 1e6) return String(n);
  if (a >= 1e-3 && a < 1e6) return String(Number(n.toPrecision(Math.max(3, Math.floor(Math.log10(a)) + 1))));
  return n.toExponential(2).replace(/\.?0+e/, 'e');
}

export interface VectorPreview {
  /** What the cell shows: `[0.12, -0.03, 0.88, …]`, or a sparse vector's text. */
  text: string;
  /** Its dimensions (null: unknown, a sparse vector without them). */
  dims: number | null;
  /** The preview leaves numbers out. */
  cut: boolean;
  sparse: boolean;
}

/** Where a sparse vector says its dimensions: pgvector's `{1:0.5,4:2}/5`,
 *  Oracle's `[5,[1,4],[0.5,2]]`. */
function sparseDims(t: string): number | null {
  const m = /\/\s*(\d+)\s*$/.exec(t) ?? /^\[\s*(\d+)\s*,\s*\[/.exec(t);
  return m ? Number(m[1]) : null;
}

/**
 * A vector cell as the grid shows it: its first numbers, short, up to about
 * `maxChars` characters, and its dimensions. It reads only the numbers it
 * shows (and counts the commas), so a 1,536-number cell costs little on
 * every scroll. Null when the value isn't a vector (the cell shows it as
 * text).
 */
export function vectorPreview(v: Cell | undefined, meta: VectorMeta | null, maxChars = 40): VectorPreview | null {
  if (typeof v !== 'string' || !meta) return null;
  const t = v.trim();
  if (meta.sparse) return { text: t.length > 500 ? `${t.slice(0, 500)}…` : t, dims: meta.dims ?? sparseDims(t), cut: false, sparse: true };
  if (t.length < 2 || t[0] !== '[' || t[t.length - 1] !== ']') return null;
  const inner = t.slice(1, -1);
  if (/[[\]{}"]/.test(inner)) return null;
  if (!inner.trim()) return { text: '[]', dims: 0, cut: false, sparse: false };
  let dims = 1;
  for (let i = inner.indexOf(','); i >= 0; i = inner.indexOf(',', i + 1)) dims++;
  const shown: string[] = [];
  let len = 2;
  let start = 0;
  for (;;) {
    const end = inner.indexOf(',', start);
    const raw = (end < 0 ? inner.slice(start) : inner.slice(start, end)).trim();
    const n = raw === 'null' ? NaN : Number(raw);
    if (!raw || (raw !== 'null' && !Number.isFinite(n))) return null;
    const s = shortNumber(n);
    // At least one number; then while they fit (the ", …" included).
    if (shown.length && len + s.length + 2 + (end < 0 ? 0 : 3) > maxChars) break;
    shown.push(s);
    len += s.length + 2;
    if (end < 0) break;
    start = end + 1;
  }
  const cut = shown.length < dims;
  return { text: `[${shown.join(', ')}${cut ? ', …' : ''}]`, dims, cut, sparse: false };
}

export interface VectorStats {
  dims: number;
  /** Euclidean (L2) norm. */
  norm: number;
  min: number;
  max: number;
}

/** The small facts the cell viewer shows. Null for an empty vector. */
export function vectorStats(values: number[]): VectorStats | null {
  if (!values.length) return null;
  let sum = 0;
  let min = Infinity;
  let max = -Infinity;
  for (const x of values) {
    sum += x * x;
    if (x < min) min = x;
    if (x > max) max = x;
  }
  return { dims: values.length, norm: Math.sqrt(sum), min, max };
}

/** The values as rows of `perLine`, each row led by the position of its
 *  first number: the cell viewer's readable, scrollable layout. */
export function vectorLines(values: number[], perLine = 8): string {
  const cells = values.map((x) => String(x));
  const w = Math.max(1, ...cells.map((c) => c.length));
  const iw = String(Math.max(0, values.length - 1)).length;
  const lines: string[] = [];
  for (let i = 0; i < cells.length; i += perLine) {
    lines.push(`${String(i).padStart(iw)}  ${cells.slice(i, i + perLine).map((c) => c.padStart(w)).join('  ')}`);
  }
  return lines.join('\n');
}

/** Why an edited vector can't be written. */
export type VectorProblem =
  | { kind: 'format' }
  | { kind: 'dims'; expected: number; got: number }
  | { kind: 'range'; element: 'i8' | 'u8' | 'bit'; position: number; value: number };

const RANGES: Partial<Record<VectorElement, [number, number]>> = { i8: [-128, 127], u8: [0, 255], bit: [0, 1] };

/**
 * What the user typed in a dense vector cell: a JSON array of finite numbers
 * with the column's dimensions (when it declares them) and, for integer and
 * bit vectors, numbers the element type holds. The text comes back in the
 * canonical form (`[0.1,-2,3.5]`).
 */
export function checkVector(text: string, meta: VectorMeta | null): { values: number[]; text: string } | { problem: VectorProblem } {
  const values = parseVector(text);
  if (!values) return { problem: { kind: 'format' } };
  if (meta?.dims != null && values.length !== meta.dims) return { problem: { kind: 'dims', expected: meta.dims, got: values.length } };
  const range = meta ? RANGES[meta.element] : undefined;
  if (range) {
    const i = values.findIndex((x) => !Number.isInteger(x) || x < range[0] || x > range[1]);
    if (i >= 0) return { problem: { kind: 'range', element: meta!.element as 'i8' | 'u8' | 'bit', position: i, value: values[i] } };
  }
  return { values, text: JSON.stringify(values) };
}

/** Two cells hold the same vector (whatever the spacing or number spelling). */
export function sameVector(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  const x = parseVector(a);
  const y = parseVector(b);
  return !!x && !!y && x.length === y.length && x.every((n, i) => n === y[i]);
}

/** A value of a dense vector column as the driver's scripts take it: a JSON
 *  array of numbers (`update_script`, `insert_script`). Anything else (NULL,
 *  a sparse vector's text, a text that isn't a vector) goes as it is. */
export function scriptValue(meta: VectorMeta | null | undefined, v: Cell): Cell | number[] {
  if (!isDense(meta) || typeof v !== 'string') return v;
  return parseVector(v) ?? v;
}

/** Rows of a result with their dense vector cells as JSON arrays of numbers
 *  (`dbine_driver::vector::rows_for_scripts`). */
export function rowsForScripts(columns: { type_name?: string | null; vector?: VectorMeta | null }[], rows: Cell[][]): (Cell | number[])[][] {
  const metas = columns.map(vectorMeta);
  if (!metas.some(isDense)) return rows;
  return rows.map((r) => r.map((v, i) => scriptValue(metas[i], v)));
}

/** A vector index in one line: `hnsw · Coseno · m = 16, ef_construction = 64`
 *  (`metricName` gives the metric's label: translated when it's standard). */
export function vectorIndexText(v: VectorIndex | null | undefined, metricName: (m: string) => string = (m) => m): string {
  if (!v) return '';
  const params = Object.entries(v.params ?? {}).map(([k, x]) => `${k} = ${x}`).join(', ');
  return [v.algorithm, v.metric ? metricName(v.metric) : '', params].filter(Boolean).join(' · ');
}
