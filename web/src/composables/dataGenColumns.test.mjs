// Tests for "Generar datos de prueba…"'s columns (dataGenColumns.ts): the
// vector generator's settings, what the backend gets (a vector it doesn't read
// as one, such as a Redis search index's field, gets the vector generator with
// the type's dimensions) and how the sample shows vectors. Run: `node --test
// src/composables/dataGenColumns.test.mjs` from web/ (Node 23.6+ strips the
// TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';

// dataGenColumns.ts imports its siblings without an extension (as Vite
// does): resolve them to the .ts files.
register('data:text/javascript,' + encodeURIComponent(`
export async function resolve(spec, ctx, next) {
  if (spec.startsWith('.') && !/\\.[cm]?[jt]s$/.test(spec)) return next(spec + '.ts', ctx);
  return next(spec, ctx);
}`));
const { autoOf, generatorOptions, paramsOf, sampleVectorOf, sentSpecs, unclassifiedVector } = await import('./dataGenColumns.ts');

const col = (name, data_type, auto, vector) => ({ name, data_type, auto, nullable: true, primary_key: false, auto_increment: false, foreign_key: null, ...(vector === undefined ? {} : { vector }) });
const auto = (extra = {}) => ({ generator: 'auto', params: {}, null_percent: 0, ...extra });
const specsOf = (cols, over = {}) => Object.fromEntries(cols.map((c) => [c.name, over[c.name] ?? auto()]));

// pgvector: the backend reads `vector(1536)` and picks the vector generator.
const PG = [col('id', 'int8', 'sequence'), col('titulo', 'text', 'text'), col('embedding', 'vector(1536)', 'vector')];
// A Redis search index as an older backend described it: its catalog didn't
// read `VECTOR(3, FLOAT32)` (its "auto" picked decimals on 'float') and the
// columns came without their vector.
const REDIS = [col('id', 'key', 'text'), col('title', 'TEXT', 'text'), col('embedding', 'VECTOR(3, FLOAT32)', 'decimal')];

test("a vector the backend doesn't read as one is filled with vectors of its type's dimensions", () => {
  assert.deepEqual(unclassifiedVector(REDIS[2]), { dims: 3, element: 'f32' });
  assert.equal(autoOf(REDIS[2]), 'vector');
  assert.deepEqual(sentSpecs(REDIS, specsOf(REDIS)), [{ name: 'embedding', generator: 'vector', params: { dims: '3' }, null_percent: 0 }]);
  // Chosen by hand, the same; its share of NULLs is kept.
  assert.deepEqual(sentSpecs(REDIS, specsOf(REDIS, { embedding: auto({ generator: 'vector', null_percent: 10 }) })), [
    { name: 'embedding', generator: 'vector', params: { dims: '3' }, null_percent: 10 },
  ]);
  // Another generator chosen by hand is the user's call.
  assert.deepEqual(sentSpecs(REDIS, specsOf(REDIS, { embedding: auto({ generator: 'null' }) })), [
    { name: 'embedding', generator: 'null', params: {}, null_percent: 0 },
  ]);
  // The vector generator is offered even when the backend's list lacks it.
  assert.deepEqual(generatorOptions(['text', 'decimal'], REDIS), ['auto', 'text', 'decimal', 'vector']);
});

test('columns the backend reads as vectors, and the rest, go as before', () => {
  assert.equal(unclassifiedVector(PG[2]), null);
  assert.equal(autoOf(PG[2]), 'vector');
  assert.equal(autoOf(PG[1]), 'text');
  // Left to "auto": nothing to send.
  assert.deepEqual(sentSpecs(PG, specsOf(PG)), []);
  // Chosen by hand: the type's dimensions go with it.
  assert.deepEqual(sentSpecs(PG, specsOf(PG, { embedding: auto({ generator: 'vector' }), titulo: auto({ generator: 'text', params: { min: '2' } }) })), [
    { name: 'titulo', generator: 'text', params: { min: '2' }, null_percent: 0 },
    { name: 'embedding', generator: 'vector', params: { dims: '1536' }, null_percent: 0 },
  ]);
  // "auto" with NULLs is sent as "auto".
  assert.deepEqual(sentSpecs(PG, specsOf(PG, { titulo: auto({ null_percent: 5 }) })), [{ name: 'titulo', generator: 'auto', params: {}, null_percent: 5 }]);
  assert.deepEqual(generatorOptions(['text', 'vector'], PG), ['auto', 'text', 'vector']);
  assert.deepEqual(generatorOptions(['text'], [PG[0], PG[1]]), ['auto', 'text']);
});

test('only dense vectors of numbers are taken from the type', () => {
  // Sparse vectors are written in their engine's text: the backend's call.
  assert.equal(unclassifiedVector(col('s', 'sparsevec(5)', 'skip')), null);
  // Not vectors: a fixed-length list of text, an array, plain types.
  for (const ty of ['vector<text, 2>', 'Array(Float32)', 'float', 'text', 'vectors', '']) {
    assert.equal(unclassifiedVector(col('x', ty, 'decimal')), null, ty);
  }
});

