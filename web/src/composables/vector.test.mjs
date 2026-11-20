// Tests for the vector helpers (vector.ts): which columns hold vectors (the
// driver's mark or the type name), the grid's preview, the edit check, the
// values scripts get and the vector index line. Run: `node --test
// src/composables/vector.test.mjs` from web/ (Node 23.6+ strips the
// TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  bitVectorOfType, checkVector, compareVectors, indexableVectorType, isDense, parseVector, rowsForScripts, sameVector, sameVectorIndex,
  scriptValue, shortNumber, shownVectorMeta, vectorArrayOfType, vectorDiffPreview, vectorIndexTag, vectorIndexText, vectorLines, vectorMeta, vectorOfType,
  vectorPreview, vectorStats, vectorValueProblem,
} from './vector.ts';

// -- type names ------------------------------------------------------------------------

test('type names of every family say dims, element and sparse', () => {
  const cases = [
    ['vector(1536)', { dims: 1536, element: 'f32' }], // pgvector, MySQL 9, MariaDB, TiDB, Cockroach
    ['vector', { element: 'f32' }],
    ['halfvec(3)', { dims: 3, element: 'f16' }],
    ['sparsevec(5)', { dims: 5, element: 'f32', sparse: true }],
    ['VECTOR(3, FLOAT32)', { dims: 3, element: 'f32' }], // Oracle 23ai
    ['VECTOR(3,FLOAT64)', { dims: 3, element: 'f64' }],
    ['VECTOR(8, INT8)', { dims: 8, element: 'i8' }],
    ['VECTOR(16, BINARY)', { dims: 16, element: 'bit' }],
    ['VECTOR(*, *)', { element: 'f32' }],
    ['VECTOR(100, FLOAT32, SPARSE)', { dims: 100, element: 'f32', sparse: true }],
    ['vector(3, float16)', { dims: 3, element: 'f16' }], // SQL Server 2025
    ['VECTOR(FLOAT, 256)', { dims: 256, element: 'f32' }], // Snowflake
    ['VECTOR(INT, 4)', { dims: 4, element: 'i32' }],
    ['vector<float, 3>', { dims: 3, element: 'f32' }], // Cassandra
    ['VECTOR<FLOAT32>(1024)', { dims: 1024, element: 'f32' }], // Neo4j / TiDB
    ['F32_BLOB(3)', { dims: 3, element: 'f32' }], // libSQL
    ['FB16_BLOB(3)', { dims: 3, element: 'bf16' }],
    ['F1BIT_BLOB(64)', { dims: 64, element: 'bit' }],
    ['REAL_VECTOR(3)', { dims: 3, element: 'f32' }], // HANA
    ['HALF_VECTOR(3)', { dims: 3, element: 'f16' }],
    ['dense_vector', { element: 'f32' }], // Elasticsearch
    ['knn_vector', { element: 'f32' }], // OpenSearch
    ['sparse_vector', { element: 'f32', sparse: true }],
    ['ARRAY<FLOAT32>(vector_length=>3)', { dims: 3, element: 'f32' }], // Spanner
    ['VECTOR(3, F32)', { dims: 3, element: 'f32' }], // SingleStore
    ['vector(8, I64)', { dims: 8, element: 'other' }],
    ['vector(1536, F16)', { dims: 1536, element: 'f16' }],
    ['float_vector(3)', { dims: 3, element: 'f32' }], // CrateDB, Manticore
    ['FLOAT_VECTOR(1536)', { dims: 1536, element: 'f32' }],
    ['FLOAT32(4)', { dims: 4, element: 'f32' }], // libSQL's other names
    ['FLOAT8(4)', { dims: 4, element: 'other' }],
    ['FLOAT1BIT(8)', { dims: 8, element: 'bit' }],
    ['frozen<vector<float, 1536>>', { dims: 1536, element: 'f32' }], // Cassandra
    ['vector<double, 2>', { dims: 2, element: 'f64' }],
    ['vector<tinyint, 4>', { dims: 4, element: 'i8' }],
    ['VECTOR<INTEGER8>(1024)', { dims: 1024, element: 'i8' }], // Neo4j
    ['vector<float>(3)', { dims: 3, element: 'f32' }], // TiDB's long form
    ['dense_vector(3, byte)', { dims: 3, element: 'i8' }], // Elasticsearch
    ['dense_vector(16, bit)', { dims: 16, element: 'bit' }],
    ['knn_vector(4, binary)', { dims: 4, element: 'bit' }], // OpenSearch
    ['vector(9, bit)', { dims: 9, element: 'bit' }], // Couchbase
    ['vector(int1)', { element: 'bit' }],
    ['array|vector(3)|null', { dims: 3, element: 'f32' }], // a document field of several types
    ['VECTOR(3,FLOAT32,DENSE)', { dims: 3, element: 'f32' }],
  ];
  for (const [ty, want] of cases) assert.deepEqual(vectorOfType(ty), want, ty);
});

