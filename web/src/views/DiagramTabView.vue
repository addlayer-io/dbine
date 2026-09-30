<script setup lang="ts">
import { onErrorCaptured, onMounted, onUnmounted, ref } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import type { TableSchema } from '../api/schema-types';
import DatabaseDiagram from '../components/DatabaseDiagram.vue';
import { saveBinaryFile, saveTextFile } from '../composables/files';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore, type DiagramTab } from '../stores/tabs';

// The ER diagram tab: loads the database structure (tables, keys, foreign
// keys, indexes) through the driver and draws it.

const props = defineProps<{ tab: DiagramTab }>();
const conns = useConnectionsStore();
const tabs = useTabsStore();
const { t } = useTranslation();

const schema = ref<TableSchema[] | null>(null);
const error = ref<string | null>(null);
const loading = ref(false);

// Seconds spent reading the structure (a big database takes a while).
const elapsed = ref(0);
let timer: ReturnType<typeof setInterval> | undefined;
onUnmounted(() => clearInterval(timer));

async function load() {
  loading.value = true;
  error.value = null;
  elapsed.value = 0;
  clearInterval(timer);
  timer = setInterval(() => elapsed.value++, 1000);
  try {
    if (!(await conns.ensureConnected(props.tab.connectionId))) {
      error.value = t('diagram:tab.connectFailed');
      return;
    }
    schema.value = await invoke<TableSchema[]>('database_schema', { args: { connection_id: props.tab.connectionId, database: props.tab.database } });
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    loading.value = false;
    clearInterval(timer);
  }
}
onMounted(load);

// A failure drawing the diagram shows up here instead of a blank tab.
const drawError = ref<string | null>(null);
onErrorCaptured((e) => {
  drawError.value = e instanceof Error ? e.message : String(e);
  return false;
});
function retry() {
  drawError.value = null;
  load();
}

function openTable(t: { schema: string | null; name: string }) {
  const kind = schema.value?.find((x) => x.schema === t.schema && x.name === t.name)?.kind ?? 'table';
  tabs.openObject(props.tab.connectionId, props.tab.database, { kind, schema: t.schema, name: t.name }, 'data', false);
}

const base = () => t('diagram:tab.fileName', { name: props.tab.database || t('diagram:tab.fileNameDefault') });
function exportSvg(svg: string) {
  saveTextFile(svg, `${base()}.svg`, [{ name: 'SVG', extensions: ['svg'] }]);
}
function exportPng(url: string) {
  saveBinaryFile(url.split(',', 2)[1], `${base()}.png`, [{ name: 'PNG', extensions: ['png'] }]);
}
</script>

<template>
  <div class="dt">
    <div v-if="loading" class="dt-busy">
      <el-icon class="is-loading" :size="22"><ei-loading /></el-icon>
      <span>{{ $t('diagram:tab.reading') }}</span>
      <span v-if="elapsed >= 2" class="dt-time">{{ elapsed }} s</span>
    </div>
    <div v-else-if="error || drawError" class="nm-content">
      <el-alert type="error" :title="error ?? $t('diagram:tab.drawFailed', { error: drawError })" :closable="false" />
      <el-button style="margin-top: 10px" @click="retry">{{ $t('common:retry') }}</el-button>
    </div>
    <DatabaseDiagram
      v-else-if="schema"
      :schema="schema"
      :title="tab.database || conns.byId(tab.connectionId)?.name"
      @open-table="openTable"
      @export-svg="exportSvg"
      @export-png="exportPng"
    />
  </div>
</template>

<style scoped>
.dt { display: flex; flex-direction: column; height: 100%; min-height: 0; }
.dt > :deep(*) { flex: 1; min-height: 0; }
.dt-busy { display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 10px; color: var(--nm-text-dim); font-size: 13px; }
.dt-time { font-variant-numeric: tabular-nums; font-size: 12px; opacity: 0.7; }
</style>
