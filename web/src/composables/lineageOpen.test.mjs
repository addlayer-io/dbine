// Tests for lineageOpen.ts (the open flow with the cache). Run: `node --test src/composables/lineageOpen.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { openLineage, panelCalls, panelScope } from './lineageOpen.ts';
import { runChunks } from './lineageChunks.ts';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

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

// -- the panel's scope: the cache lookup, the whole read and the chunks ---------------------------
// Regression (the DBA's "I closed the lineage and it loads again"): the panel looked the cache up
// with the scope it was opened with, but read and stored with `jobs: true`. On every engine with
// jobs (Power BI, SQL Server, Oracle…) the lookup never found anything: each opening read the whole
// lineage again, a half-read one started over and the precompute was never shown.

/** A fake api that keeps what it is asked, and the scopes each call got. */
function recorder() {
  const scopes = { cached: [], fetch: [], chunk: [] };
  return {
    scopes,
    lineageGraphCached: async (_c, _d, s) => { scopes.cached.push(s); return { graph: null, updated_at: null }; },
    getLineageGraph: async (_c, _d, s) => { scopes.fetch.push(s); return G('s'); },
    getLineageChunk: async (_c, _d, s) => { scopes.chunk.push(s); return G('s'); },
  };
}

test('the panel looks the cache up with the scope it reads and stores with', async () => {
  for (const opened of [{}, { schemas: ['dbo'], kinds: ['view'], limit: 50 }]) {
    const api = recorder();
    const calls = panelCalls(api, 'c1', 'db', opened);
    await calls.cached();
    await calls.fetch();
    await calls.chunk({ step: null });
    const want = { ...opened, jobs: true };
    assert.deepEqual(api.scopes.cached, [want]);
    assert.deepEqual(api.scopes.fetch, [want]);
    assert.deepEqual(api.scopes.chunk, [want]);
    assert.equal(opened.jobs, undefined, 'the scope the panel was opened with is left as it was');
  }
  assert.deepEqual(panelScope({}), { jobs: true }, 'what the precompute reads (Rust panel_scope)');
});

/** The backend as far as the cache goes: a graph is kept under its scope's key (scope_key of
 *  effective_scope in lineage_cache.rs: the lists sorted, the jobs only where the engine has them). */
function backend(engineHasJobs) {
  const kept = new Map();
  const key = (s) => JSON.stringify({
    schemas: [...new Set(s.schemas ?? [])].sort(),
    kinds: [...new Set(s.kinds ?? [])].sort(),
    limit: s.limit ?? 0,
    jobs: !!s.jobs && engineHasJobs,
  });
  const reads = [];
  // Power BI's chunks: the structure, then one chunk of dependencies; the graph so far is kept after each.
  const deps = { stage: 'dependencies', offset: 0, len: 2, total: 2, skip: false };
  return {
    reads,
    kept,
    lineageGraphCached: async (_c, _d, s) => kept.get(key(s)) ?? { graph: null, updated_at: null },
    getLineageGraph: async (_c, _d, s) => {
      reads.push('whole');
      // The server takes a moment: the cache answers first.
      await later(5);
      const graph = { id: 'whole', notes: ['whole'], next: null };
      kept.set(key(s), { graph, updated_at: new Date().toISOString() });
      return graph;
    },
    getLineageChunk: async (_c, _d, s, req) => {
      reads.push(req.step?.stage ?? 'structure');
      const so_far = req.step ? kept.get(key(s)).graph.notes : [];
      const graph = req.step
        ? { notes: [...so_far, 'dependencies'], next: null }
        : { notes: ['structure'], next: deps };
      kept.set(key(s), { graph, updated_at: new Date().toISOString() });
      return graph;
    },
  };
}

const fresh = (at) => !!at && Date.now() - Date.parse(at) < 6 * 3600 * 1000;
async function openChunked(api, opts = {}) {
  let shown = null;
  let done = null;
  const calls = panelCalls(api, 'c1', 'model', {});
  await runChunks(
    { cached: calls.cached, chunk: calls.chunk },
    { cached: (g) => { shown = g; }, progress: () => {}, stage: () => {}, done: (g) => { done = g; }, failed: (e) => { throw e; } },
    { useCache: opts.useCache ?? true, reuseComplete: fresh, cancelled: opts.cancelled },
  );
  return { shown, done };
}

for (const engineHasJobs of [true, false]) {
  test(`a lineage read in chunks opens again from the cache (engine ${engineHasJobs ? 'with' : 'without'} jobs)`, async () => {
    const api = backend(engineHasJobs);
    // "Actualizar": everything is read and kept.
    await openChunked(api, { useCache: false });
    assert.deepEqual(api.reads, ['structure', 'dependencies']);
    // Closed and opened again: the kept graph, nothing read.
    api.reads.length = 0;
    const again = await openChunked(api);
    assert.deepEqual(api.reads, [], 'the second opening reads nothing');
    assert.deepEqual(again.done.notes, ['structure', 'dependencies']);
  });

  test(`a lineage closed half way goes on where it stopped (engine ${engineHasJobs ? 'with' : 'without'} jobs)`, async () => {
    const api = backend(engineHasJobs);
    // The panel is closed as soon as the first chunk is read (the backend kept it already).
    await openChunked(api, { cancelled: () => api.reads.length >= 1 });
    assert.deepEqual(api.reads, ['structure'], 'closed after the first chunk');
    api.reads.length = 0;
    const again = await openChunked(api);
    assert.deepEqual(api.reads, ['dependencies'], 'only what was missing, not the structure again');
    assert.deepEqual(again.done.notes, ['structure', 'dependencies']);
  });

  test(`a lineage read whole is drawn from the cache when opened again (engine ${engineHasJobs ? 'with' : 'without'} jobs)`, async () => {
    const api = backend(engineHasJobs);
    const calls = panelCalls(api, 'c1', 'db', {});
    await openLineage({ cached: calls.cached, fetch: calls.fetch }, { cached: () => {}, fresh: () => {}, failed: (e) => { throw e; } });
    const log = [];
    await openLineage(
      { cached: calls.cached, fetch: calls.fetch },
      { cached: (g) => log.push(['cached', g.id]), fresh: (g) => log.push(['fresh', g.id]), failed: (e) => { throw e; } },
    );
    assert.deepEqual(log[0], ['cached', 'whole'], 'the kept graph is drawn at once');
  });
}

test('the lineage is only read and looked up through panelCalls', () => {
  // Any other call site could send another scope and miss the cache again.
  const src = fileURLToPath(new URL('..', import.meta.url));
  const files = [];
  const walk = (d) => {
    for (const f of readdirSync(d)) {
      const p = join(d, f);
      if (statSync(p).isDirectory()) walk(p);
      else if (/\.(ts|vue)$/.test(f)) files.push(p);
    }
  };
  walk(src);
  const allowed = new Set(['api/client.ts', 'composables/lineageOpen.ts']);
  const offenders = files
    .filter((p) => !allowed.has(relative(src, p).split('\\').join('/')))
    .filter((p) => /\b(lineageGraphCached|getLineageGraph|getLineageChunk)\s*\(/.test(readFileSync(p, 'utf8')));
  assert.deepEqual(offenders.map((p) => relative(src, p)), []);
});
