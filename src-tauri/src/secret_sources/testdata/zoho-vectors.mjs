// How zoho-vectors.json was made (not run by the tests, which only read the
// JSON): Zoho Vault's own browser crypto (zohovault.js: Chris Veness' AES-CTR
// and the webtoolkit SHA-256 Zoho's web client ships; not committed here),
// Node's crypto for PBKDF2, and a dummy RSA key from OpenSSL:
//
//   openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out k.pem
//   openssl pkcs8 -topk8 -nocrypt -in k.pem -outform DER -out k.der
//   printf %s dummy-org-key-0123456789abcdef0123456789abcdef > org.txt
//   openssl pkeyutl -encrypt -pubin -inkey pub.pem -pkeyopt rsa_padding_mode:oaep \
//     -pkeyopt rsa_oaep_md:sha256 -pkeyopt rsa_mgf1_md:sha256 -in org.txt -out oaep.bin
//   openssl pkeyutl -encrypt -pubin -inkey pub.pem -pkeyopt rsa_padding_mode:pkcs1 -in org.txt -out pkcs1.bin
//   node zoho-vectors.mjs path/to/zohovault.mjs <folder with the files above> > zoho-vectors.json
//
// Every value is a dummy.
import crypto from 'node:crypto';
import fs from 'node:fs';
import { pathToFileURL } from 'node:url';
const Z = (await import(pathToFileURL(process.argv[2]).href)).default;
const dir = process.argv[3];
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
const orgKey = fs.readFileSync(dir + '/org.txt', 'utf8');
const privB64 = fs.readFileSync(dir + '/k.der').toString('base64');
const out = {
  note: 'Dummy values only. AES ciphertexts from zohovault.js (Zoho Vault web crypto) AES.encrypt(plain, key, 256); master = PBKDF2-HMAC-SHA256(passphrase, salt, iterations, 32) as hex (Node crypto); legacy = zohovault.js hash(passphrase); RSA vectors from OpenSSL.',
  passphrase, salt, iterations, master,
  legacy_passphrase: legacyPass, legacy_master: legacy, legacy_passauth: legacyAuth,
  passphrase_check: passphraseCheck,
  aes,
  org_key: orgKey,
  private_key: Z.AES.encrypt(privB64, master, 256),
  shared_key_oaep: fs.readFileSync(dir + '/oaep.bin').toString('base64'),
  shared_key_pkcs1: fs.readFileSync(dir + '/pkcs1.bin').toString('base64'),
  personal_secret_data: JSON.stringify({ username: Z.AES.encrypt('app_user', master, 256), password: Z.AES.encrypt('pg-secret-1', master, 256), file: '' }),
  shared_secret_data: JSON.stringify({ username: Z.AES.encrypt('org_user', orgKey, 256), password: Z.AES.encrypt('org-secret-9', orgKey, 256) }),
  shared_custom_column: Buffer.from(JSON.stringify({ customcol: [{ colname: 'Port', value: Z.AES.encrypt('5432', orgKey, 256), type: 'text', id: 'custColDiv_0' }] })).toString('base64'),
};
// Round trip with the same library, as a sanity check.
for (const a of aes) if (Z.AES.decrypt(a.cipher, a.key, 256) !== a.plain) throw new Error('round trip ' + a.plain);
process.stdout.write(JSON.stringify(out, null, 2) + '\n');
