// Tests for the query compare's column pairing and key suggestion
// (queryCompare.ts). Run: `node --test src/composables/queryCompare.test.mjs`
// from web/ (Node 23.6+ strips the TypeScript types itself). Not part of the
// app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  DEFAULT_FORM, bareName, compareWithPicks, comparedPairs, mergePairs, pairColumns, pairRows, queryOptions, sideSignature, suggestKey, unpairedRight,
} from './queryCompare.ts';

const col = (name, type_name = '') => ({ name, type_name });

test('bare names match across engines', () => {
  assert.equal(bareName('Date[Year]'), 'year');
  assert.equal(bareName("'Date'[Year]"), 'year');
  assert.equal(bareName('[Total Sales]'), 'totalsales');
  assert.equal(bareName('total_sales'), 'totalsales');
  assert.equal(bareName('dbo.Ventas.Año'), 'ano');
  assert.equal(bareName('"Región"'), 'region');
});

test('columns pair by name, then by bare name, then in order', () => {
  // A DAX result against a SQL one.
  assert.deepEqual(
    pairColumns(['Date[Year]', 'Geo[Region]', '[Ventas]'], ['year', 'region', 'total']),
    ['year', 'region', 'total'],
  );
  // Exact names first, whatever the order; case only when needed.
  assert.deepEqual(pairColumns(['ID', 'id'], ['id', 'ID']), ['ID', 'id']);
  assert.deepEqual(pairColumns(['Amount'], ['other', 'amount']), ['amount']);
  // Left alone when the counts of what's left differ.
  assert.deepEqual(pairColumns(['a', 'b', 'c'], ['a', 'x']), ['a', '', '']);
  assert.deepEqual(pairColumns(['a', 'b'], ['a']), ['a', '']);
  // A right column pairs once.
  assert.deepEqual(pairColumns(['Year', 'year'], ['YEAR']), ['YEAR', '']);
});

test('the key: a DAX side\'s group-by columns', () => {
  const cols = [
    { left: 'Date[Year]', right: 'year', leftType: 'Int64', rightType: 'int' },
    { left: 'Geo[Region]', right: 'region', leftType: 'String', rightType: 'nvarchar' },
    { left: '[Clientes]', right: 'clientes', leftType: 'Int64', rightType: 'int' },
  ];
  assert.deepEqual(suggestKey(cols), [0, 1]);
  // On the right side too.
  const flipped = cols.map((c) => ({ left: c.right, right: c.left, leftType: c.rightType, rightType: c.leftType }));
  assert.deepEqual(suggestKey(flipped), [0, 1]);
});

test('the key: an id-like column alone, else what describes', () => {
  const t = (left, leftType = '', leftSample = undefined) => ({ left, right: left, leftType, rightType: '', leftSample });
  assert.deepEqual(suggestKey([t('name', 'text'), t('customer_id', 'int'), t('total', 'numeric')]), [1]);
  assert.deepEqual(suggestKey([t('CustomerKey', 'int'), t('name', 'text')]), [0]);
  // `valid` isn't an id.
  assert.deepEqual(suggestKey([t('valid', 'boolean'), t('region', 'text'), t('amount', 'decimal(12,2)')]), [0, 1]);
  // Measures by type, or by a fractional value when there's no type.
  assert.deepEqual(suggestKey([t('year', 'int'), t('region', 'varchar'), t('ventas', 'double')]), [0, 1]);
  assert.deepEqual(suggestKey([t('year', '', 2024), t('region', '', 'Norte'), t('ventas', '', 12.5)]), [0, 1]);
  assert.deepEqual(suggestKey([t('year', '', 2024), t('ventas', '', '12.50')]), [0]);
  // Nothing looks like a measure: all but the last (a count, usually).
  assert.deepEqual(suggestKey([t('year', 'int'), t('region', 'text'), t('orders', 'bigint')]), [0, 1]);
  assert.deepEqual(suggestKey([t('only', 'text')]), [0]);
  // Everything looks like a measure: the first column.
  assert.deepEqual(suggestKey([t('a', 'float'), t('b', 'float')]), [0]);
  assert.deepEqual(suggestKey([]), []);
});

