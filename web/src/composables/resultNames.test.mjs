// Tests for named results: the comment markers, the name above a statement
// (or at its head, where the engine's splitter keeps the comment in it), the
// statements an engine numbers itself, batches of several statements, a
// run's outcome split by statement, and the kept limit sparing named tabs.
// Run: `node --test src/composables/resultNames.test.mjs` from web/ (Node
// 23.6+ strips the TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  commentMarkers, linesAt, nameAbove, numberedStatements, oneStatementEach, splitOutcome, statementHead, uniqueNames,
} from './resultNames.ts';
import { overflow } from './resultRuns.ts';

const SQL = ['--'];

test('the markers follow the engine', () => {
  assert.deepEqual(commentMarkers('sql', 'postgres'), ['--']);
  assert.deepEqual(commentMarkers('sql', 'mysql'), ['--', '#']);
  assert.deepEqual(commentMarkers('sql', 'clickhouse'), ['--', '#']);
  assert.deepEqual(commentMarkers('sql', 'snowflake'), ['//', '--']);
  assert.deepEqual(commentMarkers('cql', ''), ['//', '--']);
  assert.deepEqual(commentMarkers('json', ''), ['//', '#']);
  assert.deepEqual(commentMarkers('cypher', ''), ['//']);
  assert.deepEqual(commentMarkers('redis', ''), ['#']);
  assert.deepEqual(commentMarkers('sql', 'q'), ['/']);
  assert.deepEqual(commentMarkers('dax', 'powerbi'), ['//', '--']);
});

test('the comment right above a statement names it', () => {
  const t = '-- Clientes activos\nSELECT * FROM clientes;\n';
  assert.equal(nameAbove(t, t.indexOf('SELECT'), SQL), 'Clientes activos');
  // Spaces, repeated markers and CRLF don't count.
  const c = '----   Totales   del  mes\r\n  SELECT 1;';
  assert.equal(nameAbove(c, c.indexOf('SELECT'), SQL), 'Totales del mes');
  assert.equal(nameAbove('# Pedidos\nSELECT 1', 10, ['--', '#']), 'Pedidos');
  assert.equal(nameAbove('// Nodos\nMATCH (n) RETURN n', 9, ['//']), 'Nodos');
});

test('the closest of several comment lines wins', () => {
  const t = '-- Reporte\n-- Ventas por mes\nSELECT 1;';
  assert.equal(nameAbove(t, t.indexOf('SELECT'), SQL), 'Ventas por mes');
});

test('no name without a comment line right above', () => {
  const blank = '-- Clientes\n\nSELECT 1;';
  assert.equal(nameAbove(blank, blank.indexOf('SELECT'), SQL), null);
  assert.equal(nameAbove('SELECT 1;', 0, SQL), null);
  // A trailing comment after code isn't a comment line.
  const trailing = 'SELECT 1; -- uno\nSELECT 2;';
  assert.equal(nameAbove(trailing, trailing.indexOf('SELECT 2'), SQL), null);
  // Code before it on its line: the comment names the line's first statement.
  const two = '-- Uno\nSELECT 1; SELECT 2;';
  assert.equal(nameAbove(two, two.indexOf('SELECT 1'), SQL), 'Uno');
  assert.equal(nameAbove(two, two.indexOf('SELECT 2'), SQL), null);
  // Only the marker: no name. Block comments don't name.
  assert.equal(nameAbove('--   \nSELECT 1', 6, SQL), null);
  assert.equal(nameAbove('/* Uno */\nSELECT 1', 10, SQL), null);
  // Another engine's marker isn't a comment there.
  assert.equal(nameAbove('# Uno\nSELECT 1', 6, SQL), null);
});

test("Db2's terminator directives aren't names", () => {
  const t = '--#SET TERMINATOR @\nCREATE PROCEDURE p() BEGIN END@\n--#SET TERMINATOR ;\nSELECT 1;';
  assert.equal(nameAbove(t, t.indexOf('CREATE'), SQL, 'db2'), null);
  assert.equal(nameAbove(t, t.indexOf('SELECT'), SQL, 'db2'), null);
  // Elsewhere it's a comment like any other.
  assert.equal(nameAbove(t, t.indexOf('SELECT'), SQL, 'postgres'), '#SET TERMINATOR ;');
  const named = '-- Totales\nSELECT 1;';
  assert.equal(nameAbove(named, named.indexOf('SELECT'), SQL, 'db2'), 'Totales');
});

