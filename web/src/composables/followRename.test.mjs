// An open query tab keeps its own copy of the saved query, and autosave writes
// that copy back: it has to follow a rename made elsewhere (#229). Run:
// `node --test src/composables/followRename.test.mjs` from web/. Not part of
// the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { effectScope, nextTick, reactive, ref } from 'vue';
import { followRename } from './followRename.ts';

const saved = (over = {}) => ({
  id: 'q1', connection_id: 'c1', database: 'main', name: 'Query 1', sql: '', updated_at: '1', last_run_at: null, ...over,
});

/** A tab's copy and the explorer's list, which replaces an entry on each save. */
function setup({ loaded = true, listed = true } = {}) {
  const open = ref(loaded ? saved() : null);
  const list = reactive({ items: listed ? [saved()] : [] });
  const scope = effectScope();
  scope.run(() => followRename(open, () => open.value ? list.items.find((x) => x.id === open.value.id) : undefined));
  return { open, list, stop: () => scope.stop() };
}

test('a rename from the explorer reaches the open tab, so autosave keeps it', async () => {
  const { open, list, stop } = setup();
  list.items[0] = saved({ name: '1_New', updated_at: '2' });
  await nextTick();
  // What autosave sends after the next keystroke.
  assert.equal({ ...open.value, sql: 'select 1' }.name, '1_New');
  stop();
});

test('only the name is taken: the text being edited is left alone', async () => {
  const { open, list, stop } = setup();
  open.value = saved({ sql: 'select 2' });
  list.items[0] = saved({ name: '1_New', sql: 'select 1', updated_at: '2' });
  await nextTick();
  assert.deepEqual(open.value, saved({ name: '1_New', sql: 'select 2' }));
  stop();
});

test('a second rename is followed too', async () => {
  const { open, list, stop } = setup();
  for (const name of ['1_New', '2_New']) {
    list.items[0] = saved({ name });
    await nextTick();
    assert.equal(open.value.name, name);
  }
  stop();
});

test('a list that was already there when the tab loaded does not override it', async () => {
  const { open, list, stop } = setup({ loaded: false });
  list.items[0] = saved({ name: 'stale' });
  open.value = saved({ name: 'fresh' });
  await nextTick();
  assert.equal(open.value.name, 'fresh');
  stop();
});

test('an entry that leaves or enters the list changes nothing', async () => {
  const { open, list, stop } = setup();
  list.items.splice(0, 1);
  await nextTick();
  assert.equal(open.value.name, 'Query 1');
  list.items.push(saved({ name: 'other' }));
  await nextTick();
  assert.equal(open.value.name, 'Query 1');
  stop();
});
