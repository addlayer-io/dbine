<script setup lang="ts">
import { computed } from 'vue';
import type { DiffLine } from '../composables/lineDiff';

// Side-by-side code diff (composables/lineDiff.ts), with each side's line
// numbers. Schema comparison shows object sources with it; a project's
// file shows its changes against the last commit (`git`: what's only on the
// left was removed, red; only on the right was added, green).

const props = defineProps<{ lines: DiffLine[]; git?: boolean }>();

const rows = computed(() => {
  let l = 0;
  let r = 0;
  return props.lines.map((d) => ({
    ...d,
    ln: d.left === null ? null : ++l,
    rn: d.right === null ? null : ++r,
  }));
});
</script>

<template>
  <div class="cd" :class="{ git }">
    <div v-for="(l, i) in rows" :key="i" class="cd-line" :class="l.kind">
      <span class="cd-n">{{ l.ln ?? '' }}</span>
      <pre class="cd-l" :class="{ none: l.left === null }">{{ l.left ?? '' }}</pre>
      <span class="cd-n">{{ l.rn ?? '' }}</span>
      <pre class="cd-l" :class="{ none: l.right === null }">{{ l.right ?? '' }}</pre>
    </div>
  </div>
</template>

<style scoped>
.cd { flex: 1; overflow: auto; font-family: var(--nm-mono); font-size: 12px; user-select: text; cursor: text; }
.cd-line { display: grid; grid-template-columns: auto 1fr auto 1fr; }
.cd-n {
  min-width: 34px; padding: 0 6px; text-align: right; line-height: 18px; color: var(--nm-text-muted);
  font-variant-numeric: tabular-nums; user-select: none; border-right: 1px solid var(--nm-border-soft);
}
.cd-l { margin: 0; padding: 0 12px; white-space: pre-wrap; word-break: break-all; min-height: 18px; line-height: 18px; border-right: 1px solid var(--nm-border-soft); }
.cd-line.changed .cd-l { background: color-mix(in srgb, var(--nm-warning) 14%, transparent); }
.cd-line.left .cd-l:nth-child(2) { background: color-mix(in srgb, #3794ff 16%, transparent); }
.cd-line.right .cd-l:nth-child(4) { background: color-mix(in srgb, #89d185 16%, transparent); }
.cd.git .cd-line.left .cd-l:nth-child(2) { background: color-mix(in srgb, var(--nm-danger) 16%, transparent); }
.cd.git .cd-line.changed .cd-l:nth-child(2) { background: color-mix(in srgb, var(--nm-danger) 12%, transparent); }
.cd.git .cd-line.changed .cd-l:nth-child(4) { background: color-mix(in srgb, #89d185 14%, transparent); }
.cd-l.none { background: repeating-linear-gradient(135deg, transparent 0 6px, color-mix(in srgb, var(--nm-text-muted) 10%, transparent) 6px 7px) !important; }
</style>
