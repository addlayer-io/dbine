// Tests for "Calcular sobre todo el resultado" (chartWhole.ts): the request
// each chart sends to chart_aggregate and its answer as the chart's shape.
// Run: `node --test src/composables/chartWhole.test.mjs` from web/ (Node 23.6+
// strips the TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';

// chartWhole.ts imports its siblings without an extension (as Vite does):
// resolve them to the .ts files.
register('data:text/javascript,' + encodeURIComponent(`
export async function resolve(spec, ctx, next) {
  if (spec.startsWith('.') && !/\\.[cm]?[jt]s$/.test(spec)) return next(spec + '.ts', ctx);
  return next(spec, ctx);
}`));
const { aggregateRequest, shapeFromAggregate, wholeChartByDefault, WHOLE_MAX_GROUPS } = await import('./chartWhole.ts');

const L = {
  nul: '(nulo)',
  others: 'Otros',
  series: (agg, column) => (agg === 'none' ? column : `${agg}(${column})`),
  bin: (a, b) => `${a}-${b}`,
};
const base = { kind: 'bar', x: 'region', y: ['total', 'qty'], agg: 'sum', sort: 'none', split: null };
const answer = (o) => ({ categories: [], series: [], boxes: null, histogram: null, method: 'server', rows_scanned: null, rows_capped: false, groups_truncated: false, ...o });

test('group charts ask for one measure per value column; count is COUNT(*)', () => {
  assert.deepEqual(aggregateRequest(base), {
    kind: 'group', x: 'region', split: null, max_groups: WHOLE_MAX_GROUPS,
    measures: [{ column: 'total', func: 'sum' }, { column: 'qty', func: 'sum' }],
  });
  assert.deepEqual(aggregateRequest({ ...base, agg: 'count' }).measures, [{ column: null, func: 'count' }, { column: null, func: 'count' }]);
  // A split (or a heatmap's Y) keeps one measure.
  const sp = aggregateRequest({ ...base, kind: 'stacked', split: 'kind' });
  assert.deepEqual([sp.split, sp.measures.length], ['kind', 1]);
  assert.equal(aggregateRequest({ ...base, kind: 'heatmap', split: 'kind' }).split, 'kind');
  assert.equal(aggregateRequest({ ...base, kind: 'heatmap', split: null }), null);
  // Combo: no split, every measure.
  assert.equal(aggregateRequest({ ...base, kind: 'combo', split: 'kind' }).split, null);
});

test('raw charts are not computed over the whole result', () => {
  assert.equal(aggregateRequest({ ...base, agg: 'none' }), null);
  assert.equal(aggregateRequest({ ...base, kind: 'scatter' }), null);
  assert.equal(aggregateRequest({ ...base, y: [] }), null);
});

test('a KPI asks for a total without X: its aggregate and the rows with a value', () => {
  for (const agg of ['sum', 'avg', 'min', 'max']) {
    assert.deepEqual(aggregateRequest({ ...base, kind: 'kpi', x: null, agg }), {
      kind: 'group', x: null, split: null,
      measures: [{ column: 'total', func: agg }, { column: 'total', func: 'count' }],
    });
  }
  assert.deepEqual(aggregateRequest({ ...base, kind: 'kpi', x: null, agg: 'count' }).measures[0], { column: null, func: 'count' });
});

test('histograms and box plots ask for themselves', () => {
  assert.deepEqual(aggregateRequest({ ...base, kind: 'histogram', x: null, agg: 'none' }), { kind: 'histogram', column: 'total' });
  assert.deepEqual(aggregateRequest({ ...base, kind: 'histogram', x: null, agg: 'none', bins: 300 }), { kind: 'histogram', column: 'total', bins: 200 });
  assert.deepEqual(aggregateRequest({ ...base, kind: 'box', agg: 'none' }), { kind: 'box', column: 'total', by: 'region', max_groups: 40 });
});

test('a group answer becomes series by measure, NULL labelled, sorted on request', () => {
  const a = answer({ categories: ['n', null, 's'], series: [{ measure: 0, split: null, values: [1, 5, null] }, { measure: 1, split: null, values: [2, 3, 4] }] });
  const sh = shapeFromAggregate({ ...base, sort: 'value' }, a, L);
  assert.deepEqual(sh.cats, ['(nulo)', 'n', 's']);
  assert.deepEqual(sh.names, ['sum(total)', 'sum(qty)']);
  assert.deepEqual(sh.series[1], [3, 2, 4]);
  assert.ok(Number.isNaN(sh.series[0][2]));
});

