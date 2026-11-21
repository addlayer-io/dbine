<script setup lang="ts">
// An object's lineage: what it uses to the left, what uses it to the right,
// one level loaded; each card's `+` on either side loads the next level
// that way. Solid lines are in the engine's catalog, dashed ones come from
// reading the source. Sources outside the database are grey cards. Same
// canvas as the ER diagram: drag to pan, ⌘/Ctrl + wheel to zoom, F fits.
// A click selects a card (the lines that name the other are below); a
// double click opens it. The server's jobs that run an object are cards of
// their own to its right: a job's left side is what its steps use, and a
// double click opens it in Programadas › En los servidores.
import { computed, nextTick, onMounted, ref, shallowRef, triggerRef, watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import type { ObjectRef } from '../api/types';
import { tb } from '../i18n/backend';
import { useTabsStore } from '../stores/tabs';
import { usePanZoom } from '../composables/usePanZoom';
import { KIND_ICONS } from '../composables/kindIcons';
import { MAX_NODES, addUsedBy, addUses, jobNodeId, layout, newLineage, nodeId, sideCounts, type LineageEdge, type LineageNode } from '../composables/lineage';

const props = defineProps<{ connectionId: string; database: string; root: ObjectRef; rootJobId?: string | null }>();
const tabs = useTabsStore();
const { t } = useTranslation();

/** A card's size. */
const W = 214;
const H = 46;

const graph = shallowRef(newLineage(props.root, props.rootJobId));
const touch = () => triggerRef(graph);
const placed = computed(() => layout(graph.value, W, H));
const nodes = computed(() => [...graph.value.nodes.values()]);
const selected = ref<string | null>(null);
const search = ref('');

/** Reads one side of a card: what it uses (`up`) or what uses it (`down`). */
async function load(id: string, side: 'up' | 'down') {
  const g = graph.value;
  const n = g.nodes.get(id);
  if (!n || n.external || n[side] === 'loading' || n[side] === 'done') return;
  n[side] = 'loading';
  n.error = undefined;
  touch();
  const target = { object: { kind: n.kind, schema: n.schema, name: n.name }, column: null };
  try {
    const r = n.jobId
      ? await api.getJobReferences(props.connectionId, props.database, n.jobId)
      : side === 'up'
        ? await api.getReferences(props.connectionId, props.database, target)
        : await api.getDependents(props.connectionId, props.database, target);
    if (graph.value !== g) return;
    if (side === 'up') addUses(g, id, r);
    else addUsedBy(g, id, r);
  } catch (e) {
    if (graph.value !== g) return;
    n[side] = 'error';
    n.error = errorMessage(e);
  }
  touch();
}
function start() {
  const id = props.rootJobId ? jobNodeId(props.rootJobId) : nodeId(props.root);
  void load(id, 'up');
  void load(id, 'down');
}
/** Read again from the start (the parent's "Actualizar"). */
function reload() {
  graph.value = newLineage(props.root, props.rootJobId);
  selected.value = null;
  pz.markMoved();
  start();
  void nextTick(fit);
}
defineExpose({ reload });
onMounted(start);

// -- canvas ------------------------------------------------------------------------------------
const pz = usePanZoom({
  bounds: () => placed.value.bounds,
  minK: 0.1,
  maxK: 2.5,
  maxFitK: 1,
  noPan: '.ln-card, .ln-zoom',
});
const { vp, view, panning, zoomBy, actualSize, fit, onWheel, onPointerDown } = pz;
// Until the user moves the view, it follows what loads.
watch(() => placed.value.bounds, () => {
  if (!pz.isUserMoved()) void nextTick(fit);
});
function onCanvasKey(e: KeyboardEvent) {
  if (pz.onKey(e)) return;
  if (e.key === 'Escape') selected.value = null;
}

// -- cards -------------------------------------------------------------------------------------
const kindLabel = (n: LineageNode) => (n.external ? t('dependencies:lineage.external') : t(`dependencies:kind.${n.kind}`, { defaultValue: n.kind }));
const icon = (n: LineageNode) => (n.external ? 'connection' : n.jobId ? 'alarm-clock' : KIND_ICONS[n.kind] ?? 'document');
/** The second line: the kind and the schema, or where an outside source is. */
function sub(n: LineageNode) {
  if (n.external) {
    const x = n.external;
    return [x.system, x.server, x.database].filter(Boolean).join(' · ');
  }
  return [kindLabel(n), n.jobKind, n.schema].filter(Boolean).join(' · ');
}
const counts = (id: string) => sideCounts(graph.value, id);
/** A side's button: `+` to read it, a spinner, the count, or `!`. */
function sideText(n: LineageNode, side: 'up' | 'down') {
  const s = n[side];
  if (s === 'none') return '+';
  if (s === 'error') return '!';
  if (s === 'loading') return '';
  return String(side === 'up' ? counts(n.id).up : counts(n.id).down);
}
function sideTitle(n: LineageNode, side: 'up' | 'down') {
  if (n[side] === 'error') return t('dependencies:lineage.error', { error: n.error ?? '' });
  return side === 'up' ? t('dependencies:lineage.loadUses') : t('dependencies:lineage.loadUsedBy');
}

const q = computed(() => search.value.trim().toLowerCase());
const hits = computed(() => new Set(q.value ? nodes.value.filter((n) => `${n.schema ?? ''}.${n.name}`.toLowerCase().includes(q.value)).map((n) => n.id) : []));
/** The selected card's edges are lit, the rest dimmed. */
const hot = (e: LineageEdge) => !!selected.value && (e.from === selected.value || e.to === selected.value);

const sel = computed(() => (selected.value ? graph.value.nodes.get(selected.value) ?? null : null));
/** The selected card's neighbours: what it uses and what uses it, with the lines. */
const selEdges = computed(() => {
  const id = selected.value;
  if (!id) return { uses: [], usedBy: [] };
  const name = (other: string) => graph.value.nodes.get(other);
  return {
    uses: graph.value.edges.filter((e) => e.to === id).map((e) => ({ e, other: name(e.from) })),
    usedBy: graph.value.edges.filter((e) => e.from === id).map((e) => ({ e, other: name(e.to) })),
  };
});
const fullName = (n?: LineageNode | null) => (n ? (n.schema ? `${n.schema}.${n.name}` : n.name) : '');

function open(n: LineageNode) {
  if (n.external) return;
  if (n.jobId) return tabs.openServerJob(props.connectionId, n.jobId, n.name);
  const object = { kind: n.kind, schema: n.schema, name: n.name };
  tabs.openObject(props.connectionId, props.database, object, ['table', 'collection'].includes(n.kind) ? 'structure' : 'definition');
}
/** The lineage with this card in the middle, in its own tab. */
function recenter(n: LineageNode) {
  if (n.external) return;
  tabs.openDependencies(props.connectionId, props.database, { kind: n.kind, schema: n.schema, name: n.name }, null, 'lineage', n.jobId ?? null);
}
</script>

<template>
  <div class="ln">
    <div class="ln-bar">
      <el-input v-model="search" size="small" clearable :placeholder="$t('dependencies:lineage.search')" style="width: 220px" />
      <span class="ln-legend"><i class="ln-key solid" />{{ $t('dependencies:confidence.confirmed') }}</span>
      <span class="ln-legend"><i class="ln-key dashed" />{{ $t('dependencies:confidence.probable') }}</span>
      <span class="ln-hint">{{ $t('dependencies:lineage.hint') }}</span>
    </div>
    <p v-if="graph.capped" class="ln-note warn">{{ $t('dependencies:lineage.capped', { n: MAX_NODES }) }}</p>
    <p v-for="note in graph.notes" :key="note" class="ln-note warn">{{ tb(note) }}</p>
    <p v-if="graph.unreadable.length" class="ln-note warn">{{ $t('dependencies:unreadable', { names: graph.unreadable.join(', ') }) }}</p>
    <div class="ln-body">
      <div
        ref="vp"
        class="ln-vp"
        :class="{ panning }"
        tabindex="0"
        @wheel="onWheel"
        @pointerdown="onPointerDown"
        @keydown="onCanvasKey"
        @click.self="selected = null"
      >
        <div class="ln-world" :style="{ transform: `translate(${view.x}px, ${view.y}px) scale(${view.k})` }">
          <svg
            class="ln-edges"
            :style="{ left: placed.bounds.x - 60 + 'px', top: placed.bounds.y - 60 + 'px' }"
            :width="placed.bounds.width + 120"
            :height="placed.bounds.height + 120"
            :viewBox="`${placed.bounds.x - 60} ${placed.bounds.y - 60} ${placed.bounds.width + 120} ${placed.bounds.height + 120}`"
          >
            <defs>
              <marker id="ln-arrow" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
                <path d="M0,0 L8,4 L0,8 z" class="ln-arrow" />
              </marker>
            </defs>
            <path
              v-for="p in placed.paths"
              :key="`${p.edge.from}>${p.edge.to}`"
              :d="p.d"
              class="ln-edge"
              :class="[p.edge.confidence, p.edge.relation, { hot: hot(p.edge), dim: !!selected && !hot(p.edge) }]"
              marker-end="url(#ln-arrow)"
            />
          </svg>
          <div
            v-for="n in nodes"
            :key="n.id"
            class="ln-card"
            :class="{ root: n.root, external: !!n.external, sel: selected === n.id, hit: hits.has(n.id), dim: hits.size > 0 && !hits.has(n.id) }"
            :style="{ left: placed.pos.get(n.id)?.x + 'px', top: placed.pos.get(n.id)?.y + 'px', width: W + 'px', height: H + 'px' }"
            :title="fullName(n)"
            @click.stop="selected = n.id"
            @dblclick.stop="open(n)"
          >
            <button
              v-if="!n.external"
              class="ln-side left"
              :class="n.up"
              :title="sideTitle(n, 'up')"
              :aria-label="sideTitle(n, 'up')"
              @click.stop="load(n.id, 'up')"
            ><el-icon v-if="n.up === 'loading'" class="is-loading"><ei-loading /></el-icon><template v-else>{{ sideText(n, 'up') }}</template></button>
            <el-icon class="ln-ic"><component :is="`ei-${icon(n)}`" /></el-icon>
            <div class="ln-text">
              <div class="ln-name">{{ n.name }}</div>
              <div class="ln-sub">{{ sub(n) }}</div>
            </div>
            <button
              v-if="!n.external && !n.jobId"
              class="ln-side right"
              :class="n.down"
              :title="sideTitle(n, 'down')"
              :aria-label="sideTitle(n, 'down')"
              @click.stop="load(n.id, 'down')"
            ><el-icon v-if="n.down === 'loading'" class="is-loading"><ei-loading /></el-icon><template v-else>{{ sideText(n, 'down') }}</template></button>
          </div>
        </div>
        <div class="ln-zoom" @pointerdown.stop>
          <button :aria-label="$t('diagram:zoom.zoomOut')" :title="$t('diagram:zoom.zoomOut')" @click="zoomBy(1 / 1.2)"><el-icon><ei-minus /></el-icon></button>
          <button class="ln-zoom-pct" :title="$t('diagram:zoom.actualSize')" @click="actualSize">{{ Math.round(view.k * 100) }}%</button>
          <button :aria-label="$t('diagram:zoom.zoomIn')" :title="$t('diagram:zoom.zoomIn')" @click="zoomBy(1.2)"><el-icon><ei-plus /></el-icon></button>
          <button :aria-label="$t('diagram:zoom.fit')" :title="$t('diagram:zoom.fit')" @click="fit"><el-icon><ei-full-screen /></el-icon></button>
        </div>
      </div>
      <aside v-if="sel" class="ln-detail nm-selectable">
        <div class="ln-detail-head">
          <el-icon><component :is="`ei-${icon(sel)}`" /></el-icon>
          <strong>{{ fullName(sel) }}</strong>
          <span class="ln-dim">{{ kindLabel(sel) }}</span>
        </div>
        <div v-if="sel.external" class="ln-dim">{{ [sel.external.system, sel.external.server, sel.external.database, sel.external.schema, sel.external.name].filter(Boolean).join(' · ') }}</div>
        <div v-if="sel.error" class="ln-note warn">{{ sel.error }}</div>
        <div v-if="!sel.external" class="ln-actions">
          <el-button size="small" @click="open(sel)">{{ sel.jobId ? $t('dependencies:jobs.open') : $t('dependencies:lineage.open') }}</el-button>
          <el-button v-if="!sel.root" size="small" @click="recenter(sel)">{{ $t('dependencies:lineage.recenter') }}</el-button>
        </div>
        <template v-for="side in (['uses', 'usedBy'] as const)" :key="side">
          <h4 v-if="selEdges[side].length">{{ $t(`dependencies:lineage.${side}`) }} <span class="ln-dim">{{ selEdges[side].length }}</span></h4>
          <div v-for="{ e, other } in selEdges[side]" :key="`${e.from}>${e.to}`" class="ln-rel">
            <div>
              <a @click="other && (selected = other.id)">{{ fullName(other) }}</a>
              <span class="ln-badge" :class="e.confidence">{{ $t(`dependencies:confidence.${e.confidence}`) }}</span>
            </div>
            <div v-if="e.detail" class="ln-dim ln-mono">{{ e.detail }}</div>
            <div v-for="m in e.mentions" :key="m.line" class="ln-mention"><span class="ln-ln">{{ m.line }}</span><code>{{ m.text }}</code></div>
          </div>
        </template>
      </aside>
    </div>
  </div>
</template>

<style scoped>
.ln { flex: 1; min-height: 0; display: flex; flex-direction: column; }
.ln-bar { display: flex; align-items: center; gap: 12px; padding: 8px 0; font-size: 12px; color: var(--nm-text-dim); }
.ln-legend { display: inline-flex; align-items: center; gap: 5px; }
.ln-key { display: inline-block; width: 22px; border-top: 2px solid var(--nm-text-dim); }
.ln-key.dashed { border-top-style: dashed; }
.ln-hint { margin-left: auto; }
.ln-note { margin: 4px 0; font-size: 12px; color: var(--nm-text-dim); }
.ln-note.warn { color: var(--nm-text); }
.ln-body { flex: 1; min-height: 0; display: flex; gap: 10px; }
.ln-vp {
  position: relative; flex: 1; min-width: 0; overflow: hidden; outline: none; cursor: grab;
  border: 1px solid var(--nm-border-soft); border-radius: 4px; background: var(--ide-editor);
}
.ln-vp.panning { cursor: grabbing; }
.ln-world { position: absolute; left: 0; top: 0; transform-origin: 0 0; }
.ln-edges { position: absolute; overflow: visible; pointer-events: none; }
.ln-edge { fill: none; stroke: var(--nm-text-dim); stroke-width: 1.4; opacity: 0.75; }
.ln-edge.probable, .ln-edge.review { stroke-dasharray: 5 4; }
.ln-edge.external { stroke: var(--nm-syn-type); }
.ln-edge.hot { stroke: var(--ide-focus); opacity: 1; stroke-width: 2; }
.ln-edge.dim { opacity: 0.25; }
.ln-arrow { fill: var(--nm-text-dim); }
.ln-card {
  position: absolute; box-sizing: border-box; display: flex; align-items: center; gap: 7px; padding: 0 22px;
  background: var(--ide-sidebar); border: 1px solid var(--nm-border); border-radius: 6px; cursor: pointer; user-select: none;
}
.ln-card:hover { border-color: var(--nm-border-hover); }
.ln-card.root { border-color: var(--ide-focus); box-shadow: 0 0 0 1px var(--ide-focus); }
.ln-card.sel { border-color: var(--ide-focus); background: color-mix(in srgb, var(--ide-focus) 12%, var(--ide-sidebar)); }
.ln-card.external { border-style: dashed; color: var(--nm-text-dim); }
.ln-card.hit { box-shadow: 0 0 0 2px var(--nm-warning); }
.ln-card.dim { opacity: 0.4; }
.ln-ic { flex-shrink: 0; color: var(--nm-accent-2, var(--nm-text-dim)); }
.ln-text { min-width: 0; }
.ln-name { font-size: 12.5px; color: var(--nm-text-strong); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.ln-sub { font-size: 10.5px; color: var(--nm-text-dim); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.ln-side {
  position: absolute; top: 50%; transform: translateY(-50%); min-width: 18px; height: 18px; padding: 0 3px;
  display: inline-flex; align-items: center; justify-content: center; font: 600 10.5px var(--nm-font);
  color: var(--nm-text); background: var(--ide-editor); border: 1px solid var(--nm-border); border-radius: 9px; cursor: pointer;
}
.ln-side.left { left: -10px; }
.ln-side.right { right: -10px; }
.ln-side.none { color: var(--ide-focus); border-color: var(--ide-focus); }
.ln-side.done { cursor: default; color: var(--nm-text-dim); }
.ln-side.error { color: var(--nm-danger); border-color: var(--nm-danger); }
.ln-zoom {
  position: absolute; right: 10px; bottom: 10px; display: flex; gap: 2px; padding: 2px;
  background: var(--ide-sidebar); border: 1px solid var(--nm-border); border-radius: 4px;
}
.ln-zoom button { height: 22px; min-width: 24px; border: none; background: transparent; color: var(--nm-text); cursor: pointer; border-radius: 3px; }
.ln-zoom button:hover { background: var(--ide-hover); }
.ln-zoom-pct { font-size: 11px; font-variant-numeric: tabular-nums; }
.ln-detail {
  width: 320px; flex-shrink: 0; overflow: auto; padding: 10px 12px; font-size: 12px;
  border: 1px solid var(--nm-border-soft); border-radius: 4px; background: var(--ide-sidebar);
}
.ln-detail-head { display: flex; align-items: center; gap: 6px; margin-bottom: 6px; color: var(--nm-text-strong); }
.ln-detail h4 { margin: 12px 0 4px; font-size: 12px; color: var(--nm-text-strong); }
.ln-actions { display: flex; gap: 6px; margin: 6px 0; }
.ln-actions .el-button { margin: 0; }
.ln-rel { padding: 4px 0; border-bottom: 1px solid var(--nm-border-soft); }
.ln-rel a { color: var(--nm-text-strong); cursor: pointer; }
.ln-rel a:hover { text-decoration: underline; }
.ln-badge { margin-left: 6px; font-size: 10px; padding: 0 5px; border-radius: 8px; }
.ln-badge.confirmed { background: color-mix(in srgb, var(--nm-success) 20%, transparent); color: var(--nm-success); }
.ln-badge.probable { background: color-mix(in srgb, var(--nm-accent) 18%, transparent); color: var(--nm-text); }
.ln-badge.review { background: color-mix(in srgb, var(--nm-warning) 22%, transparent); color: var(--nm-warning); }
.ln-dim { color: var(--nm-text-dim); }
.ln-mono { font-family: var(--nm-mono); font-size: 11.5px; }
.ln-mention { display: flex; gap: 6px; margin-top: 2px; }
.ln-ln { min-width: 26px; text-align: right; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; }
.ln-mention code { white-space: pre; overflow: hidden; text-overflow: ellipsis; }
</style>
