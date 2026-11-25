// Run: `node --test src/stores/companyData.test.mjs` from web/ (Node 23.6+).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { databaseLost, isCompanyId, usedCompany, usedLost, withoutCompanyChats, withoutLostChats } from './companyData.ts';

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

test('only what was lost goes: a database, a connection, never the rest', () => {
  const lost = {
    connections: ['ent-a'],
    databases: [
      { connection_id: 'ent-b', databases: ['d1'], keep: ['d2'] },
      { connection_id: 'ent-c', databases: [], keep: ['d3'] },
    ],
  };
  assert.equal(databaseLost(lost, 'ent-a', 'anything'), true);
  assert.equal(databaseLost(lost, 'ent-b', 'd1'), true);
  assert.equal(databaseLost(lost, 'ent-b', 'd2'), false);
  assert.equal(databaseLost(lost, 'ent-b', 'd9'), true, 'not in keep');
  assert.equal(databaseLost(lost, 'ent-b', ''), true, 'the default one can be it');
  assert.equal(databaseLost(lost, 'ent-c', 'd3'), false);
  assert.equal(databaseLost(lost, 'ent-c', 'd4'), true);
  assert.equal(databaseLost(lost, 'ent-z', 'd1'), false);
  assert.equal(databaseLost(lost, 'c1', 'd1'), false);
});

test('a chat goes only if it quoted what was lost', () => {
  const lost = { connections: ['ent-a'], databases: [{ connection_id: 'ent-b', databases: ['d1'], keep: ['d2'] }] };
  const chat = (id, ...messages) => ({ id, messages });
  const list = [
    chat('on-a', { company: true, companyConnection: 'ent-a', companyDatabase: 'x' }),
    chat('on-b-d1', { company: true, companyConnection: 'ent-b', companyDatabase: 'd1' }),
    chat('on-b-d2', { company: true, companyConnection: 'ent-b', companyDatabase: 'd2' }),
    chat('old-untagged', { company: true }),
    chat('normal', {}, {}),
  ];
  assert.equal(usedLost(list[2].messages, lost), false);
  assert.deepEqual(withoutLostChats(list, lost).map((c) => c.id), ['on-b-d2', 'normal']);
  assert.deepEqual(withoutLostChats([], lost), []);
});
