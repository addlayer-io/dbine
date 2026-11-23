// Tests for the failed-query analysis (fixError.ts, textDiff.ts): the
// prompt carries the SQL and the error and never rows, the answer is read
// (also while it streams), a stale proposal is detected, and the diff's
// chunks rebuild the corrected text. Run: `node --test
// src/composables/fixError.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { buildFixPrompt, destructive, fitEnds, isChange, isStale, parseFixAnswer } from './fixError.ts';
import { applyChunks, diffChunks } from './textDiff.ts';

test('the prompt has the SQL, the engine and the error, and no result data', () => {
  const p = buildFixPrompt({ sql: 'SELECT nmae FROM users', error: 'column "nmae" does not exist', engine: 'PostgreSQL' });
  assert.match(p, /<failed_query>\nSELECT nmae FROM users\n<\/failed_query>/);
  assert.match(p, /<error>\ncolumn "nmae" does not exist\n<\/error>/);
  assert.match(p, /Engine: PostgreSQL/);
  // Its only inputs are those three: nothing like rows can be in it.
  assert.doesNotMatch(p, /rows|result set/i);
  assert.match(p, /Do not run or claim to have run anything/);
});

test('a huge query or error is cut', () => {
  const p = buildFixPrompt({ sql: 'x'.repeat(50_000), error: 'e'.repeat(50_000) });
  assert.ok(p.length < 26_000);
});

test('an answer with a tag and a block is read', () => {
  const a = parseFixAnswer('<explanation>The column is called name.</explanation>\n```sql\nSELECT name FROM users;\n```\n');
  assert.equal(a.explanation, 'The column is called name.');
  assert.equal(a.code, 'SELECT name FROM users;');
  assert.ok(a.complete);
});

test('an answer without the tag takes the text before the block', () => {
  const a = parseFixAnswer('The table is misspelled.\n\n```\nSELECT 1\n```');
  assert.equal(a.explanation, 'The table is misspelled.');
  assert.equal(a.code, 'SELECT 1');
});

test('with no block there is only the explanation', () => {
  const a = parseFixAnswer('<explanation>Permission denied: ask your DBA.</explanation>');
  assert.equal(a.code, null);
  assert.equal(a.explanation, 'Permission denied: ask your DBA.');
});

test('a streaming answer shows its explanation and holds the unfinished code back', () => {
  let a = parseFixAnswer('<explanation>The column na');
  assert.equal(a.explanation, 'The column na');
  assert.equal(a.code, null);
  a = parseFixAnswer('<explanation>Done.</explanation>\n```sql\nSELECT na');
  assert.equal(a.explanation, 'Done.');
  assert.equal(a.code, null);
  assert.equal(a.complete, false);
});

test('only the first block counts, and reasoning is dropped', () => {
  const a = parseFixAnswer('<think>hmm ```sql\nSELECT 0\n```</think><explanation>E</explanation>\n```sql\nSELECT 1\n```\nand also\n```sql\nSELECT 2\n```');
  assert.equal(a.code, 'SELECT 1');
});

test('an empty block is no correction', () => {
  assert.equal(parseFixAnswer('<explanation>E</explanation>\n```sql\n\n```').code, null);
});

test('the same query is not a change', () => {
  assert.equal(isChange('SELECT 1;', 'SELECT 1;\n'), false);
  assert.equal(isChange('SELECT 1', null), false);
  assert.equal(isChange('SELECT 1', 'SELECT 2'), true);
});

test('stale: the text at the place changed', () => {
  const at = { from: 7, to: 15, original: 'SELECT 1' };
  assert.equal(isStale('-- hi\n\nSELECT 1\n', at), false);
  assert.equal(isStale('-- hi\n\nSELECT 2\n', at), true);
  assert.equal(isStale('', at), true);
  // Text typed before it moves it: stale too.
  assert.equal(isStale('x-- hi\n\nSELECT 1\n', at), true);
});

test('the corrected query keeps the ending of what ran', () => {
  assert.equal(fitEnds('SELECT 1;\n', 'SELECT 2'), 'SELECT 2;\n');
  assert.equal(fitEnds('SELECT 1', 'SELECT 2;\n'), 'SELECT 2;');
  assert.equal(fitEnds('db.users.find({})', 'db.users.find({a:1})'), 'db.users.find({a:1})');
});

test('destructive code is flagged, comments and strings are not', () => {
  assert.equal(destructive('DELETE FROM t'), true);
  assert.equal(destructive("SELECT 'drop table' -- delete\n"), false);
  assert.equal(destructive('SELECT 1'), false);
});

test('the diff chunks turn the original into the corrected text', () => {
  const cases = [
    ['SELECT nmae FROM users', 'SELECT name FROM users'],
    ['SELECT * FROM a JOIN b ON a.x = b.y', 'SELECT * FROM a JOIN b ON a.id = b.a_id WHERE a.z > 1'],
    ['', 'SELECT 1'],
    ['SELECT 1', ''],
    ['same', 'same'],
    ['a\nb\nc', 'a\nc'],
  ];
  for (const [a, b] of cases) {
    const chunks = diffChunks(a, b);
    assert.equal(applyChunks(a, chunks), b);
    for (const c of chunks) assert.ok(c.from <= c.to);
  }
  assert.deepEqual(diffChunks('same', 'same'), []);
});

test('a one-word fix is one small chunk', () => {
  const [c, ...rest] = diffChunks('SELECT nmae FROM users', 'SELECT name FROM users');
  assert.equal(rest.length, 0);
  assert.deepEqual(c, { from: 7, to: 11, insert: 'name' });
});

test('chunks can be applied one by one', () => {
  const a = 'SELECT a, b FROM t WHERE x = 1';
  const b = 'SELECT aa, b FROM tt WHERE x = 1';
  const chunks = diffChunks(a, b);
  assert.equal(chunks.length, 2);
  assert.equal(applyChunks(a, chunks, new Set([0])), 'SELECT aa, b FROM t WHERE x = 1');
  assert.equal(applyChunks(a, chunks, new Set()), a);
});
