<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import type { Plan, PlanNode } from '../api/types';
import CodeEditor from './CodeEditor.vue';
import { saveTextFile } from '../composables/files';
import { useTranslation } from 'i18next-vue';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';

// Execution plan as an SSMS-style tree on a pannable, zoomable canvas:
// the statement on the left, the operators feeding it to the right, each
// parent centered on its children, arrows as thick as the rows through them
// and the costliest path highlighted. Drag to pan; pinch or ⌘/Ctrl + wheel
// to zoom around the pointer; F fits, 0 resets. Subtrees collapse. One
// statement at a time (tabs above when the batch has several).

const props = defineProps<{ plans: Plan[] }>();
const { t } = useTranslation();

// -- geometry ---------------------------------------------------------------------------
const W = 212;
const H = 104;
const GAP_X = 104;
const GAP_Y = 26;
const PAD = 40;

interface Laid {
  node: PlanNode;
  key: string;
  x: number;
  y: number;
  parent: Laid | null;
  pct: number | null;
  hidden: number;
  hot: boolean;
}

const active = ref(0);
const collapsed = reactive<Record<number, Set<string>>>({});
const plan = computed(() => props.plans[active.value] ?? props.plans[0]);

function selfCost(n: PlanNode): number | null {
  const raw = n.self_cost ?? (n.total_cost !== null
    ? n.total_cost - n.children.reduce((a, c) => a + (c.total_cost ?? 0), 0)
    : null);
  // Clamped: per-loop inner costs (PostgreSQL, MySQL) can make it negative.
  return raw === null ? null : Math.max(0, raw);
}

function countBelow(n: PlanNode): number {
  return n.children.reduce((a, c) => a + 1 + countBelow(c), 0);
}

/** The costliest root-to-leaf path (by subtree cost, else time, else rows). */
function hotKeys(root: PlanNode): Set<string> {
  const weight = (n: PlanNode) => n.total_cost ?? n.actual_ms ?? n.actual_rows ?? n.est_rows ?? 0;
  const keys = new Set<string>(['0']);
  let n = root;
  let key = '0';
  while (n.children.length) {
    let best = 0;
    n.children.forEach((c, i) => { if (weight(c) > weight(n.children[best])) best = i; });
    key = `${key}.${best}`;
    keys.add(key);
    n = n.children[best];
  }
  return keys;
}

const layout = computed(() => {
  const p = plan.value;
  if (!p) return { nodes: [] as Laid[], width: 0, height: 0 };
  const shut = collapsed[active.value] ?? new Set<string>();
  const total = p.root.total_cost ?? null;
  const hot = hotKeys(p.root);
  const nodes: Laid[] = [];
  let nextY = 0;
  const place = (node: PlanNode, depth: number, parent: Laid | null, key: string): Laid => {
    const self = selfCost(node);
    const me: Laid = {
      node, key, parent, x: depth * (W + GAP_X), y: 0,
      pct: total && total > 0 && self !== null ? (self / total) * 100 : null,
      hidden: shut.has(key) ? countBelow(node) : 0,
      hot: hot.has(key),
    };
    nodes.push(me);
    const kids = shut.has(key) ? [] : node.children;
    if (!kids.length) {
      me.y = nextY;
      nextY += H + GAP_Y;
    } else {
      const placed = kids.map((c, i) => place(c, depth + 1, me, `${key}.${i}`));
      me.y = (placed[0].y + placed[placed.length - 1].y) / 2;
    }
    return me;
  };
  place(p.root, 0, null, '0');
  return {
    nodes,
    width: Math.max(...nodes.map((n) => n.x)) + W,
    height: Math.max(...nodes.map((n) => n.y)) + H,
  };
});

function toggle(key: string) {
  const set = collapsed[active.value] ?? (collapsed[active.value] = new Set());
  if (set.has(key)) set.delete(key);
  else set.add(key);
}
function expandAll() { collapsed[active.value] = new Set(); }
function collapseBelow(depth: number) {
  const set = new Set<string>();
  const walk = (n: PlanNode, key: string, d: number) => {
    if (d >= depth && n.children.length) { set.add(key); return; }
    n.children.forEach((c, i) => walk(c, `${key}.${i}`, d + 1));
  };
  if (plan.value) walk(plan.value.root, '0', 0);
  collapsed[active.value] = set;
}

