// Tests for the SQL editor's completion (sqlCompletion.ts). Run:
// `node --test src/composables/sqlCompletion.test.mjs` from web/ (Node 23.6+
// strips the TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { EditorState } from '@codemirror/state';
import { CompletionContext } from '@codemirror/autocomplete';
import { StandardSQL } from '@codemirror/lang-sql';
import {
  FILE_READERS, fileCompletion, isSensitivePath, isTableSlot, nameCompletion, nameTree, readerCompletion, readerPathCompletion, readerSnippet,
} from './sqlCompletion.ts';

/** `|` marks the cursor. */
function ctxOf(text, explicit = true) {
  const pos = text.indexOf('|');
  const doc = text.slice(0, pos) + text.slice(pos + 1);
  const state = EditorState.create({ doc, extensions: [StandardSQL.language] });
  return new CompletionContext(state, pos, explicit);
}
const quote = (l) => `"${l}"`;
const labels = (r) => (r ? r.options.map((o) => o.label) : []);

// A results cell's schema: the cells above, `pendiente` never run (no columns).
const CELLS = { ventas_pg: ['id', 'cliente', 'total'], clientes_mssql: ['id', 'nombre'], pendiente: [] };
const tree = nameTree(CELLS);

test('the cells above are offered after FROM, with no aliases or columns in the way', () => {
  const r = nameCompletion(ctxOf('SELECT * FROM |'), tree, quote).result;
  assert.deepEqual(labels(r).sort(), ['clientes_mssql', 'pendiente', 'ventas_pg']);
});

test('…and after every kind of JOIN, on the same line or the next', () => {
  for (const join of ['JOIN', 'INNER JOIN', 'LEFT JOIN', 'LEFT OUTER JOIN', 'CROSS JOIN', 'FULL JOIN']) {
    const r = nameCompletion(ctxOf(`SELECT * FROM ventas_pg v\n${join} |`), tree, quote).result;
    assert.deepEqual(labels(r).sort(), ['clientes_mssql', 'pendiente', 'ventas_pg'], join);
  }
  const r = nameCompletion(ctxOf('SELECT *\nFROM\n  |'), tree, quote).result;
  assert.deepEqual(labels(r).sort(), ['clientes_mssql', 'pendiente', 'ventas_pg']);
});

test('a cell that never ran is still a table', () => {
  const r = nameCompletion(ctxOf('SELECT * FROM pen|'), tree, quote).result;
  assert.ok(labels(r).includes('pendiente'));
  // After its dot there is nothing to list: its columns are asked for, no error.
  const a = nameCompletion(ctxOf('SELECT pendiente.| FROM pendiente'), tree, quote);
  assert.equal(a.result, null);
  assert.deepEqual(a.needColumns, ['pendiente']);
});

test("a cell's columns after `name.` and after its alias", () => {
  assert.deepEqual(labels(nameCompletion(ctxOf('SELECT ventas_pg.| FROM ventas_pg'), tree, quote).result), ['id', 'cliente', 'total']);
  assert.deepEqual(labels(nameCompletion(ctxOf('SELECT v.| FROM ventas_pg v JOIN clientes_mssql c ON c.id = v.cliente'), tree, quote).result), ['id', 'cliente', 'total']);
  assert.deepEqual(labels(nameCompletion(ctxOf('SELECT * FROM ventas_pg v JOIN clientes_mssql AS c ON c.|'), tree, quote).result), ['id', 'nombre']);
});

test('the select list offers the columns of the tables the statement reads', () => {
  const r = nameCompletion(ctxOf('SELECT n| FROM ventas_pg v JOIN clientes_mssql c ON c.id = v.cliente'), tree, quote).result;
  const l = labels(r);
  for (const c of ['id', 'cliente', 'total', 'nombre', 'v', 'c']) assert.ok(l.includes(c), c);
  // Below the tables; a name both tables have, once per table, qualified.
  const ids = r.options.filter((o) => o.label === 'id');
  assert.deepEqual(ids.map((o) => [o.detail, o.apply]), [['v (ventas_pg)', 'v.id'], ['c (clientes_mssql)', 'c.id']]);
  const nombre = r.options.find((o) => o.label === 'nombre');
  assert.equal(nombre.boost, -1);
  assert.equal(nombre.apply, undefined);
});

test('a comma in a FROM list is a table position; one in the select list is not', () => {
  assert.ok(isTableSlot('SELECT * FROM ventas_pg v, '));
  assert.ok(!isTableSlot('SELECT id, '));
  assert.ok(!isTableSlot('SELECT * FROM t WHERE x IN (SELECT a, '));
  assert.ok(!isTableSlot('SELECT * FROM t WHERE f(a, '));
});

