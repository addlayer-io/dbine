// "No sort" goes back to the order the rows arrived in, also when the result
// is reactive, as the editor's are. Run:
// `node --test src/composables/arrivalOrder.test.mjs` from web/. Not part of
// the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { ref } from 'vue';
import { arrivalOrder } from './arrivalOrder.ts';

/** As ResultsPane's sort: the rows are replaced in the result object. */
function sort(order, result, next) {
  const before = result.rows;
  const after = next === 'none' ? order.of(before) : before.slice().sort((a, b) => (next === 'desc' ? b[0] - a[0] : a[0] - b[0]));
  order.moved(before, after);
  result.rows = after;
}
const values = (rows) => rows.map((r) => r[0]);

for (const [name, make] of [['a plain result', (x) => x], ['a reactive one', (x) => ref(x).value]]) {
  test(`after sorting twice, no sort is the arrival order again: ${name}`, () => {
    const order = arrivalOrder();
    const result = make({ rows: [[2], [3], [1]] });
    sort(order, result, 'asc');
    sort(order, result, 'desc');
    assert.deepEqual(values(order.of(result.rows)), [2, 3, 1]);
    sort(order, result, 'none');
    assert.deepEqual(values(result.rows), [2, 3, 1]);
  });

  test(`rows added after a sort stay last: ${name}`, () => {
    const order = arrivalOrder();
    const result = make({ rows: [[2], [1]] });
    sort(order, result, 'asc');
    result.rows.push([0]);
    assert.deepEqual(values(order.of(result.rows)), [2, 1, 0]);
  });
}
