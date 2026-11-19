// Tests for the grid's column kinds (gridFilter.ts) and the local sort
// (gridSort.ts) with vector columns: a vector type is text, even when its
// name says FLOAT or INT8, so its filters and sort work on the text. Run:
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
const { columnKind } = await import('./gridFilter.ts');
const { sortRows } = await import('./gridSort.ts');

test('vector types are text, whatever their element type says', () => {
  for (const ty of ['VECTOR(3, FLOAT32)', 'vector<float, 3>', 'REAL_VECTOR(3)', 'VECTOR(8, INT8)', 'vector(1536)', 'halfvec(3)', 'F32_BLOB(3)', 'VECTOR(FLOAT, 3)']) {
    assert.equal(columnKind(ty, '[1,2,3]'), 'text', ty);
  }
  // The other types as before.
  assert.equal(columnKind('float8'), 'number');
  assert.equal(columnKind('int'), 'number');
  assert.equal(columnKind('timestamp'), 'date');
  assert.equal(columnKind('boolean'), 'bool');
});

test('the local sort orders vector cells by their text', () => {
  const columns = [{ name: 'id', type_name: 'int' }, { name: 'v', type_name: 'VECTOR(2, FLOAT32)' }];
  const rows = [[1, '[0.5,1]'], [2, null], [3, '[-1,2]'], [4, '[0.25,3]']];
  const sorted = sortRows(rows, columns, [{ column: 'v', descending: false }]).map((r) => r[0]);
  // The grid's text order (its collator reads digit runs as numbers: "0.5"
  // before "0.25"), not NaN as numbers; NULLs last.
  assert.deepEqual(sorted, [3, 1, 4, 2]);
});
