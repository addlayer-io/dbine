// Fills a vaultwarden server (Bitwarden-API compatible) for the Bitwarden
// tests, and records what its API returns (bitwarden-recorded.json, read by
// the unit tests). Not run by the tests.
//
//   docker run -d --name dbine-test-vaultwarden -e SIGNUPS_ALLOWED=true -e SIGNUPS_VERIFY=false \
//     -e I_REALLY_WANT_VOLATILE_STORAGE=true -p 28080:80 vaultwarden/server
//   npm install @bitwarden/cli                 (in some scratch folder)
//   node bitwarden-setup.mjs http://127.0.0.1:28080 <that folder>/node_modules > live.json
//
// Two accounts are registered here (a PBKDF2 one and an Argon2id one): their
// keys are made with Node's crypto as Bitwarden's clients make them. Every
// item is then created by the official Bitwarden CLI (`bw create item`),
// which encrypts it with its own code, so reading them back checks DBine
// against the official client. The PBKDF2 account also creates an
// organization (its key RSA-OAEP-SHA1-encrypted for the user, as the clients
// do) with one item in it. `live.json` has what the live test needs
// (DBINE_TEST_BITWARDEN=live.json); `--record <file>` also writes the token
// response and the sync of the PBKDF2 account, for the unit tests.
//
// Every value is a dummy.
import crypto from 'node:crypto';
import { execFile, execFileSync } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import os from 'node:os';
import path from 'node:path';

const [server, modules] = process.argv.slice(2);
const recordAt = process.argv.indexOf('--record') > 0 ? process.argv[process.argv.indexOf('--record') + 1] : null;
const BW = path.join(path.resolve(modules), '@bitwarden/cli/build/bw.js');

const hmac = (key, ...parts) => { const h = crypto.createHmac('sha256', key); for (const p of parts) h.update(p); return h.digest(); };
function hkdfExpand(prk, info, len) {
  let t = Buffer.alloc(0);
  const out = [];
  for (let i = 1; Buffer.concat(out).length < len; i++) { t = hmac(prk, t, Buffer.from(info), Buffer.from([i])); out.push(t); }
  return Buffer.concat(out).subarray(0, len);
}
function enc2(plain, key64) {
  const iv = crypto.randomBytes(16);
  const c = crypto.createCipheriv('aes-256-cbc', key64.subarray(0, 32), iv);
  const ct = Buffer.concat([c.update(plain), c.final()]);
  return `2.${iv.toString('base64')}|${ct.toString('base64')}|${hmac(key64.subarray(32), iv, ct).toString('base64')}`;
}
function masterKey(u) {
  const salt = u.email.trim().toLowerCase();
  if (u.kdf === 0) return crypto.pbkdf2Sync(Buffer.from(u.password), Buffer.from(salt), u.kdfIterations, 32, 'sha256');
  return crypto.argon2Sync('argon2id', {
    message: Buffer.from(u.password), nonce: crypto.createHash('sha256').update(salt).digest(),
    parallelism: u.kdfParallelism, tagLength: 32, memory: u.kdfMemory * 1024, passes: u.kdfIterations,
  });
}
const hashOf = (mk, password) => crypto.pbkdf2Sync(mk, Buffer.from(password), 1, 32, 'sha256').toString('base64');
const stretch = (mk) => Buffer.concat([hkdfExpand(mk, 'enc', 32), hkdfExpand(mk, 'mac', 32)]);

async function call(method, url, { json, form, token } = {}) {
  const headers = { accept: 'application/json' };
  if (token) headers.authorization = `Bearer ${token}`;
  let body;
  if (json) { headers['content-type'] = 'application/json'; body = JSON.stringify(json); }
  if (form) { headers['content-type'] = 'application/x-www-form-urlencoded'; body = new URLSearchParams(form).toString(); }
  const r = await fetch(url, { method, headers, body });
  const text = await r.text();
  if (!r.ok) throw new Error(`${method} ${url}: ${r.status} ${text}`);
  return text ? JSON.parse(text) : null;
}

