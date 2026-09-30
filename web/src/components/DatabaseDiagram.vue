<script setup lang="ts">
import { computed, nextTick, onMounted, reactive, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import dagre from '@dagrejs/dagre';
import type { ColumnDef, ForeignKeyDef, TableSchema } from '../api/schema-types';
import { usePanZoom } from '../composables/usePanZoom';
import { useTranslation } from 'i18next-vue';
import { language } from '../i18n';

// Entity-relationship diagram of a whole database: tables as cards (keys,
// types, NOT NULL), foreign keys as curves from the FK column to the
// referenced column with crow's-foot ends ("many" at the FK side). Laid out
// with dagre (left to right, referenced tables first); tables without
// relations are gridded below. Tables can be dragged; "Reordenar" re-runs
// the layout. Same canvas as the plan view: drag to pan, pinch or ⌘/Ctrl +
// wheel to zoom, F fits, 0 resets. Double-click opens a table.

const props = defineProps<{ schema: TableSchema[]; title?: string }>();
const emit = defineEmits<{
  'open-table': [table: { schema: string | null; name: string }];
  'export-svg': [svg: string];
  'export-png': [dataUrl: string];
}>();
const { t } = useTranslation();

// -- geometry -----------------------------------------------------------------------------
const HEAD = 34;
const ROW = 22;
const FOOT = 24;
const PADB = 6;
const MIN_W = 190;
const MAX_W = 380;
/** Tables with more columns than this start collapsed to the first SHOW (plus keys). */
const LIMIT = 14;
const SHOW = 10;
const STUB = 14;
const SANS = '-apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, Ubuntu, sans-serif';
const MONO = '"SF Mono", Menlo, Monaco, Consolas, "JetBrains Mono", monospace';
const F_HEAD = `600 12.5px ${SANS}`;
const F_SCHEMA = `400 12.5px ${SANS}`;
const F_COL = `400 12px ${SANS}`;
const F_TYPE = `400 11px ${MONO}`;
const SCHEMA_COLORS = ['#3794ff', '#4ec9b0', '#c586c0', '#d7ba7d', '#ce9178', '#9cdcfe', '#b5cea8', '#f48771'];

let measureCtx: CanvasRenderingContext2D | null = null;
function textW(s: string, font: string) {
  measureCtx ??= document.createElement('canvas').getContext('2d');
  if (!measureCtx) return s.length * 7;
  measureCtx.font = font;
  return measureCtx.measureText(s).width;
}
/** Cuts `s` with an ellipsis so it fits in `max` px (SVG export has no CSS ellipsis). */
function fitText(s: string, font: string, max: number) {
  if (textW(s, font) <= max) return s;
  let lo = 0;
  let hi = s.length;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (textW(s.slice(0, mid) + '…', font) <= max) lo = mid;
    else hi = mid - 1;
  }
  return s.slice(0, lo) + '…';
}

// -- model ----------------------------------------------------------------------------------
interface Col { c: ColumnDef; pk: boolean; fk: boolean; ref: boolean }
interface TNode { key: string; t: TableSchema; cols: Col[]; w: number; color: string }
interface Rel { id: string; from: string; to: string; fk: ForeignKeyDef; self: boolean }

const keyOf = (schema: string | null, name: string) => `${schema ?? ''}\u0001${name}`;
const qualified = (t: TableSchema) => (t.schema ? `${t.schema}.${t.name}` : t.name);

const schemas = computed(() => [...new Set(props.schema.map((t) => t.schema).filter((s): s is string => !!s))].sort());

const model = computed(() => {
  const nodes = new Map<string, TNode>();
  const byLower = new Map<string, string>();
  const byName = new Map<string, string[]>();
  const colorOf = (s: string | null) => (schemas.value.length > 1 && s ? SCHEMA_COLORS[schemas.value.indexOf(s) % SCHEMA_COLORS.length] : '');
  for (const t of props.schema) {
    const key = keyOf(t.schema, t.name);
    const pk = new Set(t.primary_key?.columns ?? []);
    const fk = new Set(t.foreign_keys.flatMap((f) => f.columns));
    nodes.set(key, { key, t, cols: t.columns.map((c) => ({ c, pk: pk.has(c.name), fk: fk.has(c.name), ref: false })), w: 0, color: colorOf(t.schema) });
    byLower.set(`${t.schema ?? ''}.${t.name}`.toLowerCase(), key);
    const list = byName.get(t.name.toLowerCase()) ?? [];
    list.push(key);
    byName.set(t.name.toLowerCase(), list);
  }
  // Engines don't always say which schema a reference points to: exact
  // match, then case-insensitive, then by name when it's unambiguous.
  const resolve = (t: TableSchema, f: ForeignKeyDef) => {
    const s = f.ref_schema ?? t.schema;
    const exact = keyOf(s, f.ref_table);
    if (nodes.has(exact)) return exact;
    const lower = byLower.get(`${s ?? ''}.${f.ref_table}`.toLowerCase());
    if (lower) return lower;
    const named = byName.get(f.ref_table.toLowerCase());
    return named?.length === 1 ? named[0] : null;
  };
  const rels: Rel[] = [];
  for (const n of nodes.values()) {
    n.t.foreign_keys.forEach((f, i) => {
      const to = resolve(n.t, f);
      if (!to) return;
      rels.push({ id: `${n.key}\u0002${i}`, from: n.key, to, fk: f, self: to === n.key });
      const target = nodes.get(to)!;
      for (const rc of f.ref_columns) {
        const col = target.cols.find((c) => c.c.name === rc);
        if (col) col.ref = true;
      }
    });
  }
  for (const n of nodes.values()) {
    const head = textW(n.t.name, F_HEAD) + (n.t.schema ? textW(`${n.t.schema}.`, F_SCHEMA) : 0) + 34;
    const cols = n.cols.reduce((m, c) => Math.max(m, 10 + 28 + textW(c.c.name, F_COL) + 16 + textW(c.c.data_type, F_TYPE) + 12), 0);
    n.w = Math.round(Math.min(MAX_W, Math.max(MIN_W, head, cols)));
  }
  return { nodes, rels };
});

// -- filters & visible rows -------------------------------------------------------------------
/** The schemas shown; none picked = all of them. */
const schemaFilter = ref<string[]>([]);
const keysOnly = ref(false);
const expanded = reactive(new Set<string>());

const visible = computed(() => [...model.value.nodes.values()].filter((n) => !schemaFilter.value.length || schemaFilter.value.includes(n.t.schema ?? '')));
const visibleKeys = computed(() => new Set(visible.value.map((n) => n.key)));
const rels = computed(() => model.value.rels.filter((r) => visibleKeys.value.has(r.from) && visibleKeys.value.has(r.to)));

const isKey = (c: Col) => c.pk || c.fk || c.ref;
interface View { rows: Col[]; hidden: number; foot: boolean; h: number; index: Map<string, number> }
const views = computed(() => {
  const out = new Map<string, View>();
  for (const n of visible.value) {
    const open = expanded.has(n.key);
    let rows: Col[];
    if (open) rows = n.cols;
    else if (keysOnly.value) rows = n.cols.filter(isKey);
    else if (n.cols.length > LIMIT) rows = n.cols.filter((c, i) => i < SHOW || isKey(c));
    else rows = n.cols;
    const hidden = n.cols.length - rows.length;
    const collapsible = keysOnly.value ? n.cols.some((c) => !isKey(c)) : n.cols.length > LIMIT;
    const foot = hidden > 0 || (open && collapsible);
    const h = HEAD + Math.max(rows.length, n.cols.length ? 0 : 1) * ROW + (foot ? FOOT : 0) + PADB;
    out.set(n.key, { rows, hidden, foot, h, index: new Map(rows.map((c, i) => [c.c.name, i])) });
  }
  return out;
});
function toggleExpand(key: string) {
  if (expanded.has(key)) expanded.delete(key);
  else expanded.add(key);
}