test('the column table marks the key and leaves unpaired columns out', () => {
  const rows = pairRows(
    [col('Date[Year]', 'Int64'), col('Geo[Region]', 'String'), col('[Ventas]', 'Double'), col('[Extra]', '')],
    [col('year', 'int'), col('region', 'nvarchar'), col('ventas', 'decimal')],
    [2024, 'Norte', 10.5, 1],
    [2024, 'Norte', '10.50'],
  );
  assert.deepEqual(rows, [
    { left: 'Date[Year]', right: 'year', key: true },
    { left: 'Geo[Region]', right: 'region', key: true },
    { left: '[Ventas]', right: 'ventas', key: false },
    { left: '[Extra]', right: '', key: false },
  ]);
  assert.deepEqual(comparedPairs(rows), [
    { left: 'Date[Year]', right: 'year', key: true },
    { left: 'Geo[Region]', right: 'region', key: true },
    { left: '[Ventas]', right: 'ventas', key: false },
  ]);
  assert.deepEqual(unpairedRight(rows, [col('year'), col('region'), col('ventas'), col('notes')]), ['notes']);
});

test('reading the columns again keeps the user\'s pairs and key', () => {
  const before = [
    { left: 'id', right: 'ID', key: true },
    { left: 'name', right: '', key: false },
    { left: 'total', right: 'amount', key: false },
  ];
  const fresh = [
    { left: 'id', right: 'id', key: true },
    { left: 'name', right: 'name', key: false },
    { left: 'total', right: 'total', key: false },
    { left: 'added', right: 'added', key: false },
  ];
  const right = [col('ID'), col('id'), col('name'), col('total'), col('added')];
  assert.deepEqual(mergePairs(before, fresh, right), [
    { left: 'id', right: 'ID', key: true },
    // Left without a pair on purpose.
    { left: 'name', right: '', key: false },
    // Its pair is gone: as suggested.
    { left: 'total', right: 'total', key: false },
    { left: 'added', right: 'added', key: false },
  ]);
  assert.deepEqual(mergePairs([], fresh, right), fresh);
  // A kept pair takes a right column another row was offered.
  const clash = mergePairs([{ left: 'a', right: 'x', key: true }], [{ left: 'a', right: 'a', key: false }, { left: 'b', right: 'x', key: false }], [col('x'), col('a')]);
  assert.deepEqual(clash, [{ left: 'a', right: 'x', key: true }, { left: 'b', right: '', key: false }]);
  // Also when the suggestion comes first: the user's pair and key stay.
  const first = mergePairs(
    [{ left: 'a', right: 'x', key: true }],
    [{ left: 'b', right: 'x', key: true }, { left: 'a', right: 'y', key: false }],
    [col('x'), col('y')],
  );
  assert.deepEqual(first, [{ left: 'b', right: '', key: false }, { left: 'a', right: 'x', key: true }]);
  // Two kept rows the user paired with one right column both stay.
  const shared = mergePairs(
    [{ left: 'a', right: 'x', key: true }, { left: 'b', right: 'x', key: false }],
    [{ left: 'a', right: 'a', key: false }, { left: 'b', right: 'b', key: false }],
    [col('x'), col('a'), col('b')],
  );
  assert.deepEqual(shared, [{ left: 'a', right: 'x', key: true }, { left: 'b', right: 'x', key: false }]);
});

test('"Comparar con…" puts the query on the left', () => {
  const p = compareWithPicks('c1', 'db', 'EVALUATE T');
  assert.equal(p.mode, 'queries');
  assert.deepEqual(p.queries.left, { connectionId: 'c1', database: 'db', query: 'EVALUATE T' });
  assert.equal(p.queries.right.query, '');
});

test('the options as the backend takes them', () => {
  assert.deepEqual(queryOptions(DEFAULT_FORM), { absolute: 0, relative: 0, null_as_blank: false, ignore_case: false, trim: false });
  assert.deepEqual(
    queryOptions({ absolute: 0.01, relativePct: 0.5, nullAsBlank: true, ignoreCase: true, trim: false }),
    { absolute: 0.01, relative: 0.005, null_as_blank: true, ignore_case: true, trim: false },
  );
  // Blank or negative tolerances are none.
  assert.deepEqual(queryOptions({ ...DEFAULT_FORM, absolute: -1, relativePct: Number.NaN }).absolute, 0);
  assert.equal(queryOptions({ ...DEFAULT_FORM, relativePct: Number.NaN }).relative, 0);
});

test('a side needs its columns read again when its query changes', () => {
  const s = { connectionId: 'c', database: 'd', query: 'SELECT 1' };
  assert.equal(sideSignature(s), sideSignature({ ...s, query: '  SELECT 1\n' }));
  assert.notEqual(sideSignature(s), sideSignature({ ...s, query: 'SELECT 2' }));
  assert.notEqual(sideSignature(s), sideSignature({ ...s, database: 'e' }));
});