// -- look ---------------------------------------------------------------------------------
const rowsThrough = (n: PlanNode) => n.actual_rows ?? (n.est_rows !== null ? n.est_rows * Math.max(1, n.executions ?? 1) : 0);
const maxRows = computed(() => Math.max(1, ...layout.value.nodes.map((n) => rowsThrough(n.node))));
function stroke(n: PlanNode) {
  return 1.25 + 7 * (Math.log10(rowsThrough(n) + 1) / Math.log10(maxRows.value + 1));
}
/** Rounded elbow from the child's left edge to the parent's right edge. */
function edgePath(c: Laid) {
  const p = c.parent!;
  const x1 = c.x;
  const y1 = c.y + H / 2;
  const x2 = p.x + W + 7;
  const y2 = p.y + H / 2;
  const mid = x2 + (x1 - x2) / 2;
  if (Math.abs(y1 - y2) < 1) return `M ${x1} ${y1} H ${x2}`;
  const r = Math.min(10, Math.abs(y1 - y2) / 2, (x1 - x2) / 2);
  const s = y1 > y2 ? -1 : 1;
  return `M ${x1} ${y1} H ${mid + r} Q ${mid} ${y1} ${mid} ${y1 + s * r} V ${y2 - s * r} Q ${mid} ${y2} ${mid - r} ${y2} H ${x2}`;
}
function severity(pct: number | null) {
  if (pct === null) return 'none';
  if (pct >= 50) return 'high';
  if (pct >= 20) return 'mid';
  return 'low';
}
function category(op: string): { icon: string; tone: string } {
  const o = op.toLowerCase();
  if (/^(select|insert|update|delete|merge|statement|query|plan|with)\b/.test(o) && !/scan|seek/.test(o)) return { icon: 'document', tone: 'root' };
  if (/insert|update|delete|write|upsert|merge/.test(o)) return { icon: 'edit', tone: 'write' };
  if (/lookup/.test(o)) return { icon: 'key', tone: 'seek' };
  if (/seek|index (only )?scan|index range|bitmap|ixscan|get ?item|key/.test(o)) return { icon: 'aim', tone: 'seek' };
  if (/scan|full|table access|collscan|search|read/.test(o)) return { icon: 'search', tone: 'scan' };
  if (/loop|hash|merge|join|lookup|nested/.test(o)) return { icon: 'connection', tone: 'join' };
  if (/sort|order|top|limit/.test(o)) return { icon: 'sort', tone: 'order' };
  if (/aggregate|group|count|distinct|window/.test(o)) return { icon: 'histogram', tone: 'order' };
  if (/filter|where|predicate/.test(o)) return { icon: 'filter', tone: 'calc' };
  if (/parallel|gather|exchange|repartition|shuffle|remote|broadcast/.test(o)) return { icon: 'share', tone: 'calc' };
  if (/spool|material|cache|buffer/.test(o)) return { icon: 'box', tone: 'calc' };
  return { icon: 'cpu', tone: 'calc' };
}
const fmt = (v: number | null | undefined, digits = 0) =>
  v === null || v === undefined ? '—' : v.toLocaleString(locale(), { maximumFractionDigits: digits });
function compact(v: number | null | undefined) {
  if (v === null || v === undefined) return '—';
  return Intl.NumberFormat(locale(), { notation: 'compact', maximumFractionDigits: 1 }).format(v);
}
function rowsLine(n: PlanNode) {
  const est = n.est_rows !== null ? n.est_rows * Math.max(1, n.executions ?? 1) : null;
  if (n.actual_rows !== null) return { text: `${compact(n.actual_rows)} / ${compact(est)}`, off: est ? Math.max(n.actual_rows / est, est / Math.max(n.actual_rows, 1)) >= 10 : false };
  return { text: est !== null ? t('plan:node.estimatedShort', { rows: compact(est) }) : '', off: false };
}

// -- viewport: pan & zoom -----------------------------------------------------------------
const vp = ref<HTMLDivElement | null>(null);
const view = reactive({ x: PAD, y: PAD, k: 1 });
const size = reactive({ w: 800, h: 400 });
const MIN_K = 0.15;
const MAX_K = 2.5;

/** Until the user pans or zooms, the plan re-fits when the canvas resizes. */
let userMoved = false;

function zoomAt(k: number, cx: number, cy: number) {
  userMoved = true;
  const nk = Math.min(MAX_K, Math.max(MIN_K, k));
  view.x = cx - (cx - view.x) * (nk / view.k);
  view.y = cy - (cy - view.y) * (nk / view.k);
  view.k = nk;
}
function zoomBy(f: number) { zoomAt(view.k * f, size.w / 2, size.h / 2); }
function fit() {
  userMoved = false;
  const { width, height } = layout.value;
  if (!width) return;
  const k = Math.min(1.1, (size.w - PAD * 2) / width, (size.h - PAD * 2) / height);
  view.k = Math.max(MIN_K, k);
  view.x = (size.w - width * view.k) / 2;
  view.y = Math.max(PAD / 2, (size.h - height * view.k) / 2);
}
function actualSize() {
  zoomAt(1, size.w / 2, size.h / 2);
}
function panBy(dx: number, dy: number) {
  userMoved = true;
  view.x += dx;
  view.y += dy;
}

function onWheel(e: WheelEvent) {
  e.preventDefault();
  if (e.ctrlKey || e.metaKey) {
    // Pinch on a trackpad arrives as ctrl + wheel.
    const r = vp.value!.getBoundingClientRect();
    zoomAt(view.k * Math.exp(-e.deltaY * 0.0025), e.clientX - r.left, e.clientY - r.top);
  } else {
    panBy(-e.deltaX, -e.deltaY);
  }
}

