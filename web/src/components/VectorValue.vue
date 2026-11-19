<script setup lang="ts">
import { computed } from 'vue';
import { useTranslation } from 'i18next-vue';
import type { VectorMeta } from '../api/types';
import { ELEMENT_NAMES, type VectorPreview } from '../composables/vector';

// A vector cell outside the results grid (data compare, a write cell's
// preview, generated test data): its first numbers and a badge with its
// dimensions, as the grid shows it (composables/vector.ts). `from`: the
// numbers start there (data compare, from the first one that differs).

const props = defineProps<{ preview: VectorPreview; meta?: VectorMeta | null; from?: number | null }>();
const { t } = useTranslation();
const badgeTitle = computed(() => [
  t('vectors:cell.dims', { count: props.preview.dims ?? 0 }),
  props.meta ? ELEMENT_NAMES[props.meta.element] : '',
  props.preview.sparse ? t('vectors:cell.sparse') : '',
].filter(Boolean).join(' · '));
</script>

<template>
  <span class="vv"><span v-if="from != null" class="vv-from">{{ $t('vectors:compare.from', { position: from + 1 }) }}</span><span
    class="vv-text"
  >{{ preview.text }}</span><span v-if="preview.dims != null" class="vv-dims" :class="{ sparse: preview.sparse }" :title="badgeTitle">{{ preview.dims }}</span></span>
</template>

<style scoped>
.vv { display: inline-flex; align-items: center; gap: 6px; max-width: 100%; vertical-align: top; }
.vv-text { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.vv-from { flex-shrink: 0; font-size: 10.5px; color: var(--nm-text-dim); font-family: var(--nm-font); }
.vv-dims {
  flex-shrink: 0; padding: 0 5px; border-radius: 8px; font-size: 10.5px; line-height: 15px; font-family: var(--nm-font); font-variant-numeric: tabular-nums;
  color: var(--nm-syn-type); background: color-mix(in srgb, var(--nm-syn-type) 14%, transparent);
}
.vv-dims.sparse { background: transparent; box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--nm-syn-type) 45%, transparent); }
</style>
