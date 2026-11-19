<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import type { ObjectRef } from '../api/types';
import { useConnectionsStore } from '../stores/connections';
import { runTask } from '../stores/tasks';
import type { VectorMeta } from '../api/types';
import { shownVectorMeta, vectorPreview } from '../composables/vector';
import VectorValue from './VectorValue.vue';

// "Generar datos de prueba…" (docs/test-data.md): one row per column
// with its generator ("auto" picks by name and type), its settings and the
// share of NULLs; a sample from the backend, and the run as a background
// task with progress and cancel.

interface Column { name: string; data_type: string; nullable: boolean; primary_key: boolean; auto_increment: boolean; foreign_key: string | null; auto: string }
interface Preview { table_columns: Column[]; generators: string[]; columns: string[]; rows: unknown[][] }
interface Spec { generator: string; params: Record<string, string>; null_percent: number }

const props = defineProps<{ connectionId: string; database: string; table: ObjectRef }>();
const emit = defineEmits<{ close: [] }>();
const { t } = useTranslation();
const conns = useConnectionsStore();

const open = ref(true);
const loading = ref(true);
const error = ref<string | null>(null);
const preview = ref<Preview | null>(null);
const specs = ref<Record<string, Spec>>({});
const count = ref(100);
const seed = ref(Math.floor(Math.random() * 1e9));

/** The settings each generator takes. */
const PARAMS: Record<string, string[]> = {
  fixed: ['value'],
  sequence: ['start', 'step'],
  integer: ['min', 'max'],
  decimal: ['min', 'max', 'scale'],
  date: ['from', 'to'],
  datetime: ['from', 'to'],
  text: ['min', 'max'],
  list: ['values'],
  vector: ['dims'],
};

/** A column's settings: a vector's dimensions only where its type doesn't
 *  fix them (`vector(1536)` takes vectors of 1536 numbers, nothing else). */
function paramsOf(c: Column): string[] {
  const g = specs.value[c.name]?.generator ?? 'auto';
  if (g === 'vector' && shownVectorMeta({ type_name: c.data_type })?.dims) return [];
  return PARAMS[g] ?? [];
}

function payload() {
  return Object.entries(specs.value)
    .filter(([, s]) => s.generator !== 'auto' || s.null_percent > 0)
    .map(([name, s]) => ({ name, generator: s.generator, params: s.params, null_percent: s.null_percent }));
}

async function load() {
  loading.value = true;
  error.value = null;
  try {
    preview.value = await invoke<Preview>('datagen_preview', {
      args: { connection_id: props.connectionId, database: props.database, table: props.table, columns: payload(), rows: 8, seed: seed.value },
    });
    for (const c of preview.value.table_columns) specs.value[c.name] ??= { generator: 'auto', params: {}, null_percent: 0 };
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    loading.value = false;
  }
}
onMounted(load);

function reshuffle() {
  seed.value = Math.floor(Math.random() * 1e9);
  load();
}

function cell(v: unknown): string {
  if (v === null || v === undefined) return 'NULL';
  return typeof v === 'object' ? JSON.stringify(v) : String(v);
}
/** Each sample column's vectors, by the table column's type; else, where
 *  the type doesn't say it, dense ones when the backend fills the column
 *  with vectors (its "auto" or the chosen generator is the vector one). */
const sampleVectors = computed(() => (preview.value?.columns ?? []).map((name) => {
  const c = preview.value?.table_columns.find((x) => x.name === name);
  const typed = shownVectorMeta({ type_name: c?.data_type });
  if (typed || !c) return { meta: typed, shown: typed };
  const filled = c.auto === 'vector' || specs.value[name]?.generator === 'vector';
  // Its element isn't known: the badge says only the dimensions.
  return filled ? { meta: { element: 'f32' } as VectorMeta, shown: null } : { meta: null, shown: null };
}));
/** A sample vector as its first numbers and dims; null: shown as text. */
function sampleVector(v: unknown, j: number) {
  const meta = sampleVectors.value[j]?.meta;
  return meta && v !== null && v !== undefined ? vectorPreview(typeof v === 'string' ? v : JSON.stringify(v), meta) : null;
}

const genLabel = (g: string) => t(`dataGen:gen.${g}`);
const options = computed(() => ['auto', ...(preview.value?.generators ?? [])]);

