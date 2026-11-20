// Tests for a key database's other kinds (keyKinds.ts): which objects of a
// schema read the explorer lists next to the key search (Redis' search
// indexes), how the store reads them (only `database_schema`, the latest
// read, nothing after the connection is forgotten, failures kept with why),
// the folders the explorer shows for them and "Mostrar en el explorador"'s
// separator. Run: `node --test src/composables/keyKinds.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { hasOtherKinds, LatestRead, otherKindFolders, otherKindObjects, readOtherKinds, revealSeparator } from './keyKinds.ts';

const kind = (id, label) => ({ id, label, has_columns: true, browsable: true, has_definition: true });
const REDIS = [kind('key', 'Claves'), kind('index', 'Índices de búsqueda')];
const ETCD = [kind('key', 'Claves')];
const table = (k, name, schema = null) => ({ kind: k, schema, name, columns: [], primary_key: null, foreign_keys: [], indexes: [] });

test('only drivers with kinds besides their keys list others', () => {
  assert.equal(hasOtherKinds(REDIS), true);
  assert.equal(hasOtherKinds(ETCD), false);
  assert.equal(hasOtherKinds([]), false);
  assert.equal(hasOtherKinds(undefined), false);
});

test("a key database lists its other declared kinds from the schema read, by name", () => {
  const tables = [table('index', 'idx:docs'), table('index', 'Idx:Autos'), table('index', 'idx:10'), table('index', 'idx:9')];
  assert.deepEqual(otherKindObjects(REDIS, tables).map((o) => o.name), ['idx:9', 'idx:10', 'Idx:Autos', 'idx:docs']);
  assert.deepEqual(otherKindObjects(REDIS, [table('index', 'docs')]), [{ kind: 'index', schema: null, name: 'docs', parent: null }]);
});

test('never the keys, a kind the driver does not declare, or the same object twice', () => {
  const tables = [table('key', 'user:1'), table('table', 'x'), table('', 'no-kind'), table('index', 'docs'), table('index', 'docs')];
  assert.deepEqual(otherKindObjects(REDIS, tables).map((o) => `${o.kind}:${o.name}`), ['index:docs']);
  // A driver with keys only: nothing besides them.
  assert.deepEqual(otherKindObjects(ETCD, [table('index', 'docs')]), []);
});

// -- the store's read (connections.ts loadOtherKinds) ------------------------------------

const KEY = 'c1\u0000db0';
/** A `call` whose replies the test hands out, one per call, in any order. */
function server() {
  const calls = [];
  const call = (cmd, args) => new Promise((resolve, reject) => calls.push({ cmd, args, resolve, reject }));
  return { calls, call };
}
const read = (state, latest, call, key = KEY) => readOtherKinds({
  state, latest, key, kinds: REDIS, connectionId: 'c1', database: 'db0', call, errorText: (e) => `error: ${e.message}`,
});
const names = (st) => st.items.map((o) => o.name);

test("the read asks for the database's schema only, never the object list that walks the keys", async () => {
  const { calls, call } = server();
  const state = {};
  const done = read(state, new LatestRead(), call);
  assert.deepEqual(state[KEY], { status: 'loading', items: [], error: null });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].cmd, 'database_schema');
  assert.deepEqual(calls[0].args, { args: { connection_id: 'c1', database: 'db0' } });
  calls[0].resolve([table('index', 'idx:b'), table('index', 'idx:a')]);
  assert.equal(await done, null);
  assert.equal(state[KEY].status, 'ready');
  assert.deepEqual(names(state[KEY]), ['idx:a', 'idx:b']);
});

test('a refresh keeps the list while it reads, and shows the new one', async () => {
  const { calls, call } = server();
  const state = {};
  const latest = new LatestRead();
  const first = read(state, latest, call);
  calls[0].resolve([table('index', 'old')]);
  await first;
  const again = read(state, latest, call);
  assert.equal(calls.length, 2);
  assert.equal(calls[1].cmd, 'database_schema');
  assert.deepEqual({ status: state[KEY].status, names: names(state[KEY]) }, { status: 'loading', names: ['old'] });
  calls[1].resolve([table('index', 'new')]);
  await again;
  assert.deepEqual({ status: state[KEY].status, names: names(state[KEY]) }, { status: 'ready', names: ['new'] });
});

test('an older reply that lands after a newer one is dropped', async () => {
  const { calls, call } = server();
  const state = {};
  const latest = new LatestRead();
  const older = read(state, latest, call);
  const newer = read(state, latest, call);
  calls[1].resolve([table('index', 'newer')]);
  await newer;
  calls[0].resolve([table('index', 'older')]);
  await older;
  assert.deepEqual(names(state[KEY]), ['newer']);
  // A failure of the older one doesn't hide the newer list either.
  const third = read(state, latest, call);
  const fourth = read(state, latest, call);
  calls[3].resolve([table('index', 'fourth')]);
  await fourth;
  calls[2].reject(new Error('busy'));
  assert.equal(await third, null);
  assert.deepEqual({ status: state[KEY].status, names: names(state[KEY]) }, { status: 'ready', names: ['fourth'] });
});

