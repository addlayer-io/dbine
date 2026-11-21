// Tests for the lineage panel's source logic (lineageSource.ts). Run:
// `node --test src/composables/lineageSource.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';

const { planForSource, loginReady, lookupConnection, internalWhere } = await import('./lineageSource.ts');

const cand = (id, via = 'auto') => ({ connection_id: id, name: id, driver: 'sqlserver', via });
const match = (status, candidates = [], extra = {}) => ({ status, signature: 's', candidates, prefill: null, needs_login: false, ...extra });
const prefill = { driver: 'sqlserver', host: 'srv', port: 1433, database: 'd' };

test('one candidate is followed at once, whichever way it came', () => {
  for (const via of ['link', 'auto', 'enterprise', 'internal']) {
    const p = planForSource(match('one', [cand('c1', via)]));
    assert.equal(p.kind, 'follow');
    assert.equal(p.candidate.connection_id, 'c1');
  }
});

test('several candidates ask for a choice, none for a login only where the engine has one', () => {
  assert.equal(planForSource(match('many', [cand('a'), cand('b')])).kind, 'choose');
  assert.equal(planForSource(match('none', [], { prefill, needs_login: true })).kind, 'login');
  assert.equal(planForSource(match('none', [], { prefill, needs_login: false })).kind, 'connect');
  assert.equal(planForSource(match('none')).kind, 'none');
  assert.equal(planForSource(null), null);
});

test('the form needs a user, not a password', () => {
  assert.equal(loginReady(''), false);
  assert.equal(loginReady('   '), false);
  assert.equal(loginReady('lector'), true);
});

test('a hidden connection is found by id and never shadows an explorer one', () => {
  const list = [{ id: 'a', n: 'saved' }];
  const hidden = [{ id: 'h', n: 'hidden' }, { id: 'a', n: 'dup' }];
  assert.equal(lookupConnection(list, hidden, 'h').n, 'hidden');
  assert.equal(lookupConnection(list, hidden, 'a').n, 'saved');
  assert.equal(lookupConnection(list, hidden, 'x'), undefined);
  // Looking one up does not put it in the explorer's list.
  assert.equal(list.length, 1);
});

test('the Settings line says where the connection points', () => {
  assert.equal(internalWhere({ config: { host: 'srv', port: 1433, database: 'ventas' } }), 'srv:1433 · ventas');
  assert.equal(internalWhere({ config: { host: 'srv', port: 0, database: '' } }), 'srv');
});
