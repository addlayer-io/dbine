// Tests for where the results grid scrolls when the arrow keys move the
// selected cell (gridReveal.ts, issue #128). Run:
// `node --test src/composables/gridReveal.test.mjs` from web/ (Node 23.6+
// strips the TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { revealCell } from './gridReveal.ts';

// A 600×300 view: one 24px header row and the 52px row-number column stick.
const view = (top = 0, left = 0) => ({ top, left, height: 300, width: 600 });
const ROW = 24, HEAD = 24, NUM = 52;
const cell = (r, left, width) => ({ top: r * ROW, height: ROW, left, width });

test('moving right past the edge scrolls the grid sideways to show the cell whole', () => {
  // Columns of 150px: the fourth (450–600) ends past the 548px left of the numbers.
  assert.deepEqual(revealCell(view(), cell(0, 450, 150), HEAD, NUM), { top: 0, left: 52 });
  // Further right, from there.
  assert.deepEqual(revealCell(view(0, 52), cell(0, 600, 150), HEAD, NUM), { top: 0, left: 202 });
});

test('moving left past the edge scrolls back to the cell, not under the row numbers', () => {
  assert.deepEqual(revealCell(view(0, 202), cell(0, 150, 150), HEAD, NUM), { top: 0, left: 150 });
  assert.deepEqual(revealCell(view(0, 202), cell(0, 0, 150), HEAD, NUM), { top: 0, left: 0 });
});

test('a cell already in view leaves the scroll as it is', () => {
  assert.deepEqual(revealCell(view(48, 100), cell(3, 150, 150), HEAD, NUM), { top: 48, left: 100 });
});

test('a column wider than the view lines up with its left edge', () => {
  assert.deepEqual(revealCell(view(0, 0), cell(0, 300, 2000), HEAD, NUM), { top: 0, left: 300 });
  assert.deepEqual(revealCell(view(0, 900), cell(0, 300, 2000), HEAD, NUM), { top: 0, left: 300 });
});

test('up and down keep working: below the bottom, and under the header row', () => {
  // Row 11 (264–288) ends past the 276px under the header.
  assert.deepEqual(revealCell(view(), cell(11, 0, 150), HEAD, NUM), { top: 12, left: 0 });
  assert.deepEqual(revealCell(view(100), cell(2, 0, 150), HEAD, NUM), { top: 48, left: 0 });
});
