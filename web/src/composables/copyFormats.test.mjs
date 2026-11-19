// Tests for the grid's copy formats (copyFormats.ts) with vector columns:
// JSON, YAML and Mongo get a vector's numbers as an array, and a copied
// UPDATE never puts a vector column in its WHERE. Run: `node --test
// src/composables/copyFormats.test.mjs` from web/ (Node 23.6+ strips the
// TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';

// copyFormats.ts imports its siblings without an extension (as Vite does):
// resolve them to the .ts files. Texts come back as their keys.
register('data:text/javascript,' + encodeURIComponent(`
const stubs = {
  '../i18n': 'export const t = (k) => k;',
  '../stores/settings': 'export const useSettingsStore = () => ({ get: (k, d) => d, set() {} });',
  './i18nLabels': 'export const rt = (k) => k;',
};
export async function resolve(spec, ctx, next) {
  if (stubs[spec]) return { url: 'data:text/javascript,' + encodeURIComponent(stubs[spec]), shortCircuit: true };
  if (spec.startsWith('.') && !/\\.[cm]?[jt]s$/.test(spec)) return next(spec + '.ts', ctx);
  return next(spec, ctx);
}`));
const { formatRows } = await import('./copyFormats.ts');

const columns = [
  { name: 'embedding', type_name: 'vector(3)' },
  { name: 'id', type_name: 'int' },
  { name: 'title', type_name: 'text' },
  { name: 'sparse', type_name: 'sparsevec(5)' },
];
const rows = [['[0.1,0.2,0.3]', 1, 'a', '{1:0.5}/5'], [null, 2, '[1,2]', null]];
const ctx = { table: { schema: 'public', name: 'docs' }, dialect: 'postgres' };

test('JSON copies have a vector as its numbers', () => {
  assert.deepEqual(JSON.parse(formatRows('json', columns, rows, ctx)), [
    { embedding: [0.1, 0.2, 0.3], id: 1, title: 'a', sparse: '{1:0.5}/5' },
    // A text column that looks like an array stays text.
    { embedding: null, id: 2, title: '[1,2]', sparse: null },
  ]);
  assert.equal(formatRows('json_lines', columns, rows.slice(0, 1), ctx), '{"embedding":[0.1,0.2,0.3],"id":1,"title":"a","sparse":"{1:0.5}/5"}');
  assert.match(formatRows('mongo_insert', columns, rows.slice(0, 1), ctx), /"embedding": \[\n\s+0\.1,\n\s+0\.2,\n\s+0\.3\n\s+\]/);
});

test('YAML copies have a vector as a flow sequence', () => {
  assert.equal(formatRows('yaml', columns, rows.slice(0, 1), ctx), '- embedding: [0.1, 0.2, 0.3]\n  id: 1\n  title: a\n  sparse: "{1:0.5}/5"');
});

test('a copied UPDATE never compares a vector column', () => {
  // Guessed key: "id", never the vector column that comes first.
  assert.equal(formatRows('sql_update', columns, rows.slice(0, 1), ctx),
    `UPDATE "public"."docs" SET "embedding" = '[0.1,0.2,0.3]', "title" = 'a', "sparse" = '{1:0.5}/5' WHERE "id" = 1;`);
  // No "id": the first column that isn't a vector.
  const noId = [columns[0], columns[2]];
  assert.equal(formatRows('sql_update', noId, [['[1,2,3]', 'x']], ctx), `UPDATE "public"."docs" SET "embedding" = '[1,2,3]' WHERE "title" = 'x';`);
  // Every column as the key (a table without a primary key): the vectors are left out.
  const all = { ...ctx, keyColumns: ['embedding', 'id', 'title', 'sparse'] };
  assert.equal(formatRows('sql_update', columns, rows.slice(0, 1), all),
    `UPDATE "public"."docs" SET "embedding" = '[0.1,0.2,0.3]', "sparse" = '{1:0.5}/5' WHERE "id" = 1 AND "title" = 'a';`);
});

test('a generic INSERT writes the vector text as a literal (the driver writes its own when it can)', () => {
  assert.equal(formatRows('sql_insert', columns, rows.slice(0, 1), ctx),
    `INSERT INTO "public"."docs" ("embedding", "id", "title", "sparse") VALUES ('[0.1,0.2,0.3]', 1, 'a', '{1:0.5}/5');`);
});
