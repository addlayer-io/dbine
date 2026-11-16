// Tests for query parameter values (paramValues.ts). Run:
// `node --test src/composables/paramValues.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { paramProblem, silentValues } from './paramValues.ts';

test('issue #111: with "don\'t ask again", a run uses the remembered values', () => {
  const last = { parent: { type: 'number', value: '1234' } };
  assert.deepEqual(silentValues(['parent'], last, true), { parent: { type: 'number', value: '1234' } });
});

test('without "don\'t ask again" every run asks', () => {
  const last = { parent: { type: 'number', value: '1234' } };
  assert.equal(silentValues(['parent'], last, false), null);
});

test('a parameter with no remembered value asks, even with "don\'t ask again"', () => {
  const last = { parent: { type: 'number', value: '1234' } };
  assert.equal(silentValues(['parent', 'desde'], last, true), null);
});

test('a remembered value that can\'t be used asks', () => {
  assert.equal(silentValues(['n'], { n: { type: 'number', value: 'abc' } }, true), null);
  assert.equal(silentValues(['d'], { d: { type: 'date', value: 'ayer' } }, true), null);
});

test('only the text\'s parameters are used, and empty text and NULL count as values', () => {
  const last = {
    a: { type: 'text', value: '' },
    b: { type: 'null', value: '' },
    other: { type: 'text', value: 'x' },
  };
  assert.deepEqual(silentValues(['a', 'b'], last, true), { a: last.a, b: last.b });
});

test('paramProblem checks numbers and dates', () => {
  assert.equal(paramProblem({ type: 'number', value: ' -3.5e2 ' }), null);
  assert.equal(paramProblem({ type: 'number', value: '12,5' }), 'number');
  assert.equal(paramProblem({ type: 'date', value: '2024-05-01 10:30' }), null);
  assert.equal(paramProblem({ type: 'date', value: '01/05/2024' }), 'date');
  assert.equal(paramProblem({ type: 'text', value: 'anything' }), null);
});
