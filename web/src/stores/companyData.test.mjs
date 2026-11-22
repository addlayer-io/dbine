// Run: `node --test src/stores/companyData.test.mjs` from web/ (Node 23.6+).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { isCompanyId, usedCompany, withoutCompanyChats } from './companyData.ts';

test('only ent- ids are the company', () => {
  assert.equal(isCompanyId('ent-p1'), true);
  assert.equal(isCompanyId('c1'), false);
  assert.equal(isCompanyId('entx'), false);
  assert.equal(isCompanyId(''), false);
  assert.equal(isCompanyId(null), false);
});

test('a chat that used a company connection goes, the others stay', () => {
  const company = { id: 'a', messages: [{}, { company: true }] };
  const normal = { id: 'b', messages: [{}, {}] };
  assert.equal(usedCompany(company.messages), true);
  assert.deepEqual(withoutCompanyChats([company, normal]).map((c) => c.id), ['b']);
  assert.deepEqual(withoutCompanyChats([]), []);
});
