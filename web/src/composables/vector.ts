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
  [/^(bfloat16|bf16|floatb16)$/i, 'bf16'],
  // Elasticsearch / OpenSearch `byte`, Neo4j `INTEGER8`.
  [/^(int8|i8|tinyint|byte|integer8)$/i, 'i8'],
  [/^(uint8|u8)$/i, 'u8'],
  // Couchbase's `int1` is a bit.
  [/^(binary|bit|f1bit|int1|float1bit)$/i, 'bit'],
  [/^(int|integer|smallint|bigint|i16|i32|i64|int16|int32|int64|integer16|integer32|integer64)$/i, 'other'],
];
/** Element types that aren't numbers: Cassandra's `vector<text, 2>` is a
 *  fixed-length list, not an embedding. */
const NOT_NUMBERS = /^(text|varchar|ascii|blob|uuid|timeuuid|inet|boolean|bool|date|time|timestamp|duration|string)$/i;
const elementOf = (token: string): VectorElement => ELEMENT_TOKENS.find(([re]) => re.test(token))?.[1] ?? 'f32';

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
  // CrateDB, Manticore.
  float_vector: { element: 'f32' },
};
/** libSQL's other names of its vector types (`FLOAT32(3)`…): vectors only
 *  with their dimensions, as plain `FLOAT8` is a number elsewhere. */
const DIMS_HEADS: Record<string, VectorElement> = {
  float32: 'f32', float64: 'f64', float16: 'f16', floatb16: 'bf16', float8: 'other', float1bit: 'bit',
};

/**
 * What a column's type name says about vectors in it, for results and
 * schemas without the driver's mark (`ResultColumn.vector`): pgvector's
 * `vector(3)`, `halfvec(3)`, `sparsevec(5)`; Oracle's `VECTOR(3, FLOAT32)`;
 * SQL Server's `vector(3, float16)`; Snowflake's `VECTOR(FLOAT, 3)`;
 * Cassandra's `vector<float, 3>` (and `frozen<…>`); libSQL's `F32_BLOB(3)`
 * and `FLOAT32(3)`; HANA's `REAL_VECTOR(3)`; Elasticsearch's
 * `dense_vector(3, byte)`; CrateDB's `float_vector(3)`; Spanner's
 * `ARRAY<FLOAT32>(vector_length=>3)`; a document field's `array|vector(3)`.
 * Null for any other type, and for a vector of something that isn't a
 * number (`vector<text, 2>`). Arrays of floats that hold vectors on some
 * engines (`FLOAT[3]`, `Array(Float32)`) are `vectorArrayOfType`.
 */
export function vectorOfType(typeName: string | null | undefined): VectorMeta | null {
  const ty = (typeName ?? '').trim();
  // A field of documents whose values have several types: `array|vector(3)|null`.
  if (ty.includes('|')) {
    for (const part of ty.split('|')) {
      const m = vectorOfType(part);
      if (m) return m;
    }
    return null;
  }
  const head = /^([a-z_][a-z0-9_]*)/i.exec(ty)?.[1].toLowerCase();
  if (!head) return null;
  const rest = ty.slice(head.length).trim();
  if (head === 'frozen') {
    const inner = /^<(.*)>$/.exec(rest)?.[1];
    return inner ? vectorOfType(inner) : null;
  }
  if (head === 'array') {
    const m = /^<\s*(float32|float64)\s*>\s*\(\s*vector_length\s*=>\s*(\d+)\s*\)$/i.exec(rest);
    return m ? { dims: Number(m[2]), element: /64/.test(m[1]) ? 'f64' : 'f32' } : null;
  }
  if (head in DIMS_HEADS) {
    const m = /^\(\s*(\d+)\s*\)$/.exec(rest);
    return m ? { dims: Number(m[1]), element: DIMS_HEADS[head] } : null;
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
    if (NOT_NUMBERS.test(a)) return null;
    const el = ELEMENT_TOKENS.find(([re]) => re.test(a))?.[1];
    if (el) meta.element = el;
  }
  // Cassandra's `vector<int, 2>` is a fixed-length list too: its vectors
  // (the ones it indexes) hold floats, doubles or tinyints.
  if (/^<[^<>]*,[^<>]*>$/.test(rest) && meta.element === 'other') return null;
  return meta;
}

