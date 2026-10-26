// Tests for the engine's own TLS options in the connection form. Run:
// `node --test src/composables/tlsFields.test.mjs` from web/ (Node 23.6+
// strips the TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { asText, fieldRequired, formValue, legacyIndex, legacyTlsValues, legacyValue, newConnectionPort, portForLevel, tlsLevel, tlsWarning, writeTls } from './tlsFields.ts';

const field = (key, kind, extra = {}) => ({ key, label: key, kind: { type: kind }, required: false, secret: false, placeholder: '', default: '', help: '', ...extra });

const sslmode = {
  ...field('sslmode', 'select'),
  kind: { type: 'select', options: [['disable', 'disable'], ['prefer', 'prefer'], ['require', 'require'], ['verify-ca', 'verify-ca'], ['verify-full', 'verify-full']] },
  default: 'verify-full',
  section: 'ssl',
  tls: { role: 'mode', levels: [['disable', 'off'], ['prefer', 'optional'], ['require', 'unverified'], ['verify-ca', 'verify_ca'], ['verify-full', 'verify_full']] },
  legacy: ['prefer', 'require', 'verify-full', 'require'],
};
const trust = { ...field('TrustServerCertificate', 'bool'), default: 'false', section: 'ssl', tls: { role: 'skip_verify', inverted: false }, legacy: ['false', 'true', 'false', 'true'] };
const ca = { ...field('sslrootcert', 'file'), section: 'ssl', tls: { role: 'ca_file' } };

const pg = {
  id: 'pg', name: 'PG', family: 'relational', language: 'sql', dialect: 'postgres', default_port: 5432,
  fields: [field('host', 'text'), field('port', 'number'), sslmode, trust, ca],
  databases_label: '', has_schemas: true, object_kinds: [], tls: 'configurable', tls_port: null,
};

const saved = (encrypt, trustCert, options = {}) => ({
  driver: 'pg', host: 'db', port: 0, database: '', username: null, password: null,
  encrypt, trust_server_certificate: trustCert, read_only: false, options,
});

/** What the form does: pick() fills the values, buildConfig() writes them. */
function form(d, existing) {
  const values = {};
  for (const f of d.fields) values[f.key] = f.kind.type === 'bool' ? f.default === 'true' : f.default;
  let original = null;
  if (existing) {
    for (const f of d.fields) if (existing.options[f.key] !== undefined) values[f.key] = formValue(f, existing.options[f.key]);
    original = legacyTlsValues(d, existing);
    for (const [k, v] of Object.entries(original.legacy)) values[k] = formValue(d.fields.find((f) => f.key === k), v);
  }
  const build = (shown = d.fields) => {
    const cfg = saved(false, false);
    writeTls(shown, values, original, cfg);
    return cfg;
  };
  return { values, original, build };
}

test('the legacy table is read by (encrypt, trust)', () => {
  assert.deepEqual([legacyIndex(false, false), legacyIndex(false, true), legacyIndex(true, false), legacyIndex(true, true)], [0, 1, 2, 3]);
  assert.equal(legacyValue(sslmode, true, false), 'verify-full');
  assert.equal(legacyValue({ ...sslmode, legacy: ['', '', 'x', 'x'] }, false, false), null);
  assert.equal(legacyValue(ca, true, false), null);
  assert.equal(asText(true), 'true');
  assert.equal(formValue(trust, 'true'), true);
});

test('an old connection shows its legacy values, not the secure default', () => {
  const { values, original } = form(pg, saved(false, false));
  assert.equal(values.sslmode, 'prefer');
  assert.equal(values.TrustServerCertificate, false);
  assert.deepEqual(original.legacy, { sslmode: 'prefer', TrustServerCertificate: 'false' });
  assert.equal(form(pg, saved(true, true)).values.sslmode, 'require');
  assert.equal(form(pg, saved(true, true)).values.TrustServerCertificate, true);
});

test('saving an untouched old connection writes nothing and keeps its bools (R1)', () => {
  for (const [enc, tr] of [[false, false], [false, true], [true, false], [true, true]]) {
    const cfg = form(pg, saved(enc, tr)).build();
    assert.equal(cfg.options.sslmode, undefined, `${enc}/${tr}`);
    assert.equal(cfg.options.TrustServerCertificate, undefined);
    // (false, true) projects to (true, true) by level: the original pair stays.
    assert.deepEqual([cfg.encrypt, cfg.trust_server_certificate], [enc, tr]);
  }
});

test('a CA file set on an untouched old connection keeps the legacy mode and bools', () => {
  const f = form(pg, saved(false, true));
  f.values.sslrootcert = '/ca.pem';
  const cfg = f.build();
  assert.deepEqual(cfg.options, { sslrootcert: '/ca.pem' });
  assert.deepEqual([cfg.encrypt, cfg.trust_server_certificate], [false, true]);
});

