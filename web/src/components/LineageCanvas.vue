<script setup lang="ts">
// The canvas the lineage views share (an object's and a whole database's): the
// cards, the lines between them and the zoom buttons, over `usePanZoom` like the
// ER diagram: drag to pan, ⌘/Ctrl + wheel to zoom, F fits. A click selects a
// card, a double click opens it; what goes at a card's sides (the object lineage's
// `+` buttons) comes in the `before` and `after` slots. A line's width follows its
// `weight` (the database overview's lines stand for several dependencies).
import { nextTick, watch } from 'vue';
import { usePanZoom } from '../composables/usePanZoom';
import type { LineageEdge, LineageNode, Placed } from '../composables/lineage';

const props = defineProps<{
  nodes: LineageNode[];
  placed: Placed;
  /** A card's size. */
  w: number;
  h: number;
  selected: string | null;
  /** Cards that match the search: lit, the rest dimmed. */
  hits: Set<string>;
  icon: (n: LineageNode) => string;
  sub: (n: LineageNode) => string;
  title: (n: LineageNode) => string;
}>();
const emit = defineEmits<{ 'update:selected': [id: string | null]; open: [n: LineageNode] }>();

const pz = usePanZoom({
  bounds: () => props.placed.bounds,
  minK: 0.05,
  maxK: 2.5,
  maxFitK: 1,
  noPan: '.ln-card, .ln-zoom',
});
const { vp, view, panning, zoomBy, actualSize, fit, onWheel, onPointerDown } = pz;
// Until the user moves the view, it follows what loads.
watch(() => props.placed.bounds, () => {
  if (!pz.isUserMoved()) void nextTick(fit);
});
defineExpose({ fit, markMoved: pz.markMoved, isUserMoved: pz.isUserMoved });

function onKey(e: KeyboardEvent) {
  if (pz.onKey(e)) return;
  if (e.key === 'Escape') emit('update:selected', null);
}
/** The selected card's lines are lit, the rest dimmed. */
const hot = (e: LineageEdge) => !!props.selected && (e.from === props.selected || e.to === props.selected);
/** A line's width: one dependency is thin, hundreds are thick. */
const weightStyle = (e: LineageEdge) => ((e.weight ?? 1) > 1 ? { '--ln-w': Math.min(7, 1.4 + Math.log2(e.weight!)).toFixed(2) } : undefined);
</script>

<template>
  <div
    ref="vp"
    class="ln-vp"
    :class="{ panning }"
    tabindex="0"
    @wheel="onWheel"
    @pointerdown="onPointerDown"
    @keydown="onKey"
    @click.self="emit('update:selected', null)"
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
          <marker id="ln-arrow" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="9" markerHeight="9" markerUnits="userSpaceOnUse" orient="auto-start-reverse">
            <path d="M0,0 L8,4 L0,8 z" class="ln-arrow" />
          </marker>
        </defs>
        <path
          v-for="p in placed.paths"
          :key="`${p.edge.from}>${p.edge.to}`"
          :d="p.d"
          class="ln-edge"
          :class="[p.edge.confidence, p.edge.relation, { hot: hot(p.edge), dim: !!selected && !hot(p.edge) }]"
          :style="weightStyle(p.edge)"
          marker-end="url(#ln-arrow)"
        />
      </svg>
      <div
        v-for="n in nodes"
        :key="n.id"
        class="ln-card"
        :class="{ root: n.root, external: !!n.external, schema: n.kind === 'schema', sel: selected === n.id, hit: hits.has(n.id), dim: hits.size > 0 && !hits.has(n.id) }"
        :style="{ left: placed.pos.get(n.id)?.x + 'px', top: placed.pos.get(n.id)?.y + 'px', width: w + 'px', height: h + 'px' }"
        :title="title(n)"
        @click.stop="emit('update:selected', n.id)"
        @dblclick.stop="emit('open', n)"
      >
        <slot name="before" :n="n" />
        <el-icon class="ln-ic"><component :is="`ei-${icon(n)}`" /></el-icon>
        <div class="ln-text">
          <div class="ln-name">{{ n.name }}</div>
          <div class="ln-sub">{{ sub(n) }}</div>
        </div>
        <slot name="after" :n="n" />
      </div>
    </div>
    <div class="ln-zoom" @pointerdown.stop>
      <button :aria-label="$t('diagram:zoom.zoomOut')" :title="$t('diagram:zoom.zoomOut')" @click="zoomBy(1 / 1.2)"><el-icon><ei-minus /></el-icon></button>
      <button class="ln-zoom-pct" :title="$t('diagram:zoom.actualSize')" @click="actualSize">{{ Math.round(view.k * 100) }}%</button>
      <button :aria-label="$t('diagram:zoom.zoomIn')" :title="$t('diagram:zoom.zoomIn')" @click="zoomBy(1.2)"><el-icon><ei-plus /></el-icon></button>
      <button :aria-label="$t('diagram:zoom.fit')" :title="$t('diagram:zoom.fit')" @click="fit"><el-icon><ei-full-screen /></el-icon></button>
    </div>
  </div>
</template>

<style scoped>
.ln-vp {
  position: relative; flex: 1; min-width: 0; overflow: hidden; outline: none; cursor: grab;
  border: 1px solid var(--nm-border-soft); border-radius: 4px; background: var(--ide-editor);
}
.ln-vp.panning { cursor: grabbing; }
.ln-world { position: absolute; left: 0; top: 0; transform-origin: 0 0; }
.ln-edges { position: absolute; overflow: visible; pointer-events: none; }
.ln-edge { fill: none; stroke: var(--nm-text-dim); stroke-width: var(--ln-w, 1.4); opacity: 0.75; }
.ln-edge.probable, .ln-edge.review { stroke-dasharray: 5 4; }
.ln-edge.external { stroke: var(--nm-syn-type); }
.ln-edge.hot { stroke: var(--ide-focus); opacity: 1; stroke-width: calc(var(--ln-w, 1.4) + 0.6); }
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
.ln-card.schema { border-width: 2px; background: color-mix(in srgb, var(--ide-sidebar) 85%, var(--nm-text-dim)); }
.ln-card.hit { box-shadow: 0 0 0 2px var(--nm-warning); }
.ln-card.dim { opacity: 0.4; }
.ln-ic { flex-shrink: 0; color: var(--nm-accent-2, var(--nm-text-dim)); }
.ln-text { min-width: 0; }
.ln-name { font-size: 12.5px; color: var(--nm-text-strong); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.ln-sub { font-size: 10.5px; color: var(--nm-text-dim); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.ln-zoom {
  position: absolute; right: 10px; bottom: 10px; display: flex; gap: 2px; padding: 2px;
  background: var(--ide-sidebar); border: 1px solid var(--nm-border); border-radius: 4px;
}
.ln-zoom button { height: 22px; min-width: 24px; border: none; background: transparent; color: var(--nm-text); cursor: pointer; border-radius: 3px; }
.ln-zoom button:hover { background: var(--ide-hover); }
.ln-zoom-pct { font-size: 11px; font-variant-numeric: tabular-nums; }
</style>
