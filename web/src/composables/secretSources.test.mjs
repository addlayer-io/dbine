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

test('Vault database credentials, CyberArk and Delinea round-trip', () => {
  const cases = [
    { kind: 'vault_db', address: 'http://127.0.0.1:8200', role: 'readonly' },
    { kind: 'vault_db', address: 'https://v:8200', mount: 'pg/prod', role: 'app', static_role: true, namespace: 'team', auth: 'approle', role_id: 'r-1', approle_mount: 'ar' },
    { kind: 'vault', address: 'https://v', path: 'secret/x', key: 'pw', auth: 'approle', role_id: 'r-1' },
    { kind: 'conjur', url: 'https://conjur', conjur_account: 'myorg', login: 'host/apps/dbine', variable: 'db/pw', username_variable: 'db/user', ca_file: '~/ca.pem' },
    { kind: 'ccp', url: 'https://ccp', app_id: 'DBine', safe: 'DB', object: 'pg prod', client_cert: '~/c.p12', fill_username: true },
    { kind: 'ccp', url: 'https://ccp', app_id: 'DBine', account_user: 'app', account_address: 'db.example.com', client_cert: '~/c.crt', client_key: '~/c.key' },
    { kind: 'delinea', url: 'https://acme.secretservercloud.com', username: 'svc', secret_name: 'pg-prod', master: 'ask', fill_username: true },
    { kind: 'delinea', url: 'https://acme.secretservercloud.eu', platform_url: 'https://acme.delinea.app', username: 'client-1', domain: 'CORP', secret_name: '42', attribute: 'username' },
  ];
  for (const c of cases) assert.deepEqual(sourceFrom(draftFrom(c), 'password'), c, c.kind);
  // A token Vault definition has no sign-in inputs (as the backend stores it).
  const v = draftFrom({ kind: 'vault', address: 'https://v', path: 'p', key: 'k' });
  Object.assign(v, { role_id: 'left-over' });
  assert.equal(sourceFrom(v, 'password').role_id, undefined);
  // The user only fills from the password's source.
  assert.equal(sourceFrom(draftFrom(cases[3]), 'api_key').username_variable, undefined);
  assert.equal(sourceFrom(draftFrom(cases[4]), 'api_key').fill_username, undefined);
});

test('what the new kinds need, and what they keep', () => {
  assert.deepEqual(missingInputs(emptyDraft('vault_db')), ['address', 'role']);
  const ar = emptyDraft('vault_db');
  Object.assign(ar, { address: 'https://v', role: 'ro', auth: 'approle' });
  assert.deepEqual(missingInputs(ar), ['role_id']);
  Object.assign(ar, { token: 'not-used', approle_secret_id: 'sid' });
  assert.deepEqual(typedTokens(ar), { secret_id: 'sid' }, 'AppRole keeps the secret ID, not a token');
  ar.auth = 'token';
  assert.deepEqual(typedTokens(ar), { '': 'not-used' });
  assert.deepEqual(missingInputs(emptyDraft('conjur')), ['url', 'conjur_account', 'login', 'variable']);
  const cj = emptyDraft('conjur');
  cj.token = 'api-key';
  assert.deepEqual(typedTokens(cj), { '': 'api-key' });
  const ccp = emptyDraft('ccp');
  Object.assign(ccp, { url: 'https://ccp', app_id: 'A' });
  assert.deepEqual(missingInputs(ccp), ['object'], 'an account to ask for');
  ccp.account_address = 'db.example.com';
  assert.deepEqual(missingInputs(ccp), []);
  const dl = emptyDraft('delinea');
  assert.deepEqual(missingInputs(dl), ['url', 'username', 'secret_name']);
  dl.token = 'pw';
  assert.deepEqual(typedTokens(dl), { '': 'pw' });
  dl.master = 'ask';
  assert.deepEqual(typedTokens(dl), {}, 'an asked password is never kept');
  dl.master = 'command';
  assert.ok(missingInputs(dl).includes('master_command'));
  const rows = sourceRows({ kind: 'ccp', url: 'https://ccp', app_id: 'DBine', object: 'pg', client_cert: '/c.p12' });
  assert.deepEqual(rows.map((r) => r[0]), ['url', 'app_id', 'object', 'client_cert']);
});