test('changing the mode writes every TLS option and dual-writes the bools', () => {
  const f = form(pg, saved(false, false));
  f.values.sslmode = 'verify-full';
  let cfg = f.build();
  assert.deepEqual(cfg.options, { sslmode: 'verify-full', TrustServerCertificate: 'false' });
  assert.deepEqual([cfg.encrypt, cfg.trust_server_certificate], [true, false]);
  f.values.TrustServerCertificate = true;
  cfg = f.build();
  assert.deepEqual([cfg.encrypt, cfg.trust_server_certificate], [true, true]);
  f.values.sslmode = 'disable';
  cfg = f.build();
  assert.equal(cfg.options.sslmode, 'disable');
  assert.deepEqual([cfg.encrypt, cfg.trust_server_certificate], [false, false]);
});

test('a new connection writes the secure defaults', () => {
  const cfg = form(pg, null).build();
  assert.deepEqual(cfg.options, { sslmode: 'verify-full', TrustServerCertificate: 'false' });
  assert.deepEqual([cfg.encrypt, cfg.trust_server_certificate], [true, false]);
});

test('a connection saved with native options keeps them and dual-writes', () => {
  const f = form(pg, saved(false, false, { sslmode: 'verify-ca', TrustServerCertificate: 'false' }));
  assert.equal(f.values.sslmode, 'verify-ca');
  assert.deepEqual(f.original.legacy, {});
  const cfg = f.build();
  assert.equal(cfg.options.sslmode, 'verify-ca');
  assert.deepEqual([cfg.encrypt, cfg.trust_server_certificate], [true, false]);
});

test('hidden fields are not written', () => {
  const f = form(pg, null);
  const cfg = f.build([sslmode]);
  assert.deepEqual(cfg.options, { sslmode: 'verify-full' });
});

test('a driver still on the legacy checkboxes keeps its bools from them', () => {
  const legacyDriver = { ...pg, fields: [field('host', 'text'), field('encrypt', 'bool'), field('trust_server_certificate', 'bool'), ca] };
  const cfg = saved(true, true);
  writeTls(legacyDriver.fields, { sslrootcert: '/ca.pem' }, { encrypt: false, trust: false, legacy: {} }, cfg);
  assert.deepEqual([cfg.encrypt, cfg.trust_server_certificate, cfg.options.sslrootcert], [true, true, '/ca.pem']);
  // No TLS fields at all: nothing happens.
  const plain = saved(true, false);
  writeTls([field('host', 'text')], {}, null, plain);
  assert.deepEqual([plain.encrypt, plain.options], [true, {}]);
});

test('the level follows the mode and the skip options', () => {
  const get = (v) => (k) => v[k];
  assert.equal(tlsLevel(pg.fields, get({ sslmode: 'verify-full', TrustServerCertificate: false })), 'verify_full');
  assert.equal(tlsLevel(pg.fields, get({ sslmode: 'verify-full', TrustServerCertificate: true })), 'unverified');
  assert.equal(tlsLevel(pg.fields, get({ sslmode: 'prefer', TrustServerCertificate: true })), 'optional');
  assert.equal(tlsLevel(pg.fields, get({ sslmode: 'bogus' })), null);
  const inverted = [{ ...trust, key: 'validate', tls: { role: 'skip_verify', inverted: true } }, sslmode];
  assert.equal(tlsLevel(inverted, get({ sslmode: 'verify-full', validate: false })), 'unverified');
  assert.equal(tlsLevel(inverted, get({ sslmode: 'verify-full', validate: true })), 'verify_full');
  const host = [{ ...trust, key: 'noHost', tls: { role: 'skip_hostname' } }, sslmode];
  assert.equal(tlsLevel(host, get({ sslmode: 'verify-full', noHost: true })), 'verify_ca');
  assert.equal(tlsLevel(host, get({ sslmode: 'verify-full', noHost: false })), 'verify_full');
  // An inverted skip-hostname (ssl_assert_hostname=false skips the check).
  const assertHost = [{ ...trust, key: 'ssl_assert_hostname', default: 'true', tls: { role: 'skip_hostname', inverted: true } }, sslmode];
  assert.equal(tlsLevel(assertHost, get({ sslmode: 'verify-full', ssl_assert_hostname: false })), 'verify_ca');
  assert.equal(tlsLevel(assertHost, get({ sslmode: 'verify-full', ssl_assert_hostname: true })), 'verify_full');
});

