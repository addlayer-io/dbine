// Tests for the results pane's search (gridSearch.ts, issue #142): what
// matches, the per-column counts, moving from match to match and the
// highlighted parts. Run: `node --test src/composables/gridSearch.test.mjs`
// from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';

const { matcher, cellText, emptyHits, scan, rowColumns, rowsWithColumn, step, placeOf, parts, searchRows } = await import('./gridSearch.ts');

const rows = [
  ['customer_active', 1, null, true],
  ['order_pending', 22, 'Customer note', false],
  ['customer_inactive', 3, 'none', null],
  ['shipped', 10, null, null],
];
const cols = [0, 1, 2, 3];
const found = (text, caseSensitive = false) => {
  const re = matcher({ text, caseSensitive });
  const h = emptyHits(4, rows.length);
  scan(rows, cols, re, 0, rows.length, h);
  return { h, re, list: Array.from(h.rows.slice(0, h.n)) };
};

test('the example in the issue: "customer" matches the first two values and a note', () => {
  const { h, list } = found('customer');
  assert.deepEqual(list, [0, 1, 2]);
  assert.equal(h.cells, 3);
  assert.deepEqual(h.perColumn, [2, 0, 1, 0]);
  assert.deepEqual(Array.from(h.counts.slice(0, h.n)), [1, 1, 1]);
});

test('case-sensitive search tells "Customer" apart', () => {
  assert.deepEqual(found('Customer', true).list, [1]);
  assert.deepEqual(found('customer', true).list, [0, 2]);
});

test('numbers and booleans match their text; NULL never matches', () => {
  assert.deepEqual(found('2').list, [1]);
  assert.deepEqual(found('true').list, [0]);
  assert.deepEqual(found('null').list, []);
  assert.equal(cellText(null), null);
  assert.equal(cellText(1.5), '1.5');
});

test('regular expression characters are taken literally', () => {
  const r = [['a.b'], ['axb'], ['(x)']];
  const h = emptyHits(1, r.length);
  scan(r, [0], matcher({ text: 'a.b', caseSensitive: false }), 0, r.length, h);
  assert.equal(h.n, 1);
  const h2 = emptyHits(1, r.length);
  scan(r, [0], matcher({ text: '(x', caseSensitive: false }), 0, r.length, h2);
  assert.equal(h2.n, 1);
});

test('nothing to look for: no matcher', () => {
  assert.equal(matcher({ text: '', caseSensitive: false }), null);
});

test('the columns left out (vectors) are not searched', () => {
  const re = matcher({ text: 'customer', caseSensitive: false });
  const h = emptyHits(4, rows.length);
  scan(rows, [1, 2, 3], re, 0, rows.length, h);
  assert.deepEqual(Array.from(h.rows.slice(0, h.n)), [1]);
});

test('next and previous go cell by cell, row by row, and wrap around', () => {
  const r = [['ab', 'x', 'ab'], ['x', 'x', 'x'], ['x', 'ab', 'x']];
  const re = matcher({ text: 'ab', caseSensitive: false });
  const h = emptyHits(3, r.length);
  scan(r, [0, 1, 2], re, 0, r.length, h);
  const colsOf = (k) => rowColumns(r[h.rows[k]], [0, 1, 2], re);
  let s = step(h.n, colsOf, null, 1);
  assert.deepEqual(s, { k: 0, c: 0 });
  s = step(h.n, colsOf, s, 1);
  assert.deepEqual(s, { k: 0, c: 2 });
  s = step(h.n, colsOf, s, 1);
  assert.deepEqual(s, { k: 1, c: 1 });
  assert.equal(h.rows[s.k], 2);
  assert.equal(placeOf(h, s, colsOf), 3);
  // Past the last one, back to the first.
  assert.deepEqual(step(h.n, colsOf, s, 1), { k: 0, c: 0 });
  // Previous from the first: the last one.
  assert.deepEqual(step(h.n, colsOf, { k: 0, c: 0 }, -1), { k: 1, c: 1 });
  assert.deepEqual(step(h.n, colsOf, null, -1), { k: 1, c: 1 });
  assert.equal(step(0, colsOf, null, 1), null);
});