test('Bitwarden, Bitwarden Secrets Manager and Parameter Store', () => {
  const cases = [
    { kind: 'bitwarden', server: 'bitwarden.eu', item: 'pg-main', attribute: 'notes', json_key: 'password', client_id: 'user.6a2f40a5-d692-4c8e-953c-5271ccb77291', master: 'ask', fill_username: true },
    { kind: 'bitwarden', server: 'https://vault.example.com', item: 'pg-main', client_id: 'user.6a2f40a5-d692-4c8e-953c-5271ccb77291', master: 'command', master_command: 'pass show bw' },
    { kind: 'bitwarden_sm', server: 'bitwarden.com', secret: '1c9d6c0e-55aa-4b6f-8a43-0a1b2c3d4e5f', json_key: 'password' },
    { kind: 'ssm', parameter: '/prod/db', version: 'current', region: 'eu-west-1', profile: 'prod', list_index: '2', json_key: 'password', fill_username: true },
  ];
  for (const c of cases) assert.deepEqual(sourceFrom(draftFrom(c), 'password'), c, c.kind);
  assert.deepEqual(missingInputs(emptyDraft('bitwarden')), ['item', 'client_id']);
  assert.deepEqual(missingInputs(emptyDraft('bitwarden_sm')), ['secret']);
  assert.deepEqual(missingInputs(emptyDraft('ssm')), ['parameter']);
  const bw = draftFrom(cases[0]);
  bw.master = 'command';
  assert.deepEqual(missingInputs(bw), ['master_command']);
  bw.master = 'none';
  assert.equal(sourceFrom(bw, 'password').master, undefined, 'Bitwarden always has a master password');
  // The keychain: the master password and the client secret; the access token.
  const typed = emptyDraft('bitwarden');
  Object.assign(typed, { token: 'master', client_secret: 'cs' });
  assert.deepEqual(typedTokens(typed), { '': 'master', client_secret: 'cs' });
  typed.master = 'ask';
  assert.deepEqual(typedTokens(typed), { client_secret: 'cs' }, 'an asked password is never kept');
  const sm = emptyDraft('bitwarden_sm');
  sm.token = '0.id.secret:key';
  assert.deepEqual(typedTokens(sm), { '': '0.id.secret:key' });
  assert.deepEqual(typedTokens(emptyDraft('ssm')), {});
  // The confirmation shows the server and the item.
  assert.deepEqual(sourceRows(cases[1]).map((r) => r[0]), ['client_id', 'master', 'master_command', 'server', 'item']);
  assert.equal(sourceFrom(draftFrom(cases[3]), 'api_key').fill_username, undefined);
});

test('the selector: local kinds first, then the managers by their name in the language', async () => {
  const { orderedKinds, SOURCE_KINDS } = await import('./secretSources.ts');
  const names = { saved: 'Guardar en el llavero', ask: 'Preguntar cada vez', command: 'Línea de comandos', onepassword: '1Password', vault: 'HashiCorp Vault',
    aws: 'AWS Secrets Manager', azure: 'Azure Key Vault', gcp: 'Google Secret Manager', keepass: 'KeePass', zoho: 'Zoho Vault',
    vault_db: 'HashiCorp Vault (credenciales de base de datos)', conjur: 'CyberArk Conjur', ccp: 'CyberArk Central Credential Provider',
    delinea: 'Delinea Secret Server', bitwarden: 'Bitwarden', bitwarden_sm: 'Bitwarden Secrets Manager', ssm: 'AWS Parameter Store' };
  const { local, managers } = orderedKinds((k) => names[k], 'es', true);
  assert.deepEqual(local, ['saved', 'ask', 'command']);
  assert.deepEqual(managers.map((k) => names[k]), ['1Password', 'AWS Parameter Store', 'AWS Secrets Manager', 'Azure Key Vault', 'Bitwarden',
    'Bitwarden Secrets Manager', 'CyberArk Central Credential Provider', 'CyberArk Conjur', 'Delinea Secret Server', 'Google Secret Manager',
    'HashiCorp Vault', 'HashiCorp Vault (credenciales de base de datos)', 'KeePass', 'Zoho Vault']);
  assert.equal(local.length + managers.length, SOURCE_KINDS.length);
  // Without "ask" (SSH, tokens), and another language's names sort their own way.
  assert.deepEqual(orderedKinds((k) => names[k], 'es', false).local, ['saved', 'command']);
  const de = orderedKinds((k) => (k === 'zoho' ? 'Ärger Vault' : names[k]), 'de', false).managers;
  assert.ok(de.indexOf('zoho') < de.indexOf('bitwarden'), 'Ä sorts with A in German');
});

