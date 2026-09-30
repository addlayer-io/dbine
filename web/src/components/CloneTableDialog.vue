<script setup lang="ts">
import { computed, onBeforeUnmount, ref } from 'vue';
import { ElMessage } from 'element-plus';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { useTranslation } from 'i18next-vue';
import { errorKind, errorMessage } from '../api/client';
import { cloneTableApi, defaultCloneName, type ClonePhase, type CloneTableEvent, type CloneTableResult } from '../api/cloneTable';
import type { ObjectRef } from '../api/types';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { useUiStore } from '../stores/ui';

// "Clonar…" on a table (collection…): a copy next to it under a new name,
// proposed as <name>_yyyyMMdd_HHmmss, with its data and indexes. Whatever
// fails, the backend drops the clone; the original is only read.

const props = defineProps<{ connectionId: string; database: string; object: ObjectRef }>();
const emit = defineEmits<{ close: [] }>();

const { t } = useTranslation();
const conns = useConnectionsStore();
const ui = useUiStore();

// The longest name the engine takes, as `identifier_limit` in
// crates/dbine-transfer/src/clone_table.rs (bytes; characters on SQL
// Server, MySQL, MariaDB and TiDB); 0: no practical limit.
function nameLimit(): { max: number; chars: boolean } {
  const d = conns.driverOf(props.connectionId);
  if (!d) return { max: 0, chars: false };
  // Same as identifier_limit in crates/dbine-transfer/src/clone_table.rs.
  const byId: Record<string, number> = { firebird: 63, duckdb: 0, cassandra: 48, scylladb: 48, altibase: 40, starrocks: 1024, greptimedb: 0 };
  const byDialect: Record<string, number> = {
    postgres: 63, mysql: 64, access: 64, oracle: 128, hana: 127, sqlite: 0, clickhouse: 0, duckdb: 0, snowflake: 255, bigquery: 1024,
  };
  const max = byId[d.id] ?? byDialect[d.dialect] ?? (d.language === 'sql' || d.language === 'cql' ? 128 : 0);
  return { max, chars: d.dialect === 'mssql' || d.dialect === 'mysql' };
}

// The proposed name, its base cut so the whole fits the engine (a long
// table name would otherwise always be refused).
function proposedName(): string {
  const stamp = defaultCloneName('');
  const { max, chars } = nameLimit();
  const size = (s: string) => (chars ? [...s].length : new TextEncoder().encode(s).length);
  let base = props.object.name;
  if (max > 0) {
    const letters = [...base];
    while (letters.length && size(letters.join('') + stamp) > max) letters.pop();
    base = letters.join('');
  }
  return base + stamp;
}

const name = ref(proposedName());
const withData = ref(true);
const withIndexes = ref(true);
const running = ref(false);
const cancelling = ref(false);
const phase = ref<ClonePhase | null>(null);
const rowsDone = ref(0);
const rowsTotal = ref<number | null>(null);
const error = ref<string | null>(null);
const warnings = ref<string[]>([]);
const result = ref<CloneTableResult | null>(null);
let runId = '';
let unlisten: UnlistenFn | null = null;
onBeforeUnmount(() => unlisten?.());

const source = computed(() => (props.object.schema ? `${props.object.schema}.${props.object.name}` : props.object.name));
const valid = computed(() => name.value.trim().length > 0 && name.value.trim() !== props.object.name);
const percent = computed(() => (rowsTotal.value ? Math.min(100, Math.round((rowsDone.value / rowsTotal.value) * 100)) : 0));
const fmt = (n: number) => n.toLocaleString();

function onEvent(e: CloneTableEvent) {
  if (e.event === 'phase') phase.value = e.phase;
  else if (e.event === 'progress') {
    rowsDone.value = e.rows_done;
    rowsTotal.value = e.rows_total;
  } else if (e.level !== 'info') warnings.value.push(tb(e.text));
}