test('a reply after the connection is forgotten does not bring the list back, even with a new read started', async () => {
  const { calls, call } = server();
  const state = {};
  const latest = new LatestRead();
  const stale = read(state, latest, call);
  // forget(): the store drops the connection's entries and its reads.
  delete state[KEY];
  latest.forget('c1\u0000');
  calls[0].resolve([table('index', 'a'), table('index', 'b')]);
  await stale;
  assert.equal(state[KEY], undefined);
  // Reconnected: a new read starts; the old one landing late is still dropped.
  const stale2 = read(state, latest, call);
  latest.forget('c1\u0000');
  delete state[KEY];
  const fresh = read(state, latest, call);
  calls[1].resolve([table('index', 'old')]);
  await stale2;
  assert.deepEqual(state[KEY], { status: 'loading', items: [], error: null });
  calls[2].resolve([]);
  await fresh;
  assert.deepEqual(state[KEY], { status: 'ready', items: [], error: null });
});

test("forgetting one connection leaves another's reads alone", async () => {
  const { calls, call } = server();
  const state = {};
  const latest = new LatestRead();
  const other = read(state, latest, call, 'c10\u0000db0');
  latest.forget('c1\u0000');
  calls[0].resolve([table('index', 'kept')]);
  await other;
  assert.deepEqual(names(state['c10\u0000db0']), ['kept']);
});

test('a failed read says why, and the store gets the error to check the connection', async () => {
  const { calls, call } = server();
  const state = {};
  const latest = new LatestRead();
  const first = read(state, latest, call);
  calls[0].resolve([table('index', 'a')]);
  await first;
  const failing = read(state, latest, call);
  const err = new Error('la sesión está ocupada');
  calls[1].reject(err);
  assert.equal(await failing, err);
  assert.deepEqual(state[KEY], { status: 'error', items: [], error: 'error: la sesión está ocupada' });
  // A refresh asks again.
  const retry = read(state, latest, call);
  assert.deepEqual(state[KEY], { status: 'loading', items: [], error: null });
  calls[2].resolve([table('index', 'a')]);
  await retry;
  assert.deepEqual(names(state[KEY]), ['a']);
});

// -- the explorer's folders (ExplorerSidebar.vue databaseChildren) --------------------------

const obj = (name) => ({ kind: 'index', schema: null, name, parent: null });
const shape = (fs) => fs.map((f) => ({ kind: f.kind.id, status: f.status, count: f.count, items: f.items.map((o) => o.name), error: f.error }));

test('a folder per other kind, with its objects and count', () => {
  const two = [kind('key', 'Claves'), kind('index', 'Índices'), kind('set', 'Conjuntos')];
  const entry = { status: 'ready', items: [obj('a'), { ...obj('s'), kind: 'set' }, obj('b')], error: null };
  assert.deepEqual(shape(otherKindFolders(two, entry)), [
    { kind: 'index', status: 'ready', count: 2, items: ['a', 'b'], error: null },
    { kind: 'set', status: 'ready', count: 1, items: ['s'], error: null },
  ]);
});

test('the folder shows before the first object exists, empty, so its menu offers the templates', () => {
  assert.deepEqual(shape(otherKindFolders(REDIS, { status: 'ready', items: [], error: null })), [
    { kind: 'index', status: 'empty', count: 0, items: [], error: null },
  ]);
});

test('while the first read runs the folder says so; a refresh keeps the list', () => {
  for (const entry of [undefined, { status: 'loading', items: [], error: null }]) {
    assert.deepEqual(shape(otherKindFolders(REDIS, entry)), [{ kind: 'index', status: 'loading', count: undefined, items: [], error: null }]);
  }
  assert.deepEqual(shape(otherKindFolders(REDIS, { status: 'loading', items: [obj('a')], error: null })), [
    { kind: 'index', status: 'ready', count: 1, items: ['a'], error: null },
  ]);
});

test("a failed read shows in the folder with why, not as 'none'", () => {
  assert.deepEqual(shape(otherKindFolders(REDIS, { status: 'error', items: [], error: 'la sesión está ocupada' })), [
    { kind: 'index', status: 'error', count: undefined, items: [], error: 'la sesión está ocupada' },
  ]);
});

test('no folders for a driver with keys only', () => {
  assert.deepEqual(otherKindFolders(ETCD, { status: 'ready', items: [obj('a')], error: null }), []);
  assert.deepEqual(otherKindFolders(undefined, undefined), []);
});

// -- "Mostrar en el explorador" (ExplorerSidebar.vue reveal) ------------------------------

test("the reveal nests a key by the separator, never a search index's name", () => {
  const redis = { key_search: { syntax: 'glob', separator: ':', types: [], case_sensitive: true }, object_kinds: REDIS };
  assert.equal(revealSeparator(redis, 'key'), ':');
  assert.equal(revealSeparator(redis, 'index'), undefined);
  // Not a key database, or no separator: nothing to nest by.
  assert.equal(revealSeparator({ key_search: null, object_kinds: [kind('table', 'Tablas')] }, 'table'), undefined);
  assert.equal(revealSeparator({ ...redis, key_search: { ...redis.key_search, separator: '' } }, 'key'), undefined);
  assert.equal(revealSeparator(undefined, 'key'), undefined);
});