// Trino: the SSL switch and SSLVerification FULL / CA / NONE.
const sslSwitch = { ...field('SSL', 'bool'), default: 'true', section: 'ssl', tls: { role: 'mode', levels: [['true', 'verify_full'], ['false', 'off']] }, legacy: ['false', 'false', 'true', 'true'] };
const verification = {
  ...field('SSLVerification', 'select'),
  kind: { type: 'select', options: [['FULL', 'FULL'], ['CA', 'CA'], ['NONE', 'NONE']] },
  default: 'FULL',
  section: 'ssl',
  tls: { role: 'verify', levels: [['FULL', 'verify_full'], ['CA', 'verify_ca'], ['NONE', 'unverified']] },
  legacy: ['FULL', 'NONE', 'FULL', 'NONE'],
};
const trino = { ...pg, id: 'trino', default_port: 8080, tls_port: 8443, fields: [field('host', 'text'), sslSwitch, verification] };

test('a verification select caps the mode, never raises it', () => {
  const get = (v) => (k) => v[k];
  assert.equal(tlsLevel(trino.fields, get({ SSL: true, SSLVerification: 'FULL' })), 'verify_full');
  assert.equal(tlsLevel(trino.fields, get({ SSL: true, SSLVerification: 'CA' })), 'verify_ca');
  assert.equal(tlsLevel(trino.fields, get({ SSL: true, SSLVerification: 'NONE' })), 'unverified');
  assert.equal(tlsLevel(trino.fields, get({ SSL: false, SSLVerification: 'FULL' })), 'off');
  assert.equal(tlsLevel(trino.fields, get({ SSL: true, SSLVerification: '' })), 'verify_full');
  assert.equal(tlsLevel(trino.fields, get({ SSL: true, SSLVerification: 'bogus' })), null);
  assert.equal(tlsWarning(trino, get({ SSL: true, SSLVerification: 'NONE' })), 'unverified');
  assert.equal(newConnectionPort(trino), '8443');
});

test('a verification select keeps an old connection as it was and dual-writes when changed', () => {
  for (const [enc, tr] of [[false, false], [false, true], [true, false], [true, true]]) {
    const cfg = form(trino, saved(enc, tr)).build();
    assert.deepEqual(cfg.options, {}, `${enc}/${tr}`);
    assert.deepEqual([cfg.encrypt, cfg.trust_server_certificate], [enc, tr]);
  }
  const f = form(trino, saved(true, true));
  assert.equal(f.values.SSLVerification, 'NONE');
  f.values.SSLVerification = 'CA';
  const cfg = f.build();
  assert.deepEqual(cfg.options, { SSL: 'true', SSLVerification: 'CA' });
  assert.deepEqual([cfg.encrypt, cfg.trust_server_certificate], [true, false]);
  const fresh = form(trino, null).build();
  assert.deepEqual(fresh.options, { SSL: 'true', SSLVerification: 'FULL' });
});

test('a CA file is required only at the levels that need it', () => {
  const caAt = { ...ca, key: 'ssl-ca', tls: { role: 'ca_file', required_at: ['verify_ca'] } };
  assert.equal(fieldRequired(caAt, 'verify_ca'), true);
  assert.equal(fieldRequired(caAt, 'verify_full'), false);
  assert.equal(fieldRequired(caAt, null), false);
  assert.equal(fieldRequired(ca, 'verify_ca'), false);
  assert.equal(fieldRequired({ ...field('host', 'text'), required: true }, null), true);
});

test('the chip warns about plaintext and unverified certificates', () => {
  const get = (v) => (k) => v[k];
  assert.equal(tlsWarning(pg, get({ sslmode: 'prefer' })), 'off');
  assert.equal(tlsWarning(pg, get({ sslmode: 'require' })), 'unverified');
  assert.equal(tlsWarning(pg, get({ sslmode: 'verify-full', TrustServerCertificate: false })), null);
  const legacyDriver = { ...pg, fields: [field('encrypt', 'bool'), field('trust_server_certificate', 'bool')] };
  assert.equal(tlsWarning(legacyDriver, get({ encrypt: false })), 'off');
  assert.equal(tlsWarning(legacyDriver, get({ encrypt: true, trust_server_certificate: true })), 'unverified');
  assert.equal(tlsWarning(legacyDriver, get({ encrypt: true, trust_server_certificate: false })), null);
  assert.equal(tlsWarning({ ...pg, fields: [field('host', 'text')] }, get({})), null);
});

test('the port follows the mode between the plaintext and the TLS defaults', () => {
  const ch = { ...pg, default_port: 8123, tls_port: 8443 };
  assert.equal(newConnectionPort(ch), '8443');
  assert.equal(newConnectionPort({ ...ch, fields: [{ ...sslmode, default: 'disable' }] }), '8123');
  assert.equal(newConnectionPort(pg), '5432');
  assert.equal(portForLevel(ch, '8443', 'off'), '8123');
  assert.equal(portForLevel(ch, '8123', 'verify_full'), '8443');
  assert.equal(portForLevel(ch, '9000', 'off'), '9000');
  assert.equal(portForLevel(ch, '8123', 'optional'), '8123');
  assert.equal(portForLevel(pg, '5432', 'off'), '5432');
});