test('other types are not vectors', () => {
  for (const ty of [
    '', 'int', 'varchar(50)', 'vectors', 'vector_id', 'vectorx(3)', 'Array(Float32)', 'FLOAT[3]', 'ARRAY<FLOAT64>', 'bytea', 'jsonb', 'bit(8)',
    'float8', 'FLOAT(24)', 'float32', 'array|null', null, undefined,
    // Cassandra: a fixed-length list of something that isn't an embedding.
    'vector<text, 2>', 'vector<int, 2>', 'frozen<list<int>>',
  ]) {
    assert.equal(vectorOfType(ty), null, String(ty));
  }
});

test('arrays of floats that hold vectors on their engines', () => {
  const cases = [
    ['FLOAT[3]', { dims: 3, element: 'f32' }], // DuckDB
    ['DOUBLE[768]', { dims: 768, element: 'f64' }],
    ['Array(Float32)', { element: 'f32' }], // ClickHouse
    ['Array(BFloat16)', { element: 'bf16' }],
    ['ARRAY<FLOAT64>', { element: 'f64' }], // BigQuery
    ['ARRAY<FLOAT>', { element: 'f32' }], // Doris, StarRocks, Databricks
    ['ARRAY<FLOAT> /* dim=3 */', { dims: 3, element: 'f32' }],
    ['ARRAY<FLOAT32>', { element: 'f32' }], // Spanner without vector_length
    ['ARRAY<DOUBLE>', { element: 'f64' }],
    ['LIST<FLOAT>', { element: 'f32' }], // Neo4j
  ];
  for (const [ty, want] of cases) {
    assert.deepEqual(vectorArrayOfType(ty), want, ty);
    assert.equal(vectorOfType(ty), null, `${ty} isn't a vector type by itself`);
  }
  for (const ty of ['FLOAT[]', 'FLOAT[3][2]', 'real[]', 'Array(Array(Float32))', 'Array(Int8)', 'ARRAY<INT64>', 'ARRAY<STRING>', 'int[3]', 'text', null]) {
    assert.equal(vectorArrayOfType(ty), null, String(ty));
  }
  // A bit string only to the designer's vector indexes (pgvector's binary vectors).
  assert.deepEqual(bitVectorOfType('bit(1536)'), { dims: 1536, element: 'bit' });
  assert.equal(bitVectorOfType('bit varying(8)'), null);
  assert.equal(shownVectorMeta({ type_name: 'bit(8)' }), null);
  for (const ty of ['vector(3)', 'float_vector(3)', 'FLOAT[3]', 'Array(Float32)', 'ARRAY<FLOAT64>', 'ARRAY<FLOAT> /* dim=3 */', 'bit(8)']) {
    assert.ok(indexableVectorType(ty), ty);
  }
  for (const ty of ['int', 'text', 'jsonb', 'FLOAT[]']) assert.equal(indexableVectorType(ty), null, ty);
});

test('the results show arrays of floats as vectors; edits go by the vector types', () => {
  assert.deepEqual(shownVectorMeta({ type_name: 'FLOAT[3]' }), { dims: 3, element: 'f32' });
  assert.equal(vectorMeta({ type_name: 'FLOAT[3]' }), null);
  assert.deepEqual(shownVectorMeta({ type_name: 'blob', vector: { dims: 4, element: 'f32' } }), { dims: 4, element: 'f32' });
  assert.equal(shownVectorMeta({ type_name: 'text' }), null);
  assert.equal(shownVectorMeta(null), null);
});