const panning = ref(false);
function onPointerDown(e: PointerEvent) {
  if (e.button !== 0 || (e.target as HTMLElement).closest('.pv-node, .pv-chip, .pv-mini')) return;
  panning.value = true;
  userMoved = true;
  const start = { x: e.clientX, y: e.clientY, vx: view.x, vy: view.y };
  const move = (ev: PointerEvent) => {
    view.x = start.vx + ev.clientX - start.x;
    view.y = start.vy + ev.clientY - start.y;
  };
  const up = () => {
    panning.value = false;
    window.removeEventListener('pointermove', move);
    window.removeEventListener('pointerup', up);
  };
  window.addEventListener('pointermove', move);
  window.addEventListener('pointerup', up);
}

function onKey(e: KeyboardEvent) {
  if (e.metaKey || e.ctrlKey || e.altKey) return;
  const k = e.key.toLowerCase();
  if (k === 'f') { e.preventDefault(); fit(); }
  else if (k === '0') { e.preventDefault(); actualSize(); }
  else if (k === '+' || k === '=') { e.preventDefault(); zoomBy(1.2); }
  else if (k === '-') { e.preventDefault(); zoomBy(1 / 1.2); }
  else if (k === 'escape') selected.value = null;
}

let ro: ResizeObserver | null = null;
onMounted(() => {
  ro = new ResizeObserver(() => {
    const r = vp.value?.getBoundingClientRect();
    if (r) { size.w = r.width; size.h = r.height; }
    if (!userMoved) fit();
  });
  if (vp.value) ro.observe(vp.value);
  nextTick(fit);
});
onBeforeUnmount(() => ro?.disconnect());
watch(() => [props.plans, active.value], () => { selected.value = null; nextTick(fit); });

// -- minimap ---------------------------------------------------------------------------------
const MINI_W = 184;
const MINI_H = 116;
const showMini = ref(true);
const mini = computed(() => {
  const { width, height } = layout.value;
  const s = Math.min((MINI_W - 12) / Math.max(width, 1), (MINI_H - 12) / Math.max(height, 1));
  return { s, ox: (MINI_W - width * s) / 2, oy: (MINI_H - height * s) / 2 };
});
const miniView = computed(() => {
  const m = mini.value;
  const x = m.ox + (-view.x / view.k) * m.s;
  const y = m.oy + (-view.y / view.k) * m.s;
  // Clipped to the minimap, so it reads as a frame even when zoomed out.
  const x1 = Math.max(1, x);
  const y1 = Math.max(1, y);
  const x2 = Math.min(MINI_W - 1, x + (size.w / view.k) * m.s);
  const y2 = Math.min(MINI_H - 1, y + (size.h / view.k) * m.s);
  return { x: x1, y: y1, w: Math.max(0, x2 - x1), h: Math.max(0, y2 - y1) };
});
function miniPoint(e: PointerEvent) {
  userMoved = true;
  const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
  const m = mini.value;
  const wx = (e.clientX - r.left - m.ox) / m.s;
  const wy = (e.clientY - r.top - m.oy) / m.s;
  view.x = size.w / 2 - wx * view.k;
  view.y = size.h / 2 - wy * view.k;
}
function onMiniDown(e: PointerEvent) {
  miniPoint(e);
  const el = e.currentTarget as HTMLElement;
  const move = (ev: PointerEvent) => miniPoint({ ...ev, currentTarget: el } as unknown as PointerEvent);
  const up = () => { window.removeEventListener('pointermove', move); window.removeEventListener('pointerup', up); };
  window.addEventListener('pointermove', move);
  window.addEventListener('pointerup', up);
}

// -- selection / properties ------------------------------------------------------------------
const selected = ref<string | null>(null);
const sel = computed(() => layout.value.nodes.find((n) => n.key === selected.value) ?? null);
function select(n: Laid) {
  selected.value = n.key;
  vp.value?.focus();
}

const summary = computed(() => {
  const p = plan.value;
  if (!p) return null;
  const all: PlanNode[] = [];
  const walk = (n: PlanNode) => { all.push(n); n.children.forEach(walk); };
  walk(p.root);
  return {
    operators: all.length,
    warnings: all.reduce((a, n) => a + n.warnings.length, 0),
    cost: p.root.total_cost,
    ms: p.root.actual_ms ?? p.root.children[0]?.actual_ms ?? null,
    rows: p.root.actual_rows ?? p.root.est_rows,
  };
});
function firstWarning() {
  const n = layout.value.nodes.find((x) => x.node.warnings.length);
  if (n) {
    userMoved = true;
    select(n);
    view.x = size.w / 2 - (n.x + W / 2) * view.k;
    view.y = size.h / 2 - (n.y + H / 2) * view.k;
  }
}

const batchCost = computed(() => props.plans.reduce((a, p) => a + (p.root.total_cost ?? 0), 0));
const stmtOpen = ref(false);

// -- raw -----------------------------------------------------------------------------------------
const mode = ref<'graph' | 'raw'>('graph');
const raw = computed(() => plan.value?.raw ?? '');
const rawLang = computed(() => (plan.value?.raw_format === 'json' ? 'json' : undefined));
async function copyRaw() {
  await navigator.clipboard.writeText(raw.value);
  ElMessage.success({ message: t('common:copied'), duration: 1200 });
}
async function saveRaw() {
  const p = plan.value;
  if (!p) return;
  const ext = p.raw_format === 'showplan_xml' ? 'sqlplan' : p.raw_format === 'json' ? 'json' : 'txt';
  await saveTextFile(p.raw, `plan.${ext}`, [{ name: ext === 'sqlplan' ? t('plan:raw.sqlServerPlan') : ext.toUpperCase(), extensions: [ext] }]);
}
</script>

