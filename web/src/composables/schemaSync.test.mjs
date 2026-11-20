// Tests for "Comparar esquemas"' per-kind sync check (schemaSync.ts). Run:
// `node --test src/composables/schemaSync.test.mjs` from web/ (Node 23.6+
// strips the TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { syncsKind, syncsKinds } from './schemaSync.ts';

// Redis, Valkey and Dragonfly: search indexes, not keys.
const redis = { supports_schema_sync: false, schema_sync_kinds: ['index'] };
// PostgreSQL: every kind, as its flag says (nothing per kind).
const pg = { supports_schema_sync: true };
// An engine that applies no schema changes; null as the backend sends it.
const none = { supports_schema_sync: false, schema_sync_kinds: null };

test('a driver whose kinds differ answers per kind', () => {
  assert.equal(syncsKind(redis, 'index'), true);
  assert.equal(syncsKind(redis, 'key'), false);
  assert.equal(syncsKinds(redis, ['index', 'index']), true);
  // One key among the changes blocks the whole side.
  assert.equal(syncsKinds(redis, ['index', 'key']), false);
  // Nothing per kind to go by: its flag.
  assert.equal(syncsKinds(redis, []), false);
});

test('a driver without kinds of its own answers by its flag', () => {
  for (const kind of ['table', 'view', 'collection']) {
    assert.equal(syncsKind(pg, kind), true, kind);
    assert.equal(syncsKind(none, kind), false, kind);
  }
  assert.equal(syncsKinds(pg, ['table']), true);
  assert.equal(syncsKinds(pg, []), true);
  assert.equal(syncsKinds(none, ['table']), false);
  assert.equal(syncsKinds(none, []), false);
  // An empty list is "no kind", not "use the flag".
  assert.equal(syncsKind({ supports_schema_sync: true, schema_sync_kinds: [] }, 'table'), false);
});