test('the driver mark wins over the type name', () => {
  assert.deepEqual(vectorMeta({ type_name: 'blob', vector: { dims: 4, element: 'f32' } }), { dims: 4, element: 'f32' });
  assert.deepEqual(vectorMeta({ type_name: 'vector(3)' }), { dims: 3, element: 'f32' });
  assert.equal(vectorMeta({ type_name: 'text' }), null);
  assert.equal(vectorMeta(undefined), null);
  assert.equal(isDense({ element: 'f32', sparse: true }), false);
  assert.equal(isDense({ element: 'f32' }), true);
  assert.equal(isDense(null), false);
});

// -- reading -------------------------------------------------------------------------------

test('parseVector takes JSON arrays of finite numbers only', () => {
  assert.deepEqual(parseVector('[0.1,-2,3.5]'), [0.1, -2, 3.5]);
  assert.deepEqual(parseVector(' [1, 2.5, -3e-2] '), [1, 2.5, -0.03]);
  assert.deepEqual(parseVector('[]'), []);
  assert.deepEqual(parseVector([1, 2]), [1, 2]);
  for (const bad of ['', '1,2', '[1,]', '[1,"a"]', '[1,[2]]', '[NaN]', '[Infinity]', '[1e999]', '[+1]', '[null]', '{1:0.5}/5', 42, null, ['1']]) {
    assert.equal(parseVector(bad), null, JSON.stringify(bad));
  }
});

test('shortNumber keeps previews short and readable', () => {
  assert.equal(shortNumber(0), '0');
  assert.equal(shortNumber(0.123456), '0.123');
  assert.equal(shortNumber(-0.0312345), '-0.0312');
  assert.equal(shortNumber(0.88), '0.88');
  assert.equal(shortNumber(12.3456), '12.3');
  assert.equal(shortNumber(1234.56), '1235');
  assert.equal(shortNumber(127), '127');
  assert.equal(shortNumber(1536), '1536');
  assert.equal(shortNumber(0.00001234), '1.23e-5');
  assert.equal(shortNumber(2e9 + 0.5), '2e+9');
  assert.equal(shortNumber(NaN), 'null');
});

// -- the grid's cell ---------------------------------------------------------------------------

test('a long vector previews its first numbers, with its dims', () => {
  const values = Array.from({ length: 1536 }, (_, i) => Math.sin(i + 1) / 3);
  const p = vectorPreview(JSON.stringify(values), { dims: 1536, element: 'f32' });
  assert.equal(p.dims, 1536);
  assert.equal(p.cut, true);
  assert.equal(p.sparse, false);
  assert.match(p.text, /^\[0\.28, 0\.303, 0\.047, .*, …\]$/);
  assert.ok(p.text.length <= 40, p.text);
});

test('a short vector shows whole, never the 500-character cut', () => {
  assert.deepEqual(vectorPreview('[0.12,-0.03,0.88]', { element: 'f32' }), { text: '[0.12, -0.03, 0.88]', dims: 3, cut: false, sparse: false });
  assert.deepEqual(vectorPreview('[]', { element: 'f32' }), { text: '[]', dims: 0, cut: false, sparse: false });
  // At least one number, however long.
  const one = vectorPreview('[0.000012345678,1]', { element: 'f32' }, 4);
  assert.equal(one.text, '[1.23e-5, …]');
  // Non-finite numbers come as null from the driver.
  assert.equal(vectorPreview('[1,null,2]', { element: 'f32' }).text, '[1, null, 2]');
});

test('cells that are not vectors show as text', () => {
  const m = { element: 'f32' };
  for (const v of ['hello', '0xDEADBEEF', '[1,"a"]', '[[1,2]]', '[1,,2]', '{"a":1}', 12, null, true]) {
    assert.equal(vectorPreview(v, m), null, String(v));
  }
  assert.equal(vectorPreview('[1,2]', null), null);
});

test('sparse vectors keep their text, with the dims they say', () => {
  assert.deepEqual(vectorPreview('{1:0.5,4:2}/5', { element: 'f32', sparse: true }), { text: '{1:0.5,4:2}/5', dims: 5, cut: false, sparse: true });
  assert.equal(vectorPreview('[5,[1,4],[0.5,2]]', { element: 'f32', sparse: true }).dims, 5);
  assert.equal(vectorPreview('{1:0.5}', { element: 'f32', sparse: true, dims: 30522 }).dims, 30522);
  assert.equal(vectorPreview('{"1":0.5}', { element: 'f32', sparse: true }).dims, null);
});

