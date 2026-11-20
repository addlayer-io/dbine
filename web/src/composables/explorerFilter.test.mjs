// Tests for the explorer's filter beyond the loaded folders
// (explorerFilter.ts): which folder a found object goes in, and the code
// search's hits by object. Run: `node --test src/composables/explorerFilter.test.mjs`
// from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';

const { inFolder, foundFor, codeObjects, objectId } = await import('./explorerFilter.ts');

const o = (kind, schema, name, parent = null) => ({ kind, schema, name, parent });

test('a found object goes in its kind folder, in its schema or across schemas', () => {
  const t = o('table', 'dbo', 'Clientes');
  // Grouped by schema: the dbo schema's Tables.
  assert.ok(inFolder(t, { schema: 'dbo', kind: 'table' }));
  assert.ok(!inFolder(t, { schema: 'ventas', kind: 'table' }));
  assert.ok(!inFolder(t, { schema: 'dbo', kind: 'view' }));
  // Flat: every schema's Tables.
  assert.ok(inFolder(t, { schema: null, kind: 'table' }));
  // "Otros": the kinds the driver doesn't declare.
  assert.ok(!inFolder(t, { schema: null, kind: null, other: ['table', 'view'] }));
  assert.ok(inFolder(o('synonym', 'dbo', 's'), { schema: null, kind: null, other: ['table', 'view'] }));
  // Objects without a schema on an engine with schemas.
  assert.ok(inFolder(o('table', null, 'x'), { schema: null, noSchema: true, kind: null }));
  assert.ok(!inFolder(t, { schema: null, noSchema: true, kind: null }));
});

test('a folder gets what it does not already show, each once', () => {
  const loaded = [o('table', 'dbo', 'Clientes')];
  const found = [o('table', 'dbo', 'Clientes'), o('table', 'dbo', 'clientes_hist'), o('view', 'dbo', 'v_clientes'), o('table', 'dbo', 'clientes_hist')];
  assert.deepEqual(foundFor(found, { schema: 'dbo', kind: 'table' }, loaded).map((x) => x.name), ['clientes_hist']);
  assert.deepEqual(foundFor(found, { schema: 'dbo', kind: 'view' }, []).map((x) => x.name), ['v_clientes']);
  // A folder not opened yet shows only what was found.
  assert.deepEqual(foundFor(found, { schema: 'dbo', kind: 'table' }, []).map((x) => x.name), ['Clientes', 'clientes_hist']);
});

test('the code hits become one object each, with its first matching line', () => {
  // Power BI: the table whose partition reads indicadores_directorio.medidas, twice.
  const hits = [
    { kind: 'table', schema: null, name: 'pbi medidas', line: 4, text: 'Source = Sql.Database(…, "indicadores_directorio")' },
    { kind: 'table', schema: null, name: 'pbi medidas', line: 9, text: 'medidas = Source{[Schema="indicadores_directorio"]}' },
    { kind: 'measure', schema: null, name: 'Total', line: 1, text: "SUM('pbi medidas'[valor])" },
    { kind: 'trigger', schema: 'dbo', name: 'tr_audit', parent: 'Clientes', line: 3, text: 'insert into audit' },
  ];
  const { objects, lines } = codeObjects(hits);
  assert.deepEqual(objects.map((x) => x.name), ['pbi medidas', 'Total', 'tr_audit']);
  assert.equal(objects[2].parent, 'Clientes');
  assert.deepEqual(lines.get(objectId(objects[0])), { line: 4, text: 'Source = Sql.Database(…, "indicadores_directorio")' });
  // They go in their folders like any found object.
  assert.deepEqual(foundFor(objects, { schema: null, kind: 'table' }, []).map((x) => x.name), ['pbi medidas']);
  assert.deepEqual(foundFor(objects, { schema: null, kind: 'measure' }, []).map((x) => x.name), ['Total']);
});