// The CLI only talks HTTPS: it goes through a TLS proxy to the container,
// with a throwaway certificate it's told to trust.
const tls = fs.mkdtempSync(path.join(os.tmpdir(), 'dbine-bw-tls-'));
execFileSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1', '-subj', '/CN=localhost', '-addext', 'subjectAltName=DNS:localhost,IP:127.0.0.1',
  '-keyout', path.join(tls, 'key.pem'), '-out', path.join(tls, 'cert.pem')], { stdio: 'ignore' });
const target = new URL(server);
const proxy = https.createServer({ key: fs.readFileSync(path.join(tls, 'key.pem')), cert: fs.readFileSync(path.join(tls, 'cert.pem')) }, (req, res) => {
  const up = http.request({ host: target.hostname, port: target.port, path: req.url, method: req.method, headers: { ...req.headers, host: target.host } }, (r) => {
    res.writeHead(r.statusCode, r.headers);
    r.pipe(res);
  });
  req.pipe(up);
});
await new Promise((r) => proxy.listen(0, '127.0.0.1', r));
const tlsServer = `https://localhost:${proxy.address().port}`;

function bw(home, args, env = {}) {
  return new Promise((resolve, reject) => execFile(process.execPath, [BW, ...args], {
    env: { ...process.env, BITWARDENCLI_APPDATA_DIR: home, BW_NOINTERACTION: 'true', NODE_NO_WARNINGS: '1', NODE_EXTRA_CA_CERTS: path.join(tls, 'cert.pem'), ...env },
    encoding: 'utf8',
  }, (err, stdout, stderr) => (err ? reject(new Error(`bw ${args[0]}: ${stderr || err.message}`)) : resolve(stdout.trim()))));
}

const DEVICE = { deviceType: '21', deviceIdentifier: crypto.randomUUID(), deviceName: 'dbine-setup' };