// -- layout ---------------------------------------------------------------------------------------
const pos = reactive<Record<string, { x: number; y: number }>>({});
const GRID_GAP = 32;

/** Where the last layout put each table, and its routed edges (valid while both ends stay there). */
const laidAt = new Map<string, { x: number; y: number }>();
const routes = new Map<string, { x: number; y: number }[]>();

/**
 * A table referenced by many others (users, companies…) puts all of them in
 * one dagre rank: a column taller than anything readable. Ranks taller than
 * the diagram's natural size are wrapped into side-by-side sub-columns,
 * shifting the ranks to their right. Returns whether it moved anything.
 */
function wrapTallRanks(nodes: TNode[]): boolean {
  const h = (n: TNode) => views.value.get(n.key)!.h;
  const GAP_Y = 34;
  const GAP_X = 48;
  const area = nodes.reduce((a, n) => a + (n.w + GAP_X) * (h(n) + GAP_Y), 0);
  const maxH = Math.max(1400, Math.sqrt(area) * 1.1);
  // Ranks share a center x in LR.
  const ranks = new Map<number, TNode[]>();
  for (const n of nodes) {
    const cx = Math.round(pos[n.key].x + n.w / 2);
    const list = ranks.get(cx) ?? [];
    list.push(n);
    ranks.set(cx, list);
  }
  const tall = [...ranks.values()].some((r) => r.reduce((a, n) => a + h(n) + GAP_Y, 0) > maxH);
  if (!tall) return false;
  let top = Infinity;
  for (const n of nodes) top = Math.min(top, pos[n.key].y);
  // Split each rank (kept in dagre's order) into columns no taller than maxH…
  const blocks = [...ranks.keys()].sort((a, b) => a - b).map((cx) => {
    const list = ranks.get(cx)!.sort((a, b) => pos[a.key].y - pos[b.key].y);
    const total = list.reduce((a, n) => a + h(n) + GAP_Y, 0);
    const perCol = total / Math.ceil(total / maxH);
    const cols: TNode[][] = [[]];
    let y = 0;
    for (const n of list) {
      if (y > 0 && y + h(n) / 2 > perCol) { cols.push([]); y = 0; }
      cols[cols.length - 1].push(n);
      y += h(n) + GAP_Y;
    }
    return { cols, w: Math.max(...list.map((n) => n.w)) };
  });
  // …then stack every rank compactly, each column centered on the tallest.
  const colH = (c: TNode[]) => c.reduce((a, n) => a + h(n) + GAP_Y, -GAP_Y);
  const H = Math.max(...blocks.flatMap((b) => b.cols.map(colH)));
  let x = Math.min(...nodes.map((n) => pos[n.key].x));
  for (const b of blocks) {
    for (const c of b.cols) {
      let y = top + (H - colH(c)) / 2;
      for (const n of c) {
        pos[n.key] = { x: Math.round(x + (b.w - n.w) / 2), y: Math.round(y) };
        y += h(n) + GAP_Y;
      }
      x += b.w + GAP_X;
    }
    x += 140 - GAP_X;
  }
  return true;
}

/** The automatic layout failed and the tables were put in a grid. */
const layoutFailed = ref(false);

function relayout() {
  laidAt.clear();
  routes.clear();
  const nodes = visible.value;
  const v = views.value;
  const linked = new Set<string>();
  for (const r of rels.value) if (!r.self) { linked.add(r.from); linked.add(r.to); }

  let maxX = 0;
  let maxY = 0;
  layoutFailed.value = false;
  // A simple graph, one edge per pair of tables weighted by how many foreign
  // keys join them: dagre 3 throws "Not possible to find intersection inside
  // of the rectangle" on multigraphs with parallel edges (two FKs to the
  // same table — created_by/updated_by… — are common in real databases).
  const g = new dagre.graphlib.Graph();
  g.setGraph({ rankdir: 'LR', nodesep: 34, ranksep: 120, edgesep: 14, marginx: 0, marginy: 0 });
  g.setDefaultEdgeLabel(() => ({}));
  if (linked.size) {
    for (const n of nodes) if (linked.has(n.key)) g.setNode(n.key, { width: n.w, height: v.get(n.key)!.h });
    // Referenced (parent) tables to the left of the tables that point to them.
    const pairs = new Map<string, { to: string; from: string; weight: number }>();
    for (const r of rels.value) {
      if (r.self) continue;
      const k = `${r.to}\u0000${r.from}`;
      const p = pairs.get(k);
      if (p) p.weight++;
      else pairs.set(k, { to: r.to, from: r.from, weight: 1 });
    }
    for (const p of pairs.values()) g.setEdge(p.to, p.from, { minlen: 1, weight: p.weight });
    try {
      dagre.layout(g);
    } catch (e) {
      // Never leave the diagram blank: fall back to the grid below.
      console.error('dagre layout failed', e);
      layoutFailed.value = true;
      linked.clear();
    }
  }
  if (linked.size) {
    for (const n of nodes) {
      if (!linked.has(n.key)) continue;
      const d = g.node(n.key);
      const h = v.get(n.key)!.h;
      pos[n.key] = { x: Math.round(d.x! - n.w / 2), y: Math.round(d.y! - h / 2) };
    }
    const wrapped = wrapTallRanks(nodes.filter((n) => linked.has(n.key)));
    for (const n of nodes) {
      if (!linked.has(n.key)) continue;
      laidAt.set(n.key, { ...pos[n.key] });
      maxX = Math.max(maxX, pos[n.key].x + n.w);
      maxY = Math.max(maxY, pos[n.key].y + v.get(n.key)!.h);
    }
    // Edges spanning several ranks come with bend points in the gaps between
    // tables (dagre's dummy nodes): kept to route around the cards.
    if (!wrapped) for (const r of rels.value) {
      if (r.self) continue;
      const pts = g.edge(r.to, r.from)?.points ?? [];
      if (pts.length > 3) routes.set(r.id, pts.slice(1, -1).reverse().map((p: { x: number; y: number }) => ({ x: p.x, y: p.y })));
    }
  }

  // Tables without relations: a grid below the graph (or alone), by name.
  const loose = nodes.filter((n) => !linked.has(n.key)).sort((a, b) => a.t.name.localeCompare(b.t.name));
  if (loose.length) {
    const area = loose.reduce((a, n) => a + (n.w + GRID_GAP) * (v.get(n.key)!.h + GRID_GAP), 0);
    const limit = Math.max(linked.size ? maxX : 0, Math.min(2400, Math.sqrt(area) * 1.7), 900);
    let x = 0;
    let y = linked.size ? maxY + 90 : 0;
    let rowH = 0;
    for (const n of loose) {
      if (x > 0 && x + n.w > limit) { x = 0; y += rowH + GRID_GAP; rowH = 0; }
      pos[n.key] = { x, y };
      x += n.w + GRID_GAP;
      rowH = Math.max(rowH, v.get(n.key)!.h);
    }
  }
}

const bounds = computed(() => {
  let x1 = Infinity;
  let y1 = Infinity;
  let x2 = -Infinity;
  let y2 = -Infinity;
  for (const n of visible.value) {
    const p = pos[n.key];
    if (!p) continue;
    const h = views.value.get(n.key)!.h;
    x1 = Math.min(x1, p.x);
    y1 = Math.min(y1, p.y);
    x2 = Math.max(x2, p.x + n.w);
    y2 = Math.max(y2, p.y + h);
  }
  if (x1 === Infinity) return { x: 0, y: 0, width: 0, height: 0 };
  // Room for self-reference loops on the right.
  return { x: x1, y: y1, width: x2 - x1 + 40, height: y2 - y1 };
});

// -- viewport ---------------------------------------------------------------------------------------
const pz = usePanZoom({
  bounds: () => bounds.value,
  minK: 0.05,
  maxK: 2.5,
  maxFitK: 1,
  noPan: '.dd-card, .dd-zoom, .dd-mini, .dd-banner',
});
const { vp, view, size, panning, zoomBy, actualSize, fit, centerOn, onWheel, onPointerDown, mini, miniView, onMiniDown } = pz;
/** Far out, rows are unreadable: hide their contents (cheaper to paint). */
const lod = computed(() => view.k < 0.32);
const showMini = ref(true);

