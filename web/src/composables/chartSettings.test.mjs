// Tests for the chart settings by column name (chartSettings.ts). Run:
// `node --test src/composables/chartSettings.test.mjs` from web/ (Node 23.6+
// strips the TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  MAX_LABEL_CHARS, chartPoints, isRawChart, readCellCharts, resolveChartSettings, sameJson, storableCharts, storablePoints, writeCellCharts,
} from './chartSettings.ts';

const s = { kind: 'bar', x: 'region', y: ['total', 'qty'], agg: 'sum', sort: 'value', split: null };

test('columns are found by name, whatever their order', () => {
  const r = resolveChartSettings(s, ['qty', 'total', 'region'], [0, 1]);
  assert.deepEqual(r, { kind: 'bar', x: 2, y: [1, 0], agg: 'sum', sort: 'value', split: null, bins: 0 });
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

test('cell charts round-trip in the new format', () => {
  const pts = { categories: ['a'], series: [{ name: 'total', values: [1] }] };
  const h = { kind: 'histogram', x: null, y: ['total'], agg: 'none', sort: 'none', split: null, bins: 20 };
  const cc = {
    view: 'chart', active: 1, map: { column: null, lat: 'lat', lon: 'lon' },
    charts: [{ title: 'Ventas', settings: s, points: pts, aggregate: null }, { title: '', settings: h, points: pts, aggregate: null }],
  };
  const json = writeCellCharts(cc);
  assert.equal(json.active, 1);
  assert.equal(json.charts[0].type, 'bar');
  assert.equal(json.charts[1].bins, 20);
  assert.deepEqual(readCellCharts(JSON.parse(JSON.stringify(json))), cc);
  assert.deepEqual(readCellCharts(null), { view: null, active: 0, charts: [], map: null });
  assert.deepEqual(readCellCharts({ view: 'table' }), { view: 'table', active: 0, charts: [], map: null });
  // An out-of-range active, a malformed chart: the first, no settings.
  const odd = readCellCharts({ view: 'chart', active: 7, charts: [{ type: 'radar', y: [] }, 3] });
  assert.equal(odd.active, 0);
  assert.deepEqual(odd.charts.map((c) => c.settings), [null, null]);
  // Whole-result info only with `whole`.
  const w = readCellCharts({ charts: [{ ...writeCellCharts(cc).charts[0], whole: true, aggregate: { method: 'server', rows_scanned: 10, rows_capped: false, groups_truncated: true } }] });
  assert.deepEqual(w.charts[0].aggregate, { method: 'server', rows_scanned: 10, rows_capped: false, groups_truncated: true });
  assert.equal(readCellCharts({ charts: [{ ...writeCellCharts(cc).charts[0], aggregate: { method: 'server' } }] }).charts[0].aggregate, null);
});

test('the old one-chart format reads as a list of one', () => {
  const old = { view: 'chart', type: 'bar', x: 'region', y: ['total'], agg: 'sum', sort: 'value', split: null, points: { categories: ['a'], series: [{ name: 'total', values: [1, 'x'] }] } };
  const r = readCellCharts(old);
  assert.equal(r.view, 'chart');
  assert.equal(r.charts.length, 1);
  assert.deepEqual(r.charts[0].settings, { kind: 'bar', x: 'region', y: ['total'], agg: 'sum', sort: 'value', split: null });
  assert.deepEqual(r.charts[0].points, { categories: ['a'], series: [{ name: 'total', values: [1, null] }] });
  // The spec's example (no agg/sort/split): defaults for those.
  assert.deepEqual(readCellCharts({ type: 'bar', x: 'region', y: ['total'] }).charts[0].settings,
    { kind: 'bar', x: 'region', y: ['total'], agg: 'none', sort: 'none', split: null });
  // Written back, it's the new format.
  const json = writeCellCharts(r);
  assert.ok(Array.isArray(json.charts) && json.type === undefined);
});

test('the new kinds resolve with what they need', () => {
  const names = ['region', 'kind', 'total', 'qty'];
  const h = resolveChartSettings({ kind: 'histogram', x: null, y: ['total', 'qty'], agg: 'none', sort: 'none', split: null, bins: 10 }, names, [2, 3]);
  assert.deepEqual(h, { kind: 'histogram', x: -1, y: [2], agg: 'none', sort: 'none', split: null, bins: 10 });
  // A heatmap's Y category is its split; "none" becomes a sum.
  const hm = resolveChartSettings({ kind: 'heatmap', x: 'region', y: ['total'], agg: 'none', sort: 'none', split: 'kind' }, names, [2, 3]);
  assert.deepEqual([hm.x, hm.split, hm.agg], [0, 1, 'sum']);
  // A box plot's category is optional; a KPI has none.
  assert.equal(resolveChartSettings({ kind: 'box', x: null, y: ['total'], agg: 'none', sort: 'none', split: null }, names, [2]).x, -1);
  assert.equal(resolveChartSettings({ kind: 'kpi', x: 'region', y: ['total'], agg: 'avg', sort: 'none', split: null }, names, [2]).x, -1);
  // Combo keeps several measures and no split.
  const c = resolveChartSettings({ kind: 'combo', x: 'region', y: ['total', 'qty'], agg: 'sum', sort: 'none', split: 'kind' }, names, [2, 3]);
  assert.deepEqual([c.y, c.split], [[2, 3], null]);
  // A bad bin count: automatic.
  assert.equal(resolveChartSettings({ kind: 'histogram', x: null, y: ['total'], agg: 'none', sort: 'none', split: null, bins: -3 }, names, [2]).bins, 0);
});

test('histograms, box plots and KPIs are summaries; scatters and per-row values are raw', () => {
  assert.equal(isRawChart({ kind: 'histogram', agg: 'none' }), false);
  assert.equal(isRawChart({ kind: 'box', agg: 'none' }), false);
  assert.equal(isRawChart({ kind: 'kpi', agg: 'none' }), false);
  assert.equal(isRawChart({ kind: 'heatmap', agg: 'none' }), true);
  assert.equal(isRawChart({ kind: 'combo', agg: 'sum' }), false);
  assert.equal(isRawChart({ kind: 'scatter', agg: 'sum' }), true);
});

test('all the charts of a cell share its byte cap, in order', () => {
  const pts = (n) => ({ categories: Array.from({ length: n }, (_, i) => `c${i}`), series: [{ name: 'v', values: Array.from({ length: n }, (_, i) => i) }] });
  const one = JSON.stringify(pts(50)).length;
  const sc = { ...s, kind: 'scatter' };
  const list = [
    { title: 'a', settings: s, points: pts(50), aggregate: null },
    { title: 'raw', settings: sc, points: pts(50), aggregate: null },
    { title: 'b', settings: s, points: pts(50), aggregate: { method: 'client', rows_scanned: 5, rows_capped: false, groups_truncated: false } },
    { title: 'c', settings: s, points: pts(2), aggregate: null },
  ];
  const kept = storableCharts(list, 100, one + 90);
  assert.ok(kept[0].points);
  assert.equal(kept[1].points, null, 'a scatter keeps no points');
  assert.equal(kept[2].points, null, "the second doesn't fit");
  assert.equal(kept[2].aggregate, null);
  assert.ok(kept[3].points, 'a small one after it still fits');
  // Rows per cell apply to each chart.
  assert.equal(storableCharts(list, 10, 1e6)[0].points.categories.length, 10);
});

test('sameJson ignores key order', () => {
  assert.ok(sameJson({ b: 1, a: { d: [1, { y: 2, x: 1 }], c: null } }, { a: { c: null, d: [1, { x: 1, y: 2 }] }, b: 1 }));
  assert.ok(!sameJson({ a: [1, 2] }, { a: [2, 1] }));
});

test('a cell keeps only aggregated points, capped to its rows, labels cut', () => {
  const pts = { categories: ['a', 'b', 'c', 'x'.repeat(MAX_LABEL_CHARS + 10)], series: [{ name: 'total', values: [1, 2, 3, 4] }] };
  const kept = storablePoints({ ...s, agg: 'sum' }, pts, 3);
  assert.deepEqual(kept, { categories: ['a', 'b', 'c'], series: [{ name: 'total', values: [1, 2, 3] }] });
  const long = storablePoints({ ...s, agg: 'count' }, pts, 10);
  assert.equal(long.categories[3].length, MAX_LABEL_CHARS + 1);
  assert.ok(long.categories[3].endsWith('…'));
  // Raw rows: no aggregation, or a scatter.
  assert.equal(storablePoints({ ...s, agg: 'none' }, pts, 10), null);
  assert.equal(storablePoints({ ...s, kind: 'scatter' }, pts, 10), null);
  assert.equal(storablePoints(null, pts, 10), null);
  assert.equal(storablePoints(s, null, 10), null);
});
