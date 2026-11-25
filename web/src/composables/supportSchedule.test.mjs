// Tests for supportSchedule.ts. Run: `node --test src/composables/supportSchedule.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { FIRST_AFTER_DAYS, countDay, localDay, supportDue } from './supportSchedule.ts';

const NOW = Date.parse('2026-11-25T12:00:00Z');

test('the first launch never asks for support', () => {
  assert.equal(supportDue(1, null, NOW), false);
  assert.equal(supportDue(FIRST_AFTER_DAYS - 1, null, NOW), false);
});

test('after enough days of use it is due, then only when the next date passed', () => {
  assert.equal(supportDue(FIRST_AFTER_DAYS, null, NOW), true);
  assert.equal(supportDue(FIRST_AFTER_DAYS + 20, '2026-12-20T00:00:00Z', NOW), false);
  assert.equal(supportDue(FIRST_AFTER_DAYS + 20, '2026-11-01T00:00:00Z', NOW), true);
  // Whoever saw it already (an older version showed it on day one) keeps the monthly date.
  assert.equal(supportDue(0, '2026-11-01T00:00:00Z', NOW), true);
  // A date that can't be read counts as never shown.
  assert.equal(supportDue(2, 'garbage', NOW), false);
});

test('each day of use counts once', () => {
  let s = { daysUsed: 0, lastDay: null };
  for (const day of ['2026-11-20', '2026-11-20', '2026-11-21', '2026-11-21', '2026-11-23']) s = countDay(s.daysUsed, s.lastDay, day);
  assert.deepEqual(s, { daysUsed: 3, lastDay: '2026-11-23' });
});

test('a local day is YYYY-MM-DD', () => {
  assert.equal(localDay(new Date(2026, 0, 5, 23, 59)), '2026-01-05');
});
