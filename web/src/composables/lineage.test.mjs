// Tests for the lineage diagram's graph (lineage.ts): what a view uses goes
// to its left, what uses it to its right, each card once, sources outside the
// database, the cap and the layout's direction. Run:
// `node --test src/composables/lineage.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';

const { newLineage, addUses, addUsedBy, nodeId, sideCounts, layout, MAX_NODES } = await import('./lineage.ts');

const dep = (kind, schema, name, relation = 'code', confidence = 'probable', mentions = []) => ({ kind, schema, name, parent: null, relation, confidence, detail: null, mentions });
const report = (items, extra = {}) => ({ items, scanned: 1, unreadable: [], note: null, ...extra });
const root = { kind: 'view', schema: 'dbo', name: 'v_medidas' };

test('what the object uses goes left, what uses it goes right', () => {
  const g = newLineage(root);
  const r = nodeId(root);
  addUses(g, r, report([dep('table', 'indicadores', 'medidas', 'code', 'confirmed'), dep('function', 'dbo', 'fn_total')]));
  addUsedBy(g, r, report([dep('procedure', 'dbo', 'sp_reporte'), dep('table', 'dbo', 'x', 'index')]));
  assert.equal(g.nodes.size, 4);
  assert.deepEqual(g.edges.map((e) => `${e.from.split('\u0000')[2]} → ${e.to.split('\u0000')[2]}`), [
    'medidas → v_medidas', 'fn_total → v_medidas', 'v_medidas → sp_reporte',
  ]);
  assert.deepEqual(sideCounts(g, r), { up: 2, down: 1 });
  assert.equal(g.nodes.get(r).up, 'done');
  assert.equal(g.nodes.get(r).down, 'done');
  // A card is added once, even when two levels reach it.
  addUses(g, nodeId({ kind: 'procedure', schema: 'dbo', name: 'sp_reporte' }), report([dep('table', 'indicadores', 'medidas')]));
  assert.equal(g.nodes.size, 4);
  assert.equal(g.edges.length, 4);
});

test('sources outside the database are cards of their own', () => {
  const g = newLineage({ kind: 'table', schema: null, name: 'pbi medidas' });
  addUses(g, nodeId({ kind: 'table', schema: null, name: 'pbi medidas' }), report([], {
    external: [{ system: 'SQL Server', server: 'srv01', database: 'indicadores_directorio', schema: 'dbo', name: 'medidas', detail: 'Partición pbi medidas' }],
    unreadable: ['dbo.x'], note: 'sin permiso',
  }));
  const ext = [...g.nodes.values()].find((n) => n.external);
  assert.equal(ext.name, 'medidas');
  assert.equal(g.edges[0].relation, 'external');
  assert.deepEqual(g.unreadable, ['dbo.x']);
  assert.deepEqual(g.notes, ['sin permiso']);
});

test('past the cap a level adds no more cards and says so', () => {
  const g = newLineage(root);
  const many = Array.from({ length: MAX_NODES + 10 }, (_, i) => dep('table', 'dbo', `t${i}`));
  addUses(g, nodeId(root), report(many));
  assert.equal(g.nodes.size, MAX_NODES);
  assert.ok(g.capped);
});

test('the layout puts what is used before what uses it', () => {
  const g = newLineage(root);
  const r = nodeId(root);
  addUses(g, r, report([dep('table', 'dbo', 'a')]));
  addUsedBy(g, r, report([dep('procedure', 'dbo', 'p')]));
  const { pos, paths, bounds } = layout(g, 200, 44);
  const x = (name) => [...pos.entries()].find(([id]) => id.endsWith(`\u0000${name}`))[1].x;
  assert.ok(x('a') < x('v_medidas') && x('v_medidas') < x('p'));
  assert.equal(paths.length, 2);
  assert.ok(paths[0].d.startsWith('M '));
  assert.ok(bounds.width > 400);
});
