<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue';
import { useOutputStore } from '../stores/output';
import { locale } from '../i18n';

// Bottom panel: the log of what ran (where, how long, what failed).

defineEmits<{ close: [] }>();
const output = useOutputStore();
const filter = ref('');
const onlyErrors = ref(false);
const list = ref<HTMLDivElement | null>(null);

const entries = computed(() => {
  const q = filter.value.toLowerCase();
  return output.entries.filter((e) =>
    (!onlyErrors.value || e.level === 'error') &&
    (!q || e.text.toLowerCase().includes(q) || (e.where ?? '').toLowerCase().includes(q)));
});

watch(() => output.entries.length, async () => {
  await nextTick();
  list.value?.scrollTo({ top: list.value.scrollHeight });
});

const time = (d: Date) => d.toLocaleTimeString(locale(), { hour12: false });
</script>

<template>
  <div class="op">
    <div class="op-head">
      <span class="op-title">{{ $t('workbench:output.title') }}</span>
      <div class="nm-spacer" />
      <el-input v-model="filter" size="small" :placeholder="$t('common:filter')" clearable style="width: 180px" />
      <el-checkbox v-model="onlyErrors" size="small">{{ $t('workbench:output.onlyErrors') }}</el-checkbox>
      <button class="ide-icon-btn" :title="$t('common:clear')" @click="output.clear()"><el-icon><ei-delete /></el-icon></button>
      <button class="ide-icon-btn" :title="$t('workbench:output.closePanel')" @click="$emit('close')"><el-icon><ei-close /></el-icon></button>
    </div>
    <div ref="list" class="op-list nm-selectable">
      <div v-for="e in entries" :key="e.id" class="op-line" :class="e.level">
        <span class="op-time">{{ time(e.at) }}</span>
        <span v-if="e.where" class="op-where">[{{ e.where }}]</span>
        <span class="op-text">{{ e.text }}</span>
        <span v-if="e.elapsedMs !== undefined" class="op-ms">{{ e.elapsedMs.toLocaleString(locale()) }} ms</span>
      </div>
      <div v-if="!entries.length" class="nm-muted" style="padding: 6px 0">{{ $t('workbench:output.empty') }}</div>
    </div>
  </div>
</template>

<style scoped>
.op { display: flex; flex-direction: column; min-height: 0; background: var(--ide-panel); border-top: 1px solid var(--nm-border); }
.op-head { display: flex; align-items: center; gap: 8px; height: 32px; padding: 0 8px 0 16px; flex-shrink: 0; }
.op-title { font-size: 11px; text-transform: uppercase; letter-spacing: 0.06em; color: var(--nm-text-strong); border-bottom: 1px solid var(--ide-focus); line-height: 24px; }
.op-list { flex: 1; min-height: 0; overflow: auto; padding: 0 16px 6px; font-family: var(--nm-mono); font-size: 12px; }
.op-line { display: flex; gap: 8px; padding: 1px 0; white-space: pre-wrap; }
.op-line.error .op-text { color: var(--nm-danger); }
.op-line.warn .op-text { color: var(--nm-warning); }
.op-time { color: var(--nm-text-muted); flex-shrink: 0; }
.op-where { color: #4ec9b0; flex-shrink: 0; }
.op-text { flex: 1; min-width: 0; }
.op-ms { color: var(--nm-text-muted); flex-shrink: 0; }
</style>
