// Tests for the database lineage's logic (databaseLineage.ts): the model from the
// backend's graph, the filters, the overview by schema and the layout with the
// isolated objects at the side. Run: `node --test src/composables/databaseLineage.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';
import { readFileSync } from 'node:fs';

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
  const none = { ...m.noFilters(), relations: true };
  assert.equal(m.applyFilters(model, none).nodes.length, 6);
  assert.deepEqual(m.typesOf(model.nodes.values()), [
    { type: 'table', count: 3 }, { type: 'view', count: 1 }, { type: 'job', count: 1 }, { type: 'external', count: 1 },
  ]);

  const views = m.applyFilters(model, { ...none, hidden: ['table', 'job', 'external'] });
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
  const { nodes, edges } = m.applyFilters(model, { ...m.noFilters(), relations: true });
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

test('relations (foreign keys) start hidden, so the lineage shows alone, and come back with the switch', () => {
  const model = m.buildModel(graph());
  const fk = (e) => e.relation === 'foreign_key';
  assert.equal(m.noFilters().relations, false);
  // Hidden: only the lineage lines, whatever the other filters say.
  const lineage = m.applyFilters(model, m.noFilters());
  assert.ok(lineage.edges.length > 0 && !lineage.edges.some(fk));
  // On: the foreign key is there along with them, and nothing else changes.
  const both = m.applyFilters(model, { ...m.noFilters(), relations: true });
  assert.equal(both.edges.filter(fk).length, 1);
  assert.equal(both.edges.length, lineage.edges.length + 1);
  // A table that only a foreign key linked is a card with no line now.
  const only = m.applyFilters(m.buildModel(graph({ edges: [edge(o('table', 'dbo', 'Clientes'), o('table', 'dbo', 'Pedidos'), 'confirmed', 'foreign_key')] })), { ...m.noFilters(), onlyConnected: true });
  assert.ok(!only.nodes.some((n) => n.name === 'Pedidos'));
  assert.ok(m.isRelation({ relation: 'foreign_key' }) && !m.isRelation({ relation: 'code' }));
});

test('what outside reads the database (reports, dashboards) is a grey card to the right of the model', () => {
  const model = m.buildModel(graph({ consumers: [{ consumer: { system: 'Power BI report', name: 'Tablero', detail: 'https://app.powerbi.com/r/1' }, from: o('view', 'rep', 'v_ventas') }] }));
  const rep = [...model.nodes.values()].find((n) => n.external?.system === 'Power BI report');
  assert.ok(rep);
  assert.ok(model.edges.some((e) => e.from === nodeId(o('view', 'rep', 'v_ventas')) && e.to === rep.id && e.relation === 'external'));
});

// -- the filter by type (a checkbox per type in the toolbar) -------------------------------------

const outside = (system, name = 'x') => ({ id: `ext:${system}:${name}`, kind: 'external', schema: null, name, external: { system, name }, up: 'done', down: 'done' });

test('each card has a type: its kind, or what the source outside is', () => {
  const card = (kind) => ({ id: kind, kind, schema: 'dbo', name: kind, up: 'done', down: 'done' });
  assert.equal(m.typeOf(card('measure')), 'measure');
  assert.equal(m.typeOf(card('procedure')), 'procedure');
  assert.equal(m.typeOf({ ...card('job'), jobId: 'J', jobKind: 'SQL Server Agent' }), 'job');
  // Databricks: what ran, with the server's jobs.
  for (const [system, type] of [
    ['Databricks job', 'job'], ['Databricks notebook', 'notebook'], ['Databricks pipeline', 'pipeline'],
    ['Databricks query', 'query'], ['Databricks dashboard', 'dashboard'], ['databricks NOTEBOOK', 'notebook'],
  ]) assert.equal(m.typeOf(outside(system)), type, system);
  // Power BI's readers.
  assert.equal(m.typeOf(outside('Power BI report')), 'report');
  assert.equal(m.typeOf(outside('Power BI dashboard')), 'dashboard');
  // Files, wherever they live.
  for (const system of ['Amazon S3', 'Azure Data Lake Storage', 'Google Cloud Storage', 'Unity Catalog volume', 'DBFS', 'File', 'Excel', 'CSV', 'Folder', 'SharePoint']) {
    assert.equal(m.typeOf(outside(system)), 'file', system);
  }
  // Another database or system.
  for (const system of ['SQL Server', 'Databricks', 'Snowflake', 'postgres_fdw', '']) assert.equal(m.typeOf(outside(system)), 'external', system);
  // The card itself stays an outside source with its system (the detail panel reads it).
  const n = outside('Databricks notebook');
  m.typeOf(n);
  assert.equal(n.kind, 'external');
  assert.equal(n.external.system, 'Databricks notebook');
});

