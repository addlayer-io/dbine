<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import type { DatabaseProperties, Field } from '../api/types';
import { tb } from '../i18n/backend';
import { newQuery } from '../composables/actions';
import { useConnectionsStore } from '../stores/connections';
import OptionField from './OptionField.vue';

// "Propiedades" of a database (docs/database-properties.md): what the
// engine lets change, in tabs (General, then each group), with read-only
// facts on top of each tab. "Aplicar" shows the script of what changed and
// the warnings of disruptive changes, and runs it only after confirmation.

const props = defineProps<{ connectionId: string; database: string }>();
const emit = defineEmits<{ close: [] }>();
const { t } = useTranslation();
const conns = useConnectionsStore();

const open = ref(true);
const loading = ref(true);
const error = ref<string | null>(null);
const data = ref<DatabaseProperties | null>(null);
const values = ref<Record<string, string>>({});
const script = ref<string | null>(null);
const busy = ref(false);
const readOnly = computed(() => !!conns.byId(props.connectionId)?.config.read_only);

const GENERAL = 'General';
const tabOf = (g: string | undefined) => g || GENERAL;
const tabs = computed(() => {
  const out = [GENERAL];
  for (const f of data.value?.fields ?? []) if (!out.includes(tabOf(f.group))) out.push(tabOf(f.group));
  for (const i of data.value?.info ?? []) if (!out.includes(tabOf(i.group))) out.push(tabOf(i.group));
  return out;
});
const tab = ref(GENERAL);
const choices = computed(() => Object.fromEntries((data.value?.choices ?? []).map((c) => [c.key, c])));

function visible(f: Field): boolean {
  return !f.when || f.when.values.includes(values.value[f.when.key] ?? '');
}
const shown = computed(() => (data.value?.fields ?? []).filter((f) => visible(f) && tabOf(f.group) === tab.value));
const facts = computed(() => (data.value?.info ?? []).filter((i) => tabOf(i.group) === tab.value));

/** Field key → new value, for what differs from what was read. */
const changes = computed(() => {
  const out: Record<string, string> = {};
  for (const f of data.value?.fields ?? []) {
    const now = (values.value[f.key] ?? '').trim();
    if (now !== (data.value?.values[f.key] ?? '').trim() && visible(f)) out[f.key] = now;
  }
  return out;
});
const changed = computed(() => Object.keys(changes.value).length > 0);

async function load() {
  loading.value = true;
  error.value = null;
  try {
    data.value = await invoke<DatabaseProperties>('database_properties', { args: { connection_id: props.connectionId, database: props.database } });
    values.value = { ...data.value.values };
    script.value = null;
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    loading.value = false;
  }
}
onMounted(load);

async function buildScript(): Promise<string | null> {
  try {
    return await invoke<string>('alter_database_script', { args: { connection_id: props.connectionId, database: props.database, changes: changes.value } });
  } catch (e) {
    ElMessage.error(errorMessage(e));
    return null;
  }
}

async function showScript() {
  script.value = await buildScript();
}

async function openInQuery() {
  if (!script.value) return;
  await newQuery(props.connectionId, '', script.value, t('dbProperties:scriptName', { name: props.database }));
  close();
}

async function apply() {
  const sql = await buildScript();
  if (sql === null) return;
  const warnings = Object.keys(changes.value).map((k) => data.value?.warnings[k]).filter((w): w is string => !!w);
  const body = [
    warnings.length ? `${t('dbProperties:warningsTitle')}\n${warnings.map((w) => `• ${tb(w)}`).join('\n')}\n` : '',
    sql,
  ].filter(Boolean).join('\n');
  try {
    await ElMessageBox.confirm(body, t('dbProperties:confirmTitle', { name: props.database }), {
      confirmButtonText: t('dbProperties:apply'),
      cancelButtonText: t('common:cancel'),
      type: warnings.length ? 'warning' : 'info',
      customStyle: { whiteSpace: 'pre-wrap', maxWidth: '640px' },
    });
  } catch {
    return;
  }
  busy.value = true;
  try {
    await invoke('alter_database', { args: { connection_id: props.connectionId, database: props.database, changes: changes.value } });
    ElMessage.success(t('dbProperties:applied', { name: props.database }));
    await load();
  } catch (e) {
    ElMessage.error({ message: errorMessage(e), duration: 8000, showClose: true });
  } finally {
    busy.value = false;
  }
}

