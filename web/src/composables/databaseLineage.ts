// The lineage of a whole database (`getLineageGraph`): every object a card, every
// dependency a line, plus the server's jobs and the sources outside the database.
// Pure logic, no Vue: the model built from the backend's graph, the filters, the
// overview by schema (a card per schema, line weight = dependencies crossing) and
// the layout with the objects that have no line in a column at the side.

import type { Confidence, ExternalRef, LineageGraph } from '../api/types';
import { externalId, jobNodeId, layout, nodeId, type Lineage, type LineageEdge, type LineageNode, type Placed } from './lineage';

/** Above this many objects the diagram opens as an overview by schema. */
export const OVERVIEW_ABOVE = 200;
/** Cards drawn at most at once (an expanded schema past it says so). */
export const MAX_CARDS = 600;

/** Engines whose `lineage_graph` is a catalog query: they load on open. The rest run
 *  the generic scan (every definition read), only when the user asks. */
export const CATALOG_LINEAGE = [
  // PostgreSQL and the engines that share its catalog (Redshift reads views and routines only).
  'postgres', 'cockroachdb', 'redshift', 'greenplum', 'yugabytedb', 'timescaledb', 'kingbase', 'alloydb', 'edb', 'fujitsu', 'opengauss',
  'cloudberry', 'greengage', 'risingwave', 'materialize', 'yellowbrick',
  'mysql', 'mariadb', 'sqlserver', 'azuresql', 'fabric', 'oracle', 'oracle_adb', 'powerbi', 'db2', 'hana', 'clickhouse', 'snowflake', 'databricks', 'mongodb',
];
export const hasCatalogLineage = (driverId: string | undefined) => !!driverId && CATALOG_LINEAGE.includes(driverId);

export interface DbModel {
  nodes: Map<string, LineageNode>;
  edges: LineageEdge[];
  truncated: boolean;
  scanned: number;
  unreadable: string[];
  notes: string[];
}

const RANK: Record<Confidence, number> = { confirmed: 3, probable: 2, review: 1 };

/** The backend's graph as cards and lines: each object once, each line once (the
 *  surest confidence wins), the jobs as cards fed by what their steps use. */
export function buildModel(g: LineageGraph): DbModel {
  const nodes = new Map<string, LineageNode>();
  const edges = new Map<string, LineageEdge>();
  const object = (o: { kind: string; schema?: string | null; name: string; parent?: string | null }) => {
    const id = nodeId(o);
    if (!nodes.has(id)) nodes.set(id, { id, kind: o.kind, schema: o.schema ?? null, name: o.name, parent: o.parent ?? null, up: 'done', down: 'done' });
    return id;
  };
  const edge = (e: LineageEdge) => {
    if (e.from === e.to) return;
    const key = `${e.from}>${e.to}`;
    const have = edges.get(key);
    if (!have) return void edges.set(key, e);
    if (RANK[e.confidence] > RANK[have.confidence]) {
      have.confidence = e.confidence;
      have.relation = e.relation;
      have.detail = e.detail ?? have.detail;
    }
    for (const m of e.mentions) if (!have.mentions.some((x) => x.line === m.line && x.text === m.text)) have.mentions.push(m);
  };
  const outside = (x: ExternalRef) => {
    const id = externalId(x);
    if (!nodes.has(id)) {
      nodes.set(id, { id, kind: 'external', schema: x.schema ?? null, name: x.name ?? x.database ?? x.server ?? x.system ?? '?', external: x, up: 'done', down: 'done' });
    }
    return id;
  };
  for (const o of g.objects) object(o);
  for (const e of g.edges) {
    edge({ from: object(e.from), to: object(e.to), relation: e.relation, confidence: e.confidence, detail: e.detail, mentions: [...(e.mentions ?? [])] });
  }
  for (const x of g.external ?? []) {
    edge({ from: outside(x.source), to: object(x.to), relation: 'external', confidence: 'confirmed', detail: x.source.detail, mentions: [...(x.source.mentions ?? [])] });
  }
  // What outside the database reads its objects (a Power BI report or dashboard reading a model).
  for (const c of g.consumers ?? []) {
    edge({ from: object(c.from), to: outside(c.consumer), relation: 'external', confidence: 'confirmed', detail: c.consumer.detail, mentions: [...(c.consumer.mentions ?? [])] });
  }
  for (const j of g.jobs ?? []) {
    const id = jobNodeId(j.id);
    nodes.set(id, { id, kind: 'job', schema: j.group ?? null, name: j.name, jobId: j.id, jobKind: j.kind, up: 'done', down: 'done' });
    for (const d of j.uses) edge({ from: object(d), to: id, relation: 'code', confidence: d.confidence, detail: d.detail, mentions: [...d.mentions] });
    for (const x of j.external) edge({ from: outside(x), to: id, relation: 'external', confidence: 'confirmed', detail: x.detail, mentions: [...(x.mentions ?? [])] });
  }
  const notes = [...g.notes];
  for (const n of g.job_notes ?? []) if (!notes.includes(n)) notes.push(n);
  return { nodes, edges: [...edges.values()], truncated: g.truncated, scanned: g.scanned, unreadable: g.unreadable, notes };
}

