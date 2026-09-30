<script setup lang="ts">
import { computed } from 'vue';
import { useTranslation } from 'i18next-vue';
import { useConnectionsStore } from '../stores/connections';

// Pick an explorer folder (null = top level), shown as an indented tree.
// `exclude` hides a folder and everything inside it (a folder can't move
// into itself).

const props = defineProps<{ modelValue: string | null; exclude?: string | null }>();
const emit = defineEmits<{ 'update:modelValue': [value: string | null] }>();

const conns = useConnectionsStore();
const { t } = useTranslation();
const ROOT = '__root__';

const options = computed(() => {
  const out: { id: string; label: string; depth: number }[] = [{ id: ROOT, label: t('dialogs:folder.topLevel'), depth: 0 }];
  const walk = (parent: string | null, depth: number) => {
    for (const f of conns.folders.filter((x) => (x.parent_id ?? null) === parent)) {
      if (f.id === props.exclude) continue;
      out.push({ id: f.id, label: f.name, depth });
      walk(f.id, depth + 1);
    }
  };
  walk(null, 0);
  return out;
});
</script>

<template>
  <el-select
    :model-value="modelValue ?? ROOT"
    filterable
    style="width: 100%"
    @update:model-value="(v: string) => emit('update:modelValue', v === ROOT ? null : v)"
  >
    <el-option v-for="o in options" :key="o.id" :label="o.id === ROOT ? o.label : conns.folderPath(o.id)" :value="o.id">
      <span :style="{ paddingLeft: o.depth * 14 + 'px' }">
        <el-icon v-if="o.id !== ROOT" style="vertical-align: -2px; color: #c5a46d"><ei-folder /></el-icon>
        {{ o.label }}
      </span>
    </el-option>
  </el-select>
</template>
