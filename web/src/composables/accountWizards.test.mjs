// Tests for the "Conectar <gestor>" wizards' steps. Run: `node --test
// src/composables/accountWizards.test.mjs` from web/. Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  back, canAdvance, configOf, emptyForm, formFromAccount, isLast, keepsSecrets, next, secretsOf, serviceUrl, stepForError, stepProblems, STEPS, WIZARD_KINDS,
} from './accountWizards.ts';

const UUID = '6f1c2a9e-0d1b-4c55-9a51-2f7d8c3b1e04';

/** Fill each step as the user would, checking it can't go on before. */
function walk(kind, steps) {
  const s = emptyForm();
  for (const fill of steps) {
    Object.assign(s, fill);
    assert.deepEqual(stepProblems(kind, s), [], `${kind} step ${s.step}`);
    if (!isLast(kind, s)) assert.ok(next(kind, s));
  }
  assert.ok(isLast(kind, s), kind);
  return s;
}

test('every kind has its steps, the last one connects', () => {
  for (const k of WIZARD_KINDS) {
    assert.equal(STEPS[k].at(-1), 'connect', k);
    assert.ok(STEPS[k].length >= 2);
  }
});

test('Bitwarden: server, API key, master password; then what is sent', () => {
  const s = emptyForm();
  assert.deepEqual(stepProblems('bitwarden', s), [], 'the US cloud by default');
  s.server = 'self';
  assert.deepEqual(stepProblems('bitwarden', s), ['server_url']);
  s.server_url = 'http://vw.example.com';
  assert.deepEqual(stepProblems('bitwarden', s), ['server_url_format'], 'plain HTTP only to this computer');
  s.server_url = 'https://vw.example.com/';
  assert.ok(next('bitwarden', s));
  assert.deepEqual(stepProblems('bitwarden', s), ['client_id', 'client_secret']);
  s.client_id = 'user.nope';
  s.client_secret = 'cs';
  assert.deepEqual(stepProblems('bitwarden', s), ['client_id_format']);
  s.client_id = ` user.${UUID} `;
  assert.ok(next('bitwarden', s));
  assert.deepEqual(stepProblems('bitwarden', s), ['passphrase']);
  s.master = 'command';
  assert.deepEqual(stepProblems('bitwarden', s), ['master_command']);
  s.master = 'ask';
  s.passphrase = 'pw';
  assert.ok(next('bitwarden', s));
  assert.equal(next('bitwarden', s), false, 'not past the last step');
  assert.deepEqual(configOf('bitwarden', s), { kind: 'bitwarden', server: 'https://vw.example.com', client_id: `user.${UUID}`, master: 'ask' });
  assert.deepEqual(secretsOf('bitwarden', s), { client_secret: 'cs', passphrase: 'pw' }, 'an asked password is checked (never kept: the backend decides)');
  s.master = 'command';
  s.master_command = 'pass show bw';
  assert.deepEqual(secretsOf('bitwarden', s), { client_secret: 'cs' }, 'a command: nothing typed goes');
  back(s);
  assert.equal(s.step, 3);
});

test('every kind walks to its last step and sends only its own', () => {
  const cases = [
    ['bitwarden_sm', [{ server: 'bitwarden.eu' }, { token: `0.${UUID}.secret:AAECAwQFBgcICQoLDA0ODw==` }, {}],
      { kind: 'bitwarden_sm', server: 'bitwarden.eu' }, { token: `0.${UUID}.secret:AAECAwQFBgcICQoLDA0ODw==` }],
    ['vault', [{ address: 'https://vault.example.com:8200/', namespace: 'admin' }, { auth: 'approle', role_id: 'r-1', secret_id: 's-1', token: 'ignored' }, {}],
      { kind: 'vault', address: 'https://vault.example.com:8200', namespace: 'admin', auth: 'approle', role_id: 'r-1' }, { secret_id: 's-1' }],
    ['vault', [{ address: 'http://127.0.0.1:8200' }, {}, {}], { kind: 'vault', address: 'http://127.0.0.1:8200' }, {}],
    ['delinea', [{ url: 'https://acme.secretservercloud.com' }, { username: 'svc', domain: 'CORP', password: 'pw' }, {}],
      { kind: 'delinea', url: 'https://acme.secretservercloud.com', username: 'svc', domain: 'CORP' }, { password: 'pw' }],
    ['delinea', [{ url: 'https://acme.secretservercloud.com', delinea_mode: 'platform', platform_url: 'https://acme.delinea.app' }, { username: 'client-1', domain: 'x', password: 'cs' }, {}],
      { kind: 'delinea', url: 'https://acme.secretservercloud.com', platform_url: 'https://acme.delinea.app', username: 'client-1' }, { password: 'cs' }],
    ['conjur', [{ url: 'https://conjur.example.com', conjur_account: 'myorg' }, { login: 'host/apps/dbine', api_key: 'k' }, {}],
      { kind: 'conjur', url: 'https://conjur.example.com', conjur_account: 'myorg', login: 'host/apps/dbine' }, { api_key: 'k' }],
    ['ccp', [{ url: 'https://ccp.example.com', app_id: 'DBine', ca_file: '~/ca.pem' }, { client_cert: '~/c.p12', passphrase: 'p' }, {}],
      { kind: 'ccp', url: 'https://ccp.example.com', app_id: 'DBine', client_cert: '~/c.p12', ca_file: '~/ca.pem' }, { passphrase: 'p' }],
    ['onepassword', [{ op_account: 'my.1password.com' }, {}], { kind: 'onepassword', account: 'my.1password.com' }, {}],
    ['aws', [{ profile: 'prod', region: 'eu-west-1' }, {}], { kind: 'aws', profile: 'prod', region: 'eu-west-1' }, {}],
    ['azure', [{ vault_url: 'https://kv1.vault.azure.net/', tenant: 'contoso.onmicrosoft.com' }, {}], { kind: 'azure', tenant: 'contoso.onmicrosoft.com', vault_url: 'https://kv1.vault.azure.net' }, {}],
    ['gcp', [{ project: 'my-proj' }, {}], { kind: 'gcp', project: 'my-proj' }, {}],
    ['keepass', [{ file: '~/db.kdbx', key_file: '~/db.keyx' }, { master: 'none' }, {}], { kind: 'keepass', file: '~/db.kdbx', key_file: '~/db.keyx', master: 'none' }, {}],
    ['keepass', [{ file: '~/db.kdbx' }, { passphrase: 'pw' }, {}], { kind: 'keepass', file: '~/db.kdbx' }, { passphrase: 'pw' }],
  ];
  for (const [kind, steps, config, secrets] of cases) {
    const s = walk(kind, steps);
    assert.deepEqual(configOf(kind, s), config, kind);
    assert.deepEqual(secretsOf(kind, s), secrets, kind);
  }
});

