// Tests for the editors' function catalogs (functionCatalog.ts): read once
// per connection as it is now and database, never connecting, read again
// after a reconnect or an edit, a failed read tried again after a while, and
// a late answer for a connection that moved on dropped. Run: `node --test
// src/composables/functionCatalog.test.mjs` from web/ (Node 23.6+ strips the
// TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';

// functionCatalog.ts imports its siblings without an extension (as Vite
// does): resolve them to the .ts files. The backend call and the connections
// store are stand-ins the tests drive (`globalThis.fake`).
register('data:text/javascript,' + encodeURIComponent(`
const stubs = {
  '../api/functions': 'export const functionsApi = { catalog: (c, d) => globalThis.fake.catalog(c, d) };',
  '../stores/connections': 'export const useConnectionsStore = () => globalThis.fake.store;',
};
export async function resolve(spec, ctx, next) {
  if (stubs[spec]) return { url: 'data:text/javascript,' + encodeURIComponent(stubs[spec]), shortCircuit: true };
  if (spec.startsWith('.') && !/\\.[cm]?[jt]s$/.test(spec)) return next(spec + '.ts', ctx);
  return next(spec, ctx);
}`));

/** A backend whose answers the test gives (`answer`, `fail`), and a store
 *  whose connections it opens and closes. */
function fakes() {
  const asked = [];
  const store = {
    live: {},
    isConnected(id) { return this.live[id]?.status === 'connected'; },
  };
  globalThis.fake = {
    store,
    catalog(connectionId, database) {
      let answer, fail;
      const p = new Promise((res, rej) => { answer = res; fail = rej; });
      asked.push({ connectionId, database, answer, fail });
      return p;
    },
  };
  const connect = (id) => { store.live[id] = { status: 'connected' }; };
  return { asked, store, connect };
}
const F = (name) => ({ name, kind: 'scalar', args: '', returns: '', description: '', schema: '' });
const tick = () => new Promise((r) => setTimeout(r, 0));

const { functionCatalog, loadFunctionCatalog, shouldLoad, settled, catalogOf, RETRY_MS } = await import('./functionCatalog.ts');

test('asking never connects; an open connection is read once per database', async () => {
  const { asked, connect } = fakes();
  loadFunctionCatalog('c1', 'ventas');
  assert.equal(asked.length, 0, 'not connected: nothing read');
  assert.equal(functionCatalog('c1', 'ventas'), null);
  connect('c1');
  loadFunctionCatalog('c1', 'ventas');
  loadFunctionCatalog('c1', 'ventas'); // while it reads
  assert.equal(asked.length, 1);
  asked[0].answer([F('upper')]);
  await tick();
  assert.deepEqual(functionCatalog('c1', 'ventas').map((f) => f.name), ['upper']);
  loadFunctionCatalog('c1', 'ventas');
  assert.equal(asked.length, 1, 'read: not asked again');
  // Another database is another catalog.
  assert.equal(functionCatalog('c1', 'rrhh'), null);
  loadFunctionCatalog('c1', 'rrhh');
  assert.deepEqual(asked.map((a) => a.database), ['ventas', 'rrhh']);
  assert.equal(functionCatalog(null, 'ventas'), null);
});

test('a reconnect or an edit (a new live entry) reads it again; a closed connection has none', async () => {
  const { asked, store, connect } = fakes();
  connect('c2');
  loadFunctionCatalog('c2', '');
  asked[0].answer([F('upper')]);
  await tick();
  assert.ok(functionCatalog('c2', ''));
  // Reconnected: the store replaced its live entry.
  connect('c2');
  assert.equal(functionCatalog('c2', ''), null, 'the old one is not shown');
  loadFunctionCatalog('c2', '');
  assert.equal(asked.length, 2);
  asked[1].answer([F('lower')]);
  await tick();
  assert.deepEqual(functionCatalog('c2', '').map((f) => f.name), ['lower']);
  // Closed: no live entry, no catalog, and asking doesn't read.
  delete store.live.c2;
  assert.equal(functionCatalog('c2', ''), null);
  loadFunctionCatalog('c2', '');
  assert.equal(asked.length, 2);
});

test('an answer that arrives after the connection moved on is dropped', async () => {
  const { asked, connect } = fakes();
  connect('c3');
  loadFunctionCatalog('c3', '');
  connect('c3'); // reconnected while the first read ran
  loadFunctionCatalog('c3', '');
  assert.equal(asked.length, 2);
  asked[0].answer([F('old')]);
  await tick();
  assert.equal(functionCatalog('c3', ''), null, "the old session's answer isn't kept");
  asked[1].answer([F('new')]);
  await tick();
  assert.deepEqual(functionCatalog('c3', '').map((f) => f.name), ['new']);
});

test('a failed read is tried again only after a while; an empty catalog is final', async () => {
  const { asked, connect } = fakes();
  connect('c4');
  loadFunctionCatalog('c4', '');
  asked[0].fail(new Error('busy'));
  await tick();
  assert.equal(functionCatalog('c4', ''), null);
  loadFunctionCatalog('c4', '');
  assert.equal(asked.length, 1, 'not right away');
  // An engine without one (or a read the server took too long for): [] is kept.
  connect('c5');
  loadFunctionCatalog('c5', '');
  asked[1].answer([]);
  await tick();
  assert.deepEqual(functionCatalog('c5', ''), []);
  loadFunctionCatalog('c5', '');
  assert.equal(asked.length, 2);
});

test('the load decision and the settle guard', () => {
  const live = {};
  const other = {};
  const now = 1_000_000;
  assert.equal(shouldLoad(undefined, live, now), true);
  assert.equal(shouldLoad({ live, list: null, loading: true, failedAt: 0 }, live, now), false);
  assert.equal(shouldLoad({ live, list: [], loading: false, failedAt: 0 }, live, now), false);
  // Read for the connection as it was: again.
  assert.equal(shouldLoad({ live: other, list: [], loading: false, failedAt: 0 }, live, now), true);
  assert.equal(shouldLoad({ live: other, list: null, loading: true, failedAt: 0 }, live, now), true);
  // Failed: again only after RETRY_MS.
  const failed = { live, list: null, loading: false, failedAt: now };
  assert.equal(shouldLoad(failed, live, now + RETRY_MS - 1), false);
  assert.equal(shouldLoad(failed, live, now + RETRY_MS), true);
  // A settle for the live entry the key holds now, and only for it.
  const loading = { live, list: null, loading: true, failedAt: 0 };
  assert.deepEqual(settled(loading, live, [F('a')], now), { live, list: [F('a')], loading: false, failedAt: 0 });
  assert.deepEqual(settled(loading, live, null, now), { live, list: null, loading: false, failedAt: now });
  assert.equal(settled({ ...loading, live: other }, live, [F('a')], now), null);
  assert.equal(settled(undefined, live, [F('a')], now), null);
  assert.equal(catalogOf({ live, list: [F('a')], loading: false, failedAt: 0 }, other), null);
});
