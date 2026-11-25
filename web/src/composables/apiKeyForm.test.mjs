// Tests for apiKeyForm.ts. Run: `node --test src/composables/apiKeyForm.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { offeredModels, providerAfterRemove } from './apiKeyForm.ts';

test('what the test found is offered before saving, without repeats', () => {
  assert.deepEqual(offeredModels('', ['fake-sql-1', 'fake-sql-2'], []), ['fake-sql-1', 'fake-sql-2']);
  assert.deepEqual(offeredModels(' mine ', ['a', 'mine'], ['a', 'b']), ['mine', 'a', 'b']);
  assert.deepEqual(offeredModels('', [], []), []);
});

test('removing the chosen provider clears the choice; another one stays', () => {
  assert.equal(providerAfterRemove('openai_compatible', 'openai_compatible'), null);
  assert.equal(providerAfterRemove('ollama', 'openai_compatible'), 'ollama');
  assert.equal(providerAfterRemove(null, 'openai'), null);
});
