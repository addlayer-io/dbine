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
  // Once each, below the tables.
  assert.equal(l.filter((x) => x === 'id').length, 1);
  assert.equal(r.options.find((o) => o.label === 'nombre').boost, -1);
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
  ]);
});

test("a reader's snippet leaves the cursor between the quotes", () => {
  assert.equal(readerSnippet('read_csv'), "read_csv('${}')");
  assert.equal(readerSnippet('read_xlsx'), "read_xlsx('${}')");
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
});

test('folders and files with credentials are sensitive', () => {
  for (const p of ['/Users/ana/.ssh/config', '~/.aws/credentials', '/home/x/.env', '/home/x/app/.env.local', 'C:\\Users\\x\\.azure\\a.json',
    '/Users/ana/.config/gcloud/creds.json', '/Users/ana/Library/Keychains/login.keychain-db', '/srv/tls/server.key', '/home/x/.pgpass', '/home/x/id_rsa']) {
    assert.ok(isSensitivePath(p), p);
  }
  for (const p of ['/Users/ana/Downloads/clientes.csv', '~/data/env.parquet', '/srv/exports/keys.xlsx']) assert.ok(!isSensitivePath(p), p);
});