test('a row that no longer matches (edited since) is skipped', () => {
  const colsOf = (k) => (k === 1 ? [] : [0]);
  assert.deepEqual(step(3, colsOf, { k: 0, c: 0 }, 1), { k: 2, c: 0 });
});

test('the rows whose column matches, for a search limited to one column', () => {
  const { h, re } = found('customer');
  assert.deepEqual(Array.from(rowsWithColumn(rows, h, 0, re)), [0, 2]);
  assert.deepEqual(Array.from(rowsWithColumn(rows, h, 2, re)), [1]);
  assert.deepEqual(Array.from(rowsWithColumn(rows, h, 1, re)), []);
});

test('the highlighted parts of a cell', () => {
  const re = matcher({ text: 'cust', caseSensitive: false });
  assert.deepEqual(parts('Customer customer', re), [
    { s: 'Cust', hit: true }, { s: 'omer ', hit: false }, { s: 'cust', hit: true }, { s: 'omer', hit: false },
  ]);
  assert.deepEqual(parts('none', re), [{ s: 'none', hit: false }]);
  assert.deepEqual(parts('abc', null), [{ s: 'abc', hit: false }]);
});

test('a big result is searched in slices and the counts grow as it goes', async () => {
  const big = Array.from({ length: 200_000 }, (_, i) => [`row ${i}`, i % 7 === 0 ? 'seven' : 'other']);
  const re = matcher({ text: 'seven', caseSensitive: false });
  let slices = 0;
  const h = await searchRows(big, 2, [0, 1], re, { cancelled: () => false, onSlice: () => { slices++; }, sliceMs: 2 });
  assert.equal(h.n, Math.ceil(200_000 / 7));
  assert.equal(h.perColumn[1], h.n);
  assert.equal(h.scanned, 200_000);
  assert.ok(slices > 1, `searched in ${slices} slice(s)`);
  // The hit arrays grew past their first size and kept every row.
  assert.equal(h.rows[h.n - 1], 199_997);
});

test('with "Solo filas", an edit or deletion lands on the row it was made on', async () => {
  const { filteredView, placeIn, rowAt, keyedIn, setIn } = await import('./gridSearch.ts');
  // Five result rows, rows 1 and 3 match; two new rows follow (5 and 6).
  const view = filteredView(Uint32Array.from([1, 3]), 5, 2);
  assert.deepEqual(view, [1, 3, 5, 6]);
  // The grid's row 1 is the result's row 3; its new rows keep their indexes.
  assert.equal(rowAt(view, 1), 3);
  assert.equal(rowAt(view, 2), 5);
  assert.equal(placeIn(view, 3), 1);
  assert.equal(placeIn(view, 2), -1);
  assert.equal(placeIn(view, 6), 3);
  // Pending edits (by result row) show on the grid's rows; hidden ones stay pending, not shown.
  assert.deepEqual(keyedIn(view, { 0: { 0: 'x' }, 3: { 1: 'y' }, 6: { 0: 'new' } }), { 1: { 1: 'y' }, 3: { 0: 'new' } });
  assert.deepEqual([...setIn(view, new Set([1, 2]))], [0]);
  // An edit on the grid's row 0 goes back to the result's row 1.
  assert.equal(rowAt(view, 0), 1);
  // Without the filter, nothing moves.
  assert.equal(rowAt(null, 4), 4);
  assert.equal(placeIn(null, 4), 4);
  const e = { 2: { 0: 'z' } };
  assert.equal(keyedIn(null, e), e);
});

test('a search stops when cancelled', async () => {
  const big = Array.from({ length: 200_000 }, (_, i) => [`row ${i}`]);
  let n = 0;
  const h = await searchRows(big, 1, [0], matcher({ text: 'row', caseSensitive: false }), { cancelled: () => ++n > 1, sliceMs: 1 });
  assert.equal(h, null);
});
