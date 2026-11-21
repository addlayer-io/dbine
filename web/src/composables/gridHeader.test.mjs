// Tests for the results grid's header rows with each column's data type
// under its name (gridHeader.ts). Run:
// `node --test src/composables/gridHeader.test.mjs` from web/ (Node 23.6+
// strips the TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { headerLayout, showsTypes } from './gridHeader.ts';
import { revealCell } from './gridReveal.ts';

const typed = [{ name: 'id', type_name: 'int' }, { name: 'fecha', type_name: 'datetime' }];

test('the types show when the setting is on and the engine gave some column a type', () => {
  assert.equal(showsTypes(typed, true), true);
  assert.equal(showsTypes(typed, false), false);
  // An engine that gives no types (or only for some columns).
  assert.equal(showsTypes([{ name: 'k', type_name: '' }, { name: 'v', type_name: '' }], true), false);
  assert.equal(showsTypes([{ name: 'k', type_name: '' }, { name: 'v', type_name: 'text' }], true), true);
  assert.equal(showsTypes([], true), false);
});

test('without types the header is one data row tall, as before', () => {
  assert.deepEqual(headerLayout(22, false, false), { names: 22, top: 22 });
  assert.deepEqual(headerLayout(22, false, true), { names: 22, top: 44 });
});

test('with types the names row grows and the data rows start below it and the filters', () => {
  assert.deepEqual(headerLayout(22, true, false), { names: 35, top: 35 });
  assert.deepEqual(headerLayout(22, true, true), { names: 35, top: 57 });
  // It grows with the grid's font size, as the rows do.
  assert.deepEqual(headerLayout(33, true, true), { names: 53, top: 86 });
});

test('moving down to a row hidden under the taller header scrolls it into view', () => {
  const { top } = headerLayout(22, true, true);
  // A 300px view scrolled to row 10: row 10's top is under the sticky header.
  const view = { top: 10 * 22, left: 0, height: 300, width: 600 };
  // Moving down to the last row that fits below the 57px header: 243px / 22 = 11 rows (10..20).
  assert.equal(revealCell(view, { top: 20 * 22, height: 22, left: 0, width: 100 }, top, 52).top, 10 * 22);
  // One more scrolls just enough to show it whole.
  assert.equal(revealCell(view, { top: 21 * 22, height: 22, left: 0, width: 100 }, top, 52).top, 22 * 22 - 243);
});