/** Laying out a big database takes a moment and blocks the page: show the
 * spinner first (two frames so it paints), then compute. */
const busy = ref(true);
function relayoutAndFit() {
  busy.value = true;
  requestAnimationFrame(() => requestAnimationFrame(() => {
    try {
      relayout();
    } finally {
      busy.value = false;
    }
    nextTick(fit);
  }));
}
watch(() => props.schema, () => {
  expanded.clear();
  selected.value = null;
  schemaFilter.value = schemaFilter.value.filter((s) => schemas.value.includes(s));
  for (const k of Object.keys(pos)) delete pos[k];
  relayoutAndFit();
});
watch([schemaFilter, keysOnly], relayoutAndFit);
onMounted(relayoutAndFit);

function onKey(e: KeyboardEvent) {
  if (pz.onKey(e)) return;
  if (e.key === 'Escape') selected.value = null;
}

// -- selection, hover, search -------------------------------------------------------------------------
const selected = ref<string | null>(null);
const hoverRel = ref<string | null>(null);
const hoverCol = ref<string | null>(null);
const tip = reactive({ x: 0, y: 0 });
const colKey = (key: string, col: string) => `${key}\u0002${col}`;

const sel = computed(() => (selected.value ? model.value.nodes.get(selected.value) ?? null : null));
const neighbors = computed(() => {
  const s = selected.value;
  if (!s) return null;
  const set = new Set([s]);
  for (const r of rels.value) {
    if (r.from === s) set.add(r.to);
    if (r.to === s) set.add(r.from);
  }
  return set;
});
/** Relations to highlight: the hovered one, those on the hovered column, or the selected table's. */
const hotRels = computed(() => {
  const set = new Set<string>();
  if (hoverRel.value) set.add(hoverRel.value);
  else if (hoverCol.value) {
    for (const r of rels.value) {
      if (r.fk.columns.some((c) => colKey(r.from, c) === hoverCol.value) || r.fk.ref_columns.some((c) => colKey(r.to, c) === hoverCol.value)) set.add(r.id);
    }
  } else if (selected.value) {
    for (const r of rels.value) if (r.from === selected.value || r.to === selected.value) set.add(r.id);
  }
  return set;
});
/** Columns at both ends of the highlighted relations, per table. */
const hotCols = computed(() => {
  const out = new Map<string, Set<string>>();
  const add = (k: string, c: string) => { const s = out.get(k) ?? new Set(); s.add(c); out.set(k, s); };
  for (const r of rels.value) {
    if (!hotRels.value.has(r.id)) continue;
    r.fk.columns.forEach((c) => add(r.from, c));
    r.fk.ref_columns.forEach((c) => add(r.to, c));
  }
  return out;
});

const query = ref('');
const matchIdx = ref(0);
const matches = computed(() => {
  const q = query.value.trim().toLowerCase();
  if (!q) return [] as string[];
  return visible.value.filter((n) => qualified(n.t).toLowerCase().includes(q)).map((n) => n.key);
});
const matchSet = computed(() => new Set(matches.value));
watch(matches, (m) => {
  matchIdx.value = 0;
  if (m.length) focusTable(m[0], false);
});
function nextMatch(back = false) {
  const m = matches.value;
  if (!m.length) return;
  matchIdx.value = (matchIdx.value + (back ? m.length - 1 : 1)) % m.length;
  focusTable(m[matchIdx.value], false);
}

function focusTable(key: string, select = true) {
  const n = model.value.nodes.get(key);
  const p = pos[key];
  if (!n || !p) return;
  if (select) selected.value = key;
  const h = views.value.get(key)?.h ?? HEAD;
  centerOn(p.x + n.w / 2, p.y + h / 2, view.k < 0.6 ? 0.9 : undefined);
}

function cardState(n: TNode) {
  const hot = hotCols.value.get(n.key);
  return [
    selected.value === n.key ? 's' : '',
    neighbors.value && !neighbors.value.has(n.key) ? 'd' : '',
    matchSet.value.has(n.key) ? 'm' : '',
    matches.value[matchIdx.value] === n.key ? 'c' : '',
    hot ? [...hot].join(',') : '',
  ].join('|');
}

// -- drag tables ------------------------------------------------------------------------------------
const dragging = ref<string | null>(null);
function onCardDown(e: PointerEvent, key: string) {
  if (e.button !== 0) return;
  const start = { x: e.clientX, y: e.clientY, px: pos[key].x, py: pos[key].y };
  let moved = false;
  const move = (ev: PointerEvent) => {
    const dx = (ev.clientX - start.x) / view.k;
    const dy = (ev.clientY - start.y) / view.k;
    if (!moved && Math.abs(ev.clientX - start.x) + Math.abs(ev.clientY - start.y) < 4) return;
    moved = true;
    dragging.value = key;
    pos[key] = { x: Math.round(start.px + dx), y: Math.round(start.py + dy) };
  };
  const up = () => {
    window.removeEventListener('pointermove', move);
    window.removeEventListener('pointerup', up);
    dragging.value = null;
    if (!moved) selected.value = key;
    pz.markMoved();
  };
  window.addEventListener('pointermove', move);
  window.addEventListener('pointerup', up);
  vp.value?.focus();
}
function open(t: TableSchema) {
  emit('open-table', { schema: t.schema, name: t.name });
}

// -- edges --------------------------------------------------------------------------------------------
function anchorY(key: string, col: string | undefined) {
  const p = pos[key];
  const i = col !== undefined ? views.value.get(key)?.index.get(col) : undefined;
  return i === undefined ? p.y + HEAD / 2 : p.y + HEAD + i * ROW + ROW / 2;
}
interface Edge { r: Rel; d: string; many: 'many' | 'many-opt' }
const edges = computed<Edge[]>(() => {
  const nodes = model.value.nodes;
  const out: Edge[] = [];
  for (const r of rels.value) {
    const a = pos[r.from];
    const b = pos[r.to];
    if (!a || !b) continue;
    const wa = nodes.get(r.from)!.w;
    const wb = nodes.get(r.to)!.w;
    const ya = anchorY(r.from, r.fk.columns[0]);
    const yb = anchorY(r.to, r.fk.ref_columns[0]);
    let xa: number, xb: number, da: number, db: number;
    if (r.self) { xa = xb = a.x + wa; da = db = 1; }
    else if (a.x >= b.x + wb + 2 * STUB) { xa = a.x; da = -1; xb = b.x + wb; db = 1; }
    else if (a.x + wa + 2 * STUB <= b.x) { xa = a.x + wa; da = 1; xb = b.x; db = -1; }
    else { xa = a.x + wa; xb = b.x + wb; da = db = 1; }
    const sa = xa + da * STUB;
    const sb = xb + db * STUB;
    let d: string;
    const route = routes.get(r.id);
    const unmoved = (k: string) => laidAt.get(k)?.x === pos[k].x && laidAt.get(k)?.y === pos[k].y;
    if (route && da === -1 && db === 1 && unmoved(r.from) && unmoved(r.to)) {
      // Through dagre's bend points, with horizontal tangents at each one.
      const pts = [{ x: sa, y: ya }, ...route, { x: sb, y: yb }];
      d = `M ${xa} ${ya} H ${sa}`;
      for (let i = 1; i < pts.length; i++) {
        const p = pts[i - 1];
        const q = pts[i];
        const mx = (p.x + q.x) / 2;
        d += ` C ${mx} ${p.y} ${mx} ${q.y} ${q.x} ${q.y}`;
      }
      d += ` H ${xb}`;
    } else if (da !== db) {
      const c = Math.max(30, Math.abs(sb - sa) / 2);
      d = `M ${xa} ${ya} H ${sa} C ${sa + da * c} ${ya} ${sb + db * c} ${yb} ${sb} ${yb} H ${xb}`;
    } else {
      // Same side (stacked tables, self references): loop out to the right.
      const cx = Math.max(sa, sb) + 28 + Math.min(60, Math.abs(yb - ya) * 0.15);
      d = `M ${xa} ${ya} H ${sa} C ${cx} ${ya} ${cx} ${yb} ${sb} ${yb} H ${xb}`;
    }
    const nullable = r.fk.columns.some((c) => nodes.get(r.from)!.t.columns.find((x) => x.name === c)?.nullable ?? true);
    out.push({ r, d, many: nullable ? 'many-opt' : 'many' });
  }
  return out;
});
const hovered = computed(() => rels.value.find((r) => r.id === hoverRel.value) ?? null);
function onEdgeMove(e: PointerEvent, id: string) {
  const r = vp.value!.getBoundingClientRect();
  tip.x = e.clientX - r.left;
  tip.y = e.clientY - r.top;
  hoverRel.value = id;
}
function relText(r: Rel) {
  const from = model.value.nodes.get(r.from)!.t;
  const to = model.value.nodes.get(r.to)!.t;
  return `${from.name}(${r.fk.columns.join(', ')}) → ${to.name}(${r.fk.ref_columns.join(', ')})`;
}