test('nothing offered unasked before a word, and none in strings', () => {
  assert.equal(nameCompletion(ctxOf('SELECT * FROM |', false), tree, quote).result, null);
  assert.equal(nameCompletion(ctxOf("SELECT 'ven|'"), tree, quote).result, null);
});

test('an empty schema asks for the names', () => {
  assert.equal(nameCompletion(ctxOf('SELECT * FROM |'), nameTree({}), quote).needNames, true);
});

test('real connections keep their schemas and quoting', () => {
  const t = nameTree({ 'sales.orders': ['id'], 'sales.Order Items': ['qty'], orders: ['id'] });
  assert.deepEqual(labels(nameCompletion(ctxOf('SELECT * FROM sales.|'), t, quote).result).sort(), ['Order Items', 'orders']);
  const r = nameCompletion(ctxOf('SELECT * FROM sales.|'), t, quote).result;
  assert.equal(r.options.find((o) => o.label === 'Order Items').apply, '"Order Items"');
});

// -- file readers ------------------------------------------------------------------------------
const make = (name, format) => ({ label: name, detail: format, snippet: readerSnippet(name) });

test('the readers are offered in table positions only', () => {
  assert.deepEqual(labels(readerCompletion(ctxOf('SELECT * FROM |'), make)), FILE_READERS.map((r) => r.name));
  assert.ok(readerCompletion(ctxOf('SELECT * FROM ventas_pg v\nLEFT JOIN read|'), make));
  assert.equal(readerCompletion(ctxOf('SELECT read| FROM t'), make), null);
  assert.equal(readerCompletion(ctxOf('SELECT * FROM t WHERE |'), make), null);
});

test('only a results cell gets the readers and the file entries', () => {
  const pick = () => ({ label: 'Elegir archivo…' });
  const local = { enabled: true, grantedFiles: [], pick };
  const onServer = { enabled: false, grantedFiles: ['/tmp/a.csv'], pick };
  assert.ok(labels(fileCompletion(ctxOf('SELECT * FROM |'), local, make)).includes('read_csv'));
  assert.deepEqual(labels(fileCompletion(ctxOf("SELECT * FROM read_csv('|')"), local, make)), ['Elegir archivo…']);
  assert.equal(fileCompletion(ctxOf('SELECT * FROM |'), onServer, make), null);
  assert.equal(fileCompletion(ctxOf("SELECT * FROM read_csv('|')"), onServer, make), null);
});

test('the readers are exactly what the backend lets a results cell call', () => {
  assert.deepEqual(FILE_READERS.map((r) => r.name), [
    'read_csv', 'read_csv_auto', 'read_parquet', 'parquet_scan', 'read_json', 'read_json_auto', 'read_ndjson', 'read_ndjson_auto', 'read_xlsx',
    'st_read', 'sqlite_scan', 'read_duckdb',
  ]);
});

test("a reader's snippet leaves the cursor between the quotes", () => {
  assert.equal(readerSnippet('read_csv'), "read_csv('${}')");
  assert.equal(readerSnippet('read_xlsx'), "read_xlsx('${}')");
  assert.equal(readerSnippet('st_read'), "st_read('${}')");
  // A database file's reader: the file first, then its table (Tab).
  assert.equal(readerSnippet('sqlite_scan'), "sqlite_scan('${1}', '${2}')");
  assert.equal(readerSnippet('read_duckdb'), "read_duckdb('${1}', '${2}')");
});

