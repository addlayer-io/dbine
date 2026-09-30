<script setup lang="ts">
import { computed } from 'vue';
import { ElMessage } from 'element-plus';
import { t } from '../i18n';
import type { Cell } from '../api/types';

// A cell's full value; JSON is pretty-printed.

const props = defineProps<{ title: string; value: Cell }>();
defineEmits<{ close: [] }>();

const text = computed(() => {
  const v = props.value;
  if (v === null) return 'NULL';
  if (typeof v !== 'string') return String(v);
  const t = v.trim();
  if ((t.startsWith('{') && t.endsWith('}')) || (t.startsWith('[') && t.endsWith(']'))) {
    try { return JSON.stringify(JSON.parse(t), null, 2); } catch { /* not JSON */ }
  }
  return v;
});

async function copy() {
  await navigator.clipboard.writeText(text.value);
  ElMessage.success({ message: t('common:copied'), duration: 1200 });
}
</script>

<template>
  <el-dialog :model-value="true" :title="title" width="720px" append-to-body @close="$emit('close')">
    <pre class="cv nm-selectable">{{ text }}</pre>
    <template #footer>
      <el-button @click="copy">{{ $t('common:copy') }}</el-button>
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
</style>
