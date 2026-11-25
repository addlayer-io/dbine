// Tests for the short message shown when formatting fails (formatError.ts).
// Run: `node --test src/composables/formatError.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';

register('data:text/javascript,' + encodeURIComponent(`
export async function resolve(spec, ctx, next) {
  if (spec.startsWith('.') && !/\\.[cm]?[jt]s$/.test(spec)) return next(spec + '.ts', ctx);
  return next(spec, ctx);
}`));
const { formatErrorSummary } = await import('./formatError.ts');

test('a long parser diagnostic is cut to its first lines', () => {
  const long = 'Parse error at token: . at line 1 column 4099\nUnexpected PROPERTY_ACCESS_OPERATOR token\n' +
    'Expected: ' + 'SELECT, FROM, WHERE, '.repeat(500);
  const out = formatErrorSummary(long);
  assert.ok(out.length <= 241);
  assert.ok(out.startsWith('Parse error at token: . at line 1 column 4099 · Unexpected PROPERTY_ACCESS_OPERATOR token'));
  assert.ok(out.endsWith('…'));
  assert.ok(!out.includes('Expected'));
});

test('a short message stays as it is', () => {
  assert.equal(formatErrorSummary('Unable to format'), 'Unable to format');
});

test('one huge line is bounded too', () => {
  assert.ok(formatErrorSummary('x'.repeat(5000)).length <= 241);
});
