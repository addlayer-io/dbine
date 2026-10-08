<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { ElMessage } from 'element-plus';
import { invoke } from '@tauri-apps/api/core';
import { useTranslation } from 'i18next-vue';
import { tb } from '../i18n/backend';
import { errorMessage } from '../api/client';
import type { TableSchema } from '../api/schema-types';
import { compareApi, type CodeObject, type SyncScript } from '../api/compare';
import TableDesignerView from './TableDesignerView.vue';
import RenameDialog from '../components/RenameDialog.vue';
import { newQuery } from '../composables/actions';
import { renameAllowed, renameSpec, type RenameDialogTarget } from '../composables/rename';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore, type DesignerTab } from '../stores/tabs';

// A designer tab: feeds the driver's designer with the schemas and the
// existing tables (foreign key targets) and reacts to what it does. With
// `tab.table` it's "Modificar tabla…": the designer edits that table, and a
// column renamed in it goes through "Renombrar" (impact and script, not run).

const props = defineProps<{ tab: DesignerTab }>();
const conns = useConnectionsStore();
const tabs = useTabsStore();
const { t } = useTranslation();

const driver = computed(() => conns.driverOf(props.tab.connectionId));
const tables = ref<TableSchema[]>([]);

/** Edit mode: the table being edited, as the database has it now. */
const editing = computed(() => props.tab.table ?? null);
const loading = ref(!!props.tab.table);
/** Edit mode: the database's code objects, so the ALTER recreates the views
 *  over the table and the triggers of a rebuilt table (as "Comparar esquemas"). */
const codeObjects = ref<CodeObject[]>([]);
const loadError = ref<string | null>(null);
const initial = computed(() => {
  const e = editing.value;
  return e ? tables.value.find((t) => t.name === e.name && (t.schema ?? null) === (e.schema ?? null)) : undefined;
});

onMounted(async () => {
  if (!editing.value && !driver.value?.designer?.foreign_keys && !driver.value?.designer?.schemas) return;
  try {
    if (!(await conns.ensureConnected(props.tab.connectionId))) return;
    if (editing.value) {
      const m = await compareApi.load(props.tab.connectionId, props.tab.database);
      tables.value = m.tables;
      codeObjects.value = m.objects;
    } else {
      tables.value = await invoke<TableSchema[]>('database_schema', { args: { connection_id: props.tab.connectionId, database: props.tab.database } });
    }
  } catch (e) {
    // A new table works without the pickers; an edit needs the table.
    if (editing.value) loadError.value = errorMessage(e);
  } finally {
    loading.value = false;
  }
});

const columnTarget = (column: string): RenameDialogTarget => ({
  connectionId: props.tab.connectionId, database: props.tab.database,
  target: { what: 'column', table: { kind: initial.value?.kind ?? 'table', schema: editing.value?.schema ?? null, name: editing.value?.name ?? '' }, column },
});
const canRenameColumns = computed(() => !!editing.value && renameAllowed(renameSpec(props.tab.connectionId), columnTarget('').target));

/** A column renamed in the designer: the "Renombrar" dialog shows its impact
 *  and hands back the script (it doesn't run it). */
type Collected = { script: SyncScript; rewritten: CodeObject[] };
const renameAsk = ref<{ target: RenameDialogTarget; to: string; done: (r: Collected | null) => void } | null>(null);
function resolveRename(from: string, to: string): Promise<Collected | null> {
  return new Promise((done) => { renameAsk.value = { target: columnTarget(from), to, done }; });
}
function renameCollected(script: SyncScript, rewritten: CodeObject[]) {
  renameAsk.value?.done({ script, rewritten });
  renameAsk.value = null;
}
function renameClosed() {
  renameAsk.value?.done(null);
  renameAsk.value = null;
}

function altered(table: TableSchema) {
  conns.loadObjects(props.tab.connectionId, props.tab.database, true);
  tabs.close(props.tab.id);
  const kind = driver.value?.object_kinds.find((k) => k.id === table.kind);
  if (kind) tabs.openObject(props.tab.connectionId, props.tab.database, { kind: table.kind, schema: table.schema, name: table.name }, kind.browsable ? 'structure' : 'definition', false);
}

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
  <div v-if="editing && loading" class="nm-content nm-muted">{{ $t('designer:alter.loading', { name: editing.name }) }}</div>
  <div v-else-if="editing && (loadError || !initial)" class="nm-content nm-muted">{{ loadError ?? $t('designer:alter.notFound', { name: editing.name }) }}</div>
  <div v-else-if="editing && !driver?.supports_schema_sync" class="nm-content nm-muted">{{ $t('designer:alter.unsupported') }}</div>
  <TableDesignerView
    v-else-if="driver?.designer"
    :connection-id="tab.connectionId"
    :database="tab.database"
    :spec="driver.designer"
    :schemas="schemas"
    :existing-tables="existing"
    :language="driver.language"
    :dialect="driver.dialect"
    :initial="initial"
    :alter="!!editing"
    :code-objects="codeObjects"
    :resolve-rename="canRenameColumns ? resolveRename : null"
    :rename-blocked="editing && !canRenameColumns ? $t('designer:alter.noColumnRename') : null"
    :atomic="!!renameSpec(tab.connectionId)?.transactional"
    @created="created"
    @altered="altered"
    @open-script="openScript"
    @close="tabs.close(tab.id)"
  />
  <div v-else class="nm-content nm-muted">{{ $t('designer:noDesigner') }}</div>
  <RenameDialog
    v-if="renameAsk"
    :target="renameAsk.target"
    collect
    :initial-name="renameAsk.to"
    @collected="renameCollected"
    @close="renameClosed"
  />
</template>
