// How zoho-vectors.json was made (not run by the tests, which only read the
// JSON):
//
//   node zoho-vectors.mjs path/to/zohovault.mjs path/to/jsbn/index.js > zoho-vectors.json
//
// - zohovault.mjs: Zoho Vault's own browser crypto (zohovault.js: Chris
//   Veness' AES-CTR and the webtoolkit SHA-256 its clients ship; not
//   committed here) for every AES value, with an `export default Zohovault`.
// - jsbn: the npm package jsbn@1.1.0 (Tom Wu's BigInteger; shasum
//   b01307cb29b618a1ed26ec79e911f803c4da0040) for the RSA_1024 organization
//   key, encrypted the way Zoho's clients do it with jsbn's RSAKey (the
//   official browser extension 6.10.1, lib/rsa.js and BgZCrypt.js): the
//   private key is the text "n,e,d,p,q,dmp1,dmq1,coeff" in hex
//   (RSAKey.setPrivateEx), the shared key RSA PKCS#1 v1.5 (pkcs1pad2)
//   ciphertext in hex (RSAKey.encrypt). The npm package has no RSAKey, so
//   pkcs1pad2 and encrypt are copied below over its BigInteger.
// - Node's WebCrypto for the RSA_4096 organization key, as the extension's
//   JsCryptoRsaUtilImpl does: RSA-OAEP with SHA-256, the private key as base64
//   PKCS#8, the shared key as base64 ciphertext (2048 bits here, to keep the
//   tests fast; Zoho uses 4096, same format).
// - Node's crypto for PBKDF2.
//
// Both organization keys are the same text, so the shared secrets open with
// either. Every value is a dummy.
import crypto from 'node:crypto';
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';
const Z = (await import(pathToFileURL(process.argv[2]).href)).default;
const { BigInteger } = createRequire(import.meta.url)(process.argv[3]);

const passphrase = 'dummy-passphrase-ñ';
const salt = 'Qx7dummysaltR2v9';
const iterations = 1000;
const master = crypto.pbkdf2Sync(Buffer.from(passphrase, 'utf8'), Buffer.from(salt, 'utf8'), iterations, 32, 'sha256').toString('hex');
const legacyPass = 'dummy-legacy-pass';
const legacy = Z.hash(legacyPass);
const legacyAuth = Z.hash(legacyPass + salt);
const plains = ['pg-secret-1', 'x', 'a value longer than one block: 0123456789abcdef0123456789', 'ünïcödé € ok'];
const aes = plains.map((p) => ({ key: master, plain: p, cipher: Z.AES.encrypt(p, master, 256) }));
aes.push({ key: legacy, plain: 'legacy-secret', cipher: Z.AES.encrypt('legacy-secret', legacy, 256) });
const passphraseCheck = Z.AES.encrypt(JSON.stringify({ date: '2026-10-18T10:00:00.000Z' }), master, 256);
const orgKey = 'dummy-org-key-0123456789abcdef0123456789abcdef';

// ---- RSA_1024: jsbn's RSAKey, as in Zoho's lib/rsa.js ----
const big = (b64url) => new BigInteger(Buffer.from(b64url, 'base64url').toString('hex'), 16);
const { privateKey: k1024 } = crypto.generateKeyPairSync('rsa', { modulusLength: 1024, publicExponent: 0x10001 });
const j = k1024.export({ format: 'jwk' });
const [n, d, p, q, dp, dq, qi] = [j.n, j.d, j.p, j.q, j.dp, j.dq, j.qi].map(big);
// setPrivateEx(n, e, d, p, q, dmp1, dmq1, coeff), each as hex; e as "10001".
const jsbnKeyText = [n.toString(16), '10001', d.toString(16), p.toString(16), q.toString(16), dp.toString(16), dq.toString(16), qi.toString(16)].join(',');
// pkcs1pad2 from Zoho's lib/rsa.js (each char as UTF-8), with Node's random.
function pkcs1pad2(s, len) {
  if (len < s.length + 11) throw new Error('Message too long for RSA');
  const ba = [];
  let i = s.length - 1;
  while (i >= 0 && len > 0) {
    const c = s.charCodeAt(i--);
    if (c < 128) ba[--len] = c;
    else if (c > 127 && c < 2048) { ba[--len] = (c & 63) | 128; ba[--len] = (c >> 6) | 192; }
    else { ba[--len] = (c & 63) | 128; ba[--len] = ((c >> 6) & 63) | 128; ba[--len] = (c >> 12) | 224; }
  }
  ba[--len] = 0;
  while (len > 2) {
    let x = 0;
    while (x === 0) x = crypto.randomBytes(1)[0];
    ba[--len] = x;
  }
  ba[--len] = 2;
  ba[--len] = 0;
  return new BigInteger(ba);
}
// RSAKey.encrypt: m^e mod n, as even-length hex.
function rsaEncrypt(text) {
  const c = pkcs1pad2(text, (n.bitLength() + 7) >> 3).modPowInt(0x10001, n);
  const h = c.toString(16);
  return (h.length & 1) === 0 ? h : '0' + h;
}
// RSAKey.decrypt (CRT, then pkcs1unpad2's framing), as a round-trip check.
function rsaDecrypt(hex) {
  const c = new BigInteger(hex, 16);
  let xp = c.mod(p).modPow(dp, p);
  const xq = c.mod(q).modPow(dq, q);
  while (xp.compareTo(xq) < 0) xp = xp.add(p);
  const b = xp.subtract(xq).multiply(qi).mod(p).multiply(q).add(xq).toByteArray().map((x) => x & 255);
  let i = 0;
  while (i < b.length && b[i] === 0) ++i;
  if (b.length - i !== ((n.bitLength() + 7) >> 3) - 1 || b[i] !== 2) throw new Error('bad padding');
  return Buffer.from(b.slice(b.indexOf(0, i) + 1)).toString('utf8');
}
const jsbnShared = rsaEncrypt(orgKey);
if (rsaDecrypt(jsbnShared) !== orgKey) throw new Error('jsbn round trip');

