// Tests for the key-binding helpers. Run: `node --test src/composables/keybindings.test.mjs`
// from web/ (Node 23.6+ strips the TypeScript types itself). Not part of the
// app's build: nothing imports this file.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { eventKey, findConflicts, formatKey, isReservedKey, normalizeKey, toAccelerator } from './keybindings.ts';

const ev = (o) => ({ key: '', code: '', metaKey: false, ctrlKey: false, altKey: false, shiftKey: false, ...o });

test('spellings of the same keys compare equal', () => {
  assert.equal(normalizeKey('Shift-Mod-Enter', true), 'Mod-Shift-Enter');
  assert.equal(normalizeKey('mod-shift-enter', false), 'Mod-Shift-Enter');
  assert.equal(normalizeKey('Shift-Alt-F', true), 'Alt-Shift-f');
  assert.equal(normalizeKey('Mod--', true), 'Mod--');
  // Ctrl is the Control key on macOS, and Mod elsewhere.
  assert.equal(normalizeKey('Ctrl-Space', true), 'Ctrl-Space');
  assert.equal(normalizeKey('Ctrl-Space', false), 'Mod-Space');
  assert.equal(normalizeKey('Hyper-x', true), null);
});

test('events become the bindings\' form', () => {
  assert.equal(eventKey(ev({ key: 'Enter', code: 'Enter', metaKey: true }), true), 'Mod-Enter');
  assert.equal(eventKey(ev({ key: 'Enter', code: 'Enter', ctrlKey: true }), false), 'Mod-Enter');
  // ⌥G types "©" on macOS: the physical key counts.
  assert.equal(eventKey(ev({ key: '©', code: 'KeyG', metaKey: true, altKey: true }), true), 'Mod-Alt-g');
  assert.equal(eventKey(ev({ key: 'L', code: 'KeyL', metaKey: true, shiftKey: true }), true), 'Mod-Shift-l');
  assert.equal(eventKey(ev({ key: 'Shift', code: 'ShiftLeft', shiftKey: true }), true), null);
  // The Windows key isn't ours.
  assert.equal(eventKey(ev({ key: 'b', code: 'KeyB', metaKey: true }), false), null);
});

test('labels and menu accelerators', () => {
  assert.equal(formatKey('Mod-Shift-Enter', true), '⇧⌘↵');
  assert.equal(formatKey('Mod-Shift-Enter', false), 'Ctrl+Shift+Enter');
  assert.equal(formatKey('Mod-Alt-g', true), '⌥⌘G');
  assert.equal(toAccelerator('Mod-Shift-n', true), 'CmdOrCtrl+Shift+N');
  assert.equal(toAccelerator('Mod-,', true), 'CmdOrCtrl+,');
  assert.equal(toAccelerator(null, true), undefined);
});

test('conflicts: same keys where both act', () => {
  const c = findConflicts([
    { id: 'a', key: 'Mod-b', scope: 'global' },
    { id: 'b', key: 'mod-B', scope: 'editor' },
    { id: 'c', key: 'Mod-l', scope: 'editor' },
    { id: 'd', key: 'Mod-l', scope: 'editor' },
    { id: 'e', key: null, scope: 'global' },
    { id: 'f', key: null, scope: 'global' },
    { id: 'g', key: 'Mod-k', scope: 'global' },
  ], true);
  assert.deepEqual(c.get('a'), ['b']);
  assert.deepEqual(c.get('b'), ['a']);
  assert.deepEqual(c.get('c'), ['d']);
  assert.deepEqual(c.get('d'), ['c']);
  // No keys never clash, and a key nobody else has doesn't either.
  assert.equal(c.has('e'), false);
  assert.equal(c.has('f'), false);
  assert.equal(c.has('g'), false);
});

test('reserved system keys', () => {
  assert.equal(isReservedKey('Mod-c', true), true);
  assert.equal(isReservedKey('Shift-Mod-z', false), true);
  assert.equal(isReservedKey('Mod-k', true), false);
  assert.equal(isReservedKey('Alt-F4', false), true);
  assert.equal(isReservedKey('Alt-F4', true), false);
});
