// Tests for a key database's other kinds (keyKinds.ts): which objects of a
// schema read the explorer lists next to the key search (Redis' search
// indexes). Run: `node --test src/composables/keyKinds.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { hasOtherKinds, otherKindObjects } from './keyKinds.ts';

const kind = (id, label) => ({ id, label, has_columns: true, browsable: true, has_definition: true });
const REDIS = [kind('key', 'Claves'), kind('index', 'Índices de búsqueda')];
const ETCD = [kind('key', 'Claves')];
const table = (k, name, schema = null) => ({ kind: k, schema, name, columns: [], primary_key: null, foreign_keys: [], indexes: [] });

test('only drivers with kinds besides their keys list others', () => {
  assert.equal(hasOtherKinds(REDIS), true);
  assert.equal(hasOtherKinds(ETCD), false);
  assert.equal(hasOtherKinds([]), false);
  assert.equal(hasOtherKinds(undefined), false);
});

test("a key database lists its other declared kinds from the schema read, by name", () => {
  const tables = [table('index', 'idx:docs'), table('index', 'Idx:Autos'), table('index', 'idx:10'), table('index', 'idx:9')];
  assert.deepEqual(otherKindObjects(REDIS, tables).map((o) => o.name), ['idx:9', 'idx:10', 'Idx:Autos', 'idx:docs']);
  assert.deepEqual(otherKindObjects(REDIS, [table('index', 'docs')]), [{ kind: 'index', schema: null, name: 'docs', parent: null }]);
});

test('never the keys, a kind the driver does not declare, or the same object twice', () => {
  const tables = [table('key', 'user:1'), table('table', 'x'), table('', 'no-kind'), table('index', 'docs'), table('index', 'docs')];
  assert.deepEqual(otherKindObjects(REDIS, tables).map((o) => `${o.kind}:${o.name}`), ['index:docs']);
  // A driver with keys only: nothing besides them.
  assert.deepEqual(otherKindObjects(ETCD, [table('index', 'docs')]), []);
});