async function generate() {
  if (!preview.value) return;
  const conn = conns.byId(props.connectionId);
  try {
    await ElMessageBox.confirm(
      t('dataGen:confirm', { count: count.value.toLocaleString(), table: props.table.name, connection: conn?.name ?? '' }),
      t('dataGen:title'),
      { confirmButtonText: t('dataGen:generate'), cancelButtonText: t('common:cancel'), type: 'warning' },
    );
  } catch {
    return;
  }
  const id = `${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
  const args = {
    connection_id: props.connectionId, database: props.database, table: props.table, columns: payload(),
    rows: count.value, seed: seed.value, gen_id: id, batch: 500,
  };
  const total = count.value;
  runTask<number>({
    kind: 'generate-data',
    title: t('dataGen:task', { count: total.toLocaleString(), table: props.table.name }),
    connectionId: props.connectionId, database: props.database, background: true,
    cancel: () => invoke('cancel_query', { args: { session_id: `datagen:${id}` } }),
    run: async (task) => {
      await task.listen<{ id: string; rows: number; total: number }>('datagen-progress', (e) => {
        if (e.payload.id === id) task.progress({ done: e.payload.rows, total: e.payload.total, unit: 'rows' });
      });
      const r = await invoke<{ rows: number }>('datagen_run', { args });
      return r.rows;
    },
    summary: (n) => t('dataGen:done', { count: n.toLocaleString() }),
  });
  ElMessage.info(t('dataGen:started'));
  close();
}

function close() {
  open.value = false;
  emit('close');
}
</script>

<template>
  <el-dialog :model-value="open" :title="$t('dataGen:titleFor', { table: table.name })" width="860px" append-to-body @close="close">
    <div v-if="loading && !preview" class="dg-empty"><el-icon class="is-loading" :size="22"><ei-loading /></el-icon></div>
    <el-alert v-if="error" type="error" :title="error" :closable="false" show-icon class="dg-alert" />
    <template v-if="preview">
      <div class="dg-top">
        <span>{{ $t('dataGen:rows') }}</span>
        <el-input-number v-model="count" :min="1" :max="10000000" :step="100" size="small" controls-position="right" />
        <div class="dg-spacer" />
        <el-button size="small" :loading="loading" @click="load">{{ $t('dataGen:refresh') }}</el-button>
        <el-button size="small" :loading="loading" @click="reshuffle">{{ $t('dataGen:reshuffle') }}</el-button>
      </div>
      <div class="dg-cols">
        <table>
          <thead>
            <tr><th>{{ $t('dataGen:column') }}</th><th>{{ $t('dataGen:type') }}</th><th>{{ $t('dataGen:generator') }}</th><th>{{ $t('dataGen:settings') }}</th><th>{{ $t('dataGen:nulls') }}</th></tr>
          </thead>
          <tbody>
            <tr v-for="c in preview.table_columns" :key="c.name">
              <td>
                <strong>{{ c.name }}</strong>
                <span v-if="c.primary_key" class="dg-tag">PK</span>
                <span v-if="c.foreign_key" class="dg-tag" :title="c.foreign_key">FK</span>
              </td>
              <td class="nm-muted">{{ c.data_type }}</td>
              <td>
                <el-select v-model="specs[c.name].generator" size="small" filterable style="width: 190px">
                  <el-option v-for="g in options" :key="g" :value="g" :label="g === 'auto' ? `${genLabel('auto')} · ${genLabel(c.auto)}` : genLabel(g)" />
                </el-select>
              </td>
              <td class="dg-params">
                <el-input
                  v-for="p in paramsOf(c)"
                  :key="p"
                  v-model="specs[c.name].params[p]"
                  size="small"
                  :placeholder="$t(`dataGen:param.${p}`)"
                  :style="{ width: p === 'values' || p === 'value' ? '180px' : '90px' }"
                />
              </td>
              <td>
                <el-input-number v-if="c.nullable && !c.primary_key" v-model="specs[c.name].null_percent" :min="0" :max="100" size="small" controls-position="right" style="width: 90px" />
              </td>
            </tr>
          </tbody>
        </table>
      </div>
      <div class="dg-sample-title">{{ $t('dataGen:sample') }}</div>
      <div class="dg-sample">
        <table>
          <thead><tr><th v-for="c in preview.columns" :key="c">{{ c }}</th></tr></thead>
          <tbody>
            <tr v-for="(r, i) in preview.rows" :key="i">
              <td v-for="(v, j) in r" :key="j" :class="{ null: v === null }">
                <VectorValue v-if="sampleVector(v, j)" :preview="sampleVector(v, j)!" :meta="sampleVectors[j]?.shown" />
                <template v-else>{{ cell(v) }}</template>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </template>
    <template #footer>
      <el-button @click="close">{{ $t('common:cancel') }}</el-button>
      <el-button type="primary" :disabled="!preview || !!error" @click="generate">{{ $t('dataGen:generate') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.dg-empty { display: flex; justify-content: center; padding: 40px; color: var(--nm-text-dim); }
.dg-alert { margin-bottom: 10px; }
.dg-top { display: flex; align-items: center; gap: 8px; margin-bottom: 10px; font-size: 12.5px; }
.dg-top .el-button { margin: 0; }
.dg-spacer { flex: 1; }
.dg-cols, .dg-sample { max-height: 300px; overflow: auto; border: 1px solid var(--nm-border-soft); border-radius: 4px; }
.dg-sample { max-height: 200px; }
table { width: 100%; border-collapse: collapse; font-size: 12px; }
th { position: sticky; top: 0; z-index: 1; text-align: left; font-weight: 600; padding: 5px 8px; background: var(--nm-bg-elev); color: var(--nm-text-dim); border-bottom: 1px solid var(--nm-border); white-space: nowrap; }
td { padding: 4px 8px; border-bottom: 1px solid var(--nm-border-soft); color: var(--nm-text); white-space: nowrap; }
.dg-params { display: flex; gap: 4px; }
.dg-tag { margin-left: 6px; font-size: 10px; padding: 0 5px; border-radius: 6px; background: color-mix(in srgb, var(--nm-accent) 22%, transparent); }
.dg-sample-title { margin: 12px 0 4px; font-size: 12px; color: var(--nm-text-dim); }
.dg-sample td { font-family: var(--nm-mono); max-width: 220px; overflow: hidden; text-overflow: ellipsis; }
.dg-sample td.null { color: var(--nm-text-muted); font-style: italic; }
</style>
