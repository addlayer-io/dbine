// Tests for what exporting a multi-database run's results runs again
// (multiDbParts.ts). Run: `node --test src/composables/multiDbParts.test.mjs`
// from web/ (Node 23.6+ strips the TypeScript types itself). Not part of the
// app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { multiDbParts } from './multiDbParts.ts';

const set = (cols, rows = []) => ({ columns: cols.map((name) => ({ name, type_name: '' })), rows, total_rows: rows.length, truncated: false });
const none = () => ({ columns: [], rows: [], total_rows: 0, truncated: false, rows_affected: 0 });
const db = (database, status, results) => ({ database, status, error: null, rows: 0, rows_affected: null, elapsed_ms: 1, results, messages: [] });

test('the merged grid exports every row of every database that ended well, base first', () => {
  // The grid keeps some rows of each database; the export runs the script again on all of them.
  const merged = { ...set(['base', 'id']), total_rows: 50_254_629, truncated: true };
  const parts = multiDbParts({
    needs_confirmation: null, merged, cancelled: false, elapsed_ms: 1,
    databases: [
      db('a', 'ok', [none(), set(['n'])]),
      db('b', 'error', [set(['x'])]),
      db('c', 'ok', []),
    ],
  });
  assert.deepEqual(parts[0].export, { databases: ['a', 'c'], set: 0, base: true });
  assert.equal(parts[0].database, null);
  // a's second result set with columns (the merge took its first one).
  assert.deepEqual(parts[1].export, { databases: ['a'], set: 1, base: false });
  assert.equal(parts[1].database, 'a');
  // A database that failed: only the rows its tab has.
  assert.equal(parts[2].database, 'b');
  assert.equal(parts[2].export, null);
  assert.equal(parts.length, 3);
});

test('without a merged grid each database exports its own result sets', () => {
  const parts = multiDbParts({
    needs_confirmation: null, merged: null, cancelled: false, elapsed_ms: 1,
    databases: [db('a', 'ok', [set(['x']), none(), set(['y'])]), db('b', 'ok', [set(['z'])])],
  });
  assert.deepEqual(parts.map((p) => [p.database, p.index, p.count, p.export]), [
    ['a', 0, 2, { databases: ['a'], set: 0, base: false }],
    ['a', 1, 2, { databases: ['a'], set: 1, base: false }],
    ['b', 0, 1, { databases: ['b'], set: 0, base: false }],
  ]);
});
