<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { useTranslation } from 'i18next-vue';
import { tb } from '../i18n/backend';
import type { TableSchema } from '../api/schema-types';
import { dbKey, useConnectionsStore } from '../stores/connections';
import { useUiStore } from '../stores/ui';
import { newQuery } from '../composables/actions';
import ScriptGeneratorDialog from './ScriptGeneratorDialog.vue';
import ImportDialog from './ImportDialog.vue';
import RunScriptFileDialog from './RunScriptFileDialog.vue';

// Database-level dialogs opened from the explorer (generate script, export,
// import data, run a script file), fed with that database's objects.

const ui = useUiStore();
const conns = useConnectionsStore();
const { t } = useTranslation();

const d = computed(() => ui.dbDialog);
const driver = computed(() => (d.value ? conns.driverOf(d.value.connectionId) : undefined));
const objects = computed(() => (d.value ? conns.objects[dbKey(d.value.connectionId, d.value.database)]?.items ?? [] : []));
const kindLabels = computed(() => Object.fromEntries((driver.value?.object_kinds ?? []).map((k) => [k.id, tb(k.label)])));
const tables = ref<{ schema: string | null; name: string; columns: string[] }[]>([]);
const ready = ref(false);

watch(d, async (v) => {
  ready.value = false;
  tables.value = [];
  if (!v) return;
  if (!(await conns.ensureConnected(v.connectionId))) { ui.closeDbDialog(); return; }
  await conns.loadObjects(v.connectionId, v.database);
  if (v.kind === 'import') {
    try {
      const s = await invoke<TableSchema[]>('database_schema', { args: { connection_id: v.connectionId, database: v.database } });
      tables.value = s.map((t) => ({ schema: t.schema, name: t.name, columns: t.columns.map((c) => c.name) }));
    } catch { /* the dialog can still create a table */ }
  }
  ready.value = true;
});

const defaultSchema = computed(() => {
  if (!driver.value?.has_schemas) return null;
  return driver.value.dialect === 'mssql' ? 'dbo' : driver.value.dialect === 'postgres' ? 'public' : null;
});

function openScript(text: string, connectionId: string | null = null) {
  if (!d.value) return;
  // Converted to another engine: it opens on a connection of that engine.
  const db = d.value.database || t('dialogs:database.theDatabase');
  if (connectionId) newQuery(connectionId, '', text, t('dialogs:database.scriptNameConverted', { db }));
  else newQuery(d.value.connectionId, d.value.database, text, t('dialogs:database.scriptName', { db }));
}
function imported() {
  if (d.value) conns.loadObjects(d.value.connectionId, d.value.database, true);
}
</script>

<template>
  <template v-if="d && ready && driver">
    <ScriptGeneratorDialog
      v-if="d.kind === 'script' || d.kind === 'export'"
      :connection-id="d.connectionId"
      :database="d.database"
      :objects="objects.map((o) => ({ kind: o.kind, schema: o.schema, name: o.name }))"
      :kind-labels="kindLabels"
      :language="driver.language"
      :dialect="driver.dialect"
      :supports-data="true"
      :driver-id="driver.id"
      :title="d.kind === 'export' ? $t('dialogs:database.exportTitle', { db: d.database }) : $t('dialogs:database.scriptTitle', { db: d.database })"
      @open-script="openScript"
      @close="ui.closeDbDialog()"
    />
    <ImportDialog
      v-else-if="d.kind === 'import'"
      :connection-id="d.connectionId"
      :database="d.database"
      :tables="tables"
      :designer-available="!!driver.designer"
      :default-schema="defaultSchema"
      :dialect="driver.dialect"
      :data-types="driver.designer?.data_types ?? []"
      @imported="imported"
      @close="ui.closeDbDialog()"
    />
    <RunScriptFileDialog
      v-else-if="d.kind === 'run'"
      :connection-id="d.connectionId"
      :database="d.database"
      @close="ui.closeDbDialog()"
    />
  </template>
</template>