/**
 * Arrays of floats that are how some engines keep vectors, though the name
 * alone doesn't say it holds one: DuckDB's `FLOAT[3]` / `DOUBLE[768]`,
 * ClickHouse's `Array(Float32)`, BigQuery's `ARRAY<FLOAT64>`, Doris' and
 * StarRocks' `ARRAY<FLOAT>` (Doris adds a `dim=3` comment), Spanner's
 * `ARRAY<FLOAT32>`, Neo4j's `LIST<FLOAT>`. They show as vectors when a
 * cell reads as one (`shownVectorMeta`) and can carry a vector index (the
 * designer), but an edit of one isn't checked as a vector without the
 * driver's mark. Null for any other type.
 */
export function vectorArrayOfType(typeName: string | null | undefined): VectorMeta | null {
  const ty = (typeName ?? '').trim();
  let m = /^(float|float32|float64|double|real|float4|float8)\s*\[\s*(\d+)\s*\]$/i.exec(ty);
  if (m) return { dims: Number(m[2]), element: elementOf(m[1]) };
  m = /^array\s*\(\s*(float32|float64|bfloat16|float16)\s*\)$/i.exec(ty);
  if (m) return { element: elementOf(m[1]) };
  m = /^(?:array|list)\s*<\s*(float|float32|float64|double|real|float4|float8)\s*>\s*(?:\/\*\s*dim\s*=\s*(\d+)\s*\*\/)?$/i.exec(ty);
  if (m) return m[2] ? { dims: Number(m[2]), element: elementOf(m[1]) } : { element: elementOf(m[1]) };
  return null;
}

/** A bit string that pgvector indexes as a binary vector (`bit(1536)`, with
 *  Hamming or Jaccard): a vector only to the designer's vector indexes. */
export function bitVectorOfType(typeName: string | null | undefined): VectorMeta | null {
  const m = /^bit\s*\(\s*(\d+)\s*\)$/i.exec((typeName ?? '').trim());
  return m ? { dims: Number(m[1]), element: 'bit' } : null;
}

/** A column type a vector index can be built on, where the engine has
 *  vector indexes (the table designer): a vector type, an array of floats
 *  or a bit string. */
export function indexableVectorType(typeName: string | null | undefined): VectorMeta | null {
  return vectorOfType(typeName) ?? vectorArrayOfType(typeName) ?? bitVectorOfType(typeName);
}

/** The column's vectors: the driver's mark, else what its type name says. */
export function vectorMeta(col: { type_name?: string | null; vector?: VectorMeta | null } | null | undefined): VectorMeta | null {
  if (!col) return null;
  return col.vector ?? vectorOfType(col.type_name);
}

/** The column's vectors as the results show them: the driver's mark, a
 *  vector type, or an array of floats that holds vectors on its engine
 *  (`vectorArrayOfType`). A cell that doesn't read as a vector still shows
 *  as text. Edits go by `vectorMeta`. */
export function shownVectorMeta(col: { type_name?: string | null; vector?: VectorMeta | null } | null | undefined): VectorMeta | null {
  if (!col) return null;
  return col.vector ?? vectorOfType(col.type_name) ?? vectorArrayOfType(col.type_name);
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
  return numberText(n, 3);
}
/** A number with `digits` significant digits (more when the integer part
 *  needs them), integers whole, very small or big ones with an exponent. */
