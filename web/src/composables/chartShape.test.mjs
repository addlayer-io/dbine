// Tests for what charts plot (chartShape.ts): bins, quartiles, heatmaps,
// box plots, KPIs and points. Run: `node --test src/composables/chartShape.test.mjs`
// from web/ (Node 23.6+ strips the TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  BOX_SERIES, boxStats, dateColumns, heatmapShape, histogram, numericColumns, quartiles, shapeFromPoints, shapePoints, shapeRows,
} from './chartShape.ts';

const L = {
  nul: '(nulo)',
  others: 'Otros',
  series: (agg, column) => (agg === 'none' ? column : `${agg}(${column})`),
  bin: (a, b) => `${a}-${b}`,
};

test('histogram: equal-width bins, the last one closed', () => {
  const { edges, counts } = histogram([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 5);
  assert.deepEqual(edges, [0, 2, 4, 6, 8, 10]);
  assert.deepEqual(counts, [2, 2, 2, 2, 3]);
  assert.equal(counts.reduce((a, b) => a + b, 0), 11);
});

test('histogram: automatic bins (Sturges, 5 to 50), equal values, nothing', () => {
  const vals = Array.from({ length: 1000 }, (_, i) => i);
  assert.equal(histogram(vals).counts.length, 11);
  assert.equal(histogram([1, 2, 3]).counts.length, 5);
  assert.deepEqual(histogram([4, 4, 4]), { edges: [4, 4], counts: [3] });
  assert.deepEqual(histogram([]), { edges: [], counts: [] });
  assert.deepEqual(histogram([NaN, Infinity]), { edges: [], counts: [] });
});

test('quartiles interpolate (type 7)', () => {
  assert.deepEqual(quartiles([1, 2, 3, 4]), { min: 1, q1: 1.75, median: 2.5, q3: 3.25, max: 4 });
  assert.deepEqual(quartiles([5]), { min: 5, q1: 5, median: 5, q3: 5, max: 5 });
  assert.deepEqual(quartiles([9, 1, 5]), { min: 1, q1: 3, median: 5, q3: 7, max: 9 });
  assert.equal(quartiles([]), null);
});

test("box: whiskers stop at 1.5 IQR, the rest are outliers", () => {
  const b = boxStats([1, 2, 3, 4, 5, 6, 7, 8, 100]);
  assert.deepEqual(b.box, [1, 3, 5, 7, 8]);
  assert.deepEqual(b.outliers, [100]);
});

test('heatmap: X × Y → the aggregate; empty cells are NaN; extra categories fold', () => {
  const rows = [['a', 'x', 1], ['a', 'x', 2], ['b', 'y', 5], ['a', 'y', null]];
  const h = heatmapShape(rows, 0, 1, 2, 'sum', L);
  assert.deepEqual(h.cats, ['a', 'b']);
  assert.deepEqual(h.names, ['x', 'y']);
  assert.deepEqual(h.series[0], [3, NaN]);
  assert.deepEqual(h.series[1], [NaN, 5], 'a cell whose rows have no value is empty');
  const count = heatmapShape(rows, 0, 1, 2, 'count', L);
  assert.deepEqual(count.series, [[2, NaN], [1, 1]]);
  // More X values than the cap: the most frequent stay, the rest are "Otros".
  const many = Array.from({ length: 70 }, (_, i) => [`k${i}`, 'x', 1]).concat([['k0', 'x', 1]]);
  const f = heatmapShape(many, 0, 1, 2, 'sum', L);
  assert.equal(f.cats.length, 61);
  assert.equal(f.cats.at(-1), 'Otros');
  assert.equal(f.series[0][0], 2);
});

const cols = ['region', 'kind', 'total'];
const rows = [['n', 'a', 10], ['n', 'b', 20], ['s', 'a', 30], ['s', 'a', null], ['e', 'b', 40]];
const r = (o) => ({ kind: 'bar', x: 0, y: [2], agg: 'sum', sort: 'none', split: null, bins: 0, ...o });

test('group charts: one value per category, sorted on request', () => {
  const sh = shapeRows(rows, r({ sort: 'value' }), cols, L);
  assert.deepEqual(sh.cats, ['e', 'n', 's'], 'ties keep their order');
  assert.deepEqual(sh.series, [[40, 30, 30]]);
  assert.deepEqual(sh.names, ['sum(total)']);
  // Combo plots like bars (its second axis is the option's).
  assert.deepEqual(shapeRows(rows, r({ kind: 'combo' }), cols, L).series, [[30, 30, 40]]);
});

test('histogram, box and KPI shapes', () => {
  const h = shapeRows(rows, r({ kind: 'histogram', x: -1, bins: 3 }), cols, L);
  assert.deepEqual(h.cats, ['10-20', '20-30', '30-40']);
  assert.deepEqual(h.series, [[1, 1, 2]]);
  const b = shapeRows(rows, r({ kind: 'box', x: 1 }), cols, L);
  assert.deepEqual(b.cats, ['a', 'b']);
  assert.deepEqual(b.names, [...BOX_SERIES]);
  assert.deepEqual(b.series.map((s) => s[0]), [10, 15, 20, 25, 30]);
  const one = shapeRows(rows, r({ kind: 'box', x: -1 }), cols, L);
  assert.deepEqual(one.cats, ['total']);
  const k = shapeRows(rows, r({ kind: 'kpi', x: -1, agg: 'avg' }), cols, L);
  assert.deepEqual([k.series[0][0], k.count, k.names[0]], [25, 4, 'avg(total)']);
  // "Cantidad" counts every row, as COUNT(*).
  assert.equal(shapeRows(rows, r({ kind: 'kpi', x: -1, agg: 'count' }), cols, L).series[0][0], 5);
});

test('points: NaN → null, a KPI keeps its rows; they read back as the same shape', () => {
  const k = shapeRows(rows, r({ kind: 'kpi', x: -1 }), cols, L);
  const p = shapePoints(k);
  assert.deepEqual(p, { categories: ['total'], series: [{ name: 'sum(total)', values: [100] }, { name: 'rows', values: [4] }] });
  const back = shapeFromPoints('kpi', JSON.parse(JSON.stringify(p)));
  assert.deepEqual([back.series, back.count], [[[100]], 4]);
  const hp = shapePoints(heatmapShape([['a', 'x', 1], ['b', 'y', 2]], 0, 1, 2, 'sum', L));
  assert.deepEqual(hp.series[0].values, [1, null]);
  assert.ok(Number.isNaN(shapeFromPoints('heatmap', hp).series[0][1]));
});

test('column typing samples the values', () => {
  const rs = [['2026-01-02', '3', 'x', 1.5], ['2026-02-01', '4', 'y', null]];
  assert.deepEqual(numericColumns(rs, 4), [1, 3]);
  assert.deepEqual(dateColumns(rs, 4), [0]);
});
