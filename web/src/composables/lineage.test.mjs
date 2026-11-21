// Tests for the lineage diagram's graph (lineage.ts): what a view uses goes
// to its left, what uses it to its right, each card once, sources outside the
// database, the cap and the layout's direction. Run:
// `node --test src/composables/lineage.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';

const { newLineage, addUses, addUsedBy, nodeId, jobNodeId, sideCounts, layout, withoutRelations, MAX_NODES } = await import('./lineage.ts');

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

test('jobs that run a procedure go to its right, one card per job, with nothing past them', () => {
  const proc = { kind: 'procedure', schema: 'dbo', name: 'sp_cargar' };
  const g = newLineage(proc);
  const p = nodeId(proc);
  const job = (id, name, steps, confidence = 'probable') => ({ id, name, kind: 'SQL Server Agent', group: null, steps, confidence, mentions: [{ line: 1, text: 'EXEC dbo.sp_cargar', dynamic: false }] });
  addUsedBy(g, p, report([dep('view', 'dbo', 'v_kpis')], { jobs: [job('A1', 'Carga nocturna', ['Cargar ventas']), job('B2', 'Carga nocturna', ['Paso 1'], 'review')] }));
  // Two jobs with the same name are two cards.
  assert.equal(g.nodes.size, 4);
  const a = g.nodes.get(jobNodeId('A1'));
  assert.equal(a.kind, 'job');
  assert.equal(a.jobId, 'A1');
  assert.equal(a.jobKind, 'SQL Server Agent');
  assert.equal(a.down, 'done');
  assert.equal(a.up, 'none');
  const e = g.edges.find((x) => x.to === jobNodeId('A1'));
  assert.equal(e.from, p);
  assert.equal(e.detail, 'SQL Server Agent · Cargar ventas');
  assert.equal(e.mentions.length, 1);
  // The same job from another object: still one card, two edges.
  const view = nodeId({ kind: 'view', schema: 'dbo', name: 'v_kpis' });
  addUsedBy(g, view, report([], { jobs: [job('A1', 'Carga nocturna', ['Paso 2'])] }));
  assert.equal(g.nodes.size, 4);
  assert.equal(g.edges.filter((x) => x.to === jobNodeId('A1')).length, 2);
});

test('a job in the middle: what its steps use to its left, nothing to its right', () => {
  const g = newLineage({ kind: 'job', schema: null, name: 'Carga nocturna' }, 'A1');
  const id = jobNodeId('A1');
  assert.equal(g.nodes.get(id).root, true);
  assert.equal(g.nodes.get(id).down, 'done');
  addUses(g, id, report([dep('procedure', 'dbo', 'sp_cargar', 'code', 'confirmed')], { external: [{ database: 'stage', schema: 'dbo', name: 'tmp', detail: 'Archivar' }] }));
  assert.equal(g.nodes.size, 3);
  assert.deepEqual(sideCounts(g, id), { up: 2, down: 0 });
});

test('without relations the foreign keys go, and so do the cards only they reach', () => {
  const g = newLineage({ kind: 'table', schema: 'dbo', name: 'Pedidos' });
  const id = nodeId({ kind: 'table', schema: 'dbo', name: 'Pedidos' });
  addUses(g, id, report([dep('table', 'dbo', 'Clientes', 'foreign_key', 'confirmed'), dep('view', 'dbo', 'v_a', 'code')]));
  const lineage = withoutRelations(g);
  assert.deepEqual([...lineage.nodes.values()].map((n) => n.name).sort(), ['Pedidos', 'v_a']);
  assert.ok(lineage.edges.every((e) => e.relation !== 'foreign_key'));
  // The graph itself keeps them (the switch can bring them back).
  assert.equal(g.nodes.size, 3);
  assert.equal(g.edges.length, 2);
  // A graph with no foreign keys comes out the same.
  const plain = newLineage(root);
  assert.equal(withoutRelations(plain).nodes.size, 1);
});