// -- side panel data ------------------------------------------------------------------------------------
const outRels = computed(() => (sel.value ? model.value.rels.filter((r) => r.from === sel.value!.key) : []));
const inRels = computed(() => (sel.value ? model.value.rels.filter((r) => r.to === sel.value!.key && !r.self) : []));
/** FKs whose target isn't in the schema (another database, or unresolved). */
const outExternal = computed(() => {
  const t = sel.value?.t;
  if (!t) return [];
  const known = new Set(outRels.value.map((r) => r.fk));
  return t.foreign_keys.filter((f) => !known.has(f));
});
function tableLabel(key: string) {
  const t = model.value.nodes.get(key)?.t;
  return t ? qualified(t) : '';
}
function goTo(key: string) {
  const t = model.value.nodes.get(key)?.t;
  if (!visibleKeys.value.has(key) && t) schemaFilter.value = [...schemaFilter.value, t.schema ?? ''];
  nextTick(() => focusTable(key));
}

// -- export ---------------------------------------------------------------------------------------------
const esc = (s: string) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
const GLYPH = {
  key: (c: string) => `<g fill="none" stroke="${c}" stroke-width="1.4" stroke-linecap="round"><circle cx="3.6" cy="6" r="2.4"/><path d="M6 6 H11 M9 6 V8.4 M11 6 V8"/></g>`,
  link: (c: string) => `<g fill="none" stroke="${c}" stroke-width="1.3"><rect x="0.8" y="3.8" width="6" height="4.4" rx="2.2"/><rect x="5.2" y="3.8" width="6" height="4.4" rx="2.2"/></g>`,
  nn: (c: string) => `<path d="M6 2.8 L9.2 6 L6 9.2 L2.8 6 Z" fill="${c}"/>`,
  null: (c: string) => `<path d="M6 3.2 L8.8 6 L6 8.8 L3.2 6 Z" fill="none" stroke="${c}" stroke-width="1.1"/>`,
};
const markerDefs = (id: string, color: string, bg: string) => `
  <marker id="${id}-many" viewBox="-22 -8 24 16" refX="0" refY="0" markerWidth="24" markerHeight="16" markerUnits="userSpaceOnUse" orient="auto-start-reverse">
    <path d="M -12 0 L 0 -6 M -12 0 L 0 0 M -12 0 L 0 6 M -16 -6 L -16 6" fill="none" stroke="${color}" stroke-width="1.3"/>
  </marker>
  <marker id="${id}-many-opt" viewBox="-22 -8 24 16" refX="0" refY="0" markerWidth="24" markerHeight="16" markerUnits="userSpaceOnUse" orient="auto-start-reverse">
    <path d="M -12 0 L 0 -6 M -12 0 L 0 0 M -12 0 L 0 6" fill="none" stroke="${color}" stroke-width="1.3"/>
    <circle cx="-16.5" cy="0" r="3.2" fill="${bg}" stroke="${color}" stroke-width="1.3"/>
  </marker>
  <marker id="${id}-one" viewBox="-22 -8 24 16" refX="0" refY="0" markerWidth="24" markerHeight="16" markerUnits="userSpaceOnUse" orient="auto-start-reverse">
    <path d="M -6 -6 L -6 6 M -10 -6 L -10 6" fill="none" stroke="${color}" stroke-width="1.3"/>
  </marker>`;
const EDGE = '#666a72';
const EDGE_HOT = '#3794ff';
const CANVAS = '#1b1c1f';

/** A standalone SVG of the diagram as it is now (visible tables, current positions). */
function buildSvg(): string {
  const b = bounds.value;
  const M = 32;
  const W = Math.ceil(b.width + M * 2);
  const H = Math.ceil(b.height + M * 2);
  const o = [`<svg xmlns="http://www.w3.org/2000/svg" width="${W}" height="${H}" viewBox="${b.x - M} ${b.y - M} ${W} ${H}" font-family='${SANS}'>`,
    `<defs>${markerDefs('x', EDGE, '#232428')}</defs>`,
    `<rect x="${b.x - M}" y="${b.y - M}" width="${W}" height="${H}" fill="${CANVAS}"/>`];
  if (props.title) o.push(`<text x="${b.x}" y="${b.y - 12}" font-size="12" fill="#8b9098">${esc(props.title)}</text>`);
  for (const e of edges.value) o.push(`<path d="${e.d}" fill="none" stroke="${EDGE}" stroke-width="1.4" marker-start="url(#x-${e.many})" marker-end="url(#x-one)"/>`);
  for (const n of visible.value) {
    const p = pos[n.key];
    const v = views.value.get(n.key)!;
    const w = n.w;
    o.push(`<g transform="translate(${p.x} ${p.y})">`);
    o.push(`<rect width="${w}" height="${v.h}" rx="8" fill="#232428" stroke="#34363c"/>`);
    o.push(`<path d="M0 8 A8 8 0 0 1 8 0 H${w - 8} A8 8 0 0 1 ${w} 8 V${HEAD} H0 Z" fill="#2a2c31"/>`);
    if (n.color) o.push(`<path d="M0 8 A8 8 0 0 1 8 0 H${w - 8} A8 8 0 0 1 ${w} 8 V3 H0 Z" fill="${n.color}" opacity="0.9"/>`);
    o.push(`<line x1="0" y1="${HEAD}" x2="${w}" y2="${HEAD}" stroke="#34363c"/>`);
    const schemaTxt = n.t.schema ? `${n.t.schema}.` : '';
    const nameMax = w - 24 - (schemaTxt ? textW(schemaTxt, F_SCHEMA) : 0);
    o.push(`<text x="12" y="${HEAD / 2 + 4.5}" font-size="12.5">${schemaTxt ? `<tspan fill="#8b9098">${esc(schemaTxt)}</tspan>` : ''}<tspan fill="#eceef1" font-weight="600">${esc(fitText(n.t.name, F_HEAD, nameMax))}</tspan></text>`);
    v.rows.forEach((c, i) => {
      const y = HEAD + i * ROW;
      const typeW = Math.min(textW(c.c.data_type, F_TYPE), w * 0.45);
      o.push(`<g transform="translate(10 ${y + ROW / 2 - 6})">${c.pk ? GLYPH.key('#e2c08d') : ''}${c.fk ? `<g transform="translate(${c.pk ? 13 : 0} 0)">${GLYPH.link('#75beff')}</g>` : ''}${!c.pk && !c.fk ? (c.c.nullable ? GLYPH.null('#6f7379') : GLYPH.nn('#9da2aa')) : ''}</g>`);
      o.push(`<text x="38" y="${y + ROW / 2 + 4}" font-size="12" fill="${c.pk ? '#eceef1' : '#cccccc'}"${c.pk ? ' font-weight="600"' : ''}>${esc(fitText(c.c.name, F_COL, w - 38 - typeW - 22))}</text>`);
      o.push(`<text x="${w - 10}" y="${y + ROW / 2 + 4}" font-size="11" font-family='${MONO}' fill="#7f848c" text-anchor="end">${esc(fitText(c.c.data_type, F_TYPE, w * 0.45))}</text>`);
    });
    if (!n.cols.length) o.push(`<text x="12" y="${HEAD + ROW / 2 + 4}" font-size="11.5" fill="#6f7379" font-style="italic">${esc(t('diagram:card.noColumns'))}</text>`);
    if (v.hidden) o.push(`<text x="12" y="${HEAD + v.rows.length * ROW + FOOT / 2 + 4}" font-size="11" fill="#8b9098">${esc(t('diagram:card.moreColumns', { count: v.hidden }))}</text>`);
    o.push('</g>');
  }
  o.push('</svg>');
  return o.join('\n');
}