test('inside the quotes: "Elegir archivo…" and the granted files, never a sensitive one', () => {
  const pick = (at) => ({ label: 'Elegir archivo…', at });
  const granted = ['/Users/ana/Downloads/clientes.csv', '/Users/ana/.ssh/id_ed25519', '/Users/ana/proj/.env', '/Users/ana/.aws/credentials', '/Users/ana/data/ventas.parquet'];
  const r = readerPathCompletion(ctxOf("SELECT * FROM read_csv('|')"), granted, pick);
  assert.deepEqual(labels(r), ['Elegir archivo…', '/Users/ana/Downloads/clientes.csv', '/Users/ana/data/ventas.parquet']);
  // The text between the quotes is what gets replaced.
  const c = ctxOf("SELECT * FROM read_csv('~/Dow|nloads/x.csv', header = true)");
  const r2 = readerPathCompletion(c, [], pick);
  assert.equal(c.state.sliceDoc(r2.from, r2.to), '~/Downloads/x.csv');
  // A list's later entry, too; outside the quotes, nothing.
  assert.ok(readerPathCompletion(ctxOf("FROM read_parquet(['a.parquet', '|"), [], pick));
  assert.equal(readerPathCompletion(ctxOf("SELECT 'x|'"), granted, pick), null);
  assert.equal(readerPathCompletion(ctxOf("FROM read_csv('a.csv', delim = '|')"), granted, pick), null);
  // The new readers: the file's quotes offer files, never the table's.
  assert.ok(readerPathCompletion(ctxOf("FROM st_read('~/geo/zo|')"), [], pick));
  assert.ok(readerPathCompletion(ctxOf("FROM sqlite_scan('|', 'clientes')"), [], pick));
  assert.ok(readerPathCompletion(ctxOf("FROM read_duckdb('|')"), [], pick));
  assert.equal(readerPathCompletion(ctxOf("FROM sqlite_scan('a.db', '|')"), granted, pick), null);
  assert.equal(readerPathCompletion(ctxOf("FROM read_duckdb('a.duckdb', 'ven|')"), granted, pick), null);
});

test('granted URLs and cloud patterns are offered like granted files', () => {
  const pick = (at) => ({ label: 'Elegir archivo…', at });
  const granted = ['https://datos.gob.ar/ventas.csv', 's3://ventas/2026/*.parquet', 'https://example.com/credentials.csv', '/Users/ana/a.csv'];
  const r = readerPathCompletion(ctxOf("SELECT * FROM read_parquet('s3|')"), granted, pick);
  assert.deepEqual(labels(r), ['Elegir archivo…', 's3://ventas/2026/*.parquet']);
  const all = readerPathCompletion(ctxOf("SELECT * FROM read_csv('|')"), granted, pick);
  assert.deepEqual(labels(all), ['Elegir archivo…', 'https://datos.gob.ar/ventas.csv', 's3://ventas/2026/*.parquet', '/Users/ana/a.csv']);
});

test('folders and files with credentials are sensitive', () => {
  for (const p of ['/Users/ana/.ssh/config', '~/.aws/credentials', '/home/x/.env', '/home/x/app/.env.local', 'C:\\Users\\x\\.azure\\a.json',
    '/Users/ana/.config/gcloud/creds.json', '/Users/ana/Library/Keychains/login.keychain-db', '/srv/tls/server.key', '/home/x/.pgpass', '/home/x/id_rsa']) {
    assert.ok(isSensitivePath(p), p);
  }
  for (const p of ['/Users/ana/Downloads/clientes.csv', '~/data/env.parquet', '/srv/exports/keys.xlsx']) assert.ok(!isSensitivePath(p), p);
});

// Other databases of the connection (`Driver::cross_database`, issue #20):
// `database_a` is the tab's, `database_b` loaded, `logs` not yet, `dw` has
// schemas.
const CROSS = nameTree(
  { table_a: ['id', 'name'] },
  {
    database_a: { table_a: ['id', 'name'] },
    database_b: { table_b: ['id', 'amount'], empty_b: [] },
    logs: null,
    dw: { 'sales.orders': ['order_id'] },
  },
);

test('other databases are offered where a table goes, with the tab\'s own tables', () => {
  const r = nameCompletion(ctxOf('SELECT * FROM |'), CROSS, quote).result;
  assert.deepEqual(labels(r).sort(), ['database_a', 'database_b', 'dw', 'logs', 'table_a']);
});

test('after `database_b.` its tables; after a schema-ful database, schema then table', () => {
  assert.deepEqual(labels(nameCompletion(ctxOf('SELECT * FROM database_b.|'), CROSS, quote).result).sort(), ['empty_b', 'table_b']);
  assert.deepEqual(labels(nameCompletion(ctxOf('SELECT * FROM dw.|'), CROSS, quote).result), ['sales']);
  assert.deepEqual(labels(nameCompletion(ctxOf('SELECT * FROM dw.sales.|'), CROSS, quote).result), ['orders']);
});

test('a database not loaded yet is asked for; a table without columns asks in its database', () => {
  const a = nameCompletion(ctxOf('SELECT * FROM logs.|'), CROSS, quote);
  assert.equal(a.result, null);
  assert.equal(a.needDatabase, 'logs');
  const b = nameCompletion(ctxOf('SELECT database_b.empty_b.| FROM database_b.empty_b'), CROSS, quote);
  assert.deepEqual(b.needColumns, ['empty_b']);
  assert.equal(b.columnsIn, 'database_b');
});