test('the dimensions setting shows only where the type does not fix them', () => {
  const vec = auto({ generator: 'vector' });
  assert.deepEqual(paramsOf(col('e', 'vector(1536)', 'vector'), vec), []);
  assert.deepEqual(paramsOf(REDIS[2], vec), []);
  assert.deepEqual(paramsOf(col('e', 'FLOAT[3]', 'decimal'), vec), []);
  // A document's array, an array without a length: the user says.
  assert.deepEqual(paramsOf(col('e', 'array', 'vector'), vec), ['dims']);
  assert.deepEqual(paramsOf(col('e', 'Array(Float32)', 'decimal'), vec), ['dims']);
  // Typed dims the user left: kept where the type has none.
  assert.deepEqual(sentSpecs([col('e', 'array', 'vector')], { e: auto({ generator: 'vector', params: { dims: '16' } }) }), [
    { name: 'e', generator: 'vector', params: { dims: '16' }, null_percent: 0 },
  ]);
  // Other generators keep their settings; "auto" has none.
  assert.deepEqual(paramsOf(col('n', 'int', 'integer'), auto({ generator: 'integer' })), ['min', 'max']);
  assert.deepEqual(paramsOf(col('n', 'int', 'integer'), auto()), []);
  assert.deepEqual(paramsOf(col('n', 'int', 'integer'), undefined), []);
});

test('the sample shows vectors by the type, else where the backend fills them with vectors', () => {
  // Typed: the badge says the type's dims and element.
  assert.deepEqual(sampleVectorOf(PG[2], auto()), { meta: { dims: 1536, element: 'f32' }, shown: { dims: 1536, element: 'f32' } });
  assert.deepEqual(sampleVectorOf(REDIS[2], auto()), { meta: { dims: 3, element: 'f32' }, shown: { dims: 3, element: 'f32' } });
  // A field whose type doesn't say it: vectors when the backend's "auto" or
  // the chosen generator is the vector one, with no element claimed.
  const doc = col('resumen', 'array', 'vector');
  assert.deepEqual(sampleVectorOf(doc, auto()), { meta: { element: 'f32' }, shown: null });
  assert.deepEqual(sampleVectorOf(col('r', 'array', 'fixed'), auto({ generator: 'vector' })), { meta: { element: 'f32' }, shown: null });
  // Otherwise text.
  assert.deepEqual(sampleVectorOf(col('r', 'array', 'fixed'), auto()), { meta: null, shown: null });
  assert.deepEqual(sampleVectorOf(PG[1], auto()), { meta: null, shown: null });
  assert.deepEqual(sampleVectorOf(undefined, undefined), { meta: null, shown: null });
});

// The backend says each vector column's vector (`vector`): the dialog goes
// by it before the type's name.
test("the backend's vector comes first", () => {
  // A Redis search index as the backend describes it now: its INT8 field
  // is a vector of 3 bytes, filled by the backend's "auto".
  const i8 = { dims: 3, element: 'i8' };
  const NEW = [col('id', 'key', 'text'), col('title', 'TEXT', 'text'), col('emb', 'VECTOR(3, INT8)', 'vector', i8)];
  assert.equal(unclassifiedVector(NEW[2]), null);
  assert.equal(autoOf(NEW[2]), 'vector');
  // Left to "auto": nothing to send; chosen by hand, its dims go along.
  assert.deepEqual(sentSpecs(NEW, specsOf(NEW)), []);
  assert.deepEqual(sentSpecs(NEW, specsOf(NEW, { emb: auto({ generator: 'vector' }) })), [
    { name: 'emb', generator: 'vector', params: { dims: '3' }, null_percent: 0 },
  ]);
  assert.deepEqual(paramsOf(NEW[2], auto({ generator: 'vector' })), []);
  // The sample shows its bytes as a vector of its element.
  assert.deepEqual(sampleVectorOf(NEW[2], auto()), { meta: i8, shown: i8 });
  assert.deepEqual(generatorOptions(['text', 'vector'], NEW), ['auto', 'text', 'vector']);

  // A type whose name the dialog doesn't read: the backend's dims fix it.
  const doc = col('resumen', 'array', 'vector', { dims: 8, element: 'f32' });
  assert.deepEqual(paramsOf(doc, auto({ generator: 'vector' })), []);
  assert.deepEqual(sentSpecs([doc], { resumen: auto({ generator: 'vector' }) }), [
    { name: 'resumen', generator: 'vector', params: { dims: '8' }, null_percent: 0 },
  ]);
  assert.deepEqual(sampleVectorOf(doc, auto()), { meta: { dims: 8, element: 'f32' }, shown: { dims: 8, element: 'f32' } });

  // A vector the backend reads, though its "auto" isn't the vector one
  // (a foreign key's values, an identity): the backend's call.
  const fk = col('ref', 'vector(3)', 'fk', { dims: 3, element: 'f32' });
  assert.equal(unclassifiedVector(fk), null);
  assert.equal(autoOf(fk), 'fk');
  assert.deepEqual(sentSpecs([fk], specsOf([fk])), []);
  assert.deepEqual(generatorOptions(['text'], [fk]), ['auto', 'text']);

  // A sparse vector: shown in its engine's text, never sent as dense.
  const sp = { dims: 5, element: 'f32', sparse: true };
  const sparse = col('terms', 'sparsevec(5)', 'vector', sp);
  assert.equal(unclassifiedVector(sparse), null);
  assert.deepEqual(sampleVectorOf(sparse, auto()), { meta: sp, shown: sp });

  // A null vector (as JSON may say "none") falls back to the type.
  const old = col('emb', 'VECTOR(3, FLOAT32)', 'decimal', null);
  assert.deepEqual(unclassifiedVector(old), { dims: 3, element: 'f32' });
  assert.equal(autoOf(old), 'vector');
});
