// Tests for the "Enviar un mensaje" step editor's checks. Run:
// `node --test src/composables/messageStep.test.mjs` from web/ (Node 23.6+
// strips the TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { localHost, messageProblems } from './messageStep.ts';

const TEAMS = 'https://prod-12.westus.logic.azure.com:443/workflows/abc/triggers/manual/paths/invoke?api-version=2016-06-01&sig=x';

test('a good destination for each channel has no problems', () => {
  assert.deepEqual(messageProblems({ channel: 'teams', url: TEAMS }, null), []);
  assert.deepEqual(messageProblems({ channel: 'teams', url: 'https://contoso.webhook.office.com/webhookb2/x' }, null), []);
  assert.deepEqual(messageProblems({ channel: 'slack', url: 'https://hooks.slack.com/services/T/B/X' }, null), []);
  assert.deepEqual(messageProblems({ channel: 'telegram', token: '123456:ABC-def_9', chat_id: '-1001234567890' }, null), []);
  assert.deepEqual(messageProblems({ channel: 'telegram', token: '1:a', chat_id: '@mi_canal' }, null), []);
  assert.deepEqual(messageProblems({ channel: 'ntfy', url: 'https://ntfy.sh/dbine-alertas', token: 'tk_1' }, null), []);
});

test('hosts must be the channel’s own', () => {
  assert.deepEqual(messageProblems({ channel: 'teams', url: 'https://evil.example.com/workflows/x' }, null), ['teamsHost']);
  assert.deepEqual(messageProblems({ channel: 'teams', url: 'https://logic.azure.com.evil.example.com/x' }, null), ['teamsHost']);
  assert.deepEqual(messageProblems({ channel: 'teams', url: 'https://prod-1.logic.azure.com:8443/x' }, null), ['teamsHost']);
  assert.deepEqual(messageProblems({ channel: 'slack', url: 'https://slack.com/x' }, null), ['slackHost']);
  assert.deepEqual(messageProblems({ channel: 'slack', url: 'http://hooks.slack.com/services/x' }, null), ['notHttps']);
  assert.deepEqual(messageProblems({ channel: 'slack', url: 'hooks.slack.com/services/x' }, null), ['badUrl']);
  assert.deepEqual(messageProblems({ channel: 'slack', url: 'https://a:b@hooks.slack.com/services/x' }, null), ['badUrl']);
  for (const local of ['https://localhost/t', 'https://10.0.0.5/t', 'https://192.168.1.2/t', 'https://169.254.169.254/t', 'https://[::1]/t', 'https://nas.local/t']) {
    assert.deepEqual(messageProblems({ channel: 'ntfy', url: local }, null), ['ntfyLocal'], local);
  }
  assert.deepEqual(messageProblems({ channel: 'ntfy', url: 'https://ntfy.sh/' }, null), ['ntfyTopic']);
  assert.deepEqual(messageProblems({ channel: 'ntfy', url: 'https://ntfy.sh/t?x=1' }, null), ['ntfyTopic']);
  assert.equal(localHost('ntfy.sh'), false);
  assert.equal(localHost('8.8.8.8'), false);
});

test('empty secrets: missing, unless saved for the same channel', () => {
  assert.deepEqual(messageProblems({ channel: 'slack', url: '' }, null), ['missingUrl']);
  assert.deepEqual(messageProblems({ channel: 'slack', url: '' }, 'slack'), []);
  // Saved for another channel: it doesn't count.
  assert.deepEqual(messageProblems({ channel: 'ntfy', url: '' }, 'slack'), ['missingUrl']);
  assert.deepEqual(messageProblems({ channel: 'telegram', token: '', chat_id: '1' }, null), ['missingToken']);
  assert.deepEqual(messageProblems({ channel: 'telegram', token: '', chat_id: '1' }, 'telegram'), []);
  // The chat is not a secret: always needed.
  assert.deepEqual(messageProblems({ channel: 'telegram', token: '', chat_id: '' }, 'telegram'), ['missingChat']);
});

test('telegram token and chat have their shapes', () => {
  assert.deepEqual(messageProblems({ channel: 'telegram', token: '1:a/../x', chat_id: '1' }, null), ['badToken']);
  assert.deepEqual(messageProblems({ channel: 'telegram', token: 'abc', chat_id: '1' }, null), ['badToken']);
  assert.deepEqual(messageProblems({ channel: 'telegram', token: '1:a', chat_id: '1; x' }, null), ['badChat']);
  assert.deepEqual(messageProblems({ channel: 'ntfy', url: 'https://ntfy.sh/t', token: 'a b' }, null), ['badToken']);
});