async function setupUser(u) {
  const mk = masterKey(u);
  const userKey = crypto.randomBytes(64);
  const { publicKey, privateKey } = crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
  const pub = publicKey.export({ type: 'spki', format: 'der' });
  const priv = privateKey.export({ type: 'pkcs8', format: 'der' });
  // Vaultwarden with SIGNUPS_VERIFY off answers the verification token itself.
  const verify = await call('POST', `${server}/identity/accounts/register/send-verification-email`, { json: { email: u.email, name: u.name, receiveMarketingEmails: false } });
  await call('POST', `${server}/identity/accounts/register/finish`, {
    json: {
      email: u.email, emailVerificationToken: verify, masterPasswordHash: hashOf(mk, u.password), masterPasswordHint: null,
      userSymmetricKey: enc2(userKey, stretch(mk)), kdf: u.kdf, kdfIterations: u.kdfIterations, kdfMemory: u.kdfMemory ?? null, kdfParallelism: u.kdfParallelism ?? null,
      userAsymmetricKeys: { publicKey: pub.toString('base64'), encryptedPrivateKey: enc2(priv, userKey) },
    },
  });
  const tok = await call('POST', `${server}/identity/connect/token`, {
    form: { grant_type: 'password', username: u.email, password: hashOf(mk, u.password), scope: 'api offline_access', client_id: 'cli', ...DEVICE },
  });
  const profile = await call('GET', `${server}/api/accounts/profile`, { token: tok.access_token });
  const api = await call('POST', `${server}/api/accounts/api-key`, { json: { masterPasswordHash: hashOf(mk, u.password) }, token: tok.access_token });
  let org = null;
  if (u.org) {
    const orgKey = crypto.randomBytes(64);
    const ok = crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
    const wrapped = crypto.publicEncrypt({ key: publicKey, padding: crypto.constants.RSA_PKCS1_OAEP_PADDING, oaepHash: 'sha1' }, orgKey);
    org = await call('POST', `${server}/api/organizations`, {
      token: tok.access_token,
      json: {
        name: 'DBine test org', billingEmail: u.email, planType: 0, key: `4.${wrapped.toString('base64')}`,
        keys: { publicKey: ok.publicKey.export({ type: 'spki', format: 'der' }).toString('base64'), encryptedPrivateKey: enc2(ok.privateKey.export({ type: 'pkcs8', format: 'der' }), orgKey) },
        collectionName: enc2(Buffer.from('Default collection'), orgKey),
      },
    });
    const cols = await call('GET', `${server}/api/organizations/${org.id}/collections`, { token: tok.access_token });
    org.collectionId = cols.data[0].id;
  }
  // The official CLI creates the items.
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'dbine-bw-'));
  await bw(home, ['config', 'server', tlsServer]);
  const session = await bw(home, ['login', u.email, u.password, '--raw']);
  const create = async (item) => JSON.parse(await bw(home, ['create', 'item', Buffer.from(JSON.stringify(item)).toString('base64')], { BW_SESSION: session })).id;
  const login = (name, username, password, extra = {}) => ({
    type: 1, name, notes: null, favorite: false, fields: [], reprompt: 0,
    login: { username, password, totp: null, uris: [{ match: null, uri: 'postgres://127.0.0.1:25010' }] }, ...extra,
  });
  const items = {};
  items.main = await create(login('dbine-test-postgres', 'postgres', 'pw', {
    notes: '{"username":"postgres","password":"pw","port":5432}',
    fields: [{ name: 'Port', value: '5432', type: 0 }, { name: 'API token', value: 'hidden-token-1', type: 1 }],
  }));
  items.twin1 = await create(login('twin', 'a', 'twin-1'));
  items.twin2 = await create(login('twin', 'b', 'twin-2'));
  items.note = await create({ type: 2, name: 'pg notes', notes: '{"password":"pw-from-note"}', secureNote: { type: 0 }, fields: [], reprompt: 0 });
  if (org) {
    await bw(home, ['sync'], { BW_SESSION: session });
    items.org = await create(login('org-db', 'org_user', 'org-secret-9', { organizationId: org.id, collectionIds: [org.collectionId] }));
  }
  const listed = JSON.parse(await bw(home, ['list', 'items'], { BW_SESSION: session }));
  if (!listed.some((i) => i.id === items.main && i.login.password === 'pw')) throw new Error('the CLI did not read its own item back');
  await bw(home, ['logout']);
  fs.rmSync(home, { recursive: true, force: true });
  return { email: u.email, password: u.password, client_id: `user.${profile.id}`, client_secret: api.apiKey, items, organization_id: org?.id ?? null };
}

const users = [
  { email: 'pbkdf2@dbine.test', name: 'PBKDF2', password: 'dummy-master-ñ-1', kdf: 0, kdfIterations: 100000, org: true },
  { email: 'Argon2@DBine.test', name: 'Argon2', password: 'dummy-master-2', kdf: 1, kdfIterations: 3, kdfMemory: 16, kdfParallelism: 2 },
];
const out = { server, users: [] };
for (const u of users) out.users.push(await setupUser(u));

if (recordAt) {
  // What DBine itself asks for, with the API key: recorded for the unit tests.
  const u = out.users[0];
  const tok = await call('POST', `${server}/identity/connect/token`, {
    form: { grant_type: 'client_credentials', scope: 'api', client_id: u.client_id, client_secret: u.client_secret, ...DEVICE },
  });
  const sync = await call('GET', `${server}/api/sync?excludeDomains=true`, { token: tok.access_token });
  for (const k of ['access_token', 'refresh_token']) if (tok[k]) tok[k] = 'recorded-token';
  fs.writeFileSync(recordAt, JSON.stringify({
    note: 'Recorded from vaultwarden (bitwarden-setup.mjs): the items were encrypted by the official Bitwarden CLI. Dummy values only.',
    email: u.email, password: u.password, client_id: u.client_id, items: u.items, token_response: tok, sync,
  }, null, 2) + '\n');
}
proxy.close();
fs.rmSync(tls, { recursive: true, force: true });
process.stdout.write(JSON.stringify(out, null, 2) + '\n');