test('aliases in a cross-database JOIN resolve to their own database\'s columns', () => {
  const sql = (at) => `SELECT ${at}\nFROM database_a.table_a AS a\nJOIN database_b.table_b AS b ON a.id = b.id`;
  assert.deepEqual(labels(nameCompletion(ctxOf(sql('a.|')), CROSS, quote).result).sort(), ['id', 'name']);
  assert.deepEqual(labels(nameCompletion(ctxOf(sql('b.|')), CROSS, quote).result).sort(), ['amount', 'id']);
  // The select list offers both tables' columns.
  const all = labels(nameCompletion(ctxOf(sql('|')), CROSS, quote).result);
  for (const c of ['amount', 'name']) assert.ok(all.includes(c), c);
});

test('without other databases nothing changes', () => {
  assert.deepEqual(labels(nameCompletion(ctxOf('SELECT * FROM database_b.|'), nameTree({ table_a: [] }), quote).result), []);
});

// Issue #22: columns from the statement under the cursor, ambiguous names,
// CTEs and derived tables.
const SHOP = nameTree({
  customers: ['customer_id', 'customer_name', 'created_at'],
  orders: ['order_id', 'customer_id', 'total'],
  audit_logs: ['log_id', 'customer_ref', 'created_at'],
});
const cols = (r) => (r ? r.options.filter((o) => o.type === 'property').map((o) => o.label) : []);

test('the select list offers only the columns of the tables in FROM, never audit_logs', () => {
  const r = nameCompletion(ctxOf('SELECT cust| FROM customers c'), SHOP, quote).result;
  assert.deepEqual(cols(r).sort(), ['created_at', 'customer_id', 'customer_name']);
  assert.ok(!labels(r).includes('customer_ref'));
  const w = nameCompletion(ctxOf('SELECT c.customer_name FROM customers c WHERE cust|'), SHOP, quote).result;
  assert.deepEqual(cols(w).sort(), ['created_at', 'customer_id', 'customer_name']);
});

test('aliases and tables come from the statement under the cursor only', () => {
  const doc = (at) => `SELECT * FROM audit_logs a;\nSELECT ${at} FROM customers c;\nSELECT o.total FROM orders o`;
  const r = nameCompletion(ctxOf(doc('|')), SHOP, quote).result;
  assert.deepEqual(cols(r).sort(), ['created_at', 'customer_id', 'customer_name']);
  assert.ok(labels(r).includes('c'));
  assert.ok(!labels(r).includes('a') && !labels(r).includes('o'));
  // An alias of another statement doesn't resolve after its dot.
  assert.equal(nameCompletion(ctxOf(doc('o.|')), SHOP, quote).result, null);
  // A `;` inside a string or a comment doesn't end the statement.
  const s = nameCompletion(ctxOf("SELECT ';' AS x, | -- ; no\nFROM orders o"), SHOP, quote).result;
  assert.deepEqual(cols(s).sort(), ['customer_id', 'order_id', 'total']);
});

test('a GO line ends a batch', () => {
  const r = nameCompletion(ctxOf('SELECT * FROM orders o\nGO\nSELECT | FROM customers c\ngo\nSELECT * FROM audit_logs'), SHOP, quote).result;
  assert.deepEqual(cols(r).sort(), ['created_at', 'customer_id', 'customer_name']);
});

test('a column two tables have is offered once per table, qualified by its alias', () => {
  const r = nameCompletion(ctxOf('SELECT cust| FROM customers c JOIN orders o ON o.customer_id = c.customer_id'), SHOP, quote).result;
  const ids = r.options.filter((o) => o.label === 'customer_id');
  assert.deepEqual(ids.map((o) => [o.detail, o.apply]), [['c (customers)', 'c.customer_id'], ['o (orders)', 'o.customer_id']]);
  // The others are left as typed, with their table next to them.
  const total = r.options.find((o) => o.label === 'total');
  assert.equal(total.detail, 'o (orders)');
  assert.equal(total.apply, undefined);
  // Without aliases the table's name qualifies it.
  const t = nameCompletion(ctxOf('SELECT | FROM customers JOIN audit_logs ON 1 = 1'), SHOP, quote).result;
  assert.deepEqual(t.options.filter((o) => o.label === 'created_at').map((o) => o.apply), ['customers.created_at', 'audit_logs.created_at']);
});