function close() {
  open.value = false;
  emit('close');
}
</script>

<template>
  <el-dialog :model-value="open" :title="$t('dbProperties:title', { name: database })" width="700px" append-to-body @close="close">
    <div v-if="loading" class="dp-empty"><el-icon class="is-loading" :size="22"><ei-loading /></el-icon></div>
    <el-alert v-else-if="error" type="error" :title="error" :closable="false" show-icon />
    <el-form v-else-if="data" label-position="top" @submit.prevent>
      <el-tabs v-if="tabs.length > 1" v-model="tab" class="dp-tabs">
        <el-tab-pane v-for="g in tabs" :key="g" :label="tb(g)" :name="g" />
      </el-tabs>
      <dl v-if="facts.length" class="dp-facts">
        <template v-for="i in facts" :key="i.label">
          <dt>{{ tb(i.label) }}</dt>
          <dd :title="i.value">{{ tb(i.value) }}</dd>
        </template>
      </dl>
      <div class="dp-grid">
        <OptionField
          v-for="f in shown"
          :key="f.key"
          v-model="values[f.key]"
          :f="f"
          :choices="choices[f.key]"
          :placeholder="choices[f.key]?.default ? t('createDatabase:serverDefault', { value: choices[f.key]!.default }) : ''"
          :disabled="readOnly"
          @update:model-value="script = null"
        />
      </div>
      <p v-if="!shown.length && !facts.length" class="nm-muted dp-none">{{ $t('dbProperties:nothing') }}</p>
      <p v-if="readOnly" class="nm-muted dp-none">{{ $t('dbProperties:readOnly') }}</p>

      <div v-if="script !== null" class="dp-script">
        <div class="dp-script-head">
          <strong>{{ $t('createDatabase:script') }}</strong>
          <el-button size="small" text @click="openInQuery">{{ $t('createDatabase:openInQuery') }}</el-button>
        </div>
        <pre>{{ script || $t('dbProperties:noChanges') }}</pre>
      </div>
    </el-form>
    <template #footer>
      <el-button @click="close">{{ $t('common:close') }}</el-button>
      <el-button v-if="!readOnly && data?.fields.length" :disabled="!changed" @click="showScript">{{ $t('createDatabase:showScript') }}</el-button>
      <el-button v-if="!readOnly && data?.fields.length" type="primary" :disabled="!changed" :loading="busy" @click="apply">{{ $t('dbProperties:apply') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.dp-empty { display: flex; justify-content: center; padding: 40px; color: var(--nm-text-dim); }
.dp-tabs { margin-top: -6px; }
.dp-tabs :deep(.el-tabs__content) { display: none; }
.dp-facts {
  display: grid; grid-template-columns: minmax(0, max-content) minmax(0, 1fr); gap: 4px 14px; margin: 0 0 14px;
  padding: 8px 10px; border: 1px solid var(--nm-border-soft); border-radius: 4px; background: var(--nm-bg-elev); font-size: 12px;
}
.dp-facts dt { color: var(--nm-text-dim); overflow-wrap: anywhere; }
.dp-facts dd { margin: 0; color: var(--nm-text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
/* minmax(0, …): long labels (file names without spaces) wrap instead of widening the column past the dialog. */
.dp-grid { display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 1fr); gap: 0 14px; }
.dp-grid > * { min-width: 0; }
.dp-grid .wide { grid-column: 1 / -1; }
.dp-grid :deep(.el-form-item) { margin-bottom: 12px; }
.dp-grid :deep(.el-form-item__label) { display: flex; align-items: center; gap: 4px; line-height: 1.3; margin-bottom: 4px; overflow-wrap: anywhere; }
.dp-none { font-size: 12px; margin: 4px 0; }
.dp-script { margin-top: 12px; border: 1px solid var(--nm-border-soft); border-radius: 4px; }
.dp-script-head { display: flex; align-items: center; justify-content: space-between; padding: 4px 6px 4px 10px; border-bottom: 1px solid var(--nm-border-soft); font-size: 12px; }
.dp-script pre {
  margin: 0; padding: 8px 10px; max-height: 220px; overflow: auto; white-space: pre-wrap; word-break: break-word;
  font-family: var(--nm-mono); font-size: 12px; color: var(--nm-text);
}
</style>
