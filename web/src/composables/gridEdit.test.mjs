// Tests for editing a result's cells (gridEdit.ts) with vector columns: they
// are never in the WHERE (by the driver's mark or the type name), and the
// changes and new rows send a vector as its numbers. Run: `node --test
// src/composables/gridEdit.test.mjs` from web/ (Node 23.6+ strips the
// TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';

// gridEdit.ts imports its siblings without an extension (as Vite does):
// resolve them to the .ts files. The texts come back as their keys, and the
// connections store isn't used by what's tested.
register('data:text/javascript,' + encodeURIComponent(`
const stubs = {
  '../i18n': 'export const t = (k, o) => o ? k + " " + JSON.stringify(o) : k;',
  '../stores/connections': 'export const dbKey = () => ""; export const useConnectionsStore = () => ({});',
};
export async function resolve(spec, ctx, next) {
  if (stubs[spec]) return { url: 'data:text/javascript,' + encodeURIComponent(stubs[spec]), shortCircuit: true };
  if (spec.startsWith('.') && !/\\.[cm]?[jt]s$/.test(spec)) return next(spec + '.ts', ctx);
  return next(spec, ctx);
}`));
const { buildChanges, buildInserts, coerceNew, comparableType, rowKey, skippedKeyColumns } = await import('./gridEdit.ts');

const target = { kind: 'table', schema: 'public', name: 'docs' };
const columns = [
  { name: 'id', type_name: 'int4' },
  { name: 'title', type_name: 'text' },
  // pgvector: the type name says it.
  { name: 'embedding', type_name: 'vector(3)' },
  // A driver's mark on a type name that doesn't say it (MySQL 9's binary wire, Redis…).
  { name: 'emb2', type_name: 'blob_or_whatever', vector: { dims: 2, element: 'f32' } },
  // Sparse: its text goes as it is.
  { name: 'sparse', type_name: 'sparsevec(5)' },
];
const rows = [
  [1, 'a', '[0.1,0.2,0.3]', '[1,2]', '{1:0.5}/5'],
  [2, 'b', '[0.4,0.5,0.6]', '[3,4]', null],
];
const byPk = { target, keyColumns: ['id'], all: false, note: null, identity: [] };
const byAll = { target, keyColumns: columns.map((c) => c.name), all: true, note: 'no pk', identity: [] };

test('vector columns are never comparable in a WHERE', () => {
  assert.equal(comparableType(columns[0], 'postgres'), true);
  assert.equal(comparableType(columns[1], 'postgres'), true);
  assert.equal(comparableType(columns[2], 'postgres'), false);
  assert.equal(comparableType(columns[3], 'mysql'), false);
  assert.equal(comparableType(columns[4], 'postgres'), false);
  assert.equal(comparableType({ name: 'v', type_name: 'VECTOR(3, FLOAT32)' }, 'oracle'), false);
  // Binary stays as it was.
  assert.equal(comparableType({ name: 'b', type_name: 'bytea' }, 'postgres'), false);
});

test('a table without a key: the WHERE leaves the vector columns out', () => {
  assert.deepEqual(rowKey(columns, rows[0], byAll, 'postgres'), [['id', 1], ['title', 'a']]);
  assert.deepEqual(skippedKeyColumns(columns, byAll, 'postgres'), ['embedding', 'emb2', 'sparse']);
  assert.deepEqual(skippedKeyColumns(columns, byPk, 'postgres'), []);
});

test('a primary key on a vector column cannot find the row', () => {
  const setup = { ...byPk, keyColumns: ['emb2'] };
  assert.throws(() => rowKey(columns, rows[0], setup, 'mysql'), /core:gridEdit\.keyNotComparable .*"emb2"/);
  const onlyVectors = { ...byAll, keyColumns: ['embedding', 'emb2'] };
  assert.throws(() => rowKey(columns, rows[0], onlyVectors, ''), /core:gridEdit\.noComparableColumns/);
});

test('an edited vector goes to update_script as its numbers', () => {
  const edits = { 0: { 2: '[1,0,0]', 3: '[5,6]', 1: 'A' }, 1: { 4: '{2:1}/5' } };
  const changes = buildChanges(columns, rows, edits, byPk, 'postgres');
  assert.deepEqual(changes[0].key, [['id', 1]]);
  assert.deepEqual(changes[0].set, [['title', 'A'], ['embedding', [1, 0, 0]], ['emb2', [5, 6]]]);
  // The whole row (engines that rewrite it) has the vectors as numbers too.
  assert.deepEqual(changes[0].row, [['id', 1], ['title', 'a'], ['embedding', [0.1, 0.2, 0.3]], ['emb2', [1, 2]], ['sparse', '{1:0.5}/5']]);
  assert.deepEqual(changes[1].set, [['sparse', '{2:1}/5']]);
  // Set to NULL stays NULL.
  assert.deepEqual(buildChanges(columns, rows, { 1: { 2: null } }, byPk, 'postgres')[0].set, [['embedding', null]]);
});

test('without a key, the edit of a vector keeps it out of the WHERE', () => {
  const [ch] = buildChanges(columns, rows, { 1: { 2: '[0,0,1]' } }, byAll, 'postgres');
  assert.deepEqual(ch.key, [['id', 2], ['title', 'b']]);
  assert.deepEqual(ch.set, [['embedding', [0, 0, 1]]]);
});

test('a new row sends its vectors as numbers', () => {
  const inserts = buildInserts(columns, [{ 0: 3, 2: '[0.7,0.8,0.9]', 3: '[7,8]', 4: '{1:1}/5' }, {}]);
  assert.deepEqual(inserts, [[['id', 3], ['embedding', [0.7, 0.8, 0.9]], ['emb2', [7, 8]], ['sparse', '{1:1}/5']]]);
});

test('a new row keeps a vector typed as text (the grid checks it)', () => {
  // `VECTOR(3, FLOAT32)` says FLOAT, but it isn't a number.
  assert.equal(coerceNew('VECTOR(3, FLOAT32)', '[1,2,3]'), '[1,2,3]');
  assert.equal(coerceNew('vector(3)', '[1,2,3]'), '[1,2,3]');
  assert.equal(coerceNew('int', '42'), 42);
});
