<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { ElMessage } from 'element-plus';
import { invoke } from '@tauri-apps/api/core';
import { useTranslation } from 'i18next-vue';
import { tb } from '../i18n/backend';
import { errorMessage } from '../api/client';
import type { TableSchema } from '../api/schema-types';
import TableDesignerView from './TableDesignerView.vue';
import { newQuery } from '../composables/actions';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore, type DesignerTab } from '../stores/tabs';

// A designer tab: feeds the driver's designer with the schemas and the
// existing tables (foreign key targets) and reacts to what it does.

const props = defineProps<{ tab: DesignerTab }>();
const conns = useConnectionsStore();
const tabs = useTabsStore();
const { t } = useTranslation();

const driver = computed(() => conns.driverOf(props.tab.connectionId));
const tables = ref<TableSchema[]>([]);

onMounted(async () => {
  if (!driver.value?.designer?.foreign_keys && !driver.value?.designer?.schemas) return;
  try {
    if (!(await conns.ensureConnected(props.tab.connectionId))) return;
    tables.value = await invoke<TableSchema[]>('database_schema', { args: { connection_id: props.tab.connectionId, database: props.tab.database } });
  } catch { /* the designer works without the pickers */ }
});

const schemas = computed(() => {
  const set = new Set<string>(tables.value.map((t) => t.schema ?? '').filter(Boolean));
  if (props.tab.schema) set.add(props.tab.schema);
  return [...set].sort();
});
const existing = computed(() => tables.value.map((t) => ({ schema: t.schema, name: t.name, columns: t.columns.map((c) => c.name) })));

function created(table: TableSchema) {
  const label = driver.value?.designer?.label;
  // "Nueva tabla" → "tabla" (translated first, then without its "new").
  const object = label ? tb(label).replace(/^(nuev[oa]|new|nov[oa]|nouvel(le)?|nouveau|nuov[oa]) /i, '') : t('designer:object');
  ElMessage.success(t('designer:createdToast', { object, name: table.name }));
  conns.loadObjects(props.tab.connectionId, props.tab.database, true);
  tabs.close(props.tab.id);
  const kind = driver.value?.object_kinds.find((k) => k.id === table.kind);
  if (kind) tabs.openObject(props.tab.connectionId, props.tab.database, { kind: table.kind, schema: table.schema, name: table.name }, kind.browsable ? 'structure' : 'definition', false);
}

function openScript(ddl: string) {
  newQuery(props.tab.connectionId, props.tab.database, ddl).catch((e) => ElMessage.error(errorMessage(e)));
}
</script>

<template>
  <TableDesignerView
    v-if="driver?.designer"
    :connection-id="tab.connectionId"
    :database="tab.database"
    :spec="driver.designer"
    :schemas="schemas"
    :existing-tables="existing"
    :language="driver.language"
    :dialect="driver.dialect"
    @created="created"
    @open-script="openScript"
    @close="tabs.close(tab.id)"
  />
  <div v-else class="nm-content nm-muted">{{ $t('designer:noDesigner') }}</div>
</template>
