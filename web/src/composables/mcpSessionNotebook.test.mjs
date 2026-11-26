// Tests for mcpSessionNotebook.ts. Run: `node --test src/composables/mcpSessionNotebook.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';

// The module imports its sibling without an extension (as Vite does): add it for Node.
register('data:text/javascript,' + encodeURIComponent(
  "export async function resolve(s, c, n) { return n(/^\\.\\.?\\//.test(s) && !/\\.[a-z]+$/.test(s) ? s + '.ts' : s, c); }",
));

const { fenced, sessionNotebook } = await import('./mcpSessionNotebook.ts');

const text = {
  title: (s) => `TITLE ${s.client}`,
  summary: (s) => `SUMMARY ${s.calls}`,
  explored: (items) => `EXPLORED\n${items}`,
  plan: () => 'PLAN',
  wrote: (at, rows) => `WROTE ${at} ${rows}`,
  rejected: (timeout) => (timeout ? 'TIMEOUT' : 'REJECTED'),
  failed: (e) => `FAILED ${e}`,
  notKept: (summary) => `NOTKEPT ${summary}`,
};
const time = (iso) => `@${iso}`;
const session = { client: 'Claude Code', first_id: 1, last_id: 99, start: 't0', end: 't9', connections: ['ventas'], calls: 9, reads: 3, writes: 1, rejected: 1, errors: 1 };
let id = 0;
const call = (tool, extra = {}) => ({
  id: ++id, at: `t${id}`, client: 'Claude Code', connection: 'ventas', database: 'shop', tool, summary: '', code: null, ok: true, rows: null, error: null, ...extra,
});

test('an agent session becomes text, SQL cells on its targets and its writes as text', () => {
  const nb = sessionNotebook(session, [
    call('list_connections', { connection: '' }),
    call('list_objects', { summary: 'shop' }),
    call('list_objects', { summary: 'shop' }),
    call('describe_object', { summary: 'shop.clientes' }),
    call('run_query:enforced', { code: 'SELECT count(*) FROM clientes' }),
    call('explain:enforced', { code: 'SELECT * FROM pedidos', connection: 'dw', database: '' }),
    call('run_query:request', { code: 'SELECT fn()' }),
    call('run_query:rejected', { code: 'SELECT fn()', ok: false }),
    call('run_query', { code: 'SELECT nope', ok: false, error: 'no such column' }),
    call('execute:request', { code: "UPDATE clientes SET activo = 0 WHERE id = 7" }),
    call('execute:approved', { code: "UPDATE clientes SET activo = 0 WHERE id = 7", rows: 1 }),
    call('execute:timeout', { code: 'DELETE FROM clientes', ok: false }),
    call('sample_rows', { summary: 'shop.pedidos' }),
  ], text, time);

  const kinds = nb.cells.map((c) => c.kind);
  assert.deepEqual(kinds, ['markdown', 'markdown', 'sql', 'markdown', 'sql', 'markdown', 'markdown', 'sql', 'markdown', 'markdown', 'markdown']);
  assert.equal(nb.cells[0].source, '## TITLE Claude Code\n\nSUMMARY 9');
  // What it only looked at: one list, without repeats, before its first query.
  assert.equal(nb.cells[1].source, 'EXPLORED\n- `list_connections`\n- `list_objects` ventas › shop\n- `describe_object` ventas › shop.clientes');
  // Its reads: SQL cells on the connection and database it used.
  assert.equal(nb.cells[2].source, 'SELECT count(*) FROM clientes');
  assert.deepEqual(nb.cells[2].target, { connection: 'ventas', database: 'shop', environment: null });
  assert.equal(nb.cells[3].source, 'PLAN');
  assert.deepEqual(nb.cells[4].target, { connection: 'dw', database: null, environment: null });
  // A rejected read is text; a failed one keeps its cell, after the error.
  assert.equal(nb.cells[5].source, 'REJECTED\n\n```sql\nSELECT fn()\n```');
  assert.equal(nb.cells[6].source, 'FAILED no such column');
  assert.equal(nb.cells[7].source, 'SELECT nope');
  // Writes never as SQL cells: "Run all" can't run them again.
  assert.equal(nb.cells[8].source, 'WROTE @t11 1\n\n```sql\nUPDATE clientes SET activo = 0 WHERE id = 7\n```');
  assert.equal(nb.cells[9].source, 'TIMEOUT\n\n```sql\nDELETE FROM clientes\n```');
  assert.ok(!nb.cells.some((c) => c.kind === 'sql' && /UPDATE|DELETE/.test(c.source)));
  // What it looked at last closes the notebook.
  assert.equal(nb.cells[10].source, 'EXPLORED\n- `sample_rows` ventas › shop.pedidos');
  // New cells go where its first query went; no results kept from the agent.
  assert.deepEqual(nb.target, { connection: 'ventas', database: 'shop', environment: null });
  assert.ok(nb.cells.every((c) => c.result === null));
  assert.equal(new Set(nb.cells.map((c) => c.id)).size, nb.cells.length, 'cell ids are unique');
});

test('a query the log did not keep whole says so instead of a broken cell', () => {
  const nb = sessionNotebook(session, [call('run_query:enforced', { code: null, summary: '[shop] SELECT …' })], text, time);
  assert.deepEqual(nb.cells.map((c) => c.kind), ['markdown', 'markdown']);
  assert.equal(nb.cells[1].source, 'NOTKEPT [shop] SELECT …');
  assert.equal(nb.target, null);
});

test('a session with no queries is its title and what it looked at', () => {
  const nb = sessionNotebook(session, [call('list_databases'), call('list_databases')], text, time);
  assert.deepEqual(nb.cells.map((c) => c.kind), ['markdown', 'markdown']);
  assert.equal(nb.cells[1].source, 'EXPLORED\n- `list_databases` ventas');
});

test('code with backticks keeps its fence closed', () => {
  assert.equal(fenced('select 1'), '```sql\nselect 1\n```');
  assert.equal(fenced('db.c.find({ a: "```" })', 'js'), '````js\ndb.c.find({ a: "```" })\n````');
});