test('a statement that starts with its comment lines: the code past them, and the closest names it', () => {
  // MongoDB's `//` and Redis's `#` are code to the splitter: the unit starts on the comment.
  const mongo = '// Usuarios\ndb.users.find({})';
  assert.deepEqual(statementHead(mongo, 0, mongo.length, ['//', '#']), { start: mongo.indexOf('db.'), name: 'Usuarios' });
  const redis = '# Pedidos\nGET k';
  assert.deepEqual(statementHead(redis, 0, redis.length, ['#']), { start: redis.indexOf('GET'), name: 'Pedidos' });
  const several = '// Reporte\n// Ventas\ndb.sales.find()';
  assert.equal(statementHead(several, 0, several.length, ['//']).name, 'Ventas');
  // A blank line between the comment and the code: no name.
  const blank = '// Usuarios\n\ndb.users.find({})';
  assert.deepEqual(statementHead(blank, 0, blank.length, ['//']), { start: blank.indexOf('db.'), name: null });
  // Nothing but comments: no statement.
  assert.deepEqual(statementHead('// fin\n', 0, 7, ['//']), { start: null, name: null });
});

test('a statement that starts with its code is named by the line above', () => {
  const t = '-- Clientes\nSELECT 1;';
  const at = t.indexOf('SELECT');
  assert.deepEqual(statementHead(t, at, t.length, SQL), { start: at, name: 'Clientes' });
  // From where the statement before ended: blank space and lines skipped.
  const m = 'db.a.find();\n// B\ndb.b.find()';
  assert.deepEqual(statementHead(m, m.indexOf(';') + 1, m.length, ['//']), { start: m.indexOf('db.b'), name: 'B' });
  // A comment after the code before it, on its line, is that one's.
  const trailing = 'db.a.find(); // nota\ndb.b.find()';
  assert.deepEqual(statementHead(trailing, trailing.indexOf(';') + 1, trailing.length, ['//']), { start: trailing.indexOf('db.b'), name: null });
  const inline = 'SELECT 1; SELECT 2;';
  assert.equal(statementHead(inline, 10, inline.length, SQL).name, null);
});

test('long names are kept whole', () => {
  const long = 'x'.repeat(300);
  assert.equal(nameAbove(`-- ${long}\nSELECT 1`, 4 + long.length, SQL), long);
});

test('a name taken earlier in the run stays with the first statement', () => {
  const u = uniqueNames([{ name: 'A' }, { name: null }, { name: 'A' }, { name: 'B' }]);
  assert.deepEqual(u.map((x) => x.name), ['A', null, null, 'B']);
});

test('lines of offsets', () => {
  const t = 'a\nb\n\nc';
  assert.deepEqual(linesAt(t, [6, 0, 2]), [4, 1, 2]);
});

const res = (statement, offset, line, extra = {}) => ({
  columns: [{ name: 'n', type_name: 'int' }], rows: [[statement]], total_rows: 1, truncated: false, rows_affected: null,
  statement, offset, line, tag: null, elapsed_ms: 10, ...extra,
});

test('a run splits by statement: named parts move, the rest stays', () => {
  // -- A\nSELECT 1;\nSELECT 2;\n-- B\nSELECT 3;
  const units = [
    { start: 5, line: 2, name: 'A' },
    { start: 15, line: 3, name: null },
    { start: 30, line: 5, name: 'B' },
  ];
  const o = {
    results: [res(0, 5, 2), res(1, 15, 3), res(2, 30, 5)],
    messages: ['aviso de 1', 'suelto'],
    error: 'boom',
    elapsed_ms: 50,
    plans: [],
    log: [
      { level: 'info', text: 'aviso de 1', statement: 1, code: null, line: 3 },
      { level: 'error', text: 'boom', statement: 2, code: '42', line: 5 },
    ],
    errors: [{ message: 'boom', code: '42', sqlstate: null, statement: 2, offset: 33, line: 5, fatal: false }],
    transaction: 'idle',
  };
  const { rest, parts } = splitOutcome(o, units);
  assert.deepEqual(parts.map((p) => [p.name, p.unit]), [['A', 0], ['B', 2]]);
  const [a, b] = parts.map((p) => p.outcome);
  // Offsets and lines from the statement's own start.
  assert.deepEqual([a.results[0].offset, a.results[0].line], [0, 1]);
  assert.equal(a.error, null);
  assert.equal(a.elapsed_ms, 10);
  // A statement's error goes with it.
  assert.equal(b.error, 'boom');
  assert.deepEqual([b.errors[0].offset, b.errors[0].line, b.log[0].line], [3, 1, 1]);
  assert.deepEqual(b.messages, []);
  // The rest: the unnamed statement, its message and the plain one nobody logged.
  assert.deepEqual(rest.results.map((r) => r.statement), [1]);
  assert.deepEqual(rest.messages, ['aviso de 1', 'suelto']);
  assert.equal(rest.error, null);
  assert.equal(rest.elapsed_ms, 30);
  assert.equal(rest.transaction, 'idle');
});

