// Tests for the chart settings by column name (chartSettings.ts). Run:
// `node --test src/composables/chartSettings.test.mjs` from web/ (Node 23.6+
// strips the TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { cellChart, chartPoints, readCellChart, resolveChartSettings, sameJson } from './chartSettings.ts';

const s = { kind: 'bar', x: 'region', y: ['total', 'qty'], agg: 'sum', sort: 'value', split: null };

test('columns are found by name, whatever their order', () => {
  const r = resolveChartSettings(s, ['qty', 'total', 'region'], [0, 1]);
  assert.deepEqual(r, { kind: 'bar', x: 2, y: [1, 0], agg: 'sum', sort: 'value', split: null });
});

test('a missing X or value column falls back to defaults (null)', () => {
  assert.equal(resolveChartSettings(s, ['total', 'qty'], [0, 1]), null);
  assert.equal(resolveChartSettings(s, ['region', 'other'], [1]), null);
});

test('missing value columns are dropped, non-numeric ones too', () => {
  const r = resolveChartSettings(s, ['region', 'total', 'qty'], [1]);
  assert.deepEqual(r?.y, [1]);
});

test('a split keeps one measure; a missing split is dropped', () => {
  const r = resolveChartSettings({ ...s, split: 'kind' }, ['region', 'total', 'qty', 'kind'], [1, 2]);
  assert.equal(r?.split, 3);
  assert.deepEqual(r?.y, [1]);
  assert.equal(resolveChartSettings({ ...s, split: 'gone' }, ['region', 'total', 'qty'], [1, 2])?.split, null);
});

test('unknown kind is rejected; unknown agg and sort reset', () => {
  assert.equal(resolveChartSettings({ ...s, kind: 'radar' }, ['region', 'total'], [1]), null);
  const r = resolveChartSettings({ ...s, agg: 'median', sort: 'x' }, ['region', 'total'], [1]);
  assert.equal(r?.agg, 'none');
  assert.equal(r?.sort, 'none');
});

test('points are capped and NaN becomes null', () => {
  const p = chartPoints(['a', 'b', 'c'], [[1, NaN, 3]], ['total'], 2);
  assert.deepEqual(p, { categories: ['a', 'b'], series: [{ name: 'total', values: [1, null] }] });
});

test('a cell chart round-trips; malformed input reads as nothing', () => {
  const pts = { categories: ['a'], series: [{ name: 'total', values: [1] }] };
  const json = cellChart('chart', s, pts);
  assert.equal(json.type, 'bar');
  assert.deepEqual(readCellChart(JSON.parse(JSON.stringify(json))), { view: 'chart', settings: s });
  assert.deepEqual(readCellChart(null), { view: null, settings: null });
  assert.deepEqual(readCellChart({ view: 'table' }), { view: 'table', settings: null });
  // The spec's example (no agg/sort/split): defaults for those.
  assert.deepEqual(readCellChart({ type: 'bar', x: 'region', y: ['total'] }).settings,
    { kind: 'bar', x: 'region', y: ['total'], agg: 'none', sort: 'none', split: null });
});

test('sameJson ignores key order', () => {
  assert.ok(sameJson({ b: 1, a: { d: [1, { y: 2, x: 1 }], c: null } }, { a: { c: null, d: [1, { x: 1, y: 2 }] }, b: 1 }));
  assert.ok(!sameJson({ a: [1, 2] }, { a: [2, 1] }));
});