test("a CTE gives its select list's columns, as a table and after its alias", () => {
  const sql = (at) => `WITH recent AS (\n  SELECT o.order_id, o.total AS amount, c.customer_name name, count(*) AS n, upper(c.customer_name)\n  FROM orders o JOIN customers c ON c.customer_id = o.customer_id\n)\nSELECT ${at} FROM recent r`;
  assert.deepEqual(labels(nameCompletion(ctxOf(sql('r.|')), SHOP, quote).result), ['order_id', 'amount', 'name', 'n']);
  assert.deepEqual(cols(nameCompletion(ctxOf(sql('|')), SHOP, quote).result), ['order_id', 'amount', 'name', 'n']);
  // Its name is a table where a table goes, and after a dot without an alias.
  assert.ok(labels(nameCompletion(ctxOf('WITH recent AS (SELECT 1 AS x) SELECT * FROM |'), SHOP, quote).result).includes('recent'));
  assert.deepEqual(labels(nameCompletion(ctxOf('WITH recent AS (SELECT 1 AS x) SELECT recent.| FROM recent'), SHOP, quote).result), ['x']);
  // A column list, and `*` of a known table.
  const ab = nameCompletion(ctxOf('WITH a(p, q) AS (SELECT 1, 2), b AS (SELECT * FROM orders) SELECT | FROM a, b'), SHOP, quote).result;
  assert.deepEqual(cols(ab), ['p', 'q', 'order_id', 'customer_id', 'total']);
  // A CTE reading an earlier one.
  assert.deepEqual(labels(nameCompletion(ctxOf('WITH a AS (SELECT order_id FROM orders), b AS (SELECT x.* FROM a x) SELECT b.| FROM b'), SHOP, quote).result), ['order_id']);
});

test('inside a CTE body its own FROM is in scope, and its aliases resolve after a dot', () => {
  const sql = (at) => `WITH x AS (\n  SELECT ${at}\n  FROM orders o\n)\nSELECT * FROM x JOIN customers c ON 1 = 1`;
  assert.deepEqual(labels(nameCompletion(ctxOf(sql('o.|')), SHOP, quote).result), ['order_id', 'customer_id', 'total']);
  assert.deepEqual(cols(nameCompletion(ctxOf(sql('|')), SHOP, quote).result).sort(), ['customer_id', 'order_id', 'total']);
  // The outer query's alias isn't visible in there.
  assert.equal(nameCompletion(ctxOf(sql('c.|')), SHOP, quote).result, null);
  // Still typing: the body's `)` isn't there yet.
  assert.deepEqual(labels(nameCompletion(ctxOf('WITH x AS (SELECT o.| FROM orders o'), SHOP, quote).result), ['order_id', 'customer_id', 'total']);
});

test('a derived table gives its columns, and a subquery sees the query around it', () => {
  const sql = (at) => `SELECT ${at} FROM (SELECT customer_id, sum(total) AS spent FROM orders GROUP BY customer_id) AS t JOIN customers c ON c.customer_id = t.customer_id`;
  assert.deepEqual(labels(nameCompletion(ctxOf(sql('t.|')), SHOP, quote).result), ['customer_id', 'spent']);
  const r = nameCompletion(ctxOf(sql('|')), SHOP, quote).result;
  assert.deepEqual(r.options.filter((o) => o.label === 'customer_id').map((o) => o.apply), ['c.customer_id', 't.customer_id']);
  assert.ok(labels(r).includes('t') && labels(r).includes('spent'));
  // Inside the derived table, only its own FROM.
  const inner = nameCompletion(ctxOf('SELECT * FROM (SELECT | FROM orders) t JOIN customers c ON 1 = 1'), SHOP, quote).result;
  assert.deepEqual(cols(inner).sort(), ['customer_id', 'order_id', 'total']);
  // A correlated subquery: its own columns, and the outer alias after a dot.
  const sub = (at) => `SELECT * FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = ${at})`;
  assert.deepEqual(cols(nameCompletion(ctxOf(sub('|')), SHOP, quote).result).sort(), ['customer_id', 'order_id', 'total']);
  assert.deepEqual(labels(nameCompletion(ctxOf(sub('c.|')), SHOP, quote).result), ['customer_id', 'customer_name', 'created_at']);
});

test('a comma in the select list is not a table with an alias', () => {
  const r = nameCompletion(ctxOf('SELECT customer_id, orders o| FROM customers'), SHOP, quote).result;
  assert.ok(!labels(r).includes('o'));
  assert.ok(isTableSlot('SELECT * FROM (SELECT 1 FROM x) t, '));
});