/** PNG (data URL) rendered from the SVG; 2× unless the diagram is huge. */
async function exportPng(): Promise<string> {
  const svg = buildSvg();
  const b = bounds.value;
  const w = Math.ceil(b.width + 64);
  const h = Math.ceil(b.height + 64);
  const img = new Image();
  img.src = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svg)}`;
  await img.decode();
  const scale = Math.max(0.25, Math.min(2, Math.sqrt(16_000_000 / (w * h)), 16_000 / w, 16_000 / h));
  const canvas = document.createElement('canvas');
  canvas.width = Math.round(w * scale);
  canvas.height = Math.round(h * scale);
  const ctx = canvas.getContext('2d')!;
  ctx.scale(scale, scale);
  ctx.drawImage(img, 0, 0, w, h);
  return canvas.toDataURL('image/png');
}

async function onExport(kind: 'svg' | 'png') {
  try {
    if (kind === 'svg') emit('export-svg', buildSvg());
    else emit('export-png', await exportPng());
  } catch (err) {
    ElMessage.error(t('diagram:exportFailed', { error: err instanceof Error ? err.message : String(err) }));
  }
}

defineExpose({ exportSvg: buildSvg, exportPng, fit, relayout: relayoutAndFit, focusTable: (schema: string | null, name: string) => focusTable(keyOf(schema, name)) });

const fmtRules = (f: ForeignKeyDef) =>
  [f.on_delete ? `ON DELETE ${f.on_delete}` : '', f.on_update ? `ON UPDATE ${f.on_update}` : ''].filter(Boolean).join(' · ');
</script>

<template>
  <div class="dd">
    <!-- toolbar -->
    <div class="dd-top">
      <strong v-if="title" class="dd-title">{{ title }}</strong>
      <span class="dd-metric"><em>{{ $t('diagram:toolbar.tables') }}</em>{{ visible.length }}</span>
      <span class="dd-metric"><em>{{ $t('diagram:toolbar.relations') }}</em>{{ rels.length }}</span>
      <span v-if="layoutFailed" class="dd-metric" style="color: var(--nm-warning)" :title="$t('diagram:toolbar.simplifiedLayoutTitle')">
        {{ $t('diagram:toolbar.simplifiedLayout') }}
      </span>
      <div class="nm-spacer" />
      <el-input
        v-model="query"
        class="dd-search"
        size="small"
        :placeholder="$t('diagram:toolbar.searchPlaceholder')"
        clearable
        @keydown.enter.prevent="nextMatch(($event as KeyboardEvent).shiftKey)"
        @keydown.esc="query = ''"
      >
        <template #prefix><el-icon><ei-search /></el-icon></template>
        <template #suffix>
          <span v-if="query.trim()" class="dd-count">{{ matches.length ? `${matchIdx + 1}/${matches.length}` : '0' }}</span>
        </template>
      </el-input>
      <el-select
        v-if="schemas.length > 1"
        v-model="schemaFilter"
        multiple
        collapse-tags
        collapse-tags-tooltip
        clearable
        :placeholder="$t('diagram:toolbar.allSchemas')"
        size="small"
        class="dd-schema"
      >
        <el-option v-for="s in schemas" :key="s" :label="s" :value="s" />
      </el-select>
      <div class="dd-seg" role="group">
        <button :class="{ on: !keysOnly }" @click="keysOnly = false">{{ $t('diagram:toolbar.allColumns') }}</button>
        <button :class="{ on: keysOnly }" @click="keysOnly = true">{{ $t('diagram:toolbar.keysOnly') }}</button>
      </div>
      <el-button size="small" :title="$t('diagram:toolbar.relayoutTitle')" @click="relayoutAndFit">
        <el-icon><ei-refresh /></el-icon>&nbsp;{{ $t('diagram:toolbar.relayout') }}
      </el-button>
      <el-dropdown trigger="click" @command="onExport">
        <el-button size="small"><el-icon><ei-download /></el-icon>&nbsp;{{ $t('common:export') }}</el-button>
        <template #dropdown>
          <el-dropdown-menu>
            <el-dropdown-item command="svg">{{ $t('diagram:toolbar.svgImage') }}</el-dropdown-item>
            <el-dropdown-item command="png">{{ $t('diagram:toolbar.pngImage') }}</el-dropdown-item>
          </el-dropdown-menu>
        </template>
      </el-dropdown>
    </div>

    <div class="dd-body">
      <div
        ref="vp"
        class="dd-viewport"
        :class="{ panning, dragging: !!dragging }"
        tabindex="0"
        @wheel="onWheel"
        @pointerdown="onPointerDown"
        @keydown="onKey"
        @click.self="selected = null"
      >
        <div v-if="busy" class="dd-busy">
          <el-icon class="is-loading" :size="22"><ei-loading /></el-icon>
          <span>{{ $t('diagram:busy', { tables: visible.length, relations: rels.length }) }}</span>
        </div>
        <div v-else class="dd-world" :class="{ lod }" :style="{ transform: `translate(${view.x}px, ${view.y}px) scale(${view.k})` }">
          <svg
            class="dd-edges"
            :style="{ left: bounds.x - 100 + 'px', top: bounds.y - 100 + 'px' }"
            :width="bounds.width + 200"
            :height="bounds.height + 200"
            :viewBox="`${bounds.x - 100} ${bounds.y - 100} ${bounds.width + 200} ${bounds.height + 200}`"
          >
            <defs v-html="markerDefs('dd', EDGE, '#1b1c1f') + markerDefs('ddh', EDGE_HOT, '#1b1c1f')" />
            <g
              v-for="e in edges"
              :key="e.r.id"
              v-memo="[e.d, e.many, hotRels.has(e.r.id), hotRels.size > 0]"
              class="dd-edge"
              :class="{ hot: hotRels.has(e.r.id), dim: hotRels.size > 0 && !hotRels.has(e.r.id) }"
            >
              <path
                :d="e.d"
                class="dd-edge-line"
                :marker-start="`url(#${hotRels.has(e.r.id) ? 'ddh' : 'dd'}-${e.many})`"
                :marker-end="`url(#${hotRels.has(e.r.id) ? 'ddh' : 'dd'}-one)`"
              />
              <path
                :d="e.d"
                class="dd-edge-hit"
                @pointermove="onEdgeMove($event, e.r.id)"
                @pointerleave="hoverRel = null"
              />
            </g>
          </svg>

          <div
            v-for="n in visible"
            :key="n.key"
            v-memo="[pos[n.key]?.x, pos[n.key]?.y, views.get(n.key), cardState(n), language]"
            class="dd-card"
            :class="{
              sel: selected === n.key,
              dim: neighbors && !neighbors.has(n.key),
              match: matchSet.has(n.key),
              current: matches[matchIdx] === n.key,
            }"
            :style="{ left: pos[n.key]?.x + 'px', top: pos[n.key]?.y + 'px', width: n.w + 'px', height: views.get(n.key)!.h + 'px' }"
            @pointerdown.stop="onCardDown($event, n.key)"
            @dblclick.stop="open(n.t)"
          >
            <div class="dd-head" :style="n.color ? { boxShadow: `inset 0 3px 0 ${n.color}` } : undefined" :title="`${qualified(n.t)}${n.t.comment ? '\n' + n.t.comment : ''}\n${$t('diagram:card.openHint')}`">
              <span class="dd-name"><span v-if="n.t.schema" class="dd-sch">{{ n.t.schema }}.</span>{{ n.t.name }}</span>
              <span v-if="n.t.kind && n.t.kind !== 'table'" class="dd-kind">{{ n.t.kind }}</span>
            </div>
            <div
              v-for="c in views.get(n.key)!.rows"
              :key="c.c.name"
              class="dd-row"
              :class="{ pk: c.pk, hot: hotCols.get(n.key)?.has(c.c.name) }"
              @pointerenter="isKey(c) && (hoverCol = colKey(n.key, c.c.name))"
              @pointerleave="hoverCol = null"
            >
              <span class="dd-ic">
                <svg v-if="c.pk" viewBox="0 0 12 12" width="12" height="12"><title>{{ $t('diagram:primaryKey') }}</title><g fill="none" stroke="#e2c08d" stroke-width="1.4" stroke-linecap="round"><circle cx="3.6" cy="6" r="2.4" /><path d="M6 6 H11 M9 6 V8.4 M11 6 V8" /></g></svg>
                <svg v-if="c.fk" viewBox="0 0 12 12" width="12" height="12"><title>{{ $t('diagram:foreignKey') }}</title><g fill="none" stroke="#75beff" stroke-width="1.3"><rect x="0.8" y="3.8" width="6" height="4.4" rx="2.2" /><rect x="5.2" y="3.8" width="6" height="4.4" rx="2.2" /></g></svg>
                <svg v-if="!c.pk && !c.fk" viewBox="0 0 12 12" width="12" height="12">
                  <title>{{ c.c.nullable ? $t('diagram:card.nullable') : 'NOT NULL' }}</title>
                  <path v-if="c.c.nullable" d="M6 3.2 L8.8 6 L6 8.8 L3.2 6 Z" fill="none" stroke="#6f7379" stroke-width="1.1" />
                  <path v-else d="M6 2.8 L9.2 6 L6 9.2 L2.8 6 Z" fill="#9da2aa" />
                </svg>
              </span>
              <span class="dd-col" :title="c.c.comment ?? c.c.name">{{ c.c.name }}</span>
              <span class="dd-type" :title="c.c.data_type">{{ c.c.data_type }}</span>
            </div>
            <div v-if="!n.cols.length" class="dd-row dd-nocols">{{ $t('diagram:card.noColumns') }}</div>
            <button
              v-if="views.get(n.key)!.foot"
              class="dd-foot"
              @pointerdown.stop
              @click.stop="toggleExpand(n.key)"
            >{{ views.get(n.key)!.hidden ? $t('diagram:card.moreColumns', { count: views.get(n.key)!.hidden }) : $t('diagram:card.showLess') }}</button>
          </div>
        </div>

        <div v-if="!schema.length" class="dd-empty">
          <el-icon class="dd-empty-ic"><ei-grid /></el-icon>
          <strong>{{ $t('diagram:empty.title') }}</strong>
          <span>{{ $t('diagram:empty.text') }}</span>
        </div>
        <div v-else-if="!model.rels.length" class="dd-banner">
          <el-icon><ei-info-filled /></el-icon>
          {{ $t('diagram:empty.noRelations') }}
        </div>

        <!-- relation tooltip -->
        <div v-if="hovered" class="dd-tip" :style="{ left: tip.x + 14 + 'px', top: tip.y + 14 + 'px' }">
          <strong>{{ hovered.fk.name ?? $t('diagram:unnamedForeignKey') }}</strong>
          <code>{{ relText(hovered) }}</code>
          <span v-if="fmtRules(hovered.fk)">{{ fmtRules(hovered.fk) }}</span>
        </div>

        <!-- zoom controls -->
        <div v-if="schema.length" class="dd-zoom" @pointerdown.stop>
          <button :title="$t('diagram:zoom.zoomOut')" @click="zoomBy(1 / 1.2)"><el-icon><ei-minus /></el-icon></button>
          <button class="dd-zoom-pct" :title="$t('diagram:zoom.actualSize')" @click="actualSize">{{ Math.round(view.k * 100) }}%</button>
          <button :title="$t('diagram:zoom.zoomIn')" @click="zoomBy(1.2)"><el-icon><ei-plus /></el-icon></button>
          <span class="dd-zoom-sep" />
          <button :title="$t('diagram:zoom.fit')" @click="fit"><el-icon><ei-full-screen /></el-icon></button>
          <button :class="{ on: showMini }" :title="$t('diagram:zoom.minimap')" @click="showMini = !showMini"><el-icon><ei-map-location /></el-icon></button>
        </div>

        <!-- minimap -->
        <div v-if="showMini && visible.length > 3" class="dd-mini" :style="{ width: mini.w + 'px', height: mini.h + 'px' }" @pointerdown.stop="onMiniDown">
          <svg :width="mini.w" :height="mini.h">
            <rect
              v-for="n in visible"
              :key="n.key"
              :x="mini.ox + ((pos[n.key]?.x ?? 0) - mini.bx) * mini.s"
              :y="mini.oy + ((pos[n.key]?.y ?? 0) - mini.by) * mini.s"
              :width="Math.max(2, n.w * mini.s)"
              :height="Math.max(2, views.get(n.key)!.h * mini.s)"
              :class="['dd-mini-node', { sel: selected === n.key, match: matchSet.has(n.key) }]"
              rx="1"
            />
            <rect class="dd-mini-view" :x="miniView.x" :y="miniView.y" :width="miniView.w" :height="miniView.h" rx="2" />
          </svg>
        </div>

        <div v-if="schema.length" class="dd-hint">{{ $t('diagram:hint') }}</div>
      </div>

      <!-- properties -->
      <aside v-if="sel" class="dd-props nm-selectable">
        <header class="dd-props-head">
          <span class="dd-props-ic"><el-icon><ei-grid /></el-icon></span>
          <div class="dd-props-title">
            <strong>{{ sel.t.name }}</strong>
            <span>{{ [sel.t.schema, sel.t.kind !== 'table' ? sel.t.kind : ''].filter(Boolean).join(' · ') || $t('diagram:props.table') }}</span>
          </div>
          <button class="ide-icon-btn" :title="$t('diagram:props.openTable')" @click="open(sel.t)"><el-icon><ei-top-right /></el-icon></button>
          <button class="ide-icon-btn" :title="$t('diagram:props.close')" @click="selected = null"><el-icon><ei-close /></el-icon></button>
        </header>

        <section v-if="sel.t.comment" class="dd-sec"><p class="dd-comment">{{ sel.t.comment }}</p></section>

        <section class="dd-sec">
          <h4>{{ $t('diagram:props.columns') }} <em>{{ sel.cols.length }}</em></h4>
          <div v-for="c in sel.cols" :key="c.c.name" class="dd-pcol" :class="{ pk: c.pk }">
            <span class="dd-pcol-flags">
              <span v-if="c.pk" class="dd-flag pk" :title="$t('diagram:primaryKey')">PK</span>
              <span v-if="c.fk" class="dd-flag fk" :title="$t('diagram:foreignKey')">FK</span>
            </span>
            <span class="dd-pcol-name">{{ c.c.name }}</span>
            <span class="dd-pcol-type">{{ c.c.data_type }}<template v-if="!c.c.nullable"> · not null</template><template v-if="c.c.auto_increment"> · auto</template></span>
            <span v-if="c.c.default_value" class="dd-pcol-def" :title="c.c.default_value">= {{ c.c.default_value }}</span>
          </div>
        </section>

        <section v-if="sel.t.primary_key" class="dd-sec">
          <h4>{{ $t('diagram:primaryKey') }}</h4>
          <div class="dd-idx">
            <span class="dd-idx-name">{{ sel.t.primary_key.name ?? $t('diagram:props.unnamed') }}</span>
            <code>{{ sel.t.primary_key.columns.join(', ') }}</code>
          </div>
        </section>

        <section v-if="sel.t.indexes.length" class="dd-sec">
          <h4>{{ $t('diagram:props.indexes') }} <em>{{ sel.t.indexes.length }}</em></h4>
          <div v-for="ix in sel.t.indexes" :key="ix.name" class="dd-idx">
            <span class="dd-idx-name">{{ ix.name }}<span v-if="ix.unique" class="dd-flag uq">{{ $t('diagram:props.unique') }}</span><span v-if="ix.kind" class="dd-idx-kind">{{ ix.kind }}</span></span>
            <code>{{ ix.columns.join(', ') }}</code>
            <span v-if="ix.filter" class="dd-idx-filter">WHERE {{ ix.filter }}</span>
          </div>
        </section>

        <section v-if="outRels.length || outExternal.length" class="dd-sec">
          <h4>{{ $t('diagram:props.references') }} <em>{{ outRels.length + outExternal.length }}</em></h4>
          <div v-for="r in outRels" :key="r.id" class="dd-fk">
            <code>{{ r.fk.columns.join(', ') }}</code>
            <span class="dd-arrow">→</span>
            <a class="dd-link" @click="goTo(r.to)">{{ tableLabel(r.to) }}</a>
            <code class="dim">({{ r.fk.ref_columns.join(', ') }})</code>
            <span v-if="r.fk.name || fmtRules(r.fk)" class="dd-fk-meta">{{ [r.fk.name, fmtRules(r.fk)].filter(Boolean).join(' · ') }}</span>
          </div>
          <div v-for="(f, i) in outExternal" :key="'x' + i" class="dd-fk">
            <code>{{ f.columns.join(', ') }}</code>
            <span class="dd-arrow">→</span>
            <span class="dd-ext">{{ f.ref_schema ? f.ref_schema + '.' : '' }}{{ f.ref_table }}</span>
            <code class="dim">({{ f.ref_columns.join(', ') }})</code>
            <span class="dd-fk-meta">{{ $t('diagram:props.outside') }}</span>
          </div>
        </section>

        <section v-if="inRels.length" class="dd-sec">
          <h4>{{ $t('diagram:props.referencedBy') }} <em>{{ inRels.length }}</em></h4>
          <div v-for="r in inRels" :key="r.id" class="dd-fk">
            <a class="dd-link" @click="goTo(r.from)">{{ tableLabel(r.from) }}</a>
            <code class="dim">({{ r.fk.columns.join(', ') }})</code>
            <span class="dd-arrow">→</span>
            <code>{{ r.fk.ref_columns.join(', ') }}</code>
            <span v-if="r.fk.name || fmtRules(r.fk)" class="dd-fk-meta">{{ [r.fk.name, fmtRules(r.fk)].filter(Boolean).join(' · ') }}</span>
          </div>
        </section>
      </aside>
    </div>
  </div>
