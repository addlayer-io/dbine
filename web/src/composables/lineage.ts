// The lineage diagram's graph: an object in the middle, what it uses to its
// left (`getReferences`: the tables a view reads, the routines a procedure
// calls, a table's foreign keys' tables, sources outside the database) and
// what uses it to its right (`getDependents`, with the server's jobs whose
// steps run it). Each card loads the next level on either side when asked; a
// job's left side is what its steps use (`getJobReferences`), and nothing is
// to its right. Laid out left to right with dagre.

import dagre from '@dagrejs/dagre';
import type { Confidence, CrossUser, DependencyReport, ExternalRef, JobDependent, LocatedSource, Mention, ObjectRef, Relation } from '../api/types';

/** Whether a side of a card was read: not yet, now, done or failed. */
export type SideState = 'none' | 'loading' | 'done' | 'error';

/** The connection a card belongs to when it isn't the diagram's own (a Power BI table, the real
 *  object behind a source): its sides are read there. `driver` empty: look it up by `id`. */
export interface NodeConn {
  id: string;
  name: string;
  database: string;
  driver?: string;
}

export interface LineageNode {
  id: string;
  kind: string;
  schema: string | null;
  name: string;
  /** A trigger's table… */
  parent?: string | null;
  /** A source outside the database: it has no sides to read. */
  external?: ExternalRef;
  /** A server job (`kind` "job"): its id for the jobs calls, and the
   *  engine's word for it ("SQL Server Agent", "pg_cron"…). */
  jobId?: string;
  jobKind?: string;
  root?: boolean;
  /** Another connection's card (absent: the diagram's own). */
  conn?: NodeConn;
  /** Added by "Incluir otras conexiones": hidden when the switch is off. */
  cross?: boolean;
  /** A schema's card in the database lineage's overview: how many objects it holds. */
  members?: number;
  /** What it uses (to the left). */
  up: SideState;
  /** What uses it (to the right). */
  down: SideState;
  error?: string;
}

/** `from` is used by `to`: data and calls flow left to right. */
export interface LineageEdge {
  from: string;
  to: string;
  relation: Relation | 'external';
  confidence: Confidence;
  /** A foreign key's or a source's description. */
  detail?: string | null;
  /** The lines of `to`'s source (or `from`'s, for what uses it) that name the other. */
  mentions: Mention[];
  /** The database lineage's overview: how many dependencies this line stands for. */
  weight?: number;
}

export interface Lineage {
  nodes: Map<string, LineageNode>;
  edges: LineageEdge[];
  /** The cap stopped a level: some cards weren't added. */
  capped: boolean;
  /** Definitions that couldn't be read, from every report. */
  unreadable: string[];
  /** The reports' notes (a missing permission…). */
  notes: string[];
}

/** At most this many cards: past it a level adds no more and says so. */
export const MAX_NODES = 200;

export const nodeId = (o: { kind: string; schema?: string | null; name: string }) => `${o.kind}\u0000${o.schema ?? ''}\u0000${o.name}`;
/** A job's card is its id's: two jobs may share a name. */
/** A card of another connection: the same name in two connections is two cards. */
export const connNodeId = (c: NodeConn, o: { kind: string; schema?: string | null; name: string }) => `${c.id}\u0001${c.database}\u0001${nodeId(o)}`;
export const jobNodeId = (jobId: string) => `job\u0000${jobId}`;
export const externalId = (x: ExternalRef, c?: NodeConn) =>
  `${c ? `${c.id}\u0001${c.database}\u0001` : ''}external\u0000${[x.system, x.server, x.database, x.schema, x.name, x.detail].map((p) => p ?? '').join('\u0000')}`;

/** The graph with `root` in the middle; `rootJobId` when it's a server job
 *  (nothing uses a job: its right side is done). */
export function newLineage(root: ObjectRef, rootJobId?: string | null): Lineage {
  const node: LineageNode = rootJobId
    ? { id: jobNodeId(rootJobId), kind: 'job', schema: root.schema ?? null, name: root.name, jobId: rootJobId, root: true, up: 'none', down: 'done' }
    : { id: nodeId(root), kind: root.kind, schema: root.schema ?? null, name: root.name, root: true, up: 'none', down: 'none' };
  return {
    nodes: new Map([[node.id, node]]),
    edges: [],
    capped: false,
    unreadable: [],
    notes: [],
  };
}

function remember(g: Lineage, report: DependencyReport) {
  for (const u of report.unreadable) if (!g.unreadable.includes(u)) g.unreadable.push(u);
  for (const n of [report.note, ...(report.job_notes ?? [])]) if (n && !g.notes.includes(n)) g.notes.push(n);
}

function addEdge(g: Lineage, e: LineageEdge) {
  if (e.from === e.to || g.edges.some((x) => x.from === e.from && x.to === e.to)) return;
  g.edges.push(e);
}

