// Tests for notebook parameters' helpers (notebookParams.ts). Run:
// `node --test src/composables/notebookParams.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  effectiveValues, formatParamValues, isCredentialParam, mentionedParams, missingRequired, newParamName, paramNameProblem, paramValueProblem,
  recordableValues, resultIsStale, usedValues, validParamName, valuesKey,
} from './notebookParams.ts';

const defs = [
  { name: 'mes', type: 'text', required: true },
  { name: 'tope', type: 'number', default: 10 },
  { name: 'desde', type: 'date' },
  { name: 'activo', type: 'boolean', default: false },
  { name: 'region', type: 'list', options: ['Norte', 'Sur'] },
];

test('names are plain identifiers, unique ignoring case', () => {
  for (const ok of ['mes', '_x', 'A1', 'a'.repeat(63)]) assert.ok(validParamName(ok), ok);
  for (const bad of ['', '1a', 'a-b', 'año', 'a b', 'a'.repeat(64)]) assert.ok(!validParamName(bad), bad);
  assert.equal(paramNameProblem(defs, 0, 'TOPE'), 'taken');
  assert.equal(paramNameProblem(defs, 1, 'tope'), null);
  assert.equal(paramNameProblem(defs, 1, 'x y'), 'invalid');
  assert.equal(newParamName([{ name: 'p1', type: 'text' }]), 'p2');
});

test('mentioned parameters: defined ones, anywhere, once, as defined', () => {
  assert.deepEqual(mentionedParams("SELECT {{MES}}, {{ tope }}, '{{mes}}', {{nada}} -- {{region}}", defs), ['mes', 'tope', 'region']);
  assert.deepEqual(mentionedParams('x {{{mes}}} y', defs), []);
});

test('values: given, else defaults; required ones without either are asked for', () => {
  assert.deepEqual(effectiveValues(defs, { mes: '2026-10', tope: '' }), { mes: '2026-10', tope: 10, desde: null, activo: false, region: null });
  assert.deepEqual(missingRequired(defs, {}), ['mes']);
  assert.deepEqual(missingRequired(defs, { mes: '  ' }), ['mes']);
  assert.deepEqual(missingRequired(defs, {}, ['tope']), []);
  assert.deepEqual(missingRequired(defs, { mes: 'x' }), []);
});

test('each type checks its values', () => {
  const by = (n) => defs.find((d) => d.name === n);
  assert.equal(paramValueProblem(by('tope'), '1; DROP TABLE x'), 'number');
  assert.equal(paramValueProblem(by('tope'), '-2.5e3'), null);
  assert.equal(paramValueProblem(by('desde'), '2026-13-01'), 'date');
  assert.equal(paramValueProblem(by('desde'), '2026-10-01'), null);
  assert.equal(paramValueProblem({ name: 'x', type: 'datetime' }, '2026-10-01 14:30'), null);
  assert.equal(paramValueProblem({ name: 'x', type: 'datetime' }, '2026-10-01'), 'datetime');
  assert.equal(paramValueProblem(by('region'), 'Este'), 'list');
  assert.equal(paramValueProblem(by('region'), ''), null);
});

test('a result records its values and is stale when they change', () => {
  const cell = { kind: 'sql', source: 'SELECT * FROM v WHERE mes = {{mes}} AND n < {{tope}}', result: null };
  const recorded = usedValues(defs, { mes: '2026-10' }, mentionedParams(cell.source, defs));
  assert.deepEqual(recorded, { mes: '2026-10', tope: 10 });
  cell.result = { params: recorded };
  assert.equal(resultIsStale(cell, defs, { mes: '2026-10' }), false);
  assert.equal(resultIsStale(cell, defs, { mes: '2026-10', tope: '10' }), false);
  assert.equal(resultIsStale(cell, defs, { mes: '2026-11' }), true);
  // Another parameter, not used by the cell: still fresh.
  assert.equal(resultIsStale(cell, defs, { mes: '2026-10', region: 'Sur' }), false);
  // A result from before parameters (nothing recorded) isn't marked.
  assert.equal(resultIsStale({ ...cell, result: { params: null } }, defs, { mes: 'x' }), false);
  assert.equal(formatParamValues({ mes: '2026-10', activo: true, x: null }, 'sí', 'no'), 'mes = 2026-10 · activo = sí · x = NULL');
  assert.equal(valuesKey({ b: 1, a: 'x' }), valuesKey({ a: 'x', b: 1 }));
  assert.notEqual(valuesKey({ a: 'x' }), valuesKey({ a: 'y' }));
});

test('a result never records a credential parameter\'s value', () => {
  for (const n of ['password', 'api_token', 'DB_PASSWORD', 'secret_key', 'apiKey', 'host', 'url', 'conn_str']) assert.ok(isCredentialParam(n), n);
  for (const n of ['user', 'username', 'login', 'port', 'mes', 'db_host', 'tope']) assert.ok(!isCredentialParam(n), n);
  assert.deepEqual(recordableValues({ mes: '2026-10', api_token: 'tok', password: 'pw', user: 'ana' }), { mes: '2026-10', user: 'ana' });
  assert.equal(recordableValues({ password: 'pw' }), null);
  assert.equal(recordableValues(null), null);
  // A result without the credential's value isn't stale for it.
  const defs2 = [{ name: 'mes', type: 'text' }, { name: 'api_token', type: 'text' }];
  const cell = { kind: 'sql', source: 'SELECT {{mes}}, {{api_token}}', result: { params: { mes: '2026-10' } } };
  assert.equal(resultIsStale(cell, defs2, { mes: '2026-10', api_token: 'tok' }), false);
  assert.equal(resultIsStale(cell, defs2, { mes: '2026-11', api_token: 'tok' }), true);
});
