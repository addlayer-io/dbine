// Tests for the feedback URL builder. Run: `node --test src/composables/feedbackUrl.test.mjs`
// from web/ (Node 23.6+ strips the TypeScript types itself). Not part of the
// app's build: nothing imports this file.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { buildFeedbackUrl, diagnosticsText, FEEDBACK_KINDS, MAX_URL, templateFor } from './feedbackUrl.ts';

const BASE = 'https://github.com/addlayer-io/dbine/issues/new?';
const params = (url) => new URL(url).searchParams;

const DIAG = { app_version: '0.1.10', os: 'macOS 15.1', arch: 'aarch64', locale: 'es', engine: 'postgres', server_version: 'PostgreSQL 16.4' };

test('every kind opens its own form', () => {
  for (const kind of FEEDBACK_KINDS) {
    const { url } = buildFeedbackUrl({ kind, title: 'Algo pasa', description: 'Una descripción larga' });
    assert.ok(url.startsWith(`${BASE}template=${kind}.yml&`), url);
    assert.equal(params(url).get('template'), templateFor(kind));
    // Labels are the form's business, never the URL's.
    assert.equal(params(url).get('labels'), null);
    assert.equal(params(url).get('assignees'), null);
  }
});

test('driver requests carry the engine and a title made from it', () => {
  const { url } = buildFeedbackUrl({ kind: 'driver-request', title: '  Firebird   5 ', description: 'La usamos en producción' });
  assert.equal(params(url).get('engine'), 'Firebird 5');
  assert.equal(params(url).get('title'), 'Support for Firebird 5');
});

test('everything is URL-encoded and round-trips', () => {
  const description = 'a & b = c?\n#1 50% <tag> "q" \'s\' + ñ 🚀 /path\\x';
  const { url, truncated } = buildFeedbackUrl({ kind: 'bug', title: 'Título & más', description, steps: '1. x\n2. y' });
  assert.equal(truncated, false);
  assert.equal(params(url).get('description'), description);
  assert.equal(params(url).get('title'), 'Título & más');
  assert.equal(params(url).get('steps'), '1. x\n2. y');
  // Nothing outside the encoded set (what the Rust side accepts).
  assert.match(url, /^[A-Za-z0-9\-_.!~*'()%&=?:/]+$/);
  assert.ok(!/[ "<>\\`#]/.test(url));
});

test('steps only go with bugs', () => {
  const { url } = buildFeedbackUrl({ kind: 'question', title: 'Cómo hago', description: 'Una pregunta', steps: 'no' });
  assert.equal(params(url).get('steps'), null);
});

test('control characters and lone surrogates are cleaned', () => {
  const { url } = buildFeedbackUrl({ kind: 'question', title: 'a\u0007b\tc', description: 'x\u0000y\r\nz \uD800 w' });
  assert.equal(params(url).get('title'), 'ab c');
  assert.equal(params(url).get('description'), 'xy\nz � w');
});

test('the title is one line of at most 120 characters', () => {
  const { url } = buildFeedbackUrl({ kind: 'bug', title: 'x'.repeat(300) + '\nmore', description: 'una descripción' });
  assert.equal(params(url).get('title'), 'x'.repeat(120));
});

test('a long description is cut to fit, with a note', () => {
  const description = 'ñ'.repeat(10000);
  const { url, truncated } = buildFeedbackUrl({ kind: 'bug', title: 'Un error', description, diagnostics: DIAG });
  assert.equal(truncated, true);
  assert.ok(url.length <= MAX_URL, String(url.length));
  const d = params(url).get('description');
  assert.ok(d.endsWith('…(truncated)'));
  assert.ok(d.length > 500);
  // The diagnostics survive the cut.
  assert.equal(params(url).get('diagnostics'), diagnosticsText(DIAG));
});

test('emoji are never split when cutting', () => {
  const { url } = buildFeedbackUrl({ kind: 'enhancement', title: 'Idea', description: '🚀'.repeat(5000) });
  assert.ok(url.length <= MAX_URL);
  assert.ok(!params(url).get('description').includes('�'));
});

test('very long steps are cut too, under any cap', () => {
  for (const max of [MAX_URL, 3000]) {
    const { url, truncated } = buildFeedbackUrl({ kind: 'bug', title: 'Un error', description: 'd'.repeat(9000), steps: 's'.repeat(9000) }, max);
    assert.equal(truncated, true);
    assert.ok(url.length <= max, `${url.length} > ${max}`);
    assert.ok(params(url).get('steps').endsWith('…(truncated)'));
  }
});

test('diagnostics carry only the fixed fields', () => {
  const leaky = {
    ...DIAG,
    host: 'db.internal.acme.com', user: 'sa_admin', database: 'payroll_prod',
    connection_name: 'Producción ACME', query: 'SELECT secret FROM t', password: 'hunter2',
  };
  const text = diagnosticsText(leaky);
  assert.equal(text, 'DBine: 0.1.10\nOS: macOS 15.1 (aarch64)\nLanguage: es\nEngine: postgres\nServer: PostgreSQL 16.4');
  const { url } = buildFeedbackUrl({ kind: 'bug', title: 'Un error', description: 'Algo falla', diagnostics: leaky });
  const decoded = decodeURIComponent(url);
  for (const secret of ['db.internal.acme.com', 'sa_admin', 'payroll_prod', 'ACME', 'SELECT secret', 'hunter2']) {
    assert.ok(!decoded.includes(secret), secret);
  }
});

test('the server version keeps only its first line, and an odd engine id drops both', () => {
  const multi = diagnosticsText({ ...DIAG, server_version: 'PostgreSQL 16.4 on x86\ncompiled by gcc on host-42' });
  assert.ok(multi.endsWith('Server: PostgreSQL 16.4 on x86'));
  const odd = diagnosticsText({ ...DIAG, engine: 'Prod DB (10.0.0.5)' });
  assert.ok(!odd.includes('Engine') && !odd.includes('Server'));
  const none = diagnosticsText({ ...DIAG, engine: null, server_version: null });
  assert.equal(none, 'DBine: 0.1.10\nOS: macOS 15.1 (aarch64)\nLanguage: es');
});

test('without diagnostics there is no diagnostics field', () => {
  const { url } = buildFeedbackUrl({ kind: 'enhancement', title: 'Una idea', description: 'Me gustaría…' });
  assert.equal(params(url).get('diagnostics'), null);
});
