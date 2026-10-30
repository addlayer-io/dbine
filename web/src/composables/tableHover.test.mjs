// Tests for the table under the pointer (tableHover.ts): the name the
// editor's column popup resolves. Run: `node --test src/composables/tableHover.test.mjs`
// from web/ (Node 23.6+ strips the TypeScript types itself). Not part of the
// app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';

// tableHover.ts imports its siblings without an extension (as Vite does).
register('data:text/javascript,' + encodeURIComponent(`
export async function resolve(spec, ctx, next) {
  if (spec.startsWith('.') && !/\\.[cm]?[jt]s$/.test(spec)) return next(spec + '.ts', ctx);
  return next(spec, ctx);
}`));
const { tableAt } = await import('./tableHover.ts');

const KINDS = [
  { id: 'table', label: 'Tables', has_columns: true, browsable: true, has_definition: false },
  { id: 'view', label: 'Views', has_columns: true, browsable: true, has_definition: true },
  { id: 'procedure', label: 'Procedures', has_columns: false, browsable: false, has_definition: true },
];
const obj = (name, schema = null, kind = 'table') => ({ kind, schema, name, parent: null });
const IDX = {
  language: 'sql', dialect: 'postgres', database: 'shop',
  objects: [obj('orders', 'public'), obj('customers', 'public'), obj('Order Items', 'public'), obj('orders', 'audit'), obj('Mixed', 'public'), obj('mixed', 'public'), obj('refresh', 'public', 'procedure')],
  kinds: KINDS,
  databases: { shop: null, crm: [obj('leads', 'public')], archive: null },
};

/** `|` marks the pointer. */
function at(text, idx = IDX) {
  const pos = text.indexOf('|');
  const doc = text.slice(0, pos) + text.slice(pos + 1);
  const r = tableAt(idx, doc, pos);
  return r && { database: r.database, schema: r.object?.schema ?? null, name: r.object?.name ?? null, path: r.path, word: doc.slice(r.from, r.to) };
}

test('a plain name', () => {
  assert.deepEqual(at('SELECT * FROM ord|ers'), { database: 'shop', schema: 'public', name: 'orders', path: ['orders'], word: 'orders' });
});

test('schema-qualified: the schema picks the table', () => {
  assert.equal(at('SELECT * FROM audit.ord|ers').schema, 'audit');
  assert.equal(at('SELECT * FROM public.ord|ers').schema, 'public');
});

test('the schema part itself is not a table', () => {
  assert.equal(at('SELECT * FROM aud|it.orders'), null);
});

test('quoted identifiers, with the exact spelling first', () => {
  assert.equal(at('SELECT * FROM "Order |Items"').name, 'Order Items');
  assert.equal(at('SELECT * FROM public."Order |Items"').name, 'Order Items');
  assert.equal(at('SELECT * FROM "Mi|xed"').name, 'Mixed');
  assert.equal(at('SELECT * FROM mi|xed').name, 'mixed');
  assert.equal(at('SELECT * FROM [Order |Items]', { ...IDX, dialect: 'mssql' }).name, 'Order Items');
  assert.equal(at('SELECT * FROM `Order |Items`', { ...IDX, dialect: 'mysql' }).name, 'Order Items');
});

test('case-insensitive match when the spelling differs', () => {
  assert.equal(at('SELECT * FROM ORD|ERS').name, 'orders');
});

test('cross-database: db.schema.table and db.table', () => {
  assert.deepEqual(at('SELECT * FROM crm.public.le|ads'), { database: 'crm', schema: 'public', name: 'leads', path: ['public', 'leads'], word: 'leads' });
  assert.equal(at('SELECT * FROM crm.le|ads').name, 'leads');
});

test('cross-database: a database not loaded yet asks for its objects', () => {
  assert.deepEqual(at('SELECT * FROM archive.public.ol|d'), { database: 'archive', schema: null, name: null, path: ['public', 'old'], word: 'old' });
});

test('the tab\'s own database spelled out', () => {
  assert.equal(at('SELECT * FROM shop.public.ord|ers').name, 'orders');
  assert.equal(at('SELECT * FROM shop.ord|ers').database, 'shop');
});

test('no other databases: db.table is not resolved elsewhere', () => {
  assert.equal(at('SELECT * FROM crm.le|ads', { ...IDX, databases: undefined }), null);
});

test('an alias stands for its table, not for a table with its name', () => {
  // `customers` here is an alias of orders.
  assert.equal(at('SELECT custo|mers.id FROM orders customers').name, 'orders');
  assert.equal(at('SELECT * FROM orders custo|mers').name, 'orders');
  assert.equal(at('SELECT o|.id FROM audit.orders o').schema, 'audit');
});

test('a column is not a table', () => {
  assert.equal(at('SELECT o.to|tal FROM orders o'), null);
  assert.equal(at('SELECT orders.cust|omers FROM orders'), null);
});

test('keywords are not resolved', () => {
  const idx = { ...IDX, objects: [...IDX.objects, obj('select', 'public'), obj('from', 'public')] };
  assert.equal(at('SEL|ECT * FROM orders', idx), null);
  assert.equal(at('SELECT * FR|OM orders', idx), null);
  // Quoted, it's a name.
  assert.equal(at('SELECT * FROM "sel|ect"', idx).name, 'select');
});

test('objects without columns, strings and comments are not resolved', () => {
  assert.equal(at('CALL refr|esh()'), null);
  assert.equal(at("SELECT 'ord|ers' FROM customers"), null);
  assert.equal(at('-- ord|ers\nSELECT 1'), null);
  assert.equal(at('SELECT * FROM nowh|ere'), null);
});

test('other languages: the word the explorer knows', () => {
  const mongo = { ...IDX, language: 'json', dialect: '', objects: [obj('pedidos', null, 'table')] };
  assert.equal(at('db.pedi|dos.find({})', mongo).name, 'pedidos');
});
