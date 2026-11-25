// Tests for the AI providers' helpers in ai.ts. Run: `node --test src/api/ai.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { API_PROVIDERS, apiSettingKey, isApiProvider, vendorOf } from './ai.ts';

test('the API-key providers and who bills each one', () => {
  assert.deepEqual(API_PROVIDERS, ['openai', 'anthropic', 'azure_openai', 'gemini', 'xai', 'openai_compatible']);
  for (const k of API_PROVIDERS) assert.ok(isApiProvider(k), k);
  for (const k of ['embedded', 'ollama', 'lm_studio', 'claude_code', 'codex']) assert.ok(!isApiProvider(k), k);
  assert.equal(vendorOf('anthropic'), 'Anthropic');
  assert.equal(vendorOf('claude_code'), 'Anthropic');
  assert.equal(vendorOf('codex'), 'OpenAI');
  assert.equal(vendorOf('azure_openai'), 'Microsoft Azure');
  assert.equal(vendorOf('gemini'), 'Google');
  assert.equal(vendorOf('xai'), 'xAI');
  // The user's own server, and the local ones: nobody else.
  for (const k of ['openai_compatible', 'ollama', 'embedded']) assert.equal(vendorOf(k), null, k);
});

test('the setting a provider keeps (never the key) matches the backend', () => {
  // src-tauri/src/commands/ai.rs api_setting_key: `ai.api.<provider id>`.
  assert.equal(apiSettingKey('azure_openai'), 'ai.api.azure_openai');
});
