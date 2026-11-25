<script setup lang="ts">
// The side panel the lineage views share: the selected card, what it uses and what
// uses it, each line with its confidence and the lines of source that name the other.
// Buttons (open, recenter…) come in the `actions` slot; `extra` goes under the head. A web address
// in a line's detail (a Databricks job's, a Power BI report's) is a link to the browser.
import { computed } from 'vue';
import type { Lineage, LineageNode } from '../composables/lineage';
import LinkText from './LinkText.vue';

const props = defineProps<{ graph: Lineage; sel: LineageNode; kindLabel: (n: LineageNode) => string }>();
const emit = defineEmits<{ select: [id: string] }>();

const fullName = (n?: LineageNode | null) => (n ? (n.schema && n.kind !== 'schema' ? `${n.schema}.${n.name}` : n.name) : '');
/** The selected card's neighbours: what it uses and what uses it, with the lines. */
const edges = computed(() => {
  const id = props.sel.id;
  const node = (other: string) => props.graph.nodes.get(other);
  return {
    uses: props.graph.edges.filter((e) => e.to === id).map((e) => ({ e, other: node(e.from) })),
    usedBy: props.graph.edges.filter((e) => e.from === id).map((e) => ({ e, other: node(e.to) })),
  };
});
</script>

<template>
  <aside class="ln-detail nm-selectable">
    <div class="ln-detail-head">
      <el-icon><slot name="icon" /></el-icon>
      <strong>{{ fullName(sel) }}</strong>
      <span class="ln-dim">{{ kindLabel(sel) }}</span>
    </div>
    <div v-if="sel.external" class="ln-dim">{{ [sel.external.system, sel.external.server, sel.external.database, sel.external.schema, sel.external.name].filter(Boolean).join(' · ') }}</div>
    <div v-if="sel.error" class="ln-note warn">{{ sel.error }}</div>
    <slot name="extra" />
    <div class="ln-actions"><slot name="actions" /></div>
    <template v-for="side in (['uses', 'usedBy'] as const)" :key="side">
      <h4 v-if="edges[side].length">{{ $t(`dependencies:lineage.${side}`) }} <span class="ln-dim">{{ edges[side].length }}</span></h4>
      <div v-for="{ e, other } in edges[side]" :key="`${e.from}>${e.to}`" class="ln-rel">
        <div>
          <a @click="other && emit('select', other.id)">{{ fullName(other) }}</a>
          <span class="ln-badge" :class="e.confidence">{{ $t(`dependencies:confidence.${e.confidence}`) }}</span>
          <span v-if="(e.weight ?? 1) > 1" class="ln-dim"> · {{ $t('dependencies:database.weight', { count: e.weight }) }}</span>
        </div>
        <div v-if="e.detail" class="ln-dim ln-mono"><LinkText :text="e.detail" /></div>
        <div v-for="m in e.mentions" :key="m.line" class="ln-mention"><span class="ln-ln">{{ m.line }}</span><code>{{ m.text }}</code></div>
      </div>
    </template>
  </aside>
</template>

<style scoped>
.ln-detail {
  width: 320px; flex-shrink: 0; overflow: auto; padding: 10px 12px; font-size: 12px;
  border: 1px solid var(--nm-border-soft); border-radius: 4px; background: var(--ide-sidebar);
}
.ln-detail-head { display: flex; align-items: center; gap: 6px; margin-bottom: 6px; color: var(--nm-text-strong); }
.ln-detail h4 { margin: 12px 0 4px; font-size: 12px; color: var(--nm-text-strong); }
.ln-note { margin: 4px 0; font-size: 12px; color: var(--nm-text); }
.ln-actions { display: flex; gap: 6px; margin: 6px 0; flex-wrap: wrap; }
.ln-actions :deep(.el-button) { margin: 0; }
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
