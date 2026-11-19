// Tests for the grid's column kinds (gridFilter.ts) and the local sort
// (gridSort.ts) with vector columns: a vector type is its own kind, even
// when its name says FLOAT or INT8. Engines can't filter a vector by its
// text (`LIKE` / `=` fail on pgvector, MySQL 9 and SQL Server 2025), so its
// filters are NULL / NOT NULL and SQL; the local sort goes number by number. Run:
// `node --test src/composables/gridFilter.test.mjs` from web/ (Node 23.6+
// strips the TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';

// gridFilter.ts imports its siblings without an extension (as Vite does):
// resolve them to the .ts files. Labels come back as their keys.
register('data:text/javascript,' + encodeURIComponent(`
const stubs = { './i18nLabels': 'export const rt = (k) => k;' };
export async function resolve(spec, ctx, next) {
  if (stubs[spec]) return { url: 'data:text/javascript,' + encodeURIComponent(stubs[spec]), shortCircuit: true };
  if (spec.startsWith('.') && !/\\.[cm]?[jt]s$/.test(spec)) return next(spec + '.ts', ctx);
  return next(spec, ctx);
}`));
const { columnKind, menuFor, parseFilter } = await import('./gridFilter.ts');
const { sortRows } = await import('./gridSort.ts');

test('vector types are vectors, whatever their element type says', () => {
  for (const ty of [
    'VECTOR(3, FLOAT32)', 'vector<float, 3>', 'REAL_VECTOR(3)', 'VECTOR(8, INT8)', 'vector(1536)', 'halfvec(3)', 'F32_BLOB(3)', 'VECTOR(FLOAT, 3)',
    'FLOAT[3]', 'Array(Float32)', 'ARRAY<FLOAT64>', 'float_vector(3)',
  ]) {
    assert.equal(columnKind(ty, '[1,2,3]'), 'vector', ty);
  }
  // The other types as before.
  assert.equal(columnKind('float8'), 'number');
  assert.equal(columnKind('int'), 'number');
  assert.equal(columnKind('timestamp'), 'date');
  assert.equal(columnKind('boolean'), 'bool');
});

test('a vector column filters by NULL / NOT NULL and SQL only', () => {
  assert.deepEqual(parseFilter('v', 'vector', 'NULL'), [{ column: 'v', op: 'is_null', values: [] }]);
  assert.deepEqual(parseFilter('v', 'vector', 'not null'), [{ column: 'v', op: 'not_null', values: [] }]);
  // Text, a number, `=`, a vector: nothing an engine can run (the grid says why).
  for (const raw of ['0.12', '[0.1, 0.2]', '=[1,2]', '<>x', '^[0', 'EMPTY']) assert.deepEqual(parseFilter('v', 'vector', raw), [], raw);
  const ops = menuFor('vector').map((e) => e.op ?? 'clear');
  assert.deepEqual(ops, ['clear', 'is_null', 'not_null', 'sql', 'sql_right']);
  // Text columns keep their menu.
  assert.ok(menuFor('text').some((e) => e.op === 'contains'));
});

test('the local sort orders vectors number by number', () => {
  const columns = [{ name: 'id', type_name: 'int' }, { name: 'v', type_name: 'VECTOR(2, FLOAT32)' }];
  const rows = [[1, '[0.5,1]'], [2, null], [3, '[-1,2]'], [4, '[0.25,3]'], [5, '[0.25,3,1]']];
  const asc = sortRows(rows, columns, [{ column: 'v', descending: false }]).map((r) => r[0]);
  // -1 < 0.25 < 0.5 (as numbers, not as text); the shorter first; NULLs last.
  assert.deepEqual(asc, [3, 4, 5, 1, 2]);
  const desc = sortRows(rows, columns, [{ column: 'v', descending: true }]).map((r) => r[0]);
  assert.deepEqual(desc, [1, 5, 4, 3, 2]);
  // The driver's mark when the type name doesn't say it (MySQL's binary wire, Redis).
  const marked = [{ name: 'id', type_name: 'int' }, { name: 'v', type_name: 'blob', vector: { element: 'f32' } }];
  assert.deepEqual(sortRows(rows, marked, [{ column: 'v', descending: false }]).map((r) => r[0]), [3, 4, 5, 1, 2]);
  // A cell that isn't a vector goes after the vectors.
  const odd = [[1, '[1]'], [2, 'n/a'], [3, '[0]']];
  assert.deepEqual(sortRows(odd, columns, [{ column: 'v', descending: false }]).map((r) => r[0]), [3, 1, 2]);
});