test('split answers fold past seven series where the aggregate allows', () => {
  const series = Array.from({ length: 9 }, (_, j) => ({ measure: 0, split: `s${j}`, values: [j + 1, 1] }));
  const a = answer({ categories: ['a', 'b'], series });
  const sum = shapeFromAggregate({ ...base, kind: 'stacked', split: 'kind' }, a, L);
  assert.equal(sum.names.length, 8);
  assert.equal(sum.names.at(-1), 'Otros');
  assert.deepEqual(sum.series.at(-1), [1 + 2, 2], 'the two smallest, added');
  const avg = shapeFromAggregate({ ...base, kind: 'stacked', split: 'kind', agg: 'avg' }, a, L);
  assert.equal(avg.names.length, 7, 'an average is not folded');
  const hm = shapeFromAggregate({ ...base, kind: 'heatmap', split: 'kind' }, a, L);
  assert.equal(hm.names.length, 9, 'a heatmap keeps its Y categories');
});

test('histogram, box and KPI answers', () => {
  const h = shapeFromAggregate({ ...base, kind: 'histogram' }, answer({ histogram: { edges: [0, 5, 10], counts: [3, 4], nulls: 0, non_numeric: 0 } }), L);
  assert.deepEqual([h.cats, h.series, h.names], [['0-5', '5-10'], [[3, 4]], ['total']]);
  const boxes = [
    { category: 'b', count: 3, min: 1, q1: 2, median: 9, q3: 10, max: 30, whisker_low: 1, whisker_high: 11, outlier_count: 1, outliers: [30] },
    { category: null, count: 2, min: 0, q1: 1, median: 2, q3: 3, max: 4, whisker_low: 0, whisker_high: 4, outlier_count: 0, outliers: [] },
  ];
  // Tukey's whiskers (not min / max) and the outliers, re-indexed with the order.
  const b = shapeFromAggregate({ ...base, kind: 'box', sort: 'label' }, answer({ boxes }), L);
  assert.deepEqual(b.cats, ['(nulo)', 'b']);
  assert.deepEqual(b.series.map((s) => s[1]), [1, 2, 9, 10, 11]);
  assert.deepEqual(b.outliers, [[1, 30]]);
  const byValue = shapeFromAggregate({ ...base, kind: 'box', sort: 'value' }, answer({ boxes }), L);
  assert.deepEqual([byValue.cats, byValue.outliers], [['b', '(nulo)'], [[0, 30]]]);
  const one = shapeFromAggregate({ ...base, kind: 'box', x: null }, answer({ boxes: [{ ...boxes[0], category: null }] }), L);
  assert.deepEqual(one.cats, ['total']);
  // A KPI: measure 0 is the value, measure 1 the rows with a value.
  const total = (v, n) => answer({ categories: [null], series: [{ measure: 0, split: null, values: [v] }, { measure: 1, split: null, values: [n] }] });
  const k = shapeFromAggregate({ ...base, kind: 'kpi', agg: 'count' }, total(120, 100), L);
  assert.deepEqual([k.series[0][0], k.count, k.names], [120, 120, ['count(total)']]);
  const avg = shapeFromAggregate({ ...base, kind: 'kpi', agg: 'avg' }, total(12.5, 100), L);
  assert.deepEqual([avg.series[0][0], avg.count, avg.names], [12.5, 100, ['avg(total)']]);
  const none = shapeFromAggregate({ ...base, kind: 'kpi', agg: 'sum' }, total(null, 0), L);
  assert.ok(Number.isNaN(none.series[0][0]));
  assert.equal(none.count, 0);
});

test('a DataFrame SQL cell cut for display charts its whole result by default; a connection does not', () => {
  const df = { connectionId: '', database: '', sql: 'SELECT 1', notebookTab: 'tab-1' };
  const conn = { connectionId: 'c', database: 'db', sql: 'SELECT 1' };
  const rows = Array.from({ length: 5 }, (_, i) => [i]);
  const cut = { rows, total_rows: 1_000_000, truncated: true };
  // Shown 5 of a million: on.
  assert.equal(wholeChartByDefault(df, cut), true);
  // Fewer shown than returned, even without the flag: on.
  assert.equal(wholeChartByDefault(df, { rows, total_rows: 6, truncated: false }), true);
  // Every row shown: off (the loaded rows are the whole result).
  assert.equal(wholeChartByDefault(df, { rows, total_rows: 5, truncated: false }), false);
  // A connection's query, cut or not: off (it would run again on the server).
  assert.equal(wholeChartByDefault(conn, cut), false);
  // No run to aggregate (a stored preview), or no result.
  assert.equal(wholeChartByDefault(null, cut), false);
  assert.equal(wholeChartByDefault(df, null), false);
});
