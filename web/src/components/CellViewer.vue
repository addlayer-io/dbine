<script setup lang="ts">
import { computed } from 'vue';
import { ElMessage } from 'element-plus';
import { locale, t } from '../i18n';
import type { Cell, VectorMeta } from '../api/types';
import { ELEMENT_NAMES, isDense, parseVector, vectorLines, vectorStats } from '../composables/vector';

// A cell's full value; JSON is pretty-printed. A vector (`vector`: its
// column's) shows its element type, dimensions, norm and range, and every
// number in rows with their positions; a sparse vector, its text as is.

const props = defineProps<{ title: string; value: Cell; vector?: VectorMeta | null }>();
defineEmits<{ close: [] }>();

/** A dense vector's numbers (null: not a vector, or not one that reads). */
const values = computed(() => (isDense(props.vector) ? parseVector(props.value) : null));
const stats = computed(() => (values.value ? vectorStats(values.value) : null));
const sparse = computed(() => !!props.vector?.sparse && typeof props.value === 'string');
const fmt = (n: number) => n.toLocaleString(locale(), { maximumSignificantDigits: 6 });
const element = computed(() => (props.vector ? ELEMENT_NAMES[props.vector.element] : ''));
/** The column declares other dimensions than the value has. */
const declared = computed(() => (values.value && props.vector?.dims != null && props.vector.dims !== values.value.length ? props.vector.dims : null));

const text = computed(() => {
  const v = props.value;
  if (v === null) return 'NULL';
  if (typeof v !== 'string') return String(v);
  if (values.value) return vectorLines(values.value);
  if (sparse.value) return v;
  const t = v.trim();
  if ((t.startsWith('{') && t.endsWith('}')) || (t.startsWith('[') && t.endsWith(']'))) {
    try { return JSON.stringify(JSON.parse(t), null, 2); } catch { /* not JSON */ }
  }
  return v;
});

async function copy() {
  // A vector: its numbers as a JSON array (the positions are the viewer's).
  await navigator.clipboard.writeText(values.value ? JSON.stringify(values.value) : text.value);
  ElMessage.success({ message: t('common:copied'), duration: 1200 });
}
</script>

<template>
  <el-dialog :model-value="true" :title="title" width="720px" append-to-body @close="$emit('close')">
    <div v-if="values" class="cv-facts">
      <span v-if="element"><em>{{ $t('vectors:viewer.element') }}</em>{{ element }}</span>
      <span><em>{{ $t('vectors:viewer.dims') }}</em>{{ values.length }}<small v-if="declared !== null">{{ $t('vectors:viewer.declared', { dims: declared }) }}</small></span>
      <template v-if="stats">
        <span><em>{{ $t('vectors:viewer.norm') }}</em>{{ fmt(stats.norm) }}</span>
        <span><em>{{ $t('vectors:viewer.min') }}</em>{{ fmt(stats.min) }}</span>
        <span><em>{{ $t('vectors:viewer.max') }}</em>{{ fmt(stats.max) }}</span>
      </template>
      <span v-else>{{ $t('vectors:viewer.empty') }}</span>
    </div>
    <div v-else-if="sparse" class="cv-facts">
      <span>{{ $t('vectors:viewer.sparse') }}</span>
      <span v-if="vector?.dims != null"><em>{{ $t('vectors:viewer.dims') }}</em>{{ vector.dims }}</span>
    </div>
    <pre class="cv nm-selectable" :class="{ 'cv-vec': values }">{{ text }}</pre>
    <template #footer>
      <el-button @click="copy">{{ values ? $t('vectors:viewer.copyJson') : $t('common:copy') }}</el-button>
      <el-button type="primary" @click="$emit('close')">{{ $t('common:close') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.cv {
  max-height: 60vh;
  overflow: auto;
  margin: 0;
  padding: 10px 12px;
  font-family: var(--nm-mono);
  font-size: 12px;
  background: var(--ide-editor);
  border: 1px solid var(--nm-border-soft);
  white-space: pre-wrap;
  word-break: break-word;
}
/* A vector's numbers stay in their columns: no wrapping, scroll sideways. */
.cv.cv-vec { white-space: pre; word-break: normal; color: var(--nm-syn-number); }
.cv-facts { display: flex; flex-wrap: wrap; gap: 6px 18px; margin: 0 0 8px; font-size: 12px; color: var(--nm-text); }
.cv-facts span { font-variant-numeric: tabular-nums; }
.cv-facts em { margin-right: 6px; font-style: normal; color: var(--nm-text-dim); }
.cv-facts small { margin-left: 6px; color: var(--nm-warning); }
</style>