<template>
  <div class="pv">
    <!-- statement tabs + summary -->
    <div class="pv-top">
      <div v-if="plans.length > 1" class="pv-stmts">
        <button
          v-for="(p, i) in plans"
          :key="i"
          class="pv-stmt-tab"
          :class="{ on: i === active }"
          :title="p.statement"
          @click="active = i"
        >
          {{ $t('plan:query', { n: i + 1 }) }}
          <span v-if="batchCost > 0 && p.root.total_cost !== null" class="pv-stmt-pct">{{ fmt((p.root.total_cost / batchCost) * 100, 0) }}%</span>
        </button>
      </div>
      <div v-if="summary && plan" class="pv-summary">
        <span class="pv-badge" :class="{ actual: plan.actual }">{{ plan.actual ? $t('plan:actualPlan') : $t('plan:estimatedPlan') }}</span>
        <span class="pv-metric"><em>{{ $t('plan:summary.operators') }}</em>{{ summary.operators }}</span>
        <span v-if="summary.cost !== null" class="pv-metric"><em>{{ $t('plan:summary.cost') }}</em>{{ fmt(summary.cost, 4) }}</span>
        <span v-if="summary.ms !== null" class="pv-metric"><em>{{ $t('plan:summary.time') }}</em>{{ fmt(summary.ms, 1) }} ms</span>
        <span v-if="summary.rows !== null" class="pv-metric"><em>{{ $t('plan:summary.rows') }}</em>{{ fmt(summary.rows) }}</span>
        <button v-if="summary.warnings" class="pv-metric pv-warnbtn" :title="$t('plan:summary.firstWarning')" @click="firstWarning">
          <el-icon><ei-warning-filled /></el-icon>{{ $t('plan:summary.warnings', { count: summary.warnings }) }}
        </button>
        <div class="nm-spacer" />
        <div class="pv-seg" role="group">
          <button :class="{ on: mode === 'graph' }" @click="mode = 'graph'">{{ $t('plan:diagram') }}</button>
          <button :class="{ on: mode === 'raw' }" @click="mode = 'raw'">{{ $t('plan:rawPlan') }}</button>
        </div>
      </div>
      <div v-if="plan?.statement" class="pv-sql nm-selectable" :class="{ open: stmtOpen }" :title="$t('plan:statementTitle')" @click="stmtOpen = !stmtOpen">
        {{ plan.statement }}
      </div>
    </div>

    <div v-if="mode === 'graph'" class="pv-body">
      <div
        ref="vp"
        class="pv-viewport"
        :class="{ panning }"
        tabindex="0"
        @wheel="onWheel"
        @pointerdown="onPointerDown"
        @keydown="onKey"
      >
        <div class="pv-world" :style="{ transform: `translate(${view.x}px, ${view.y}px) scale(${view.k})` }">
          <svg class="pv-edges" :width="layout.width + 40" :height="layout.height + 40">
            <defs>
              <marker id="pv-arrow" viewBox="0 0 10 10" refX="1" refY="5" markerWidth="3.2" markerHeight="3.2" orient="auto-start-reverse">
                <path d="M 10 0 L 0 5 L 10 10 z" fill="#6b6f76" />
              </marker>
              <marker id="pv-arrow-hot" viewBox="0 0 10 10" refX="1" refY="5" markerWidth="3.2" markerHeight="3.2" orient="auto-start-reverse">
                <path d="M 10 0 L 0 5 L 10 10 z" fill="#e8a33d" />
              </marker>
            </defs>
            <g v-for="n in layout.nodes.filter((x) => x.parent)" :key="n.key">
              <path
                :d="edgePath(n)"
                fill="none"
                :class="['pv-edge', { hot: n.hot }]"
                :stroke-width="stroke(n.node)"
                :marker-end="n.hot ? 'url(#pv-arrow-hot)' : 'url(#pv-arrow)'"
              >
                <title>{{ n.node.actual_rows !== null ? $t('plan:edge.actualRows', { rows: fmt(n.node.actual_rows) }) + '\n' : '' }}{{ n.node.est_rows !== null ? $t('plan:edge.estimatedRows', { rows: fmt(n.node.est_rows, 1) }) : '' }}</title>
              </path>
              <text
                v-if="view.k >= 0.55 && rowsThrough(n.node) > 0"
                class="pv-edge-label"
                :x="n.x - 10"
                :y="n.y + H / 2 - 8"
                text-anchor="end"
              >{{ compact(rowsThrough(n.node)) }}</text>
            </g>
          </svg>

          <div
            v-for="n in layout.nodes"
            :key="n.key"
            class="pv-node"
            :class="[`sev-${severity(n.pct)}`, `tone-${category(n.node.op).tone}`, { sel: selected === n.key, hot: n.hot, root: !n.parent }]"
            :style="{ left: n.x + 'px', top: n.y + 'px', width: W + 'px', height: H + 'px' }"
            @click.stop="select(n)"
          >
            <div class="pv-node-head">
              <span class="pv-ic"><el-icon><component :is="`ei-${category(n.node.op).icon}`" /></el-icon></span>
              <div class="pv-names">
                <div class="pv-op" :title="tb(n.node.op)">{{ tb(n.node.op) }}</div>
                <div v-if="n.node.detail" class="pv-detail" :title="tb(n.node.detail)">{{ tb(n.node.detail) }}</div>
              </div>
              <el-icon v-if="n.node.warnings.length" class="pv-warn" :title="n.node.warnings.map(tb).join('\n')"><ei-warning-filled /></el-icon>
            </div>
            <div class="pv-obj" :title="n.node.object ?? ''">{{ n.node.object ?? '' }}</div>
            <div class="pv-foot">
              <div class="pv-costbar" :title="n.pct !== null ? $t('plan:node.operatorCost', { pct: fmt(n.pct, 1) }) : $t('plan:node.noCost')">
                <span :style="{ width: Math.min(100, n.pct ?? 0) + '%' }" />
              </div>
              <span class="pv-pct">{{ n.pct !== null ? fmt(n.pct, 0) + '%' : '' }}</span>
            </div>
            <div class="pv-stats">
              <span :class="{ off: rowsLine(n.node).off }" :title="n.node.actual_rows !== null ? $t('plan:node.actualEstimatedRows') : $t('plan:node.estimatedRows')">{{ rowsLine(n.node).text }}</span>
              <span v-if="n.node.actual_ms !== null">{{ fmt(n.node.actual_ms, 1) }} ms</span>
            </div>
            <button
              v-if="n.node.children.length"
              class="pv-chip"
              :title="n.hidden ? $t('plan:node.expand', { count: n.hidden }) : $t('plan:node.collapse')"
              @click.stop="toggle(n.key)"
            >{{ n.hidden ? `+${n.hidden}` : '−' }}</button>
          </div>
        </div>

        <!-- zoom controls -->
        <div class="pv-zoom" @pointerdown.stop>
          <button :title="$t('plan:zoom.zoomOut')" @click="zoomBy(1 / 1.2)"><el-icon><ei-minus /></el-icon></button>
          <button class="pv-zoom-pct" :title="$t('plan:zoom.actualSize')" @click="actualSize">{{ Math.round(view.k * 100) }}%</button>
          <button :title="$t('plan:zoom.zoomIn')" @click="zoomBy(1.2)"><el-icon><ei-plus /></el-icon></button>
          <span class="pv-zoom-sep" />
          <button :title="$t('plan:zoom.fit')" @click="fit"><el-icon><ei-full-screen /></el-icon></button>
          <button :title="$t('plan:zoom.expandAll')" @click="expandAll"><el-icon><ei-expand /></el-icon></button>
          <button :title="$t('plan:zoom.collapseBelow')" @click="collapseBelow(3)"><el-icon><ei-fold /></el-icon></button>
          <button :class="{ on: showMini }" :title="$t('plan:zoom.minimap')" @click="showMini = !showMini"><el-icon><ei-map-location /></el-icon></button>
        </div>

        <!-- minimap -->
        <div v-if="showMini && layout.nodes.length > 3" class="pv-mini" :style="{ width: MINI_W + 'px', height: MINI_H + 'px' }" @pointerdown.stop="onMiniDown">
          <svg :width="MINI_W" :height="MINI_H">
            <rect
              v-for="n in layout.nodes"
              :key="n.key"
              :x="mini.ox + n.x * mini.s"
              :y="mini.oy + n.y * mini.s"
              :width="Math.max(2, W * mini.s)"
              :height="Math.max(2, H * mini.s)"
              :class="['pv-mini-node', `sev-${severity(n.pct)}`]"
              rx="1"
            />
            <rect class="pv-mini-view" :x="miniView.x" :y="miniView.y" :width="miniView.w" :height="miniView.h" rx="2" />
          </svg>
        </div>

        <div class="pv-hint">{{ $t('plan:hint') }}</div>
      </div>

      <!-- properties -->
      <aside v-if="sel" class="pv-props nm-selectable">
        <header class="pv-props-head">
          <span class="pv-ic big" :class="`tone-${category(sel.node.op).tone}`"><el-icon><component :is="`ei-${category(sel.node.op).icon}`" /></el-icon></span>
          <div class="pv-props-title">
            <strong>{{ tb(sel.node.op) }}</strong>
            <span v-if="sel.node.detail">{{ tb(sel.node.detail) }}</span>
          </div>
          <button class="ide-icon-btn" :title="$t('plan:props.close')" @click="selected = null"><el-icon><ei-close /></el-icon></button>
        </header>

        <section v-if="sel.node.warnings.length" class="pv-sec">
          <div v-for="(w, i) in sel.node.warnings" :key="i" class="pv-callout">
            <el-icon><ei-warning-filled /></el-icon><span>{{ tb(w) }}</span>
          </div>
        </section>

        <section class="pv-sec">
          <h4>{{ $t('plan:summary.cost') }}</h4>
          <div class="pv-bigbar" :class="`sev-${severity(sel.pct)}`"><span :style="{ width: Math.min(100, sel.pct ?? 0) + '%' }" /></div>
          <dl>
            <dt>{{ $t('plan:props.operator') }}</dt><dd>{{ sel.pct !== null ? fmt(sel.pct, 1) + ' %' : '—' }}</dd>
            <dt>{{ $t('plan:props.self') }}</dt><dd>{{ fmt(selfCost(sel.node), 6) }}</dd>
            <dt>{{ $t('plan:props.subtree') }}</dt><dd>{{ fmt(sel.node.total_cost, 6) }}</dd>
          </dl>
        </section>

        <section class="pv-sec">
          <h4>{{ $t('plan:props.rowsAndTime') }}</h4>
          <dl>
            <dt>{{ $t('plan:props.estimatedPerExecution') }}</dt><dd>{{ fmt(sel.node.est_rows, 2) }}</dd>
            <dt>{{ $t('plan:props.actualTotal') }}</dt><dd :class="{ off: rowsLine(sel.node).off }">{{ fmt(sel.node.actual_rows) }}</dd>
            <dt>{{ $t('plan:props.executions') }}</dt><dd>{{ fmt(sel.node.executions) }}</dd>
            <dt>{{ $t('plan:props.actualTime') }}</dt><dd>{{ sel.node.actual_ms !== null ? fmt(sel.node.actual_ms, 3) + ' ms' : '—' }}</dd>
          </dl>
        </section>

        <section v-if="sel.node.object" class="pv-sec">
          <h4>{{ $t('plan:props.object') }}</h4>
          <code class="pv-code">{{ sel.node.object }}</code>
        </section>

        <section v-if="sel.node.props.length" class="pv-sec">
          <h4>{{ $t('plan:props.properties') }}</h4>
          <dl class="pv-props-list">
            <template v-for="([k, v], i) in sel.node.props" :key="i">
              <dt>{{ tb(k) }}</dt><dd>{{ tb(v) }}</dd>
            </template>
          </dl>
        </section>
      </aside>
    </div>

    <div v-else class="pv-raw">
      <div class="pv-raw-bar">
        <el-button size="small" @click="copyRaw"><el-icon><ei-copy-document /></el-icon>&nbsp;{{ $t('common:copy') }}</el-button>
        <el-button size="small" @click="saveRaw"><el-icon><ei-download /></el-icon>&nbsp;{{ $t('plan:raw.save') }}</el-button>
        <span class="nm-muted">{{ plan?.raw_format === 'showplan_xml' ? $t('plan:raw.ssmsHint') : '' }}</span>
      </div>
      <CodeEditor :model-value="raw" :language="rawLang" read-only />
    </div>
  </div>
