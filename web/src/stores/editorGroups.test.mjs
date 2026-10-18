// Tests for the split editor's group logic. Run: `node --test src/stores/editorGroups.test.mjs`
// from web/ (Node 23.6+ strips the TypeScript types itself). Not part of the
// app's build: nothing imports this file.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  MAX_GROUPS, activeOf, focusGroup, groupOf, isShown, moveTab, removeGroup, resizeGroups,
  restoreLayout, savedGroups, slotFor, splitInto, splitTab, tabsOf, afterLeaving,
} from './editorGroups.ts';

const tab = (id, connectionId = 'c1', extra = {}) => ({ id, connectionId, preview: false, ...extra });
const ids = (s, gid) => tabsOf(s, gid).map((t) => t.id);
let n = 0;
const gid = () => `g${++n}`;

test('a save from before split editors restores as one group', () => {
  const s = restoreLayout({ tabs: [tab('a'), tab('b'), tab('c', 'c2')], activeId: 'b', collapsed: ['c2', 'gone'] }, 'main');
  assert.equal(s.groups.length, 1);
  assert.equal(s.focused, 'main');
  assert.equal(s.activeId, 'b');
  assert.equal(s.split, 'row');
  assert.deepEqual(s.tabs.map((t) => t.group), ['main', 'main', 'main']);
  assert.equal(s.groups[0].size, 1);
  // A collapsed connection keeps collapsed while it has tabs.
  assert.deepEqual(s.collapsed, ['c2']);
});

test('an empty or broken save gives one empty group', () => {
  for (const saved of [{}, { tabs: [] }, { tabs: [], groups: [{ nope: 1 }] }]) {
    const s = restoreLayout(saved, 'x');
    assert.equal(s.groups.length, 1);
    assert.equal(s.activeId, null);
  }
});

test('two groups restore with their tabs, active tabs, focus and sizes', () => {
  const saved = {
    tabs: [tab('a', 'c1', { group: 'L' }), tab('b', 'c1', { group: 'R' }), tab('c', 'c1', { group: 'R' }), tab('d', 'c1', { group: 'zzz' })],
    groups: [{ id: 'L', activeId: 'a', size: 3 }, { id: 'R', activeId: 'c', size: 1 }, { id: 'E', activeId: null, size: 1 }],
    focused: 'R', split: 'column', activeId: 'c',
  };
  const s = restoreLayout(saved, 'x');
  assert.deepEqual(s.groups.map((g) => g.id), ['L', 'R']); // E had no tabs
  assert.deepEqual(ids(s, 'L'), ['a', 'd']); // d named an unknown group
  assert.equal(s.focused, 'R');
  assert.equal(s.activeId, 'c');
  assert.equal(activeOf(s, 'L'), 'a');
  assert.equal(s.split, 'column');
  assert.equal(s.groups[0].size, 0.75);
  // And saving gives it back.
  assert.deepEqual(savedGroups(s).map((g) => g.activeId), ['a', 'c']);
});

test('an active tab that is not in its group is replaced by the first one', () => {
  const s = restoreLayout({ tabs: [tab('a', 'c1', { group: 'L' }), tab('b', 'c1', { group: 'R' })], groups: [{ id: 'L', activeId: 'b', size: 1 }, { id: 'R', activeId: 'nope', size: 1 }] }, 'x');
  assert.equal(activeOf(s, 'L'), 'a');
  assert.equal(activeOf(s, 'R'), 'b');
});

function twoGroups() {
  const s = restoreLayout({ tabs: [tab('a'), tab('b'), tab('c')], activeId: 'b' }, 'L');
  const r = splitTab(s, 'row', 'R');
  assert.equal(r, 'R');
  return s;
}

test('split moves the active tab into a new group to the right, focused', () => {
  const s = twoGroups();
  assert.deepEqual(ids(s, 'L'), ['a', 'c']);
  assert.deepEqual(ids(s, 'R'), ['b']);
  assert.equal(s.focused, 'R');
  assert.equal(s.activeId, 'b');
  // The left group shows the moved tab's neighbor.
  assert.equal(activeOf(s, 'L'), 'c');
  assert.deepEqual(s.groups.map((g) => g.size), [0.5, 0.5]);
  assert.ok(isShown(s, s.tabs.find((t) => t.id === 'b')));
  assert.ok(isShown(s, s.tabs.find((t) => t.id === 'c')));
  assert.ok(!isShown(s, s.tabs.find((t) => t.id === 'a')));
});

test('a tab alone in its group splits into an empty group beside it', () => {
  const s = restoreLayout({ tabs: [tab('a')], activeId: 'a' }, 'L');
  assert.equal(splitTab(s, 'column', 'R'), 'R');
  assert.equal(s.split, 'column');
  assert.deepEqual(ids(s, 'L'), ['a']);
  assert.deepEqual(ids(s, 'R'), []);
  assert.equal(s.focused, 'R');
  assert.equal(s.activeId, null);
  assert.equal(activeOf(s, 'L'), 'a');
});

test('no more than MAX_GROUPS groups', () => {
  const s = restoreLayout({ tabs: ['a', 'b', 'c', 'd', 'e'].map((x) => tab(x)), activeId: 'a' }, 'L');
  for (let i = 1; i < MAX_GROUPS; i++) assert.ok(splitTab(s, 'row', gid(), s.tabs[i].id));
  assert.equal(s.groups.length, MAX_GROUPS);
  assert.equal(splitTab(s, 'row', gid(), 'e'), null);
  assert.equal(s.groups.length, MAX_GROUPS);
});