test('nothing left: no rest', () => {
  const o = { results: [res(0, 0, 1), res(1, 12, 3)], messages: [], error: null, elapsed_ms: 20, plans: [], log: [], errors: [] };
  const { rest, parts } = splitOutcome(o, [{ start: 0, line: 1, name: 'A' }, { start: 12, line: 3, name: 'B' }]);
  assert.equal(rest, null);
  assert.equal(parts.length, 2);
});

test('no statement indexes: nothing moves', () => {
  const o = { results: [res(null, null, null)], messages: [], error: null, elapsed_ms: 5, plans: [], log: [], errors: [] };
  const { rest, parts } = splitOutcome(o, [{ start: 0, line: 1, name: 'A' }]);
  assert.equal(rest, o);
  assert.deepEqual(parts, []);
});

test('a run-level error with no statement stays in the rest', () => {
  const o = { results: [res(0, 0, 1)], messages: [], error: 'conexión perdida', elapsed_ms: 5, plans: [], log: [], errors: [] };
  const { rest, parts } = splitOutcome(o, [{ start: 0, line: 1, name: 'A' }, { start: 10, line: 2, name: null }]);
  assert.equal(parts.length, 1);
  assert.equal(rest.error, 'conexión perdida');
});

test('an engine that numbers a whole script itself: its statements, from where each starts to the next', () => {
  // Redis: one command per line, which its splitter sees as one unit.
  const sent = '# Usuarios\nHGETALL user:1\n# Pedidos\nLRANGE orders 0 -1\nPING';
  const o = {
    results: [res(0, sent.indexOf('HGETALL'), 2), res(1, sent.indexOf('LRANGE'), 4)],
    messages: [], error: 'boom', elapsed_ms: 9, plans: [], log: [],
    errors: [{ message: 'boom', code: null, sqlstate: null, statement: 2, offset: sent.indexOf('PING'), line: 5, fatal: false }],
  };
  const found = numberedStatements(o, sent, ['#']);
  // The comment line before the next one is the next one's.
  assert.deepEqual(found.map((s) => [s.statement, sent.slice(s.start, s.end)]), [[0, 'HGETALL user:1'], [1, 'LRANGE orders 0 -1'], [2, 'PING']]);
  // Each named by the comment above it.
  const units = found.map((s) => ({ ...s, line: 1, name: statementHead(sent, s.start, s.end, ['#']).name }));
  assert.deepEqual(units.map((u) => u.name), ['Usuarios', 'Pedidos', null]);
  const byStatement = [];
  for (const u of units) byStatement[u.statement] = u;
  const { rest, parts } = splitOutcome(o, byStatement);
  assert.deepEqual(parts.map((p) => p.name), ['Usuarios', 'Pedidos']);
  assert.equal(rest.error, 'boom');
});

test("MongoDB's starts that count the comment above, and no `;`", () => {
  const sent = '// A\ndb.a.find()\n// B\ndb.b.find()';
  const o = { results: [res(0, 0, 1), res(1, sent.indexOf('// B'), 3)], messages: [], error: null, elapsed_ms: 2, plans: [], log: [], errors: [] };
  const found = numberedStatements(o, sent, ['//', '#']);
  assert.deepEqual(found.map((s) => sent.slice(s.start, s.end)), ['// A\ndb.a.find()', '// B\ndb.b.find()']);
  assert.deepEqual(found.map((s) => statementHead(sent, s.start, s.end, ['//', '#']).name), ['A', 'B']);
});

test('no numbering at all: no statements', () => {
  const o = { results: [res(null, null, null)], messages: [], error: null, elapsed_ms: 1, plans: [], log: [], errors: [] };
  assert.deepEqual(numberedStatements(o, 'x', ['--']), []);
});

test('a batch of several statements takes no name', () => {
  // SQL Server without GO: one batch holding both statements.
  const units = [{ start: 12, end: 70, name: 'Clientes' }];
  assert.equal(oneStatementEach(units, [12, 46])[0].name, null);
  // GO between them: two batches, one statement each.
  const two = [{ start: 12, end: 40, name: 'Clientes' }, { start: 63, end: 90, name: 'Proveedores' }];
  assert.deepEqual(oneStatementEach(two, [63, 12]).map((u) => u.name), ['Clientes', 'Proveedores']);
});

const run = (id, extra = {}) => ({ id, key: String(id), outcome: null, script: '', connectionId: 'c', database: '', language: 'sql', plan: 'none', at: id, pinned: false, multi: null, lineOffset: 0, runBase: 0, ...extra });

test('the kept limit never closes a named or pinned result', () => {
  const runs = [run(1, { name: 'Clientes' }), run(2), run(3, { pinned: true }), run(4), run(5)];
  const fresh = runs[4];
  assert.deepEqual(overflow(runs, fresh, 1, null).map((r) => r.id), [2, 4]);
});
