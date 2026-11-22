// Tests for relativeAge.ts. Run: `node --test src/composables/relativeAge.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { ageParts } from './relativeAge.ts';

const now = Date.parse('2026-11-22T12:00:00Z');
const ago = (s) => new Date(now - s * 1000).toISOString();

test('under a minute is "a few seconds"', () => {
  assert.deepEqual(ageParts(ago(5), now), { unit: 'seconds', n: 0 });
  assert.deepEqual(ageParts(ago(59), now), { unit: 'seconds', n: 0 });
});
test('minutes, hours and days round down', () => {
  assert.deepEqual(ageParts(ago(60), now), { unit: 'minutes', n: 1 });
  assert.deepEqual(ageParts(ago(5 * 60 + 40), now), { unit: 'minutes', n: 5 });
  assert.deepEqual(ageParts(ago(2 * 3600 + 100), now), { unit: 'hours', n: 2 });
  assert.deepEqual(ageParts(ago(3 * 86400), now), { unit: 'days', n: 3 });
});
test('a future or invalid date counts as just now', () => {
  assert.deepEqual(ageParts(ago(-30), now), { unit: 'seconds', n: 0 });
  assert.deepEqual(ageParts('not a date', now), { unit: 'seconds', n: 0 });
});
