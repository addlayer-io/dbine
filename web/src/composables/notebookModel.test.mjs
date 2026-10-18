// Tests for the notebook's model helpers (notebookModel.ts). Run:
// `node --test src/composables/notebookModel.test.mjs` from web/ (Node 23.6+
// strips the TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  EditVersion, RunLock, applyProjectDefault, assignCellNames, cellNameProblem, completeRows, defaultCellName, emptyNotebook, emptyTarget,
  matchConnection, newCell, outline, projectFileOf, promptVariant, sameTarget, validCellName,
} from './notebookModel.ts';

const conns = [
  { id: 'a', name: 'Ventas' },
  { id: 'b', name: 'Ventas' },
  { id: 'c', name: 'Stock' },
  { id: 'd', name: 'Logs prod' },
];

test('a unique exact name resolves as before', () => {
  assert.deepEqual(matchConnection(conns, 'Stock'), { kind: 'one', id: 'c' });
  // Even with an alias around: the exact unique name wins.
  assert.deepEqual(matchConnection(conns, 'Stock', 'a', 'b'), { kind: 'one', id: 'c' });
});

test('a name several connections share asks, then uses the file\'s choice', () => {
  assert.deepEqual(matchConnection(conns, 'Ventas'), { kind: 'ambiguous', ids: ['a', 'b'] });
  // The machine-wide alias doesn't decide between same-named connections.
  assert.deepEqual(matchConnection(conns, 'Ventas', null, 'b'), { kind: 'ambiguous', ids: ['a', 'b'] });
  assert.deepEqual(matchConnection(conns, 'Ventas', 'b'), { kind: 'one', id: 'b' });
  // A remembered choice that isn't one of them any more: ask again.
  assert.deepEqual(matchConnection(conns, 'Ventas', 'c'), { kind: 'ambiguous', ids: ['a', 'b'] });
});

test('a name nobody has: the aliases, then a unique loose match', () => {
  assert.deepEqual(matchConnection(conns, 'Ventas viejo', null, 'c'), { kind: 'one', id: 'c' });
  assert.deepEqual(matchConnection(conns, 'Ventas viejo', 'd', 'c'), { kind: 'one', id: 'd' });
  assert.deepEqual(matchConnection(conns, ' logs PROD '), { kind: 'one', id: 'd' });
  assert.deepEqual(matchConnection(conns, 'ventas'), { kind: 'ambiguous', ids: ['a', 'b'] });
  assert.deepEqual(matchConnection(conns, 'Nada', null, 'gone'), { kind: 'none' });
});

test('a path inside a linked project is that project\'s file', () => {
  const projects = [{ id: 'p1', path: '/Users/me/repo' }, { id: 'p2', path: '/Users/me/repo/sub/' }, { id: 'w', path: 'C:\\Work\\Repo' }];
  assert.deepEqual(projectFileOf(projects, '/Users/me/repo/notes/a.ipynb'), { projectId: 'p1', projectPath: 'notes/a.ipynb' });
  // The innermost project wins.
  assert.deepEqual(projectFileOf(projects, '/Users/me/repo/sub/b.ipynb'), { projectId: 'p2', projectPath: 'b.ipynb' });
  assert.equal(projectFileOf(projects, '/Users/me/repository/a.ipynb'), null);
  assert.equal(projectFileOf(projects, '/Users/me/repo'), null);
  // Windows: backslashes and any case.
  assert.deepEqual(projectFileOf(projects, 'c:\\work\\repo\\x\\c.ipynb'), { projectId: 'w', projectPath: 'x/c.ipynb' });
});

test('a git project\'s default applies only when the file doesn\'t say', () => {
  const foreign = { ...emptyNotebook(false), save_results: true, save_results_set: false };
  assert.equal(applyProjectDefault({ ...foreign }, true).save_results, false);
  assert.equal(applyProjectDefault({ ...foreign }, false).save_results, true);
  const said = { ...foreign, save_results_set: true };
  assert.equal(applyProjectDefault({ ...said }, true).save_results, true);
  // New notebooks: without results only in git projects.
  assert.equal(emptyNotebook(true).save_results, false);
  assert.equal(emptyNotebook(false).save_results, true);
});