// -- the cell viewer ------------------------------------------------------------------------------

test('vectorStats gives the L2 norm and the range', () => {
  assert.deepEqual(vectorStats([3, -4]), { dims: 2, norm: 5, min: -4, max: 3 });
  assert.equal(vectorStats([]), null);
});

test('vectorLines puts the numbers in rows led by their position', () => {
  const text = vectorLines(Array.from({ length: 10 }, (_, i) => i / 4), 4);
  assert.equal(text, [
    '0     0  0.25   0.5  0.75',
    '4     1  1.25   1.5  1.75',
    '8     2  2.25',
  ].join('\n'));
});

// -- editing ----------------------------------------------------------------------------------------

test('checkVector accepts a JSON array with the declared dims, in canonical form', () => {
  assert.deepEqual(checkVector(' [ 0.1 , -2, 3.50 ] ', { dims: 3, element: 'f32' }), { values: [0.1, -2, 3.5], text: '[0.1,-2,3.5]' });
  assert.deepEqual(checkVector('[1,2,3,4]', { element: 'f32' }), { values: [1, 2, 3, 4], text: '[1,2,3,4]' });
});

test('checkVector refuses text, wrong dims and numbers the element cannot hold', () => {
  assert.deepEqual(checkVector('0.1, 0.2', { element: 'f32' }), { problem: { kind: 'format' } });
  assert.deepEqual(checkVector('', { element: 'f32' }), { problem: { kind: 'format' } });
  assert.deepEqual(checkVector('[1,NaN]', { element: 'f32' }), { problem: { kind: 'format' } });
  assert.deepEqual(checkVector('[1,2]', { dims: 3, element: 'f32' }), { problem: { kind: 'dims', expected: 3, got: 2 } });
  assert.deepEqual(checkVector('[1,200]', { element: 'i8' }), { problem: { kind: 'range', element: 'i8', position: 1, value: 200 } });
  assert.deepEqual(checkVector('[1.5]', { element: 'u8' }), { problem: { kind: 'range', element: 'u8', position: 0, value: 1.5 } });
  assert.deepEqual(checkVector('[0,1,2]', { element: 'bit' }), { problem: { kind: 'range', element: 'bit', position: 2, value: 2 } });
  assert.deepEqual(checkVector('[-128,127]', { element: 'i8' }).text, '[-128,127]');
  // Snowflake's VECTOR(INT, n): 32-bit integers.
  assert.deepEqual(checkVector('[-2147483648,2147483647]', { element: 'i32' }).values, [-2147483648, 2147483647]);
  assert.deepEqual(checkVector('[1,2147483648]', { element: 'i32' }), { problem: { kind: 'range', element: 'i32', position: 1, value: 2147483648 } });
  assert.equal(checkVector('[0.5]', { element: 'i32' }).problem.kind, 'range');
});

test('checkVector refuses floats the element would overflow, and an empty vector', () => {
  assert.deepEqual(checkVector('[1,70000]', { element: 'f16' }), { problem: { kind: 'overflow', element: 'f16', max: 65504, position: 1, value: 70000 } });
  assert.deepEqual(checkVector('[65504,-65519]', { element: 'f16' }).values, [65504, -65519]);
  assert.equal(checkVector('[65520]', { element: 'f16' }).problem.kind, 'overflow');
  assert.equal(checkVector('[3.41e38]', { element: 'f32' }).problem.kind, 'overflow');
  assert.equal(checkVector('[-1e39]', { element: 'f32' }).problem.position, 0);
  // The largest float32, as engines print it, fits.
  assert.deepEqual(checkVector('[3.4028235e38]', { element: 'f32' }).values, [3.4028235e38]);
  assert.equal(checkVector('[3.4e38]', { element: 'bf16' }).problem.kind, 'overflow');
  assert.deepEqual(checkVector('[3.39e38]', { element: 'bf16' }).values, [3.39e38]);
  assert.deepEqual(checkVector('[1e300]', { element: 'f64' }).values, [1e300]);
  assert.deepEqual(checkVector('[]', { element: 'f32' }), { problem: { kind: 'empty' } });
  assert.deepEqual(checkVector('[]', { dims: 3, element: 'f32' }), { problem: { kind: 'dims', expected: 3, got: 0 } });
});

