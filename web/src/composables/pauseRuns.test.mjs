// A query editor not shown for a while releases its result rows and reads them
// back from the results store (#216). Run:
// `node --test src/composables/pauseRuns.test.mjs` from web/. Not part of the
// app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { onWakeAll, PAUSE_AFTER_MS, pauseRuns, wakeAll, wakeRun } from './pauseRuns.ts';

const outcome = (over = {}) => ({
  results: [{ columns: [{ name: 'a' }], rows: [[1], [2]] }], messages: [], error: null, elapsed_ms: 1, plans: [], log: [], errors: [], ...over,
});
const run = (over = {}) => ({ id: 1, key: 'k1', outcome: outcome(), pinned: false, ...over });
const pane = { active: 0, view: 'table', sorts: {}, grid: null, chart: null, map: null };

function deps(over = {}) {
  const log = { held: [], released: [] };
  return {
    log,
    live: () => null,
    hidden: () => true,
    busy: () => false,
    capture: (r) => ({ pane, outcome: r.outcome }),
    hold: async (r) => { log.held.push(r.key); return true; },
    release: (keys) => log.released.push(...keys),
    ...over,
  };
}

test('the wait is thirty minutes', () => assert.equal(PAUSE_AFTER_MS, 1_800_000));

test('a hidden run the store confirms gives up its rows', async () => {
  const r = run();
  const again = await pauseRuns([r], deps());
  assert.equal(r.outcome, null);
  assert.equal(r.restored, true);
  assert.equal(r.paused, true);
  assert.deepEqual(r.pane, pane);
  assert.equal(again, false);
});

test('without the store\'s confirmation the rows stay', async () => {
  for (const hold of [async () => false, async () => { throw new Error('no keychain'); }]) {
    const r = run();
    const kept = r.outcome;
    assert.equal(await pauseRuns([r], deps({ hold })), true);
    assert.equal(r.outcome, kept);
    assert.ok(!r.paused && !r.restored);
  }
});

test('the run in progress, a busy pane and a shown editor are left alone', async () => {
  const live = run();
  for (const d of [deps({ live: () => live }), deps({ busy: () => true }), deps({ hidden: () => false })]) {
    const kept = live.outcome;
    assert.equal(await pauseRuns([live], d), true);
    assert.equal(live.outcome, kept);
    assert.deepEqual(d.log.held, []);
  }
});

test('errors, empty results and runs already out of memory are not written', async () => {
  const d = deps();
  const runs = [
    run({ outcome: outcome({ error: 'boom' }) }),
    run({ outcome: outcome({ errors: [{ message: 'x' }] }) }),
    run({ outcome: outcome({ results: [{ columns: [], rows: [] }] }) }),
    run({ outcome: null, restored: true }),
    run({ restored: true }),
  ];
  assert.equal(await pauseRuns(runs, d), false);
  assert.deepEqual(d.log.held, []);
});

test('shown again while the store writes: the rows stay and the hold goes', async () => {
  let hidden = true;
  const r = run();
  const d = deps({ hidden: () => hidden, hold: async () => { hidden = false; return true; } });
  await pauseRuns([r], d);
  assert.ok(r.outcome && !r.paused);
  assert.deepEqual(d.log.released, ['k1']);
});

test('run again or closed while the store writes: nothing is released from memory', async () => {
  const fresh = outcome({ elapsed_ms: 9 });
  const r = run();
  await pauseRuns([r], deps({ hold: async () => { r.outcome = fresh; return true; } }));
  assert.equal(r.outcome, fresh);
  const closed = run();
  const runs = [closed];
  const d = deps({ hold: async () => { runs.length = 0; return true; } });
  await pauseRuns(runs, d);
  assert.ok(closed.outcome);
  assert.deepEqual(d.log.released, ['k1']);
});

test('waking reads the rows back and lets the store\'s cap have them again', async () => {
  const r = run({ outcome: null, restored: true, paused: true, pane });
  const d = deps();
  await wakeRun(r, { load: async (x) => { x.outcome = outcome(); x.restored = false; return true; }, release: d.release, lost: 'gone' });
  assert.equal(r.paused, false);
  assert.deepEqual(r.pane, pane);
  assert.deepEqual(d.log.released, ['k1']);
});

test('rows that can\'t be read are said so, never shown as an empty result or saved', async () => {
  const r = run({ outcome: null, restored: true, paused: true, pane });
  const d = deps();
  const empty = outcome({ results: [] });
  await wakeRun(r, { load: async (x) => { x.outcome = empty; return false; }, release: d.release, lost: 'gone' });
  assert.deepEqual(r.outcome.log, [{ level: 'error', text: 'gone', statement: null, code: null, line: null }]);
  assert.equal(r.restored, true);
  assert.equal(r.paused, false);
  assert.equal(r.pane, null);
  assert.deepEqual(d.log.released, []);
});

test('before the saved results are cleared, every paused editor wakes', async () => {
  const woken = [];
  const off = onWakeAll(async () => { woken.push('a'); });
  onWakeAll(async () => { throw new Error('one fails'); });
  const off2 = onWakeAll(async () => { woken.push('b'); });
  await wakeAll();
  assert.deepEqual(woken, ['a', 'b']);
  off();
  off2();
});