test('focus switches which group activeId speaks for', () => {
  const s = twoGroups();
  focusGroup(s, 'L');
  assert.equal(s.activeId, 'c');
  assert.equal(activeOf(s, 'R'), 'b');
  focusGroup(s, 'R');
  assert.equal(s.activeId, 'b');
  assert.equal(activeOf(s, 'L'), 'c');
});

test('new tabs go into the focused group, next to its active tab', () => {
  const s = twoGroups();
  // Focused R (active b): after b.
  assert.equal(slotFor(s, 'c1'), s.tabs.findIndex((t) => t.id === 'b') + 1);
  // Another connection: at the end of R.
  s.tabs.splice(slotFor(s, 'c9'), 0, tab('z', 'c9', { group: 'R' }));
  assert.deepEqual(ids(s, 'R'), ['b', 'z']);
  // In L with its active tab c of c1.
  assert.equal(slotFor(s, 'c1', 'L'), s.tabs.findIndex((t) => t.id === 'c') + 1);
});

test('closing a group\'s last tab closes the group and gives the focus and size away', () => {
  const s = twoGroups();
  resizeGroups(s, 1, 0.3);
  const at = s.tabs.findIndex((t) => t.id === 'b');
  const [gone] = s.tabs.splice(at, 1);
  afterLeaving(s, gone, at, groupOf(s, gone));
  assert.equal(s.groups.length, 1);
  assert.equal(s.focused, 'L');
  assert.equal(s.activeId, 'c');
  assert.equal(s.groups[0].size, 1);
});

test('closing the active tab shows a neighbor of the same group', () => {
  const s = restoreLayout({ tabs: [tab('a'), tab('b'), tab('c'), tab('d')], activeId: 'a' }, 'L');
  splitTab(s, 'row', 'R', 'c'); // L: a b d · R: c
  focusGroup(s, 'L');
  s.activeId = 'b';
  const at = s.tabs.findIndex((t) => t.id === 'b');
  const [gone] = s.tabs.splice(at, 1);
  afterLeaving(s, gone, at, 'L');
  // Its right-hand neighbor in the flat list is c, but c is in R: d.
  assert.equal(s.activeId, 'd');
  assert.equal(activeOf(s, 'R'), 'c');
});

test('dragging a tab into the other group moves it and pins it', () => {
  const s = twoGroups(); // L: a c · R: b
  s.tabs.find((t) => t.id === 'a').preview = true;
  moveTab(s, 'a', 'R', 'b');
  assert.deepEqual(ids(s, 'R'), ['a', 'b']);
  assert.deepEqual(ids(s, 'L'), ['c']);
  assert.equal(s.tabs.find((t) => t.id === 'a').preview, false);
  assert.equal(s.focused, 'R');
  assert.equal(s.activeId, 'a');
});

test('reordering inside a group keeps the group', () => {
  const s = restoreLayout({ tabs: [tab('a'), tab('b'), tab('c')], activeId: 'a' }, 'L');
  moveTab(s, 'c', 'L', 'a');
  assert.deepEqual(ids(s, 'L'), ['c', 'a', 'b']);
  moveTab(s, 'c', 'L', null);
  assert.deepEqual(ids(s, 'L'), ['a', 'b', 'c']);
  assert.equal(s.groups.length, 1);
});

test('moving a group\'s only tab away closes that group', () => {
  const s = twoGroups(); // L: a c · R: b
  moveTab(s, 'b', 'L');
  assert.equal(s.groups.length, 1);
  assert.deepEqual(ids(s, 'L'), ['a', 'c', 'b']);
  assert.equal(s.focused, 'L');
  assert.equal(s.activeId, 'b');
});

test('dropping a tab on another group\'s bottom half splits it there', () => {
  const s = twoGroups(); // L: a c · R: b
  const g = splitInto(s, 'column', 'R', 'B', 'a');
  assert.equal(g, 'B');
  assert.deepEqual(s.groups.map((x) => x.id), ['L', 'R', 'B']);
  assert.equal(s.split, 'column');
  assert.deepEqual(ids(s, 'B'), ['a']);
  assert.equal(s.activeId, 'a');
});

test('dropping a group\'s only tab on its own half does nothing', () => {
  const s = twoGroups();
  assert.equal(splitInto(s, 'row', 'R', 'X', 'b'), null);
  assert.equal(s.groups.length, 2);
});

test('removing an empty group gives its place back', () => {
  const s = restoreLayout({ tabs: [tab('a')], activeId: 'a' }, 'L');
  splitTab(s, 'row', 'R');
  removeGroup(s, 'R');
  assert.equal(s.groups.length, 1);
  assert.equal(s.focused, 'L');
  assert.equal(s.activeId, 'a');
});

test('the divider keeps each group at 10% at least', () => {
  const s = twoGroups();
  resizeGroups(s, 1, 0.01);
  assert.equal(s.groups[0].size, 0.1);
  resizeGroups(s, 1, 2);
  assert.equal(Math.round(s.groups[0].size * 100), 90);
  assert.equal(Math.round((s.groups[0].size + s.groups[1].size) * 100), 100);
});