/** A node for an object, unless the cap is reached (then null). */
function ensure(g: Lineage, o: { kind: string; schema?: string | null; name: string; parent?: string | null }, external?: ExternalRef, conn?: NodeConn): LineageNode | null {
  const id = external ? externalId(external, conn) : conn ? connNodeId(conn, o) : nodeId(o);
  const have = g.nodes.get(id);
  if (have) return have;
  if (g.nodes.size >= MAX_NODES) {
    g.capped = true;
    return null;
  }
  const n: LineageNode = external
    ? { id, kind: 'external', schema: external.schema ?? null, name: external.name ?? external.database ?? external.server ?? external.system ?? '?', external, up: 'done', down: 'done', ...(conn ? { conn } : {}) }
    : { id, kind: o.kind, schema: o.schema ?? null, name: o.name, parent: o.parent ?? null, up: 'none', down: 'none', ...(conn ? { conn } : {}) };
  g.nodes.set(id, n);
  return n;
}

/** A job's card, unless the cap is reached (then null). */
function ensureJob(g: Lineage, j: JobDependent, conn?: NodeConn): LineageNode | null {
  const id = conn ? `${conn.id}\u0001${jobNodeId(j.id)}` : jobNodeId(j.id);
  const have = g.nodes.get(id);
  if (have) {
    have.jobKind ??= j.kind;
    return have;
  }
  if (g.nodes.size >= MAX_NODES) {
    g.capped = true;
    return null;
  }
  const n: LineageNode = { id, kind: 'job', schema: j.group ?? null, name: j.name, jobId: j.id, jobKind: j.kind, up: 'none', down: 'done', ...(conn ? { conn } : {}) };
  g.nodes.set(id, n);
  return n;
}

/** What `id` uses (a references report): cards to its left. */
export function addUses(g: Lineage, id: string, report: DependencyReport, conn?: NodeConn) {
  remember(g, report);
  for (const d of report.items) {
    const n = ensure(g, d, undefined, conn);
    if (n) addEdge(g, { from: n.id, to: id, relation: d.relation, confidence: d.confidence, detail: d.detail, mentions: d.mentions });
  }
  for (const x of report.external ?? []) {
    const n = ensure(g, { kind: 'external', name: '' }, x, conn);
    if (n) addEdge(g, { from: n.id, to: id, relation: 'external', confidence: 'confirmed', detail: x.detail, mentions: x.mentions ?? [] });
  }
  const self = g.nodes.get(id);
  if (self) self.up = 'done';
}

/** What uses `id` (a dependents report): cards to its right. A key, index
 *  or check on a column has no place in an object's lineage. */
export function addUsedBy(g: Lineage, id: string, report: DependencyReport, conn?: NodeConn) {
  remember(g, report);
  for (const d of report.items) {
    if (d.relation === 'index' || d.relation === 'check') continue;
    const n = ensure(g, d, undefined, conn);
    if (n) addEdge(g, { from: id, to: n.id, relation: d.relation, confidence: d.confidence, detail: d.detail, mentions: d.mentions });
  }
  for (const j of report.jobs ?? []) {
    const n = ensureJob(g, j, conn);
    const detail = [j.kind, ...(j.steps ?? [])].filter(Boolean).join(' · ');
    if (n) addEdge(g, { from: id, to: n.id, relation: 'code', confidence: j.confidence, detail, mentions: j.mentions ?? [] });
  }
  // What outside the database reads it (Power BI reports and dashboards): grey cards to its right.
  for (const x of report.external_users ?? []) {
    const n = ensure(g, { kind: 'external', name: '' }, x, conn);
    if (n) addEdge(g, { from: id, to: n.id, relation: 'external', confidence: 'confirmed', detail: x.detail, mentions: x.mentions ?? [] });
  }
  const self = g.nodes.get(id);
  if (self) self.down = 'done';
}

/** What other connections read from `rootId` (`get_cross_users`): their tables to its right, each
 *  in its own connection. Cards from an earlier answer that this one doesn't have go (a refresh). */
export function setCrossUsers(g: Lineage, rootId: string, users: CrossUser[], driverOf: (connectionId: string) => string | undefined = () => undefined) {
  const keep = new Set<string>();
  for (const u of users) {
    const conn: NodeConn = { id: u.connection_id, name: u.connection_name, database: u.database, driver: driverOf(u.connection_id) };
    keep.add(connNodeId(conn, u.object));
  }
  for (const [id, n] of [...g.nodes]) {
    if (n.cross && !keep.has(id)) {
      g.nodes.delete(id);
      g.edges = g.edges.filter((e) => e.from !== id && e.to !== id);
    }
  }
  for (const u of users) {
    const conn: NodeConn = { id: u.connection_id, name: u.connection_name, database: u.database, driver: driverOf(u.connection_id) };
    const n = ensure(g, u.object, undefined, conn);
    if (!n) continue;
    n.cross = true;
    // The object that reads it is the one on its right; its left is the root, already linked.
    n.up = 'done';
    addEdge(g, { from: rootId, to: n.id, relation: 'external', confidence: 'confirmed', detail: u.detail, mentions: [] });
  }
}

/** The card for a source outside the database becomes the real object, in the connection that
 *  holds it (`lineage_locate_source`): same lines, now from the object. Returns its id. */