</template>

<style scoped>
.dd {
  --dd-card: #232428;
  --dd-head: #2a2c31;
  --dd-line: #34363c;
  --dd-edge: #666a72;
  --dd-hot: #3794ff;
  --dd-match: #e8a33d;
  display: flex; flex-direction: column; height: 100%; min-height: 0;
}

/* toolbar */
.dd-top {
  flex-shrink: 0; display: flex; align-items: center; gap: 12px; padding: 6px 12px; flex-wrap: wrap;
  border-bottom: 1px solid var(--nm-border-soft); background: var(--ide-editor);
}
.dd-title { font-size: 12.5px; color: var(--nm-text-strong); font-weight: 600; }
.dd-metric { display: inline-flex; align-items: baseline; gap: 5px; font-size: 12.5px; color: var(--nm-text-strong); font-variant-numeric: tabular-nums; }
.dd-metric em { font-style: normal; font-size: 11px; color: var(--nm-text-dim); }
.dd-search { width: 210px; }
.dd-count { font-size: 11px; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; }
.dd-schema { width: 220px; }
.dd-seg { display: inline-flex; border: 1px solid var(--nm-border); border-radius: 4px; overflow: hidden; }
.dd-seg button { padding: 3px 10px; font: inherit; font-size: 12px; border: none; background: transparent; color: var(--nm-text-dim); cursor: pointer; }
.dd-seg button.on { background: var(--ide-selection); color: var(--nm-text-strong); }
.dd-top :deep(.el-button + .el-dropdown), .dd-top :deep(.el-dropdown) { margin-left: -4px; }

