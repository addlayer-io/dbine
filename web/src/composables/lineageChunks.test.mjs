// Tests for the chunked lineage loop (lineageChunks.ts).
// Run: `node --test src/composables/lineageChunks.test.mjs` from web/ (Node 23.6+ strips the types).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';

register('data:text/javascript,' + encodeURIComponent(`
export async function resolve(spec, ctx, next) {
  if (spec.startsWith('.') && !/\\.[cm]?[jt]s$/.test(spec)) return next(spec + '.ts', ctx);
  return next(spec, ctx);
}`));
const { runChunks, retryChunk, isPartial, failedObjects } = await import('./lineageChunks.ts');

const st = (stage, offset = 0, len = 2, total = 6) => ({ stage, offset, len, total });
const graph = (next, extra = {}) => ({ objects: [], edges: [], scanned: 0, unreadable: [], truncated: false, notes: [], next, failed: [], ...extra });
const PLAN = [st('structure', 0, 0, 0), st('dependencies', 0), st('dependencies', 2), st('consumers', 0, 0, 0)];
const at = (step) => (step ? `${step.stage}@${step.offset}` : 'start');
// A server that answers the plan in order; `bad` chunks fail that many times.
function server({ bad = new Map(), cached = null } = {}) {
  const calls = [];
  const deps = {
    cached: async () => cached ?? { graph: null, updated_at: null },
    chunk: async (req) => {
      calls.push(req);
      const k = at(req.step);
      const i = req.step ? PLAN.findIndex((p) => at(p) === k) : -1;
      if (!req.giveUp && (bad.get(k) ?? 0) > 0) {
        bad.set(k, bad.get(k) - 1);
        throw new Error('timeout');
      }
      const failed = req.giveUp ? [{ ...req.step, skip: false }] : [];
      // The start answers the structure, which says the first dependencies are next.
      const next = req.step ? PLAN[i + 1] ?? null : PLAN[1];
      return graph(next, { failed, notes: [k] });
    },
  };
  return { deps, calls };
}
function events() {
  const log = [];
  return {
    log,
    on: {
      cached: () => log.push('cached'),
      progress: () => log.push('progress'),
      stage: () => log.push('stage'),
      done: (g) => log.push(`done:${g.failed.length}`),
      failed: (_e, kept) => log.push(`failed:${kept}`),
    },
  };
}

test('it asks chunk after chunk until nothing is next, laying out at each stage', async () => {
  const { deps, calls } = server();
  const { on, log } = events();
  await runChunks(deps, on);
  assert.deepEqual(calls.map((c) => at(c.step)), ['start', 'dependencies@0', 'dependencies@2', 'consumers@0']);
  assert.equal(log.at(-1), 'done:0');
  // Laid out when the structure came, when the dependencies ended and at the end: not between their chunks.
  assert.equal(log.filter((x) => x === 'stage').length, 3);
  assert.equal(log.filter((x) => x === 'progress').length, 4);
});

test('a failing chunk is asked once more; a second failure gives it up and the loop goes on', async () => {
  const once = server({ bad: new Map([['dependencies@0', 1]]) });
  let e = events();
  await runChunks(once.deps, e.on);
  assert.equal(once.calls.filter((c) => at(c.step) === 'dependencies@0').length, 2);
  assert.equal(e.log.at(-1), 'done:0');

  const twice = server({ bad: new Map([['dependencies@0', 2]]) });
  e = events();
  await runChunks(twice.deps, e.on);
  assert.ok(twice.calls.some((c) => c.giveUp && c.step.offset === 0), 'given up');
  assert.equal(twice.calls.at(-1).step.stage, 'consumers', 'and the loop went on to the end');
});

test('closing the panel stops the loop between two chunks', async () => {
  const { deps, calls } = server();
  const { on } = events();
  let n = 0;
  await runChunks(deps, on, { cancelled: () => n++ > 2 });
  assert.ok(calls.length < 4, `${calls.length}`);
});

test('a start that fails ends with the error', async () => {
  const { deps } = server({ bad: new Map([['start', 2]]) });
  const { on, log } = events();
  await runChunks(deps, on);
  assert.deepEqual(log, ['failed:false']);
});

test('a half-read cached graph resumes at its next chunk and a complete one is reused', async () => {
  const half = server({ cached: { graph: graph(st('dependencies', 2)), updated_at: 'x' } });
  await runChunks(half.deps, events().on);
  assert.deepEqual(half.calls.map((c) => at(c.step)), ['dependencies@2', 'consumers@0']);

  const whole = server({ cached: { graph: graph(null), updated_at: 'x' } });
  const e = events();
  await runChunks(whole.deps, e.on, { reuseComplete: () => true });
  assert.equal(whole.calls.length, 0);
  assert.deepEqual(e.log, ['cached', 'done:0']);
  // Without reuse (the "Actualizar" button) the cache isn't asked and it starts over.
  const again = server({ cached: { graph: graph(null), updated_at: 'x' } });
  await runChunks(again.deps, events().on, { useCache: false });
  assert.equal(again.calls[0].step, null);
});

test('a given-up chunk is read again on its own and the graph says when it is partial', async () => {
  const { deps, calls } = server();
  await retryChunk(deps, st('dependencies', 0));
  assert.deepEqual([calls[0].retry, calls[0].step.offset], [true, 0]);
  assert.equal(isPartial(graph(null)), false);
  assert.equal(isPartial(graph(st('consumers'))), true);
  assert.equal(isPartial(graph(null, { failed: [st('dependencies', 0, 50)] })), true);
  assert.equal(failedObjects(graph(null, { failed: [st('dependencies', 0, 50), st('dependencies', 50, 20)] })), 70);
});