test('a vector field set from the JSON tree or a typed document', () => {
  const m = { dims: 3, element: 'f32' };
  assert.equal(vectorValueProblem([1, 2, 3], m), null);
  assert.equal(vectorValueProblem(null, m), null);
  // One number removed, one that isn't a number, another type.
  assert.deepEqual(vectorValueProblem([1, 2], m), { kind: 'dims', expected: 3, got: 2 });
  assert.deepEqual(vectorValueProblem([1, 'abc', 3], m), { kind: 'format' });
  assert.deepEqual(vectorValueProblem([1, null, 3], m), { kind: 'format' });
  assert.deepEqual(vectorValueProblem({}, m), { kind: 'format' });
  assert.deepEqual(vectorValueProblem('abc', m), { kind: 'format' });
  assert.deepEqual(vectorValueProblem(5, m), { kind: 'format' });
  // Sparse vectors and other columns aren't checked here.
  assert.equal(vectorValueProblem('x', { element: 'f32', sparse: true }), null);
  assert.equal(vectorValueProblem('x', null), null);
});

test('sameVector compares the numbers, not the spelling', () => {
  assert.equal(sameVector('[1, 2.50]', '[1,2.5]'), true);
  assert.equal(sameVector('[1,2]', '[1,2,3]'), false);
  assert.equal(sameVector('[1,2]', '[2,1]'), false);
  assert.equal(sameVector(null, null), true);
  assert.equal(sameVector('[1]', null), false);
  assert.equal(sameVector('abc', 'abd'), false);
});

// -- scripts -----------------------------------------------------------------------------------------

test('scripts get a dense vector as its numbers, everything else as it is', () => {
  const dense = { dims: 3, element: 'f32' };
  assert.deepEqual(scriptValue(dense, '[0.1,-2,3.5]'), [0.1, -2, 3.5]);
  assert.equal(scriptValue(dense, null), null);
  assert.equal(scriptValue(dense, 'not a vector'), 'not a vector');
  assert.equal(scriptValue({ element: 'f32', sparse: true }, '{1:0.5}/5'), '{1:0.5}/5');
  assert.equal(scriptValue(null, '[1,2]'), '[1,2]');
  const cols = [{ name: 'id', type_name: 'int' }, { name: 'emb', type_name: 'vector(2)' }, { name: 'tags', type_name: 'jsonb' }];
  assert.deepEqual(rowsForScripts(cols, [[1, '[1,2]', '[1,2]'], [2, null, null]]), [[1, [1, 2], '[1,2]'], [2, null, null]]);
  // No vector column: the same rows.
  const rows = [[1, 'a']];
  assert.equal(rowsForScripts([{ name: 'id', type_name: 'int' }, { name: 'n', type_name: 'text' }], rows), rows);
});