/* canvas */
.dd-body { flex: 1; min-height: 0; display: flex; }
.dd-viewport {
  position: relative; flex: 1; min-width: 0; overflow: hidden; outline: none; cursor: grab;
  background-color: #1b1c1f;
  background-image: radial-gradient(circle, #2c2e33 1px, transparent 1.2px);
  background-size: 20px 20px;
}
.dd-viewport.panning { cursor: grabbing; }
.dd-viewport.dragging, .dd-viewport.dragging .dd-card { cursor: move; }
.dd-busy {
  position: absolute; inset: 0; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 10px;
  color: var(--nm-text-dim); font-size: 13px; cursor: default;
}
.dd-world { position: absolute; left: 0; top: 0; transform-origin: 0 0; will-change: transform; }
.dd-edges { position: absolute; overflow: visible; pointer-events: none; }
.dd-edge-line { fill: none; stroke: var(--dd-edge); stroke-width: 1.4; transition: stroke 0.1s; }
.dd-edge-hit { fill: none; stroke: transparent; stroke-width: 12; pointer-events: stroke; cursor: pointer; }
.dd-edge.hot .dd-edge-line { stroke: var(--dd-hot); stroke-width: 2; }
.dd-edge.dim { opacity: 0.35; }

/* table card */
.dd-card {
  position: absolute; display: flex; flex-direction: column; overflow: hidden;
  background: var(--dd-card); border: 1px solid var(--dd-line); border-radius: 8px;
  box-shadow: 0 1px 2px rgba(0, 0, 0, 0.35), 0 4px 14px rgba(0, 0, 0, 0.22);
  cursor: pointer; transition: border-color 0.12s, box-shadow 0.12s, opacity 0.12s;
}
.dd-card:hover { border-color: #4a4d55; }
.dd-card.match { border-color: rgba(232, 163, 61, 0.7); }
.dd-card.current { box-shadow: 0 0 0 2px rgba(232, 163, 61, 0.55), 0 4px 14px rgba(0, 0, 0, 0.3); }
.dd-card.sel { border-color: var(--ide-focus); box-shadow: 0 0 0 2px rgba(0, 127, 212, 0.45), 0 4px 14px rgba(0, 0, 0, 0.3); }
.dd-card.dim { opacity: 0.4; }
.dd-head {
  flex-shrink: 0; display: flex; align-items: center; gap: 6px; height: 34px; padding: 0 12px;
  background: var(--dd-head); border-bottom: 1px solid var(--dd-line);
}
.dd-name { flex: 1; min-width: 0; font-size: 12.5px; font-weight: 600; color: #eceef1; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.dd-sch { font-weight: 400; color: #8b9098; }
.dd-kind { flex-shrink: 0; font-size: 10px; padding: 0 6px; line-height: 16px; border-radius: 8px; background: #33363c; color: var(--nm-text-dim); }
.dd-row {
  flex-shrink: 0; display: flex; align-items: center; gap: 0; height: 22px; padding: 0 10px; font-size: 12px; color: var(--nm-text);
}
.dd-row.pk .dd-col { color: #eceef1; font-weight: 600; }
.dd-row.hot { background: rgba(55, 148, 255, 0.16); }
.dd-row.hot .dd-col { color: #fff; }
.dd-ic { width: 28px; flex-shrink: 0; display: inline-flex; align-items: center; gap: 1px; }
.dd-col { flex: 1 1 auto; min-width: 24px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.dd-type { flex: 0 0 auto; max-width: 62%; margin-left: 12px; font-family: var(--nm-mono); font-size: 11px; color: #7f848c; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.dd-nocols { color: #6f7379; font-style: italic; font-size: 11.5px; }
.dd-foot {
  flex-shrink: 0; height: 24px; margin: 0; padding: 0 12px; text-align: left; font: inherit; font-size: 11px;
  color: #8b9098; background: transparent; border: none; border-top: 1px dashed var(--dd-line); cursor: pointer;
}
.dd-foot:hover { color: #75beff; }
.dd-world.lod .dd-row > *, .dd-world.lod .dd-foot { visibility: hidden; }

/* overlays */
.dd-empty {
  position: absolute; inset: 0; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 6px;
  color: var(--nm-text-dim); font-size: 12.5px; pointer-events: none;
}
.dd-empty strong { color: var(--nm-text-strong); font-size: 13.5px; font-weight: 600; }
.dd-empty-ic { font-size: 34px; color: #4a4d55; margin-bottom: 6px; }
.dd-banner {
  position: absolute; left: 50%; top: 12px; transform: translateX(-50%); display: flex; align-items: center; gap: 8px;
  max-width: calc(100% - 40px); padding: 6px 12px; font-size: 12px; color: var(--nm-text);
  background: rgba(37, 37, 38, 0.94); border: 1px solid #3c3c3c; border-left: 3px solid var(--nm-info); border-radius: 6px;
  box-shadow: 0 4px 14px rgba(0, 0, 0, 0.35); cursor: default;
}
.dd-banner .el-icon { color: var(--nm-info); flex-shrink: 0; }
.dd-tip {
  position: absolute; z-index: 5; display: flex; flex-direction: column; gap: 3px; max-width: 420px; padding: 7px 10px;
  font-size: 12px; color: var(--nm-text); pointer-events: none;
  background: #252526; border: 1px solid #454545; border-radius: 6px; box-shadow: 0 4px 14px rgba(0, 0, 0, 0.4);
}
.dd-tip strong { color: var(--nm-text-strong); font-weight: 600; }
.dd-tip code { font-size: 11px; color: #9cdcfe; word-break: break-all; }
.dd-tip span { font-size: 11px; color: var(--nm-text-dim); }

/* floating controls (same as the plan view) */
.dd-zoom {
  position: absolute; left: 12px; bottom: 12px; display: flex; align-items: center; gap: 1px; padding: 3px;
  background: rgba(37, 37, 38, 0.92); border: 1px solid #3c3c3c; border-radius: 8px; box-shadow: 0 4px 14px rgba(0, 0, 0, 0.35);
}
.dd-zoom button {
  display: inline-flex; align-items: center; justify-content: center; height: 26px; min-width: 26px; padding: 0 6px;
  border: none; border-radius: 5px; background: transparent; color: var(--nm-text); cursor: pointer; font: inherit; font-size: 12px;
}
.dd-zoom button:hover { background: rgba(255, 255, 255, 0.08); }
.dd-zoom button.on { color: #75beff; }
.dd-zoom-pct { min-width: 48px !important; font-variant-numeric: tabular-nums; }
.dd-zoom-sep { width: 1px; height: 16px; background: #3c3c3c; margin: 0 3px; }
.dd-mini {
  position: absolute; right: 12px; bottom: 12px; background: rgba(30, 31, 34, 0.94);
  border: 1px solid #3c3c3c; border-radius: 8px; box-shadow: 0 4px 14px rgba(0, 0, 0, 0.35); cursor: crosshair; overflow: hidden;
}
.dd-mini-node { fill: #3a3d44; }
.dd-mini-node.match { fill: #a8772c; }
.dd-mini-node.sel { fill: #3794ff; }
.dd-mini-view { fill: rgba(0, 127, 212, 0.12); stroke: #3794ff; stroke-width: 1; }
.dd-hint { position: absolute; left: 50%; bottom: 14px; transform: translateX(-50%); font-size: 11px; color: #6c7078; pointer-events: none; white-space: nowrap; }

/* properties panel */
.dd-props { width: 330px; flex-shrink: 0; border-left: 1px solid var(--nm-border); overflow: auto; background: var(--ide-sidebar); }
.dd-props-head { display: flex; align-items: center; gap: 8px; padding: 12px 10px 12px 14px; position: sticky; top: 0; background: var(--ide-sidebar); border-bottom: 1px solid var(--nm-border-soft); z-index: 1; }
.dd-props-ic { flex-shrink: 0; display: inline-flex; align-items: center; justify-content: center; width: 32px; height: 32px; border-radius: 6px; font-size: 17px; background: rgba(55, 148, 255, 0.16); color: #75beff; }
.dd-props-title { flex: 1; min-width: 0; display: flex; flex-direction: column; }
.dd-props-title strong { font-size: 13.5px; color: var(--nm-text-strong); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.dd-props-title span { font-size: 11.5px; color: var(--nm-text-dim); }
.dd-sec { padding: 10px 14px; border-bottom: 1px solid var(--nm-border-soft); }
.dd-sec h4 { margin: 0 0 8px; font-size: 10.5px; font-weight: 600; letter-spacing: 0.07em; text-transform: uppercase; color: var(--nm-text-dim); }
.dd-sec h4 em { font-style: normal; font-weight: 400; margin-left: 4px; color: var(--nm-text-muted); }
.dd-comment { margin: 0; font-size: 12px; line-height: 1.45; color: var(--nm-text); }
.dd-pcol { display: grid; grid-template-columns: 34px 1fr; column-gap: 6px; padding: 3px 0; font-size: 12px; }
.dd-pcol-flags { grid-row: span 2; display: flex; flex-direction: column; gap: 2px; padding-top: 1px; }
.dd-pcol-name { color: var(--nm-text-strong); word-break: break-all; }
.dd-pcol.pk .dd-pcol-name { font-weight: 600; }
.dd-pcol-type { font-family: var(--nm-mono); font-size: 11px; color: #7f848c; }
.dd-pcol-def { grid-column: 2; font-family: var(--nm-mono); font-size: 11px; color: #ce9178; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.dd-flag { display: inline-block; font-size: 9.5px; font-weight: 700; line-height: 14px; padding: 0 4px; border-radius: 3px; text-align: center; }
.dd-flag.pk { background: rgba(226, 192, 141, 0.16); color: #e2c08d; }
.dd-flag.fk { background: rgba(117, 190, 255, 0.14); color: #75beff; }
.dd-flag.uq { margin-left: 6px; background: rgba(78, 201, 176, 0.14); color: #4ec9b0; font-weight: 600; }
.dd-idx { display: flex; flex-direction: column; gap: 1px; padding: 3px 0; font-size: 12px; }
.dd-idx-name { color: var(--nm-text-strong); word-break: break-all; }
.dd-idx-kind { margin-left: 6px; font-size: 10.5px; color: var(--nm-text-dim); }
.dd-idx code, .dd-fk code { font-size: 11px; color: #9cdcfe; }
.dd-idx-filter { font-family: var(--nm-mono); font-size: 11px; color: var(--nm-text-dim); }
.dd-fk { display: flex; flex-wrap: wrap; align-items: baseline; gap: 4px 6px; padding: 4px 0; font-size: 12px; }
.dd-fk code.dim { color: var(--nm-text-dim); }
.dd-arrow { color: var(--nm-text-muted); }
.dd-link { color: #75beff; cursor: pointer; }
.dd-link:hover { text-decoration: underline; }
.dd-ext { color: var(--nm-text); }
.dd-fk-meta { flex-basis: 100%; font-size: 11px; color: var(--nm-text-dim); }
</style>
