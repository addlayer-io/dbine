// Tests for what a query compare's key learns from the queries
// (queryKey.ts): the one table a plain SELECT reads and whether a query
// groups. Run: `node --test src/composables/queryKey.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';

// queryKey.ts imports its siblings without an extension (as Vite does).
register('data:text/javascript,' + encodeURIComponent(`
export async function resolve(spec, ctx, next) {
  if (spec.startsWith('.') && !/\\.[cm]?[jt]s$/.test(spec)) return next(spec + '.ts', ctx);
  return next(spec, ctx);
}`));
const { singleTable, groups } = await import('./queryKey.ts');

const one = (q, dialect = 'mssql') => singleTable(q, 'sql', dialect);

test('the one table a plain SELECT reads', () => {
  // The DBA's query on Databricks.
  assert.deepEqual(one("select * from sise.simba_di\nwhere fec_emi >='20260101'", 'databricks'), ['sise', 'simba_di']);
  assert.deepEqual(one('SELECT TOP 10 a, b FROM dbo.Clientes ORDER BY a'), ['dbo', 'Clientes']);
  assert.deepEqual(one('select * from prod_silver.sise.simba_di;'), ['prod_silver', 'sise', 'simba_di']);
  assert.deepEqual(one('select * from [dbo].[Mis Clientes] c where c.id > 3'), ['dbo', 'Mis Clientes']);
  assert.deepEqual(one('select * from "public"."order""s" as o', 'postgres'), ['public', 'order"s']);
  assert.deepEqual(one('select * from `db`.`t` limit 5', 'mysql'), ['db', 't']);
  assert.deepEqual(one('select * from t with (nolock) where x = 1'), ['t']);
  // Comments and strings don't count.
  assert.deepEqual(one("-- from other join x\nselect * from t where note = 'a join b' /* union */"), ['t']);
});

test('anything more than one table\'s rows is not one table', () => {
  for (const q of [
    'select * from a join b on a.id = b.id',
    'select * from a, b',
    'select * from a union select * from b',
    'select region, sum(x) from t group by region',
    'select * from (select * from t) s',
    'with c as (select 1) select * from c',
    'select * from t where id in (select id from u)',
    'select * from dbo.fn(3)',
    'select 1',
    'update t set x = 1',
    'select * from a.b.c.d',
  ]) assert.equal(one(q), null, q);
  // Not SQL.
  assert.equal(singleTable('db.t.find({})', 'json', ''), null);
});

test('a query that groups', () => {
  assert.equal(groups('select r, sum(x) from t group by r', 'sql', 'postgres'), true);
  assert.equal(groups('select r, sum(x) from t GROUP\n  BY r', 'sql', 'postgres'), true);
  assert.equal(groups("select * from t where note = 'group by'", 'sql', 'postgres'), false);
  assert.equal(groups('select * from t -- group by x', 'sql', 'postgres'), false);
  assert.equal(groups('EVALUATE SUMMARIZECOLUMNS(...)', 'dax', ''), false);
});
