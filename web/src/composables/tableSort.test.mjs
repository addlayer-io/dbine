// The Monitor's tables sort by any column, on the rows already read. Run:
// `node --test src/composables/tableSort.test.mjs` from web/. Not part of the
// app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { clickSort, numericColumn, sortTable } from './tableSort.ts';

const column = (values, descending = false) =>
  sortTable(values.map((v) => [v]), ['c'], { column: 'c', descending }).map((r) => r[0]);

test('numbers go by value, also as text and with decimals and signs', () => {
  assert.deepEqual(column([962, 5, 11729, 63]), [5, 63, 962, 11729]);
  assert.deepEqual(column(['1.5', '-3', '1.25', '10']), ['-3', '1.25', '1.5', '10']);
});

test('sizes with their unit go by bytes', () => {
  assert.deepEqual(column(['1.5 GB', '800 MB', '12 kB', '2 TB', '900 B', '1 bytes']),
    ['1 bytes', '900 B', '12 kB', '800 MB', '1.5 GB', '2 TB']);
  assert.deepEqual(column(['1 GiB', '1023.5 MiB']), ['1023.5 MiB', '1 GiB']);
});

test('durations with their unit go by time, and percentages by value', () => {
  assert.deepEqual(column(['2.00h', '1.50m', '3.2s', '250ms', '1d', '5 min']), ['250ms', '3.2s', '1.50m', '5 min', '2.00h', '1d']);
  assert.deepEqual(column(['100%', '9.5 %', '45%']), ['9.5 %', '45%', '100%']);
});

test('text goes alphabetically, ignoring case, with numbers inside by value', () => {
  assert.deepEqual(column(['table10', 'Table2', 'alpha', 'RSA-ARG\\ekim']), ['alpha', 'RSA-ARG\\ekim', 'Table2', 'table10']);
});

test('dates and times written as text keep their order', () => {
  assert.deepEqual(column(['2026-11-27 10:00:00', '2025-01-02 23:59:59', '2026-11-27 09:00:00']),
    ['2025-01-02 23:59:59', '2026-11-27 09:00:00', '2026-11-27 10:00:00']);
});

test('empty cells stay at the bottom in both directions', () => {
  assert.deepEqual(column([null, 3, '', 1]), [1, 3, null, '']);
  assert.deepEqual(column([null, 3, '', 1], true), [3, 1, null, '']);
});

test('numbers go before text in a mixed column', () => {
  assert.deepEqual(column(['n/a', 10, 2]), [2, 10, 'n/a']);
});

test('equal values keep the order they came in, and the rows read are left as they were', () => {
  const rows = [[1, 'b'], [1, 'a'], [0, 'c']];
  assert.deepEqual(sortTable(rows, ['n', 't'], { column: 'n', descending: true }), [[1, 'b'], [1, 'a'], [0, 'c']]);
  assert.deepEqual(rows, [[1, 'b'], [1, 'a'], [0, 'c']]);
});

test('no sort, or a column the table no longer has, is the order read', () => {
  const rows = [[2], [1]];
  assert.deepEqual(sortTable(rows, ['a'], null), rows);
  assert.deepEqual(sortTable(rows, ['a'], { column: 'gone', descending: false }), rows);
});

test('a header click: text starts ascending, numbers descending; then the other way, then none', () => {
  const cycle = (numeric) => {
    const seen = [];
    let s = null;
    for (let i = 0; i < 3; i++) { s = clickSort(s, 'c', numeric); seen.push(s && s.descending); }
    return seen;
  };
  assert.deepEqual(cycle(false), [false, true, null]);
  assert.deepEqual(cycle(true), [true, false, null]);
  assert.deepEqual(clickSort({ column: 'a', descending: true }, 'b', false), { column: 'b', descending: false });
});

test('a column is numeric when everything in it is', () => {
  const rows = [[1, '800 MB', 'x', null], [2, '1 GB', 3, null]];
  assert.deepEqual([0, 1, 2, 3].map((i) => numericColumn(rows, i)), [true, true, false, false]);
});