test('each step says what is missing or malformed', () => {
  const p = (kind, fill, step = 1) => stepProblems(kind, Object.assign(emptyForm(), fill, { step }));
  assert.deepEqual(p('vault', {}), ['address']);
  assert.deepEqual(p('vault', { address: 'http://vault.example.com' }), ['address_format']);
  assert.deepEqual(p('vault', { address: 'https://user:pw@vault' }), ['address_format']);
  assert.deepEqual(p('vault', { auth: 'approle' }, 2), ['role_id']);
  assert.deepEqual(p('vault', { auth: 'token' }, 2), [], 'a token may come from VAULT_TOKEN');
  assert.deepEqual(p('delinea', { url: 'https://ss', delinea_mode: 'platform', platform_url: 'https://evil.example.com' }), ['platform_url_format']);
  assert.deepEqual(p('delinea', { username: 'u', master: 'none' }, 2), ['master']);
  assert.deepEqual(p('conjur', { url: 'https://c', conjur_account: 'a/b' }), ['conjur_account_format']);
  assert.deepEqual(p('ccp', { client_key: '~/k.pem' }, 2), ['client_cert']);
  assert.deepEqual(p('onepassword', { op_account: '--debug' }), ['op_account_format']);
  assert.deepEqual(p('aws', { profile: '--x', region: 'eu-west-1' }), ['profile_format']);
  assert.deepEqual(p('aws', { region: 'evil.com/' }), ['region_format']);
  assert.deepEqual(p('azure', { vault_url: 'https://kv1.vault.azure.net.evil.com' }), ['vault_url_format']);
  assert.deepEqual(p('azure', { vault_url: 'https://kv1.vault.azure.net', tenant: '--x' }), ['tenant_format']);
  assert.deepEqual(p('gcp', { project: 'a/b' }), ['project_format']);
  assert.deepEqual(p('keepass', { master: 'none' }, 2), ['key_file']);
  assert.deepEqual(p('keepass', { master: 'command', master_command: 'a\nb' }, 2), ['master_command_format']);
  assert.equal(canAdvance('gcp', Object.assign(emptyForm(), { project: 'p' })), true);
  for (const u of ['https://x', 'https://x:8200/a', 'http://localhost:8200', 'http://127.0.0.1', 'http://[::1]:1']) assert.ok(serviceUrl(u), u);
  for (const u of ['http://x', 'ftp://x', 'https://a@b', 'https://x?y', 'https://x/../y', 'x']) assert.ok(!serviceUrl(u), u);
});

test('reconnecting: the inputs again, never a secret; kept secrets only for the same place', () => {
  const a = { id: 'a', kind: 'vault', label: 'Vault prod', identity: '', created_at: '', updated_at: '', address: 'https://vault.example.com', auth: 'approle', role_id: 'r-1' };
  const f = Object.assign(emptyForm(), formFromAccount(a));
  assert.deepEqual([f.address, f.auth, f.role_id, f.label, f.secret_id, f.token], ['https://vault.example.com', 'approle', 'r-1', 'Vault prod', '', '']);
  assert.ok(keepsSecrets('vault', a, f));
  f.address = 'https://other.example.com';
  assert.ok(!keepsSecrets('vault', a, f), 'another server: the secret is typed again');
  assert.ok(!keepsSecrets('vault', null, f));
  const bw = formFromAccount({ id: 'b', kind: 'bitwarden', label: 'BW', identity: '', created_at: '', updated_at: '', server: 'https://vw.example.com', client_id: `user.${UUID}`, master: 'ask' });
  assert.deepEqual([bw.server, bw.server_url, bw.master], ['self', 'https://vw.example.com', 'ask']);
});

test('errors send the user back to the step that fixes them', () => {
  assert.equal(stepForError('bitwarden', 'la contraseña maestra de Bitwarden no es la correcta'), 3);
  assert.equal(stepForError('bitwarden', 'Bitwarden rechazó la clave de API (client_id o client_secret): revisala'), 2);
  assert.equal(stepForError('vault', 'Vault rechazó el role_id o el secret_id de AppRole'), 2);
  assert.equal(stepForError('conjur', 'Conjur rechazó el login «host/x» o su clave de API'), 2);
  assert.equal(stepForError('aws', 'no se pudo conectar con AWS STS: x'), 1);
  assert.equal(stepForError('gcp', 'otra cosa'), null);
});
