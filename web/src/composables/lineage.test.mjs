// Tests for the lineage diagram's graph (lineage.ts): what a view uses goes
// to its left, what uses it to its right, each card once, sources outside the
// database, the cap and the layout's direction. Run:
// `node --test src/composables/lineage.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';

const { newLineage, addUses, addUsedBy, nodeId, jobNodeId, sideCounts, layout, withoutRelations, withoutCross, setCrossUsers, replaceExternal, connNodeId, MAX_NODES } = await import('./lineage.ts');

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

// -- cross-connection lineage -----------------------------------------------------------------------

const pbi = { id: 'pbi1', name: 'Power BI', database: 'Ventas', driver: 'powerbi' };
const user = (name, database = 'Ventas') => ({ connection_id: 'pbi1', connection_name: 'Power BI', database, object: { kind: 'table', schema: null, name }, detail: 'Partición' });

test('what other connections read goes to the right, with its connection, and the switch hides it', () => {
  const g = newLineage(root);
  const r = nodeId(root);
  addUses(g, r, report([dep('table', 'dbo', 'Pedidos')]));
  setCrossUsers(g, r, [user('Pedidos pbi'), user('Pedidos pbi', 'Otro')]);
  const cross = [...g.nodes.values()].filter((n) => n.cross);
  assert.equal(cross.length, 2, 'the same table name in two models is two cards');
  assert.ok(cross.every((n) => n.conn.id === 'pbi1' && n.up === 'done' && n.down === 'none'));
  assert.equal(g.edges.filter((e) => e.from === r && e.relation === 'external').length, 2);
  // What a user's own right side loads belongs to ITS connection and is hidden with it.
  const u = cross[0];
  addUsedBy(g, u.id, report([dep('model', null, 'Ventas')], { external_users: [{ system: 'Power BI report', name: 'Tablero', detail: 'https://app.powerbi.com/r/1' }] }), u.conn);
  const model = g.nodes.get(connNodeId(u.conn, { kind: 'model', schema: null, name: 'Ventas' }));
  assert.equal(model.conn.id, 'pbi1');
  const rep = [...g.nodes.values()].find((n) => n.external?.system === 'Power BI report');
  assert.ok(rep && rep.conn.id === 'pbi1');
  assert.equal(g.edges.find((e) => e.to === rep.id).from, u.id);
  const plain = withoutCross(g);
  assert.deepEqual([...plain.nodes.values()].map((n) => n.name).sort(), ['Pedidos', 'v_medidas']);
  assert.equal(plain.edges.length, 1);
  // A refresh drops the users the new answer lacks.
  setCrossUsers(g, r, [user('Pedidos pbi')]);
  assert.equal([...g.nodes.values()].filter((n) => n.cross).length, 1);
  assert.ok(!g.nodes.has(rep.id) || g.nodes.get(u.id));
});

test('the same external twice in two connections stays two cards', () => {
  const g = newLineage(root);
  const x = { system: 'Power BI report', name: 'R' };
  addUsedBy(g, nodeId(root), report([], { external_users: [x] }));
  addUsedBy(g, nodeId(root), report([], { external_users: [x] }), pbi);
  assert.equal([...g.nodes.values()].filter((n) => n.external).length, 2);
});

test('an external card becomes the real object of the linked connection', () => {
  const g = newLineage({ kind: 'table', schema: null, name: 'Pedidos pbi' });
  const r = nodeId({ kind: 'table', schema: null, name: 'Pedidos pbi' });
  addUses(g, r, report([], { external: [{ system: 'SQL Server', server: 'srv01', database: 'ventas', schema: 'dbo', name: 'Pedidos' }] }));
  const ext = [...g.nodes.values()].find((n) => n.external);
  const sql = { id: 'sql1', name: 'Ventas SQL', database: '', driver: 'sqlserver' };
  const id = replaceExternal(g, ext.id, { database: 'ventas', object: { kind: 'table', schema: 'dbo', name: 'Pedidos' } }, sql);
  assert.ok(!g.nodes.has(ext.id));
  const real = g.nodes.get(id);
  assert.deepEqual([real.conn.id, real.conn.database, real.name, real.up, real.down, !!real.external], ['sql1', 'ventas', 'Pedidos', 'none', 'none', false]);
  assert.deepEqual(g.edges.map((e) => [e.from, e.to]), [[id, r]]);
  // Its own uses are read in its connection and stay there.
  addUses(g, id, report([dep('table', 'dbo', 'Clientes')]), real.conn);
  const c = [...g.nodes.values()].find((n) => n.name === 'Clientes');
  assert.equal(c.conn.id, 'sql1');
  assert.equal(c.id, connNodeId(real.conn, { kind: 'table', schema: 'dbo', name: 'Clientes' }));
  // Following it twice (already there) adds no second card or line.
  const ext2 = { id: 'x', kind: 'external', schema: null, name: 'again', up: 'done', down: 'done' };
  g.nodes.set('x', ext2);
  g.edges.push({ from: 'x', to: r, relation: 'external', confidence: 'confirmed', mentions: [] });
  replaceExternal(g, 'x', { database: 'ventas', object: { kind: 'table', schema: 'dbo', name: 'Pedidos' } }, sql);
  assert.equal(g.edges.filter((e) => e.to === r).length, 1);
});
