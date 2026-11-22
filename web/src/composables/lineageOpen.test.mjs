// Tests for lineageOpen.ts (the open flow with the cache). Run: `node --test src/composables/lineageOpen.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { openLineage } from './lineageOpen.ts';

const G = (id) => ({ id });
const later = (ms, v, fail = false) => new Promise((res, rej) => setTimeout(() => (fail ? rej(v) : res(v)), ms));
function run(deps, opts) {
  const log = [];
  return openLineage(deps, {
    cached: (g, at) => log.push(['cached', g.id, at]),
    fresh: (g) => log.push(['fresh', g.id]),
    failed: (e, kept) => log.push(['failed', String(e), kept]),
  }, opts).then(() => log);
}

test('no cache: only the server graph', async () => {
  const log = await run({ cached: async () => ({ graph: null, updated_at: null }), fetch: () => later(5, G('s')) });
  assert.deepEqual(log, [['fresh', 's']]);
});
test('cache first, then the server replaces it', async () => {
  const log = await run({ cached: async () => ({ graph: G('c'), updated_at: 'T' }), fetch: () => later(20, G('s')) });
  assert.deepEqual(log, [['cached', 'c', 'T'], ['fresh', 's']]);
});
test('server failure keeps the cached graph', async () => {
  const log = await run({ cached: async () => ({ graph: G('c'), updated_at: 'T' }), fetch: () => later(10, 'boom', true) });
  assert.deepEqual(log, [['cached', 'c', 'T'], ['failed', 'boom', true]]);
});
test('server failure with no cache is a plain error', async () => {
  const log = await run({ cached: async () => ({ graph: null, updated_at: null }), fetch: () => later(5, 'boom', true) });
  assert.deepEqual(log, [['failed', 'boom', false]]);
});
test('a cache lookup that fails counts as no cache', async () => {
  const log = await run({ cached: () => Promise.reject(new Error('x')), fetch: () => later(5, G('s')) });
  assert.deepEqual(log, [['fresh', 's']]);
});
test('a server answer that beats the cache is not overwritten', async () => {
  const log = await run({ cached: () => later(30, { graph: G('c'), updated_at: 'T' }), fetch: async () => G('s') });
  assert.deepEqual(log, [['fresh', 's']]);
});
test('Actualizar skips the cache and keeps what is shown on failure', async () => {
  let asked = false;
  const log = await run({ cached: async () => { asked = true; return { graph: G('c'), updated_at: 'T' }; }, fetch: () => later(5, 'boom', true) }, { useCache: false, shown: true });
  assert.equal(asked, false);
  assert.deepEqual(log, [['failed', 'boom', true]]);
});