test('the types are counted, the database\'s own first, then jobs and what is outside', () => {
  const nodes = [
    { id: 'm1', kind: 'measure', schema: null, name: 'a' }, { id: 'm2', kind: 'measure', schema: null, name: 'b' },
    { id: 't1', kind: 'table', schema: null, name: 'c' },
    outside('Databricks notebook', 'n1'), outside('Databricks notebook', 'n2'), outside('Databricks job', 'j1'),
    { id: 'J', kind: 'job', schema: null, name: 'Carga', jobId: 'J' },
    outside('Power BI report', 'r'), outside('Amazon S3', 's3'), outside('SQL Server', 'dw'),
  ];
  assert.deepEqual(m.typesOf(nodes), [
    { type: 'measure', count: 2 }, { type: 'table', count: 1 },
    { type: 'job', count: 2 }, { type: 'notebook', count: 2 }, { type: 'report', count: 1 },
    { type: 'file', count: 1 }, { type: 'external', count: 1 },
  ]);
});

test('unchecking a type hides its cards and their lines; the rest stays', () => {
  const nb = { system: 'Databricks notebook', name: 'carga_ventas' };
  const job = { system: 'Databricks job', name: 'nightly' };
  const model = m.buildModel(graph({
    external: [
      { source: { system: 'SQL Server', server: 'dw', name: 'origen' }, to: o('table', 'dbo', 'Clientes') },
      { source: nb, to: o('table', 'dbo', 'Pedidos') },
      { source: job, to: o('table', 'dbo', 'Pedidos') },
    ],
  }));
  const all = { ...m.noFilters(), relations: true };
  const every = m.typesOf(model.nodes.values()).map((t) => t.type);
  assert.deepEqual(every, ['table', 'view', 'job', 'notebook', 'external']);
  // Everything but the notebooks.
  const noNotebooks = m.applyFilters(model, { ...all, hidden: ['notebook'] });
  assert.ok(!noNotebooks.nodes.some((n) => n.name === 'carga_ventas'));
  const notebook = [...model.nodes.values()].find((n) => n.name === 'carga_ventas');
  assert.ok(model.edges.some((e) => e.from === notebook.id), 'the notebook had a line');
  assert.ok(!noNotebooks.edges.some((e) => e.from === notebook.id || e.to === notebook.id), 'its lines go with it');
  assert.equal(noNotebooks.nodes.length, model.nodes.size - 1);
  // Only the jobs: the server's job and Databricks' job together.
  const jobs = m.applyFilters(model, { ...all, hidden: every.filter((t) => t !== 'job') });
  assert.deepEqual(jobs.nodes.map((n) => n.name).sort(), ['Carga', 'nightly']);
  // Measures and tables of a Power BI model: the lines among them stay.
  const tables = m.applyFilters(model, { ...all, hidden: ['job', 'notebook', 'external'] });
  assert.deepEqual(tables.nodes.map((n) => n.name).sort(), ['Clientes', 'Pedidos', 'huerfana', 'v_ventas']);
  assert.equal(tables.edges.length, 3);
});

test('the filter applies to cards that arrive in a later chunk', () => {
  // The structure first, with the outside sources unchecked; then the consumers stage adds a Power BI report.
  const f = { ...m.noFilters(), hidden: ['external'] };
  const later = m.buildModel(graph({ consumers: [{ from: o('table', 'dbo', 'Pedidos'), consumer: { system: 'Power BI report', name: 'Ventas' } }] }));
  const shown = m.applyFilters(later, f).nodes.map((n) => n.name);
  assert.ok(!shown.includes('origen'), 'what was unchecked stays hidden in every chunk');
  assert.ok(shown.includes('Ventas'), 'a type that arrives later was never unchecked: it shows');
});

test('the Tipos list: what is checked, what hiding leaves, the last one stays, the count', () => {
  const types = [{ type: 'table', count: 3 }, { type: 'view', count: 1 }, { type: 'job', count: 1 }, { type: 'notebook', count: 2 }];
  // Nothing hidden: every type checked, the button says just "Tipos".
  assert.deepEqual(m.checkedTypes(types, []), ['table', 'view', 'job', 'notebook']);
  assert.equal(m.typesCount(types, []), null);
  // Unchecking notebooks and jobs.
  const hidden = m.hiddenFor(types, ['table', 'view']);
  assert.deepEqual(hidden, ['job', 'notebook']);
  assert.deepEqual(m.checkedTypes(types, hidden), ['table', 'view']);
  assert.deepEqual(m.typesCount(types, hidden), { shown: 2, total: 4 });
  // Unchecking the last one changes nothing: at least one type shows, and its box is disabled.
  assert.equal(m.hiddenFor(types, []), null);
  assert.equal(m.isLastChecked(['view'], 'view'), true);
  assert.equal(m.isLastChecked(['view'], 'table'), false);
  assert.equal(m.isLastChecked(['view', 'table'], 'view'), false);
  // A hidden type that is no longer on the cards (after a refresh) doesn't count.
  assert.deepEqual(m.typesCount(types, ['job', 'pipeline']), { shown: 3, total: 4 });
  assert.equal(m.typesCount(types, ['pipeline']), null);
  // Checking every type again empties hidden.
  assert.deepEqual(m.hiddenFor(types, ['table', 'view', 'job', 'notebook']), []);
});

test('the lineage panel uses these functions for the Tipos list', () => {
  // Regression guard: the list's rules live here, tested, not inline in the component.
  const vue = readFileSync(new URL('../components/DatabaseLineage.vue', import.meta.url), 'utf8');
  for (const fn of ['checkedTypes(', 'hiddenFor(', 'typesCount(', 'isLastChecked(']) assert.ok(vue.includes(fn), fn);
});
