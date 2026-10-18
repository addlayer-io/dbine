// How bitwarden-vectors.json was made (not run by the tests, which only
// read the JSON):
//
// - Secrets Manager: a machine account's access token, its encrypted
//   payload and two secrets, encrypted with Node's crypto as Bitwarden's SDK
//   does, then read back by the official SDK (@bitwarden/sdk-napi) against a
//   mock of the identity and API servers served here. The JSON is written
//   only if the official SDK decrypts every value to what was encrypted.
// - The vault's key derivation (PBKDF2-SHA256 and Argon2id with Node's
//   crypto, then HKDF-Expand "enc"/"mac") and an item encrypted with its own
//   cipher key (the format the clients write when cipher key encryption is
//   on; the recorded vaultwarden items in bitwarden-recorded.json don't use it).
//
//   npm install @bitwarden/sdk-napi            (in some scratch folder)
//   node bitwarden-vectors.mjs <that folder>/node_modules > bitwarden-vectors.json
//
// Every value is a dummy.
import crypto from 'node:crypto';
import http from 'node:http';
import { createRequire } from 'node:module';
import path from 'node:path';

const hmac = (key, ...parts) => { const h = crypto.createHmac('sha256', key); for (const p of parts) h.update(p); return h.digest(); };
/** HKDF-Expand (RFC 5869) with SHA-256: the PRK is already a key. */
function hkdfExpand(prk, info, len) {
  let t = Buffer.alloc(0);
  const out = [];
  for (let i = 1; Buffer.concat(out).length < len; i++) {
    t = hmac(prk, t, Buffer.from(info), Buffer.from([i]));
    out.push(t);
  }
  return Buffer.concat(out).subarray(0, len);
}
/** EncString type 2: AES-256-CBC (PKCS#7) + HMAC-SHA256 over iv || ct. */
function enc2(plain, key64, iv) {
  const c = crypto.createCipheriv('aes-256-cbc', key64.subarray(0, 32), iv);
  const ct = Buffer.concat([c.update(plain), c.final()]);
  const mac = hmac(key64.subarray(32), iv, ct);
  return `2.${iv.toString('base64')}|${ct.toString('base64')}|${mac.toString('base64')}`;
}
const iv = (n) => Buffer.alloc(16, n);

// The access token: `0.<id>.<client secret>:<base64 of a 16-byte seed>`.
const tokenId = '7b1d1c26-3f5e-4c8a-9a0b-2f3e4d5c6b7a';
const clientSecret = 'dummyClientSecretAbCdEf0123456789xy';
const seed = Buffer.from('000102030405060708090a0b0c0d0e0f', 'hex');
const accessToken = `0.${tokenId}.${clientSecret}:${seed.toString('base64')}`;
// derive_shareable_key(seed, "accesstoken", "sm-access-token").
const prk = hmac(Buffer.from('bitwarden-accesstoken'), seed);
const tokenKey = hkdfExpand(prk, 'sm-access-token', 64);
const orgId = 'a0a1a2a3-b0b1-4c0c-9d0d-e0e1e2e3e4e5';
const orgKey = crypto.createHash('sha512').update('dummy organization key').digest();
const payload = enc2(Buffer.from(JSON.stringify({ encryptionKey: orgKey.toString('base64') })), tokenKey, iv(1));
const secrets = [
  { id: '1c9d6c0e-55aa-4b6f-8a43-0a1b2c3d4e5f', key: 'dbine/pg', value: '{"username":"postgres","password":"pw"}', note: 'the test database' },
  { id: '2d0e7d1f-66bb-4c70-9b54-1b2c3d4e5f60', key: 'plain', value: 'ünïcödé € a value longer than one block 0123456789', note: '' },
];
const enc = secrets.map((s, i) => ({
  object: 'secret', id: s.id, organizationId: orgId, projectId: null,
  key: enc2(Buffer.from(s.key), orgKey, iv(10 + i)),
  value: enc2(Buffer.from(s.value), orgKey, iv(20 + i)),
  note: enc2(Buffer.from(s.note), orgKey, iv(30 + i)),
  creationDate: '2026-10-18T10:00:00Z', revisionDate: '2026-10-18T10:00:00Z',
}));
const b64url = (o) => Buffer.from(JSON.stringify(o)).toString('base64url');
const jwt = `${b64url({ alg: 'RS256', typ: 'JWT' })}.${b64url({ sub: tokenId, organization: orgId, exp: 4102444800, iss: 'mock', client_id: tokenId, scope: ['api.secrets'] })}.c2lnbmF0dXJl`;
const tokenResponse = { access_token: jwt, expires_in: 3600, token_type: 'Bearer', scope: 'api.secrets', encrypted_payload: payload };