function numberText(n: number, digits: number): string {
  if (!Number.isFinite(n)) return 'null';
  if (n === 0) return '0';
  const a = Math.abs(n);
  if (Number.isInteger(n) && a < 1e6) return String(n);
  if (a >= 1e-3 && a < 1e6) return String(Number(n.toPrecision(Math.min(17, Math.max(digits, Math.floor(Math.log10(a)) + 1)))));
  return n.toExponential(digits - 1).replace(/\.?0+e/, 'e');
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

/** Two vectors that differ, as data compare shows them one over the other:
 *  each from the first number that differs, with as many digits as tell
 *  them apart (the usual preview could show the same text on both sides). */
export interface VectorDiff {
  left: VectorPreview;
  right: VectorPreview;
  /** The first position (0-based) where they differ. */
  at: number;
  /** Where the previews start: `at`, or one before when one vector ends
   *  first (both then show where they part); after 0, numbers are left out. */
  start: number;
}

/** The numbers of `values` from `start`, short, within about `maxChars`
 *  characters (at least one), as a preview. */
function windowOf(values: number[], start: number, digits: number, maxChars: number): VectorPreview {
  const shown: string[] = [];
  let len = start > 0 ? 5 : 2;
  for (let i = start; i < values.length; i++) {
    const s = numberText(values[i], digits);
    if (shown.length && len + s.length + 2 + (i < values.length - 1 ? 3 : 0) > maxChars) break;
    shown.push(s);
    len += s.length + 2;
  }
  const cut = start + shown.length < values.length;
  return { text: `[${start > 0 ? '…, ' : ''}${shown.join(', ')}${cut ? ', …' : ''}]`, dims: values.length, cut: cut || start > 0, sparse: false };
}

/** Null when they aren't both dense vectors, or hold the same numbers. */
export function vectorDiffPreview(a: unknown, b: unknown, maxChars = 40): VectorDiff | null {
  const x = parseVector(a);
  const y = parseVector(b);
  if (!x || !y) return null;
  const n = Math.min(x.length, y.length);
  let at = 0;
  while (at < n && x[at] === y[at]) at++;
  if (at === n && x.length === y.length) return null;
  // One ends first: from its last number, so both show where they part.
  const start = at === n ? Math.max(0, n - 1) : at;
  let digits = 3;
  if (at < n) while (digits < 17 && numberText(x[at], digits) === numberText(y[at], digits)) digits++;
  return { left: windowOf(x, start, digits, maxChars), right: windowOf(y, start, digits, maxChars), at, start };
}

/** The order of two vectors in a sort: number by number, then the shorter
 *  first (as pgvector orders them). */
export function compareVectors(a: number[], b: number[]): number {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) if (a[i] !== b[i]) return a[i] < b[i] ? -1 : 1;
  return a.length - b.length;
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
  | { kind: 'empty' }
  | { kind: 'dims'; expected: number; got: number }
  | { kind: 'range'; element: 'i8' | 'u8' | 'bit'; position: number; value: number }
  | { kind: 'overflow'; element: 'f16' | 'bf16' | 'f32'; max: number; position: number; value: number };

const RANGES: Partial<Record<VectorElement, [number, number]>> = { i8: [-128, 127], u8: [0, 255], bit: [0, 1] };
/** Floats narrower than a double: the largest finite one, and what rounds
 *  to a finite one (beyond it, the engine stores infinity or refuses it). */
const FLOAT_LIMITS: Partial<Record<VectorElement, { max: number; fits: (x: number) => boolean }>> = {
  f16: { max: 65504, fits: (x) => Math.abs(x) < 65520 },
  bf16: { max: 3.3895313892515355e38, fits: (x) => Math.abs(x) < 3.3961775292304941e38 },
  f32: { max: 3.4028234663852886e38, fits: (x) => Number.isFinite(Math.fround(x)) },
};

/**
 * What the user typed in a dense vector cell (text) or set in a field (an
 * array, from the JSON tree): a JSON array of finite numbers, not empty,
 * with the column's dimensions (when it declares them) and numbers its
 * element type holds (integers in range for int8 / uint8 / bit, no
 * overflow for float16 / bfloat16 / float32). The text comes back in the
 * canonical form (`[0.1,-2,3.5]`).
 */
export function checkVector(text: unknown, meta: VectorMeta | null): { values: number[]; text: string } | { problem: VectorProblem } {
  const values = parseVector(text);
  if (!values) return { problem: { kind: 'format' } };
  if (meta?.dims != null && values.length !== meta.dims) return { problem: { kind: 'dims', expected: meta.dims, got: values.length } };
  if (!values.length) return { problem: { kind: 'empty' } };
  const range = meta ? RANGES[meta.element] : undefined;
  if (range) {
    const i = values.findIndex((x) => !Number.isInteger(x) || x < range[0] || x > range[1]);
    if (i >= 0) return { problem: { kind: 'range', element: meta!.element as 'i8' | 'u8' | 'bit', position: i, value: values[i] } };
  }
  const limit = meta ? FLOAT_LIMITS[meta.element] : undefined;
  if (limit) {
    const i = values.findIndex((x) => !limit.fits(x));
    if (i >= 0) return { problem: { kind: 'overflow', element: meta!.element as 'f16' | 'bf16' | 'f32', max: limit.max, position: i, value: values[i] } };
  }
  return { values, text: JSON.stringify(values) };
}

/** A field of a dense vector column set to `value` as a whole (the JSON
 *  tree, a document typed as JSON): NULL, or a vector the column holds.
 *  Null when it can go; other columns are never checked here. */
export function vectorValueProblem(value: unknown, meta: VectorMeta | null | undefined): VectorProblem | null {
  if (value === null || value === undefined || !isDense(meta)) return null;
  const r = checkVector(value, meta);
  return 'problem' in r ? r.problem : null;
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

/** Two vector indexes build the same way: algorithm and metric (case
 *  aside) and the parameters that are set (an empty one is the engine's
 *  default). Both absent: the same (neither is a vector index). */
export function sameVectorIndex(a: VectorIndex | null | undefined, b: VectorIndex | null | undefined): boolean {
  if (!a || !b) return !a && !b;
  const norm = (v: VectorIndex) => JSON.stringify([
    (v.algorithm ?? '').trim().toLowerCase(),
    (v.metric ?? '').trim().toLowerCase(),
    Object.entries(v.params ?? {}).map(([k, x]) => [k.trim().toLowerCase(), String(x ?? '').trim()]).filter(([, x]) => x).sort(),
  ]);
  return norm(a) === norm(b);
}

/** A vector index's short tag, as the explorer shows it next to the index
 *  (short enough to stay in view next to the name): its kind's tag (`HNSW`,
 *  `IVFFLAT`…), the algorithm when the tag doesn't name it (a kind that
 *  only says `VECTOR` gives way to it: `DISKANN`) and the metric
 *  (`metricName` gives its label); the parameters are left for the whole
 *  line. */
export function vectorIndexTag(tag: string, v: VectorIndex | null | undefined, metricName: (m: string) => string = (m) => m): string {
  if (!v) return tag;
  const bare = (x: string) => x.toLowerCase().replace(/[^a-z0-9]/g, '');
  const alg = (v.algorithm ?? '').trim();
  const named = !alg || (!!tag && bare(tag).includes(bare(alg)));
  const kind = !named && bare(tag) === 'vector' ? '' : tag;
  return [kind, named ? '' : alg.toUpperCase(), v.metric ? metricName(v.metric) : ''].filter(Boolean).join(' · ');
}

/** A vector index in one line: `hnsw · Coseno · m = 16, ef_construction = 64`
 *  (`metricName` gives the metric's label: translated when it's standard). */
export function vectorIndexText(v: VectorIndex | null | undefined, metricName: (m: string) => string = (m) => m): string {
  if (!v) return '';
  const params = Object.entries(v.params ?? {}).map(([k, x]) => `${k} = ${x}`).join(', ');
  return [v.algorithm, v.metric ? metricName(v.metric) : '', params].filter(Boolean).join(' · ');
}
