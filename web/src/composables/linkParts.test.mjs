// Tests for linkParts.ts (web addresses in a text become links). Run: `node --test src/composables/linkParts.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { linkParts } from './linkParts.ts';

const urls = (text) => linkParts(text).filter((p) => p.url).map((p) => p.url);

test('a Databricks job\'s address is one link', () => {
  const job = 'https://adb-7666983199141466.6.azuredatabricks.net/jobs/1014598627355';
  assert.deepEqual(linkParts(job), [{ text: job, url: job }]);
  const nb = 'https://adb-1.2.azuredatabricks.net/#notebook/3271915304643002';
  assert.deepEqual(urls(nb), [nb]);
});

test('text without an address stays text', () => {
  assert.deepEqual(linkParts('job 774 · notebook 111'), [{ text: 'job 774 · notebook 111' }]);
  assert.deepEqual(linkParts(''), []);
});

test('an address inside a sentence: the text around it stays, the punctuation that ends it is not part of it', () => {
  assert.deepEqual(linkParts('ver https://app.powerbi.com/groups/me/reports/r1?x=1, luego'), [
    { text: 'ver ' },
    { text: 'https://app.powerbi.com/groups/me/reports/r1?x=1', url: 'https://app.powerbi.com/groups/me/reports/r1?x=1' },
    { text: ', luego' },
  ]);
  assert.deepEqual(urls('(https://w.example.com/jobs/1).'), ['https://w.example.com/jobs/1']);
  assert.deepEqual(urls('Fin: https://w.example.com/sql/editor/q1.'), ['https://w.example.com/sql/editor/q1']);
  // A parenthesis the address opens is its own.
  assert.deepEqual(urls('https://en.wikipedia.org/wiki/Foo_(bar)'), ['https://en.wikipedia.org/wiki/Foo_(bar)']);
});

test('two addresses, each its own link, and the text between them', () => {
  const parts = linkParts('job https://w/jobs/1 · notebook https://w/#notebook/2');
  assert.deepEqual(parts.map((p) => p.url ?? null), [null, 'https://w/jobs/1', null, 'https://w/#notebook/2']);
  assert.equal(parts.map((p) => p.text).join(''), 'job https://w/jobs/1 · notebook https://w/#notebook/2', 'nothing is lost');
});

test('only http and https are links', () => {
  for (const t of ['javascript:alert(1)', 'file:///etc/passwd', 'ftp://host/x', 'mailto:a@b.c', 'data:text/html,x', 's3://bucket/raw/']) {
    assert.deepEqual(urls(t), [], t);
  }
  assert.deepEqual(urls('HTTPS://W.EXAMPLE.COM/a'), ['HTTPS://W.EXAMPLE.COM/a']);
});

test('the lineage panels show details through LinkText', () => {
  // Regression guard: a detail printed as plain text can't be clicked.
  const detail = readFileSync(new URL('../components/LineageDetail.vue', import.meta.url), 'utf8');
  assert.ok(detail.includes('<LinkText :text="e.detail" />'));
  assert.ok(!detail.includes('{{ e.detail }}'));
  const source = readFileSync(new URL('../components/LineageSource.vue', import.meta.url), 'utf8');
  assert.ok(!source.includes('{{ source.detail }}'));
  assert.ok(source.includes('<LinkText :text="link" />'), 'a source DBine cannot connect to shows its page');
});
