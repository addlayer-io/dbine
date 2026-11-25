// Run: `node --test src/stores/liveConnection.test.mjs` from web/ (Node 23.6+).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { connectingFrom } from './liveConnection.ts';

const connected = {
  status: 'connected', serverVersion: '16', databases: ['a', 'b'], defaultDatabase: 'a', error: null,
};

test('a connection that was connected keeps its databases while it connects again', () => {
  const live = connectingFrom(connected);
  assert.equal(live.status, 'connecting');
  assert.deepEqual(live.databases, ['a', 'b']);
  assert.equal(live.serverVersion, '16');
  assert.equal(live.defaultDatabase, 'a');
  assert.equal(live.error, null);
});

test('a new, a failed or a forgotten connection starts without databases', () => {
  assert.deepEqual(connectingFrom(undefined).databases, []);
  const failed = { ...connected, status: 'error', databases: [], error: 'x', defaultDatabase: 'a' };
  const live = connectingFrom(failed);
  assert.deepEqual(live.databases, []);
  assert.equal(live.error, null);
  assert.equal(live.defaultDatabase, 'a');
  // Not the previous object: the explorer's priming writes into the new one.
  const again = connectingFrom(connected);
  again.databases.push('c');
  assert.deepEqual(connected.databases, ['a', 'b']);
});