test('an edit during a save leaves the notebook unsaved', () => {
  const v = new EditVersion();
  v.bump();
  let at = v.start();
  assert.equal(v.end(at), 'saved');
  at = v.start();
  v.bump(); // typed while the write was in flight
  assert.equal(v.end(at), 'dirty');
});

test('the run lock refuses a second run until released', () => {
  const l = new RunLock();
  assert.ok(l.take());
  assert.ok(!l.take());
  assert.ok(l.held);
  l.release();
  assert.ok(l.take());
});

test('prompts per target language; the outline fences each cell in its language', () => {
  assert.equal(promptVariant('sql'), 'sql');
  assert.equal(promptVariant(null), 'sql');
  assert.equal(promptVariant('redis'), 'native');
  assert.equal(promptVariant('json'), 'native');
  const cells = [
    { ...emptyNotebook(false).cells[0], source: 'GET k', language: 'redis' },
    { ...emptyNotebook(false).cells[0], source: 'SELECT 1' },
  ];
  assert.equal(outline(cells, 1), '```redis\nGET k\n```\n\n```sql\nSELECT 1\n```');
});

// -- cell names and "DataFrame SQL" --------------------------------------------------

const sql = (name, source = 'SELECT 1') => ({ ...newCell('sql', source), name });
const md = () => newCell('markdown', '# t');

test('cell names are plain identifiers, not SQL keywords', () => {
  for (const ok of ['c1', 'ventas_pg', '_x', 'C2', 'a'.repeat(63)]) assert.ok(validCellName(ok), ok);
  for (const bad of ['', '1c', 'a-b', 'a b', 'año', 'select', 'From', 'a'.repeat(64)]) assert.ok(!validCellName(bad), bad);
});

test('a name is unique in the notebook, ignoring case', () => {
  const cells = [sql('ventas'), sql('c2')];
  assert.equal(cellNameProblem(cells, cells[1].id, 'VENTAS'), 'taken');
  assert.equal(cellNameProblem(cells, cells[0].id, 'Ventas'), null);
  assert.equal(cellNameProblem(cells, cells[1].id, 'x y'), 'invalid');
  assert.equal(cellNameProblem(cells, cells[1].id, 'clientes'), null);
});

test('default names follow the SQL cells\' places and skip taken ones', () => {
  const cells = [sql('c1'), md(), sql('c3')];
  assert.equal(defaultCellName(cells, 1), 'c2');
  assert.equal(defaultCellName(cells, 3), 'c4');
  assert.equal(defaultCellName([sql('c1'), sql('c2')], 1), 'c3');
  // A file from before names: c1, c2… by position; a kept name stays.
  const old = [sql(null), md(), sql('ventas'), sql(null), sql('bad name')];
  assert.deepEqual(assignCellNames(old).map((c) => c.name), ['c1', null, 'ventas', 'c3', 'c4']);
});

test('only a complete run is loaded as a table without running it again', () => {
  const r = (o) => ({ columns: [{ name: 'a', type_name: '' }], rows: [[1]], total_rows: 1, truncated: false, rows_affected: null, ...o });
  const out = (results, error = null) => ({ results, messages: [], error, elapsed_ms: 1, plans: [] });
  assert.deepEqual(completeRows(out([r()])), { columns: [{ name: 'a', type_name: '' }], rows: [[1]] });
  assert.equal(completeRows(out([r({ truncated: true })])), null);
  assert.equal(completeRows(out([r({ total_rows: 10 })])), null);
  assert.equal(completeRows(out([r()], 'boom')), null);
  assert.equal(completeRows(out([])), null);
  assert.equal(completeRows(null), null);
});

test('the results target is a target of its own, with no connection', () => {
  const results = { connection: null, database: null, environment: null, results: true };
  assert.equal(emptyTarget(results), false);
  assert.equal(sameTarget(results, { connection: null, database: null, environment: null }), false);
  assert.equal(sameTarget(results, { ...results }), true);
});
