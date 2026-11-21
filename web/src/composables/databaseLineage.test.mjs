// Tests for the database lineage's logic (databaseLineage.ts): the model from the
// backend's graph, the filters, the overview by schema and the layout with the
// isolated objects at the side. Run: `node --test src/composables/databaseLineage.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';

// The module imports its sibling without an extension (as Vite does): add it for Node.
register('data:text/javascript,' + encodeURIComponent(
  "export async function resolve(s, c, n) { return n(/^\\.\\.?\\//.test(s) && !/\\.[a-z]+$/.test(s) ? s + '.ts' : s, c); }",
));

const m = await import('./databaseLineage.ts');
const { nodeId, jobNodeId } = await import('./lineage.ts');

const o = (kind, schema, name) => ({ kind, schema, name });
const edge = (from, to, confidence = 'confirmed', relation = 'code', mentions = []) => ({ from, to, relation, confidence, detail: null, mentions });
const graph = (extra = {}) => ({
  objects: [o('table', 'dbo', 'Clientes'), o('table', 'dbo', 'Pedidos'), o('view', 'rep', 'v_ventas'), o('table', 'etl', 'huerfana')],
  edges: [
    edge(o('table', 'dbo', 'Clientes'), o('table', 'dbo', 'Pedidos'), 'confirmed', 'foreign_key'),
    edge(o('table', 'dbo', 'Pedidos'), o('view', 'rep', 'v_ventas'), 'probable'),
    edge(o('table', 'dbo', 'Clientes'), o('view', 'rep', 'v_ventas'), 'confirmed'),
  ],
  external: [{ source: { system: 'SQL Server', server: 'dw', name: 'origen' }, to: o('table', 'dbo', 'Clientes') }],
  jobs: [{ id: 'J1', name: 'Carga', kind: 'SQL Server Agent', group: null, uses: [{ ...o('view', 'rep', 'v_ventas'), parent: null, relation: 'code', confidence: 'probable', detail: null, mentions: [{ line: 1, text: 'EXEC x', dynamic: false }] }], external: [] }],
  scanned: 4, unreadable: [], truncated: false, notes: ['uno'], job_notes: ['dos', 'uno'], ...extra,
});

test('the model has each object once, the outside source, the job and every line', () => {
  const model = m.buildModel(graph());
  // 4 objects + 1 external + 1 job
  assert.equal(model.nodes.size, 6);
  assert.equal(model.edges.length, 3 + 1 + 1);
  assert.deepEqual(model.notes, ['uno', 'dos']);
  const job = model.nodes.get(jobNodeId('J1'));
  assert.equal(job.kind, 'job');
  assert.equal(job.jobKind, 'SQL Server Agent');
  assert.ok(model.edges.some((e) => e.to === jobNodeId('J1') && e.from === nodeId(o('view', 'rep', 'v_ventas'))));
  assert.ok(model.edges.some((e) => e.relation === 'external'));
  assert.equal(m.objectCount(model), 4);
});

test('a repeated line keeps the surest confidence; an edge to an unlisted object adds its card', () => {
  const a = o('table', 'dbo', 'Clientes'), b = o('table', 'dbo', 'Pedidos');
  const model = m.buildModel(graph({ objects: [a], edges: [edge(a, b, 'probable'), edge(a, b, 'confirmed')], external: [], jobs: [] }));
  assert.equal(model.edges.length, 1);
  assert.equal(model.edges[0].confidence, 'confirmed');
  assert.equal(model.nodes.size, 2);
});

test('filters: kind, text with neighbours, only confirmed, only connected', () => {
  const model = m.buildModel(graph());
  const none = m.noFilters();
  assert.equal(m.applyFilters(model, none).nodes.length, 6);
  assert.deepEqual(m.kindsOf(model.nodes.values()), ['external', 'job', 'table', 'view']);

  const views = m.applyFilters(model, { ...none, kinds: ['view'] });
  assert.deepEqual(views.nodes.map((n) => n.name), ['v_ventas']);
  assert.equal(views.edges.length, 0);

  const text = m.applyFilters(model, { ...none, text: 'PEDIDOS' });
  assert.deepEqual(text.nodes.map((n) => n.name).sort(), ['Clientes', 'Pedidos', 'v_ventas']);
  assert.equal(text.edges.length, 3);

  const sure = m.applyFilters(model, { ...none, onlyConfirmed: true });
  assert.ok(sure.edges.every((e) => e.confidence === 'confirmed'));
  assert.equal(sure.edges.length, 3);

  const linked = m.applyFilters(model, { ...none, onlyConnected: true });
  assert.ok(!linked.nodes.some((n) => n.name === 'huerfana'));
  assert.equal(linked.nodes.length, 5);
});