test('browsing fills the item and its field; the user comes from the source', async () => {
  const { applyBrowse, missingForBrowse, userFromSource, emptyDraft } = await import('./secretSources.ts');
  const kp = emptyDraft('keepass');
  assert.deepEqual(missingForBrowse(kp, true), ['file'], 'the entry is what browsing chooses');
  kp.file = '~/db.kdbx';
  assert.deepEqual(missingForBrowse(kp, true), []);
  assert.deepEqual(missingForBrowse(kp), ['token'], 'a new connection: the master password typed first');
  kp.master = 'ask';
  assert.deepEqual(missingForBrowse(kp), []);
  applyBrowse(kp, { id: 'Databases/Prod/pg-main' }, 'Password');
  assert.equal(kp.entry, 'Databases/Prod/pg-main');
  assert.equal(kp.attribute, '', "the entry's own password field is the default");
  applyBrowse(kp, { id: 'Databases/Prod/pg-main' }, 'API token', 'Login');
  assert.equal(kp.attribute, 'API token');
  assert.equal(kp.user_field, 'Login');
  applyBrowse(kp, { id: 'Databases/Prod/pg-main' }, 'Password', 'UserName');
  assert.equal(kp.user_field, '', "the entry's own user field is the default");
  const op = emptyDraft('onepassword');
  applyBrowse(op, { id: 'op://Private/pg' }, 'Database/password');
  assert.equal(op.reference, 'op://Private/pg/Database/password');
  const db = emptyDraft('vault_db');
  applyBrowse(db, { id: 'app', path: 'static-roles' }, null);
  assert.equal(db.role, 'app');
  assert.equal(db.static_role, true);
  const aws = emptyDraft('aws');
  applyBrowse(aws, { id: 'prod/db' }, '');
  assert.equal(aws.json_key, '', 'the whole value');
  assert.equal(userFromSource(db), true, 'Vault database credentials always give the user');
  assert.equal(userFromSource(aws), false);
  aws.fill_username = true;
  assert.equal(userFromSource(aws), true);
  const conjur = emptyDraft('conjur');
  conjur.username_variable = 'db/user';
  assert.equal(userFromSource(conjur), true);
  const vault = emptyDraft('vault');
  vault.fill_username = true;
  assert.equal(userFromSource(vault), false, 'Vault KV has no user to give');
});

test('entries give the user: fields, 1Password references, older definitions', async () => {
  const { draftFrom, sourceFrom, userFromSource, passwordField, setPasswordField, fieldsQuery, emptyDraft } = await import('./secretSources.ts');
  // An older KeePass definition without the user keeps the connection's user field.
  const old = draftFrom({ kind: 'keepass', file: '~/db.kdbx', entry: 'pg' });
  assert.equal(userFromSource(old), false);
  assert.deepEqual(sourceFrom(old, 'password'), { kind: 'keepass', file: '~/db.kdbx', entry: 'pg' }, 'unchanged: same confirmation');
  const kp = draftFrom({ kind: 'keepass', file: '~/db.kdbx', entry: 'pg', fill_username: true, user_field: 'Login' });
  assert.equal(userFromSource(kp), true);
  assert.equal(sourceFrom(kp, 'password').user_field, 'Login');
  assert.equal(sourceFrom(kp, 'api_key').user_field, undefined, 'only the password gives the user');
  const op = emptyDraft('onepassword');
  op.reference = 'op://Private/pg/password';
  assert.equal(passwordField(op), 'password');
  setPasswordField(op, 'Database/secret');
  assert.equal(op.reference, 'op://Private/pg/Database/secret');
  assert.deepEqual(fieldsQuery(op), { item: 'op://Private/pg', path: 'Private' });
  op.fill_username = true;
  assert.equal(userFromSource(op), true);
  assert.equal(sourceFrom(op, 'password').fill_username, true);
  assert.equal(fieldsQuery(emptyDraft('keepass')), null, 'no entry yet');
});