async function start() {
  if (!valid.value || running.value) return;
  running.value = true;
  cancelling.value = false;
  error.value = null;
  warnings.value = [];
  phase.value = null;
  rowsDone.value = 0;
  rowsTotal.value = null;
  runId = `clone-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  try {
    unlisten?.();
    unlisten = await listen<CloneTableEvent>('clone-table-progress', (e) => { if (e.payload.runId === runId) onEvent(e.payload); });
    const r = await cloneTableApi.run({
      runId, connectionId: props.connectionId, database: props.database, object: props.object,
      newName: name.value.trim(), withData: withData.value, withIndexes: withIndexes.value,
    });
    await conns.loadObjects(props.connectionId, props.database, true);
    ui.revealInExplorer({ connectionId: props.connectionId, database: props.database, object: r.table });
    ElMessage.success(t('cloneTable:done', { name: r.table.name, rows: fmt(r.rows) }));
    if (r.notes.length || warnings.value.length) result.value = r;
    else emit('close');
  } catch (e) {
    error.value = errorKind(e) === 'cancelled' ? t('cloneTable:cancelled') : errorMessage(e);
  } finally {
    running.value = false;
    unlisten?.();
    unlisten = null;
  }
}

async function cancel() {
  if (!running.value) return emit('close');
  cancelling.value = true;
  try {
    await cloneTableApi.cancel(runId);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}
</script>

<template>
  <el-dialog
    :model-value="true" :title="$t('cloneTable:title', { name: source })" width="480px" append-to-body
    :close-on-click-modal="false" :close-on-press-escape="!running" :show-close="!running" @close="emit('close')"
  >
    <template v-if="!result">
      <el-form label-position="top" @submit.prevent="start">
        <el-form-item :label="$t('cloneTable:name')">
          <el-input v-model="name" :disabled="running" autofocus @keyup.enter="start" />
          <div class="ct-help">{{ $t('cloneTable:nameHelp') }}</div>
        </el-form-item>
        <el-checkbox v-model="withData" :disabled="running">{{ $t('cloneTable:withData') }}</el-checkbox>
        <el-checkbox v-model="withIndexes" :disabled="running">{{ $t('cloneTable:withIndexes') }}</el-checkbox>
      </el-form>
      <div v-if="running" class="ct-progress">
        <div class="ct-phase">{{ phase ? $t(`cloneTable:phase.${phase}`) : $t('cloneTable:starting') }}</div>
        <el-progress
          :percentage="rowsTotal ? percent : 100" :indeterminate="!rowsTotal" :duration="2" :show-text="false" :stroke-width="4"
        />
        <div v-if="withData && (rowsDone || rowsTotal)" class="ct-rows">
          {{ rowsTotal != null ? $t('cloneTable:rowsOf', { done: fmt(rowsDone), total: fmt(rowsTotal) }) : $t('cloneTable:rows', { done: fmt(rowsDone) }) }}
        </div>
      </div>
      <el-alert v-if="error" type="error" :title="error" :closable="false" show-icon class="ct-alert" />
    </template>
    <template v-else>
      <p>{{ $t('cloneTable:done', { name: result.table.name, rows: fmt(result.rows) }) }}</p>
      <div class="ct-notes-title">{{ $t('cloneTable:notes') }}</div>
      <ul class="ct-notes">
        <li v-for="(n, i) in [...result.notes, ...warnings]" :key="i">{{ tb(n) }}</li>
      </ul>
    </template>
    <template #footer>
      <template v-if="result">
        <el-button type="primary" @click="emit('close')">{{ $t('common:close') }}</el-button>
      </template>
      <template v-else>
        <el-button :disabled="cancelling" @click="cancel">{{ running ? $t('cloneTable:stop') : $t('common:cancel') }}</el-button>
        <el-button type="primary" :loading="running" :disabled="!valid" @click="start">{{ $t('cloneTable:clone') }}</el-button>
      </template>
    </template>
  </el-dialog>
</template>

<style scoped lang="scss">
.ct-help { font-size: 12px; opacity: 0.7; margin-top: 4px; line-height: 1.4; }
.ct-progress { margin-top: 14px; }
.ct-phase { font-size: 12px; margin-bottom: 6px; }
.ct-rows { font-size: 12px; opacity: 0.8; margin-top: 6px; }
.ct-alert { margin-top: 14px; }
.ct-notes-title { font-weight: 600; margin: 8px 0 4px; }
.ct-notes { margin: 0; padding-left: 18px; font-size: 12px; line-height: 1.5; }
</style>