</template>

<style scoped>
.pv {
  --pv-card: #232428;
  --pv-card-2: #2a2c31;
  --pv-line: #34363c;
  --pv-edge: #5d6168;
  --pv-hot: #e8a33d;
  --pv-low: #3794ff;
  --pv-mid: #cca700;
  --pv-high: #f14c4c;
  display: flex; flex-direction: column; height: 100%; min-height: 0;
}

/* header */
.pv-top { flex-shrink: 0; border-bottom: 1px solid var(--nm-border-soft); background: var(--ide-editor); }
.pv-stmts { display: flex; gap: 2px; padding: 6px 10px 0; overflow-x: auto; }
.pv-stmt-tab {
  display: inline-flex; align-items: center; gap: 6px; padding: 4px 10px; font: inherit; font-size: 12px;
  color: var(--nm-text-dim); background: transparent; border: 1px solid transparent; border-bottom: none;
  border-radius: 4px 4px 0 0; cursor: pointer; white-space: nowrap;
}
.pv-stmt-tab.on { color: var(--nm-text-strong); background: var(--ide-sidebar); border-color: var(--nm-border-soft); }
.pv-stmt-pct { font-size: 10.5px; padding: 0 5px; border-radius: 8px; background: var(--ide-button-2); }
.pv-summary { display: flex; align-items: center; gap: 14px; padding: 6px 12px; flex-wrap: wrap; }
.pv-badge { font-size: 11px; font-weight: 600; letter-spacing: 0.02em; padding: 2px 8px; border-radius: 10px; background: #2d3340; color: #9fb8e6; }
.pv-badge.actual { background: #1f3a28; color: #9ed6a8; }
.pv-metric { display: inline-flex; align-items: baseline; gap: 5px; font-size: 12.5px; color: var(--nm-text-strong); font-variant-numeric: tabular-nums; }
.pv-metric em { font-style: normal; font-size: 11px; color: var(--nm-text-dim); }
.pv-warnbtn { gap: 4px; align-items: center; border: none; background: rgba(204, 167, 0, 0.12); color: #e2c08d; padding: 2px 8px; border-radius: 10px; cursor: pointer; font: inherit; font-size: 12px; }
.pv-warnbtn:hover { background: rgba(204, 167, 0, 0.2); }
.pv-seg { display: inline-flex; border: 1px solid var(--nm-border); border-radius: 4px; overflow: hidden; }
.pv-seg button { padding: 3px 10px; font: inherit; font-size: 12px; border: none; background: transparent; color: var(--nm-text-dim); cursor: pointer; }
.pv-seg button.on { background: var(--ide-selection); color: var(--nm-text-strong); }
.pv-sql {
  margin: 0 12px 8px; padding: 6px 10px; font-family: var(--nm-mono); font-size: 11.5px; line-height: 1.5;
  color: var(--nm-text); background: var(--ide-sidebar); border-radius: 4px; white-space: pre-wrap;
  max-height: 3em; overflow: hidden; cursor: pointer; position: relative;
}
.pv-sql.open { max-height: 40vh; overflow: auto; }

/* canvas */
.pv-body { flex: 1; min-height: 0; display: flex; }
.pv-viewport {
  position: relative; flex: 1; min-width: 0; overflow: hidden; outline: none; cursor: grab;
  background-color: #1b1c1f;
  background-image: radial-gradient(circle, #2c2e33 1px, transparent 1.2px);
  background-size: 20px 20px;
}
.pv-viewport.panning { cursor: grabbing; }
.pv-world { position: absolute; left: 0; top: 0; transform-origin: 0 0; will-change: transform; }
.pv-edges { position: absolute; left: 0; top: 0; overflow: visible; pointer-events: none; }
.pv-edges path { pointer-events: stroke; }
.pv-edge { stroke: var(--pv-edge); stroke-linecap: round; stroke-linejoin: round; opacity: 0.85; }
.pv-edge.hot { stroke: var(--pv-hot); opacity: 1; }
.pv-edge-label { font-size: 10.5px; fill: #8b9098; font-family: var(--nm-mono); }

/* operator card */
.pv-node {
  position: absolute; display: flex; flex-direction: column; gap: 4px; padding: 9px 11px 8px;
  background: var(--pv-card); border: 1px solid var(--pv-line); border-radius: 8px;
  box-shadow: 0 1px 2px rgba(0, 0, 0, 0.35), 0 4px 14px rgba(0, 0, 0, 0.22);
  cursor: pointer; user-select: none; transition: border-color 0.12s, box-shadow 0.12s;
}
.pv-node:hover { border-color: #4a4d55; }
.pv-node.hot { border-color: rgba(232, 163, 61, 0.55); }
.pv-node.sel { border-color: var(--ide-focus); box-shadow: 0 0 0 2px rgba(0, 127, 212, 0.45), 0 4px 14px rgba(0, 0, 0, 0.3); }
.pv-node.root { background: linear-gradient(180deg, #25303f, var(--pv-card)); }
.pv-node-head { display: flex; align-items: flex-start; gap: 8px; min-width: 0; }
.pv-ic {
  flex-shrink: 0; display: inline-flex; align-items: center; justify-content: center;
  width: 26px; height: 26px; border-radius: 6px; font-size: 15px; background: #2f3238; color: #b9c0cb;
}
.pv-ic.big { width: 32px; height: 32px; font-size: 18px; }
.tone-root .pv-ic, .pv-ic.tone-root { background: rgba(55, 148, 255, 0.16); color: #75beff; }
.tone-seek .pv-ic, .pv-ic.tone-seek { background: rgba(78, 201, 176, 0.14); color: #4ec9b0; }
.tone-scan .pv-ic, .pv-ic.tone-scan { background: rgba(206, 145, 120, 0.15); color: #ce9178; }
.tone-join .pv-ic, .pv-ic.tone-join { background: rgba(197, 134, 192, 0.15); color: #c586c0; }
.tone-order .pv-ic, .pv-ic.tone-order { background: rgba(215, 186, 125, 0.14); color: #d7ba7d; }
.tone-write .pv-ic, .pv-ic.tone-write { background: rgba(241, 76, 76, 0.14); color: #f48771; }
.pv-names { min-width: 0; flex: 1; }
.pv-op { font-weight: 600; font-size: 12.5px; color: #eceef1; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; line-height: 1.25; }
.pv-detail { font-size: 11px; color: var(--nm-text-dim); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.pv-warn { color: #e2b93b; font-size: 15px; flex-shrink: 0; }
.pv-obj { font-family: var(--nm-mono); font-size: 10.5px; color: #4ec9b0; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; min-height: 13px; }
.pv-foot { display: flex; align-items: center; gap: 8px; margin-top: auto; }
.pv-costbar { flex: 1; height: 5px; border-radius: 3px; background: #33363c; overflow: hidden; }
.pv-costbar span { display: block; height: 100%; border-radius: 3px; background: var(--pv-low); }
.sev-mid .pv-costbar span { background: var(--pv-mid); }
.sev-high .pv-costbar span { background: var(--pv-high); }
.pv-pct { font-size: 12px; font-weight: 700; min-width: 32px; text-align: right; font-variant-numeric: tabular-nums; color: #cfd3da; }
.sev-mid .pv-pct { color: #e2c08d; }
.sev-high .pv-pct { color: #f48771; }
.pv-stats { display: flex; justify-content: space-between; font-size: 10.5px; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; }
.pv-stats .off, dd.off { color: #e2b93b; }
.pv-chip {
  position: absolute; right: -11px; top: 50%; transform: translateY(-50%);
  min-width: 20px; height: 20px; padding: 0 5px; border-radius: 10px; font: inherit; font-size: 11px; font-weight: 600;
  background: #2f3238; color: #cfd3da; border: 1px solid #4a4d55; cursor: pointer; line-height: 18px;
}
.pv-chip:hover { background: var(--ide-focus); border-color: var(--ide-focus); color: #fff; }

/* floating controls */
.pv-zoom {
  position: absolute; left: 12px; bottom: 12px; display: flex; align-items: center; gap: 1px; padding: 3px;
  background: rgba(37, 37, 38, 0.92); border: 1px solid #3c3c3c; border-radius: 8px; box-shadow: 0 4px 14px rgba(0, 0, 0, 0.35);
}
.pv-zoom button {
  display: inline-flex; align-items: center; justify-content: center; height: 26px; min-width: 26px; padding: 0 6px;
  border: none; border-radius: 5px; background: transparent; color: var(--nm-text); cursor: pointer; font: inherit; font-size: 12px;
}
.pv-zoom button:hover { background: rgba(255, 255, 255, 0.08); }
.pv-zoom button.on { color: #75beff; }
.pv-zoom-pct { min-width: 48px !important; font-variant-numeric: tabular-nums; }
.pv-zoom-sep { width: 1px; height: 16px; background: #3c3c3c; margin: 0 3px; }
.pv-mini {
  position: absolute; right: 12px; bottom: 12px; background: rgba(30, 31, 34, 0.94);
  border: 1px solid #3c3c3c; border-radius: 8px; box-shadow: 0 4px 14px rgba(0, 0, 0, 0.35); cursor: crosshair; overflow: hidden;
}
.pv-mini-node { fill: #3a3d44; }
.pv-mini-node.sev-mid { fill: #8a7414; }
.pv-mini-node.sev-high { fill: #a33838; }
.pv-mini-view { fill: rgba(0, 127, 212, 0.12); stroke: #3794ff; stroke-width: 1; }
.pv-hint { position: absolute; left: 50%; bottom: 14px; transform: translateX(-50%); font-size: 11px; color: #6c7078; pointer-events: none; white-space: nowrap; }

/* properties panel */
.pv-props { width: 348px; flex-shrink: 0; border-left: 1px solid var(--nm-border); overflow: auto; background: var(--ide-sidebar); }
.pv-props-head { display: flex; align-items: center; gap: 10px; padding: 12px 10px 12px 14px; position: sticky; top: 0; background: var(--ide-sidebar); border-bottom: 1px solid var(--nm-border-soft); z-index: 1; }
.pv-props-title { flex: 1; min-width: 0; display: flex; flex-direction: column; }
.pv-props-title strong { font-size: 13.5px; color: var(--nm-text-strong); }
.pv-props-title span { font-size: 11.5px; color: var(--nm-text-dim); }
.pv-sec { padding: 10px 14px; border-bottom: 1px solid var(--nm-border-soft); }
.pv-sec h4 { margin: 0 0 8px; font-size: 10.5px; font-weight: 600; letter-spacing: 0.07em; text-transform: uppercase; color: var(--nm-text-dim); }
.pv-sec dl { display: grid; grid-template-columns: minmax(110px, 44%) 1fr; gap: 4px 10px; margin: 0; font-size: 12px; }
.pv-sec dt { color: var(--nm-text-dim); }
.pv-sec dd { margin: 0; color: var(--nm-text-strong); font-variant-numeric: tabular-nums; word-break: break-word; }
.pv-props-list dd { font-family: var(--nm-mono); font-size: 11.5px; color: var(--nm-text); }
.pv-bigbar { height: 6px; border-radius: 3px; background: #33363c; overflow: hidden; margin-bottom: 10px; }
.pv-bigbar span { display: block; height: 100%; background: var(--pv-low); }
.pv-bigbar.sev-mid span { background: var(--pv-mid); }
.pv-bigbar.sev-high span { background: var(--pv-high); }
.pv-callout {
  display: flex; gap: 8px; padding: 7px 9px; margin-bottom: 6px; font-size: 12px; line-height: 1.45; color: #e8d6a6;
  background: rgba(204, 167, 0, 0.08); border: 1px solid rgba(204, 167, 0, 0.25); border-radius: 6px; word-break: break-word;
}
.pv-callout .el-icon { color: #e2b93b; flex-shrink: 0; margin-top: 2px; }
.pv-code { display: block; font-family: var(--nm-mono); font-size: 11.5px; color: #4ec9b0; word-break: break-all; }

/* raw */
.pv-raw { flex: 1; min-height: 0; display: flex; flex-direction: column; }
.pv-raw-bar { display: flex; align-items: center; gap: 8px; padding: 6px 12px; border-bottom: 1px solid var(--nm-border-soft); }
.pv-raw > :deep(.ce) { flex: 1; }
</style>