// A mock of identity and api, for the official SDK.
const seen = [];
const server = http.createServer((req, res) => {
  let body = '';
  req.on('data', (d) => { body += d; });
  req.on('end', () => {
    seen.push(`${req.method} ${req.url}`);
    const json = (code, o) => { res.writeHead(code, { 'content-type': 'application/json' }); res.end(JSON.stringify(o)); };
    if (req.method === 'POST' && req.url === '/identity/connect/token') {
      const f = new URLSearchParams(body);
      const ok = f.get('grant_type') === 'client_credentials' && f.get('scope') === 'api.secrets' && f.get('client_id') === tokenId && f.get('client_secret') === clientSecret;
      return ok ? json(200, tokenResponse) : json(400, { error: 'invalid_client' });
    }
    const m = req.url.match(/^\/api\/secrets\/([0-9a-f-]+)$/);
    if (req.method === 'GET' && m && req.headers.authorization === `Bearer ${jwt}`) {
      const s = enc.find((e) => e.id === m[1]);
      return s ? json(200, s) : json(404, { message: 'Resource not found.' });
    }
    json(404, { message: `unexpected ${req.method} ${req.url}` });
  });
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const base = `http://127.0.0.1:${server.address().port}`;
const require = createRequire(path.join(path.resolve(process.argv[2]), 'x.js'));
const { BitwardenClient } = require('@bitwarden/sdk-napi');
const client = new BitwardenClient({ apiUrl: `${base}/api`, identityUrl: `${base}/identity`, userAgent: 'dbine-vectors', deviceType: 'SDK' }, 4);
await client.auth().loginAccessToken(accessToken);
for (const s of secrets) {
  const got = await client.secrets().get(s.id);
  if (got.key !== s.key || got.value !== s.value || got.note !== s.note) throw new Error(`the official SDK read ${JSON.stringify(got)} for ${s.id}`);
}
server.close();

// The vault: key derivation and an item with its own cipher key.
const pbkdf2 = { password: 'dummy-master-ñ-1', salt: 'pbkdf2@dbine.test', iterations: 5000 };
const pbkdf2Master = crypto.pbkdf2Sync(Buffer.from(pbkdf2.password), Buffer.from(pbkdf2.salt), pbkdf2.iterations, 32, 'sha256');
const argon = { password: 'dummy-master-2', salt: 'argon2@dbine.test', iterations: 2, memory: 16, parallelism: 2 };
const argonMaster = crypto.argon2Sync('argon2id', {
  message: Buffer.from(argon.password), nonce: crypto.createHash('sha256').update(argon.salt).digest(),
  parallelism: argon.parallelism, tagLength: 32, memory: argon.memory * 1024, passes: argon.iterations,
});
const stretched = Buffer.concat([hkdfExpand(pbkdf2Master, 'enc', 32), hkdfExpand(pbkdf2Master, 'mac', 32)]);
const userKey = crypto.createHash('sha512').update('dummy user key').digest();
const cipherKey = crypto.createHash('sha512').update('dummy cipher key').digest();
const withCipherKey = {
  object: 'cipherDetails', id: '5e6f7a8b-9c0d-4e1f-8a2b-3c4d5e6f7a8b', type: 1, organizationId: null, deletedDate: null,
  key: enc2(cipherKey, userKey, iv(40)),
  name: enc2(Buffer.from('with-cipher-key'), cipherKey, iv(41)),
  notes: null,
  login: { username: enc2(Buffer.from('ck_user'), cipherKey, iv(42)), password: enc2(Buffer.from('ck-secret'), cipherKey, iv(43)), totp: null, uris: [] },
  fields: [{ name: enc2(Buffer.from('Linked password'), cipherKey, iv(44)), value: null, type: 3, linkedId: 101 }],
};
const vault = {
  pbkdf2: { ...pbkdf2, master: pbkdf2Master.toString('hex'), stretched: stretched.toString('base64') },
  argon2id: { ...argon, master: argonMaster.toString('hex') },
  user_key: userKey.toString('base64'),
  protected_user_key: enc2(userKey, stretched, iv(50)),
  item_with_cipher_key: withCipherKey,
};
process.stdout.write(JSON.stringify({
  vault,
  note: 'Dummy values only. Made with Node crypto as described in bitwarden-vectors.mjs; the official @bitwarden/sdk-napi decrypted every Secrets Manager secret back against a mock of the API.',
  access_token: accessToken, token_key: tokenKey.toString('base64'), organization_id: orgId, organization_key: orgKey.toString('base64'),
  token_response: tokenResponse, secrets: enc, plain: secrets, sdk_requests: seen,
}, null, 2) + '\n');