/** The objects (not the jobs nor the outside sources) of each schema group. */
export const groupOf = (n: LineageNode): string | null => (n.kind === 'job' || n.kind === 'external' ? null : n.schema ?? '');
export const schemaId = (schema: string) => `schema\u0000\u0000${schema}`;

/** The schemas that hold objects, sorted. */
export function schemasOf(nodes: Iterable<LineageNode>): string[] {
  const set = new Set<string>();
  for (const n of nodes) {
    const g = groupOf(n);
    if (g !== null) set.add(g);
  }
  return [...set].sort((a, b) => a.localeCompare(b));
}

/** The kinds on the cards, for the filter's choices. */
export function kindsOf(nodes: Iterable<LineageNode>): string[] {
  const set = new Set<string>();
  for (const n of nodes) set.add(n.kind);
  return [...set].sort((a, b) => a.localeCompare(b));
}

/** Objects in the model (not counting jobs nor outside sources). */
export const objectCount = (m: DbModel) => [...m.nodes.values()].filter((n) => groupOf(n) !== null).length;

export interface Filters {
  /** Only these kinds (empty: every one). */
  kinds: string[];
  /** Part of the name or the schema; the cards that match show with their neighbours. */
  text: string;
  onlyConfirmed: boolean;
  /** Hide what has no line. */
  onlyConnected: boolean;
  /** Show the relations (foreign keys, a model's relationships) along with the lineage: they are
   *  two concepts side by side, so they start hidden and the lineage shows alone. */
  relations: boolean;
}
export const noFilters = (): Filters => ({ kinds: [], text: '', onlyConfirmed: false, onlyConnected: false, relations: false });

/** The lines that are relations (foreign keys, a model's relationships), not lineage. */
export const isRelation = (e: { relation: string }) => e.relation === 'foreign_key';

/** What the filters leave: the cards that pass and the lines among them. */
export function applyFilters(m: DbModel, f: Filters): { nodes: LineageNode[]; edges: LineageEdge[] } {
  const kindOk = (n: LineageNode) => !f.kinds.length || f.kinds.includes(n.kind);
  let edges = f.onlyConfirmed ? m.edges.filter((e) => e.confidence === 'confirmed') : m.edges;
  if (!f.relations) edges = edges.filter((e) => !isRelation(e));
  const pool = new Map([...m.nodes].filter(([, n]) => kindOk(n)));
  edges = edges.filter((e) => pool.has(e.from) && pool.has(e.to));
  const q = f.text.trim().toLowerCase();
  let keep: Set<string>;
  if (q) {
    const hit = new Set([...pool.values()].filter((n) => `${n.schema ?? ''}.${n.name}`.toLowerCase().includes(q)).map((n) => n.id));
    keep = new Set(hit);
    for (const e of edges) {
      if (hit.has(e.from)) keep.add(e.to);
      if (hit.has(e.to)) keep.add(e.from);
    }
    edges = edges.filter((e) => keep.has(e.from) && keep.has(e.to));
  } else {
    keep = new Set(pool.keys());
  }
  if (f.onlyConnected) {
    const linked = new Set(edges.flatMap((e) => [e.from, e.to]));
    keep = new Set([...keep].filter((id) => linked.has(id)));
  }
  return { nodes: [...keep].map((id) => pool.get(id)!), edges };
}

export interface DbView {
  lineage: Lineage;
  /** Objects left out because the view reached `MAX_CARDS`. */
  hidden: number;
}