// ---- RSA_4096: WebCrypto RSA-OAEP / SHA-256, as JsCryptoRsaUtilImpl ----
const subtle = crypto.webcrypto.subtle;
const alg = { name: 'RSA-OAEP', modulusLength: 2048, publicExponent: new Uint8Array([1, 0, 1]), hash: 'SHA-256' };
const pair = await subtle.generateKey(alg, true, ['encrypt', 'decrypt']);
const pkcs8 = Buffer.from(await subtle.exportKey('pkcs8', pair.privateKey)).toString('base64');
const oaep = Buffer.from(await subtle.encrypt({ name: 'RSA-OAEP' }, pair.publicKey, new TextEncoder().encode(orgKey))).toString('base64');
if (new TextDecoder().decode(await subtle.decrypt({ name: 'RSA-OAEP' }, pair.privateKey, Buffer.from(oaep, 'base64'))) !== orgKey) throw new Error('oaep round trip');

const out = {
  note: 'Dummy values only. AES ciphertexts from zohovault.js (Zoho Vault web crypto) AES.encrypt(plain, key, 256); master = PBKDF2-HMAC-SHA256(passphrase, salt, iterations, 32) as hex (Node crypto); legacy = zohovault.js hash(passphrase); RSA_1024 organization key as Zoho\'s jsbn RSAKey (jsbn@1.1.0 BigInteger), RSA_4096 as WebCrypto RSA-OAEP SHA-256. See zoho-vectors.mjs.',
  passphrase, salt, iterations, master,
  legacy_passphrase: legacyPass, legacy_master: legacy, legacy_passauth: legacyAuth,
  passphrase_check: passphraseCheck,
  aes,
  org_key: orgKey,
  rsa1024_private_key: Z.AES.encrypt(jsbnKeyText, master, 256),
  rsa1024_shared_key: jsbnShared,
  rsa4096_private_key: Z.AES.encrypt(pkcs8, master, 256),
  rsa4096_shared_key: oaep,
  personal_secret_data: JSON.stringify({ username: Z.AES.encrypt('app_user', master, 256), password: Z.AES.encrypt('pg-secret-1', master, 256), file: '' }),
  shared_secret_data: JSON.stringify({ username: Z.AES.encrypt('org_user', orgKey, 256), password: Z.AES.encrypt('org-secret-9', orgKey, 256) }),
  shared_custom_column: Buffer.from(JSON.stringify({ customcol: [{ colname: 'Port', value: Z.AES.encrypt('5432', orgKey, 256), type: 'text', id: 'custColDiv_0' }] })).toString('base64'),
};
// Round trip with the same library, as a sanity check.
for (const a of aes) if (Z.AES.decrypt(a.cipher, a.key, 256) !== a.plain) throw new Error('round trip ' + a.plain);
if (Z.AES.decrypt(out.rsa1024_private_key, master, 256) !== jsbnKeyText) throw new Error('round trip private key');
process.stdout.write(JSON.stringify(out, null, 2) + '\n');