export function replaceExternal(g: Lineage, externalNodeId: string, found: LocatedSource, conn: NodeConn): string | null {
  const ext = g.nodes.get(externalNodeId);
  if (!ext) return null;
  const real: NodeConn = { ...conn, database: found.database };
  const id = connNodeId(real, found.object);
  if (!g.nodes.has(id)) {
    g.nodes.set(id, { id, kind: found.object.kind, schema: found.object.schema ?? null, name: found.object.name, conn: real, up: 'none', down: 'none' });
  }
  g.nodes.delete(externalNodeId);
  const seen = new Set<string>();
  g.edges = g.edges.flatMap((e) => {
    const from = e.from === externalNodeId ? id : e.from;
    const to = e.to === externalNodeId ? id : e.to;
    const key = `${from}>${to}`;
    if (from === to || seen.has(key)) return [];
    seen.add(key);
    return [{ ...e, from, to }];
  });
  return id;
}

/** A Power BI report or dashboard that reads the object (a card of `consumers` / `external_users`), not a source it reads. */
export const readerKind = (n: { external?: ExternalRef }): 'report' | 'dashboard' | undefined =>
  /^Power BI (report|dashboard)$/i.exec(n.external?.system ?? '')?.[1]?.toLowerCase() as 'report' | 'dashboard' | undefined;

/** The lineage without what "Incluir otras conexiones" added: those cards, and what only they reach. */
export function withoutCross(g: Lineage): Lineage {
  if (![...g.nodes.values()].some((n) => n.cross)) return g;
  const keep = new Set([...g.nodes.values()].filter((n) => !n.cross).map((n) => n.id));
  const edges = g.edges.filter((e) => keep.has(e.from) && keep.has(e.to));
  return reachable({ ...g, edges, nodes: new Map([...g.nodes].filter(([id]) => keep.has(id))) });
}

/** Only the cards the root reaches along `g`'s lines. */
function reachable(g: Lineage): Lineage {
  const root = [...g.nodes.values()].find((n) => n.root);
  if (!root) return g;
  const next = new Map<string, string[]>();
  for (const e of g.edges) {
    (next.get(e.from) ?? next.set(e.from, []).get(e.from)!).push(e.to);
    (next.get(e.to) ?? next.set(e.to, []).get(e.to)!).push(e.from);
  }
  const reach = new Set([root.id]);
  const todo = [root.id];
  while (todo.length) for (const id of next.get(todo.pop()!) ?? []) if (!reach.has(id)) { reach.add(id); todo.push(id); }
  return { ...g, nodes: new Map([...g.nodes].filter(([id]) => reach.has(id))) };
}

/** How many cards each side of `id` has now. */
export function sideCounts(g: Lineage, id: string): { up: number; down: number } {
  let up = 0, down = 0;
  for (const e of g.edges) {
    if (e.to === id) up++;
    if (e.from === id) down++;
  }
  return { up, down };
}

export interface Placed {
  pos: Map<string, { x: number; y: number }>;
  /** Each edge's curve, from the right of `from` to the left of `to`. */
  paths: { edge: LineageEdge; d: string }[];
  bounds: { x: number; y: number; width: number; height: number };
}

/** Left to right: what's used before what uses it. Every card is `w` × `h`. */
/** The lineage without its relations (foreign keys): their lines go, and so do the cards only they
 *  reach from the root. */
export function withoutRelations(g: Lineage): Lineage {
  return reachable({ ...g, edges: g.edges.filter((e) => e.relation !== 'foreign_key') });
}

export function layout(g: Lineage, w: number, h: number): Placed {
  const dg = new dagre.graphlib.Graph();
  dg.setGraph({ rankdir: 'LR', nodesep: 16, ranksep: 90, marginx: 0, marginy: 0 });
  dg.setDefaultEdgeLabel(() => ({}));
  for (const n of g.nodes.values()) dg.setNode(n.id, { width: w, height: h });
  for (const e of g.edges) dg.setEdge(e.from, e.to);
  dagre.layout(dg);
  const pos = new Map<string, { x: number; y: number }>();
  let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
  for (const n of g.nodes.values()) {
    const p = dg.node(n.id) as { x: number; y: number };
    const x = p.x - w / 2, y = p.y - h / 2;
    pos.set(n.id, { x, y });
    minX = Math.min(minX, x); minY = Math.min(minY, y);
    maxX = Math.max(maxX, x + w); maxY = Math.max(maxY, y + h);
  }
  const paths = g.edges.flatMap((edge) => {
    const a = pos.get(edge.from), b = pos.get(edge.to);
    if (!a || !b) return [];
    const x1 = a.x + w, y1 = a.y + h / 2, x2 = b.x, y2 = b.y + h / 2;
    const c = Math.max(30, (x2 - x1) / 2);
    return [{ edge, d: `M ${x1} ${y1} C ${x1 + c} ${y1}, ${x2 - c} ${y2}, ${x2} ${y2}` }];
  });
  if (!g.nodes.size) { minX = minY = 0; maxX = maxY = 0; }
  return { pos, paths, bounds: { x: minX, y: minY, width: maxX - minX, height: maxY - minY } };
}