test('data compare shows two vectors from where they part, telling them apart', () => {
  const a = Array.from({ length: 20 }, (_, i) => i / 7);
  const b = a.slice();
  b[9] += 1e-9;
  const d = vectorDiffPreview(JSON.stringify(a), JSON.stringify(b));
  assert.equal(d.at, 9);
  assert.equal(d.start, 9);
  assert.notEqual(d.left.text, d.right.text);
  assert.match(d.left.text, /^\[…, 1\.285714286, /);
  assert.match(d.right.text, /^\[…, 1\.285714287, /);
  assert.equal(d.left.dims, 20);
  // The usual previews would read the same.
  assert.equal(vectorPreview(JSON.stringify(a), { element: 'f32' }).text, vectorPreview(JSON.stringify(b), { element: 'f32' }).text);
  // From the start; one longer than the other; equal; not vectors.
  assert.deepEqual(vectorDiffPreview('[0.5,2]', '[0.25,2]'), {
    left: { text: '[0.5, 2]', dims: 2, cut: false, sparse: false }, right: { text: '[0.25, 2]', dims: 2, cut: false, sparse: false }, at: 0, start: 0,
  });
  const longer = vectorDiffPreview('[1,2,3]', '[1,2,3,4]');
  assert.equal(longer.at, 3);
  assert.equal(longer.start, 2);
  assert.equal(longer.left.text, '[…, 3]');
  assert.equal(longer.right.text, '[…, 3, 4]');
  assert.equal(vectorDiffPreview('[1, 2]', '[1,2]'), null);
  assert.equal(vectorDiffPreview('abc', '[1]'), null);
});

test('vectors sort number by number, then the shorter first', () => {
  assert.ok(compareVectors([-1, 2], [0.25, 3]) < 0);
  assert.ok(compareVectors([0.5, 1], [0.25, 3]) > 0);
  assert.ok(compareVectors([1, 2], [1, 2, 0]) < 0);
  assert.equal(compareVectors([1, 2], [1, 2]), 0);
});

test('two vector indexes are the same when they build the same way', () => {
  const hnsw = { algorithm: 'hnsw', metric: 'cosine', params: { m: '16', ef_construction: '64' } };
  assert.equal(sameVectorIndex(hnsw, { algorithm: 'HNSW', metric: 'cosine', params: { ef_construction: ' 64', m: '16', lists: '' } }), true);
  assert.equal(sameVectorIndex(hnsw, { ...hnsw, metric: 'l2' }), false);
  assert.equal(sameVectorIndex(hnsw, { ...hnsw, algorithm: 'ivfflat' }), false);
  assert.equal(sameVectorIndex(hnsw, { ...hnsw, params: { m: '24', ef_construction: '64' } }), false);
  assert.equal(sameVectorIndex(hnsw, undefined), false);
  assert.equal(sameVectorIndex(undefined, null), true);
});

test('a vector index in one line', () => {
  assert.equal(vectorIndexText({ algorithm: 'hnsw', metric: 'cosine', params: { m: '16', ef_construction: '64' } }, (m) => m.toUpperCase()),
    'hnsw · COSINE · m = 16, ef_construction = 64');
  assert.equal(vectorIndexText({ metric: 'DOT_PRODUCT' }), 'DOT_PRODUCT');
  assert.equal(vectorIndexText({}), '');
  assert.equal(vectorIndexText(undefined), '');
});

test("a vector index's tag in the explorer names its algorithm once, with its metric", () => {
  const up = (m) => m.toUpperCase();
  // pgvector: the kind is the algorithm.
  assert.equal(vectorIndexTag('HNSW', { algorithm: 'hnsw', metric: 'cosine', params: { m: '16' } }, up), 'HNSW · COSINE');
  assert.equal(vectorIndexTag('IVFFLAT', { algorithm: 'ivfflat', metric: 'l2' }), 'IVFFLAT · l2');
  // SQL Server 2025 / MySQL: a generic kind gives way to the algorithm.
  assert.equal(vectorIndexTag('VECTOR', { algorithm: 'diskann', metric: 'cosine' }), 'DISKANN · cosine');
  assert.equal(vectorIndexTag('vector', { algorithm: 'hnsw', metric: 'l2' }), 'HNSW · l2');
  // Without an algorithm the kind stays; another kind stays with it.
  assert.equal(vectorIndexTag('VECTOR', { algorithm: '', metric: 'cosine' }), 'VECTOR · cosine');
  assert.equal(vectorIndexTag('NC', { algorithm: 'diskann', metric: 'cosine' }), 'NC · DISKANN · cosine');
  // Spelled apart (vector_similarity vs VECTOR SIMILARITY) still counts as named.
  assert.equal(vectorIndexTag('VECTOR SIMILARITY', { algorithm: 'vector_similarity', metric: 'l2' }), 'VECTOR SIMILARITY · l2');
  // No kind: the algorithm alone; no algorithm: the kind alone.
  assert.equal(vectorIndexTag('', { algorithm: 'hnsw', metric: 'ip' }), 'HNSW · ip');
  assert.equal(vectorIndexTag('VECTORSEARCH', { metric: 'dotProduct' }), 'VECTORSEARCH · dotProduct');
  // Not a vector index: the kind's tag as is.
  assert.equal(vectorIndexTag('BTREE', null), 'BTREE');
});
