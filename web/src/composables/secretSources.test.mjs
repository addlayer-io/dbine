// Tests for the secret sources of the connection form. Run:
// `node --test src/composables/secretSources.test.mjs` from web/ (Node 23.6+
// strips the TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { draftFrom, emptyDraft, isExternal, missingInputs, readSources, sourceFrom, sourceRows, SOURCES_OPTION, tokenKey, typedTokens, writeSources } from './secretSources.ts';

test('definitions round-trip through the options', () => {
  const options = { sslmode: 'require' };
  const sources = {
    password: { kind: 'command', command: 'pass show db/prod' },
    'ssh.password': { kind: 'vault', address: 'https://vault:8200', path: 'secret/app', key: 'ssh', kv_version: '2' },
  };
  writeSources(options, sources);
  assert.equal(typeof options[SOURCES_OPTION], 'string');
  assert.deepEqual(readSources(options), sources);
  writeSources(options, {});
  assert.equal(options[SOURCES_OPTION], undefined);
  assert.equal(options.sslmode, 'require');
  assert.deepEqual(readSources({ [SOURCES_OPTION]: 'not json' }), {});
  assert.deepEqual(readSources(undefined), {});
});

test('a draft gives back the definition it came from, without empty inputs', () => {
  const cases = [
    { kind: 'command', command: 'op read op://a/b/c', json_key: 'password' },
    { kind: 'onepassword', reference: 'op://Private Vault/DB/password' },
    { kind: 'vault', address: 'http://127.0.0.1:8200', path: 'secret/dbine/pg', key: 'password', kv_version: '1', namespace: 'team' },
    { kind: 'aws', secret_id: 'prod/db', region: 'eu-west-1', json_key: 'password', fill_username: true },
    { kind: 'azure', vault_url: 'https://kv1.vault.azure.net', name: 'db-pw' },
    { kind: 'gcp', project: 'my-proj', secret: 'db-pw', version: '3' },
    { kind: 'keepass', file: '~/db.kdbx', entry: 'Databases/Prod/pg-main', attribute: 'API token', key_file: '~/db.keyx', master: 'command', master_command: 'pass show kp', fill_username: true },
    { kind: 'keepass', file: '~/db.kdbx', entry: 'pg-main' },
    { kind: 'zoho', data_center: 'eu', secret_name: 'pg-main', client_id: '1000.ABC', master: 'ask', fill_username: true },
  ];
  for (const c of cases) assert.deepEqual(sourceFrom(draftFrom(c), 'password'), c, c.kind);
  // The username only fills from the password's source.
  assert.equal(sourceFrom(draftFrom(cases[3]), 'api_key').fill_username, undefined);
  // Trimmed; auto KV version left out.
  const d = emptyDraft('vault');
  Object.assign(d, { address: ' https://v ', path: 'secret/x ', key: 'k' });
  assert.deepEqual(sourceFrom(d, 'password'), { kind: 'vault', address: 'https://v', path: 'secret/x', key: 'k' });
});

test('saved and ask are not definitions', () => {
  assert.equal(sourceFrom(draftFrom(undefined), 'password'), null);
  assert.equal(draftFrom(undefined, true).kind, 'ask');
  assert.equal(sourceFrom(draftFrom(undefined, true), 'password'), null);
  assert.equal(isExternal(draftFrom(undefined, true)), false);
  assert.equal(isExternal(draftFrom({ kind: 'command', command: 'x' })), true);
});

test('what each kind needs', () => {
  assert.deepEqual(missingInputs(emptyDraft('vault')), ['address', 'path', 'key']);
  assert.deepEqual(missingInputs(emptyDraft('saved')), []);
  const d = emptyDraft('command');
  assert.deepEqual(missingInputs(d), ['command']);
  d.command = '  ';
  assert.deepEqual(missingInputs(d), ['command']);
  d.command = 'echo x';
  assert.deepEqual(missingInputs(d), []);
});

test('the confirmation shows every input that has a value, exactly', () => {
  const rows = sourceRows({ kind: 'command', command: 'curl -s https://x | jq -r .pw', json_key: '' });
  assert.deepEqual(rows, [['command', 'curl -s https://x | jq -r .pw']]);
  const aws = sourceRows({ kind: 'aws', secret_id: 'prod/db', region: 'us-east-1', fill_username: true });
  assert.deepEqual(aws.map((r) => r[0]), ['secret_id', 'region', 'fill_username']);
});

test('KeePass and Zoho Vault: the master password choice', () => {
  const kp = emptyDraft('keepass');
  Object.assign(kp, { file: '/x.kdbx', entry: 'e' });
  assert.deepEqual(missingInputs(kp), []);
  kp.master = 'none';
  assert.deepEqual(missingInputs(kp), ['key_file'], 'no password: a key file');
  kp.master = 'command';
  assert.deepEqual(missingInputs(kp), ['master_command']);
  kp.master_command = 'echo pw';
  // A command left over from another choice isn't stored.
  kp.master = 'ask';
  assert.equal(sourceFrom(kp, 'password').master_command, undefined);
  assert.equal(sourceFrom(kp, 'password').master, 'ask');
  kp.master = 'keychain';
  assert.equal(sourceFrom(kp, 'password').master, undefined, 'the default is left out, as the backend stores it');
  assert.deepEqual(missingInputs(emptyDraft('zoho')), ['data_center', 'secret_name', 'client_id']);
  const zo = emptyDraft('zoho');
  zo.master = 'none';
  assert.equal(sourceFrom(Object.assign(zo, { data_center: 'eu', secret_name: 's', client_id: 'c' }), 'password').master, undefined, 'Zoho always has a passphrase');
});

test('what goes to the keychain, by slot', () => {
  const v = emptyDraft('vault');
  v.token = 'hvs.x';
  assert.deepEqual(typedTokens(v), { '': 'hvs.x' });
  const kp = emptyDraft('keepass');
  kp.token = 'master';
  assert.deepEqual(typedTokens(kp), { '': 'master' });
  kp.master = 'ask';
  assert.deepEqual(typedTokens(kp), {}, 'an asked password is never kept');
  const zo = emptyDraft('zoho');
  Object.assign(zo, { token: 'pass', client_secret: 'cs', refresh_token: 'rt' });
  assert.deepEqual(typedTokens(zo), { '': 'pass', client_secret: 'cs', refresh_token: 'rt' });
  zo.master = 'command';
  assert.deepEqual(typedTokens(zo), { client_secret: 'cs', refresh_token: 'rt' });
  assert.deepEqual(typedTokens(emptyDraft('command')), {});
  assert.equal(tokenKey('ssh.password'), 'source_token:ssh.password');
  assert.equal(tokenKey('password', 'refresh_token'), 'source_token:password#refresh_token');
});

test('the confirmation shows a KeePass definition', () => {
  const rows = sourceRows({ kind: 'keepass', file: '/tmp/x.kdbx', entry: 'a/b', master: 'command', master_command: 'curl x | sh' });
  assert.deepEqual(rows.map((r) => r[0]), ['file', 'entry', 'master', 'master_command']);
});