/** What the canvas draws: every card of an expanded schema, one card per collapsed
 *  schema, and the lines mapped onto them (the ones that cross schemas add up to a
 *  weight). Jobs and outside sources are always cards of their own. */
export function buildView(nodes: LineageNode[], edges: LineageEdge[], expanded: ReadonlySet<string>, maxCards = MAX_CARDS): DbView {
  const degree = new Map<string, number>();
  for (const e of edges) {
    degree.set(e.from, (degree.get(e.from) ?? 0) + 1);
    degree.set(e.to, (degree.get(e.to) ?? 0) + 1);
  }
  const members = new Map<string, number>();
  const out = new Map<string, LineageNode>();
  const at = new Map<string, string>();
  // Connected cards first, so the cap leaves out the isolated ones.
  const ordered = [...nodes].sort((a, b) => (degree.get(b.id) ?? 0) - (degree.get(a.id) ?? 0));
  let hidden = 0;
  for (const n of ordered) {
    const g = groupOf(n);
    if (g !== null && !expanded.has(g)) {
      members.set(g, (members.get(g) ?? 0) + 1);
      at.set(n.id, schemaId(g));
      continue;
    }
    if (out.size >= maxCards) {
      hidden++;
      continue;
    }
    out.set(n.id, n);
    at.set(n.id, n.id);
  }
  for (const [g, count] of members) {
    out.set(schemaId(g), { id: schemaId(g), kind: 'schema', schema: g, name: g, members: count, up: 'done', down: 'done' });
  }
  const lines = new Map<string, LineageEdge>();
  for (const e of edges) {
    const from = at.get(e.from), to = at.get(e.to);
    if (!from || !to || from === to) continue;
    const key = `${from}>${to}`;
    const have = lines.get(key);
    if (!have) {
      lines.set(key, { ...e, from, to, mentions: from === e.from && to === e.to ? e.mentions : [], weight: 1 });
      continue;
    }
    have.weight = (have.weight ?? 1) + 1;
    if (RANK[e.confidence] > RANK[have.confidence]) have.confidence = e.confidence;
    have.mentions = [];
    have.detail = null;
    if (have.relation !== e.relation) have.relation = 'code';
  }
  return { lineage: { nodes: out, edges: [...lines.values()], capped: hidden > 0, unreadable: [], notes: [] }, hidden };
}

/** Whether the lines of a collapsed card stand for more than one dependency. */
export const isAggregate = (e: LineageEdge) => (e.weight ?? 1) > 1;

/** Left to right by depth (what is used on the left) for the cards that have a
 *  line; the ones with none go in columns on the far left. */
export function layoutDb(g: Lineage, w: number, h: number): Placed {
  const linked = new Set(g.edges.flatMap((e) => [e.from, e.to]));
  const main: Lineage = { ...g, nodes: new Map([...g.nodes].filter(([id]) => linked.has(id))) };
  const alone = [...g.nodes.values()]
    .filter((n) => !linked.has(n.id))
    .sort((a, b) => a.kind.localeCompare(b.kind) || `${a.schema ?? ''}.${a.name}`.localeCompare(`${b.schema ?? ''}.${b.name}`));
  const placed = layout(main, w, h);
  if (!alone.length) return placed;
  const gap = 16;
  const mainRows = main.nodes.size ? Math.ceil((placed.bounds.height + gap) / (h + gap)) : 0;
  const rows = Math.max(1, Math.min(40, Math.max(12, mainRows)));
  const cols = Math.ceil(alone.length / rows);
  const x0 = (main.nodes.size ? placed.bounds.x - 90 : 0) - cols * (w + gap) + gap;
  const y0 = main.nodes.size ? placed.bounds.y : 0;
  alone.forEach((n, i) => placed.pos.set(n.id, { x: x0 + Math.floor(i / rows) * (w + gap), y: y0 + (i % rows) * (h + gap) }));
  const xs = [...placed.pos.values()];
  const minX = Math.min(...xs.map((p) => p.x)), minY = Math.min(...xs.map((p) => p.y));
  const maxX = Math.max(...xs.map((p) => p.x + w)), maxY = Math.max(...xs.map((p) => p.y + h));
  return { pos: placed.pos, paths: placed.paths, bounds: { x: minX, y: minY, width: maxX - minX, height: maxY - minY } };
}