test('overview: a collapsed schema is one card, the lines crossing add up', () => {
  const model = m.buildModel(graph());
  const { nodes, edges } = m.applyFilters(model, m.noFilters());
  // Everything collapsed.
  const v = m.buildView(nodes, edges, new Set());
  const cards = [...v.lineage.nodes.values()];
  const schemas = cards.filter((n) => n.kind === 'schema');
  assert.deepEqual(schemas.map((n) => `${n.name}:${n.members}`).sort(), ['dbo:2', 'etl:1', 'rep:1']);
  // The job and the outside source stay cards of their own.
  assert.equal(cards.filter((n) => n.kind === 'job').length, 1);
  assert.equal(cards.filter((n) => n.kind === 'external').length, 1);
  // dbo -> rep: Pedidos->v_ventas and Clientes->v_ventas = 2 lines in one.
  const cross = v.lineage.edges.find((e) => e.from === m.schemaId('dbo') && e.to === m.schemaId('rep'));
  assert.equal(cross.weight, 2);
  assert.equal(cross.confidence, 'confirmed');
  assert.ok(m.isAggregate(cross));
  // Lines inside a schema vanish with its card.
  assert.ok(!v.lineage.edges.some((e) => e.from === e.to));

  // Expanding rep shows its object, linked to the dbo card.
  const x = m.buildView(nodes, edges, new Set(['rep']));
  const vv = nodeId(o('view', 'rep', 'v_ventas'));
  assert.ok(x.lineage.nodes.has(vv));
  const one = x.lineage.edges.find((e) => e.from === m.schemaId('dbo') && e.to === vv);
  assert.equal(one.weight, 2);
  const job = x.lineage.edges.find((e) => e.from === vv && e.to === jobNodeId('J1'));
  assert.equal(job.weight, 1);
  assert.equal(job.mentions.length, 1);
});

test('the card cap leaves out the isolated objects first and says how many', () => {
  const model = m.buildModel(graph());
  const { nodes, edges } = m.applyFilters(model, m.noFilters());
  const v = m.buildView(nodes, edges, new Set(m.schemasOf(nodes)), 5);
  assert.equal(v.hidden, 1);
  assert.equal(v.lineage.capped, true);
  assert.ok(![...v.lineage.nodes.values()].some((n) => n.name === 'huerfana'));
});

test('schemas and the engines that load a catalog query at once', () => {
  const model = m.buildModel(graph());
  assert.deepEqual(m.schemasOf(model.nodes.values()), ['dbo', 'etl', 'rep']);
  assert.ok(m.hasCatalogLineage('postgres'));
  assert.ok(m.hasCatalogLineage('powerbi'));
  assert.ok(!m.hasCatalogLineage('sqlite'));
  assert.ok(!m.hasCatalogLineage(undefined));
});

test('layout: used to the left of its users, isolated objects in columns at the left', () => {
  const model = m.buildModel(graph());
  const { nodes, edges } = m.applyFilters(model, m.noFilters());
  const v = m.buildView(nodes, edges, new Set(m.schemasOf(nodes)));
  const p = m.layoutDb(v.lineage, 214, 46);
  assert.equal(p.pos.size, v.lineage.nodes.size);
  const at = (k, name) => p.pos.get(nodeId(o(k, name === 'v_ventas' ? 'rep' : 'dbo', name)));
  assert.ok(at('table', 'Clientes').x < at('table', 'Pedidos').x);
  assert.ok(at('table', 'Pedidos').x < at('view', 'v_ventas').x);
  const alone = p.pos.get(nodeId(o('table', 'etl', 'huerfana')));
  assert.ok(alone.x < at('table', 'Clientes').x);
  // No two cards overlap.
  const cards = [...p.pos.values()];
  for (let i = 0; i < cards.length; i++) {
    for (let j = i + 1; j < cards.length; j++) {
      const a = cards[i], b = cards[j];
      assert.ok(Math.abs(a.x - b.x) >= 214 || Math.abs(a.y - b.y) >= 46, 'cards overlap');
    }
  }
  assert.ok(p.paths.length === v.lineage.edges.length);
});

test('layout of a lone graph with no lines still has bounds', () => {
  const model = m.buildModel(graph({ edges: [], external: [], jobs: [] }));
  const v = m.buildView([...model.nodes.values()], [], new Set(m.schemasOf(model.nodes.values())));
  const p = m.layoutDb(v.lineage, 214, 46);
  assert.equal(p.pos.size, 4);
  assert.ok(p.bounds.width >= 214);
});
