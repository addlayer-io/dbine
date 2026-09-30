<script setup lang="ts">
import { computed, onBeforeUnmount, reactive, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { save } from '@tauri-apps/plugin-dialog';
import { errorMessage } from '../api/client';
import { locale, t } from '../i18n';
import type { Cell, ResultColumn } from '../api/types';
import {
  EXPORT_FORMATS, defaultOptions, exportApi, fileName, type ExportFormat, type ExportOptions,
} from '../composables/export';

// Advanced export: format + its options, and what to export — the rows
// already loaded, or every row (the script runs again, read-only, and
// streams to the file). Shows progress and can cancel.

export interface ExportSource {
  connectionId: string;
  database: string;
  sql: string;
  resultIndex: number;
}

const props = defineProps<{
  columns: ResultColumn[];
  rows: Cell[][];
  totalRows: number;
  truncated: boolean;
  source: ExportSource | null;
  title: string;
  dialect: string;
  initialFormat?: ExportFormat;
}>();
const emit = defineEmits<{ close: [] }>();

const opts = reactive<ExportOptions>(defaultOptions(props.initialFormat ?? 'csv', props.title, props.dialect));
const scope = ref<'loaded' | 'all'>(props.truncated && props.source ? 'all' : 'loaded');
const running = ref(false);
const progress = ref(0);
const exportId = crypto.randomUUID();
let unlisten: UnlistenFn | null = null;

watch(() => opts.format, (f) => {
  // Keep what's typed, reset what depends on the format.
  opts.delimiter = '';
  opts.bom = f === 'csv_excel';
  opts.crlf = f === 'csv_excel';
});

const isCsv = computed(() => ['csv', 'csv_semicolon', 'csv_excel', 'tsv'].includes(opts.format));

async function run() {
  const fmt = EXPORT_FORMATS.find((f) => f.id === opts.format)!;
  let path: string | null = null;
  try {
    path = await save({ defaultPath: fileName(props.title, opts.format), filters: [{ name: fmt.filter, extensions: [fmt.ext] }] });
  } catch { /* no dialog outside Tauri */ }
  if (!path) return;
  running.value = true;
  progress.value = 0;
  try {
    let result;
    if (scope.value === 'all' && props.source) {
      unlisten = await listen<{ id: string; rows: number }>('export-progress', (e) => {
        if (e.payload.id === exportId) progress.value = e.payload.rows;
      });
      result = await exportApi.query({ exportId, ...props.source, path, options: { ...opts } });
    } else {
      result = await exportApi.rows(path, { ...opts }, props.columns, props.rows);
    }
    ElMessage.success({ message: t('results:exportDialog.done', { count: result.rows, rows: result.rows.toLocaleString(locale()), seconds: (result.elapsed_ms / 1000).toLocaleString(locale(), { minimumFractionDigits: 1, maximumFractionDigits: 1 }) }), duration: 3000 });
    emit('close');
  } catch (e) {
    ElMessage.error({ message: errorMessage(e), duration: 6000 });
  } finally {
    running.value = false;
    unlisten?.();
    unlisten = null;
  }
}

function cancel() {
  if (running.value && scope.value === 'all') exportApi.cancel(exportId).catch(() => {});
  else emit('close');
}
onBeforeUnmount(() => unlisten?.());
</script>

<template>
  <el-dialog :model-value="true" :title="$t('results:exportDialog.title')" width="560px" append-to-body :close-on-click-modal="false" @close="cancel">
    <el-form label-position="top" :disabled="running" @submit.prevent>
      <el-form-item :label="$t('results:exportDialog.format')">
        <el-select v-model="opts.format" style="width: 100%">
          <el-option v-for="f in EXPORT_FORMATS" :key="f.id" :label="$t(`results:export.format.${f.id}`, f.label)" :value="f.id" />
        </el-select>
      </el-form-item>

      <el-form-item :label="$t('results:exportDialog.rows')">
        <el-radio-group v-model="scope" class="ed-scope">
          <el-radio value="loaded">{{ $t('results:exportDialog.loaded', { rows: rows.length.toLocaleString(locale()) }) }}</el-radio>
          <el-radio value="all" :disabled="!source">
            {{ $t('results:exportDialog.all', { total: truncated ? ` (${totalRows.toLocaleString(locale())}+)` : '' }) }}
          </el-radio>
        </el-radio-group>
        <div v-if="scope === 'all'" class="ed-help">
          {{ $t('results:exportDialog.allHelp') }}
        </div>
        <div v-else-if="truncated" class="ed-help warn">
          {{ $t('results:exportDialog.truncated', { rows: rows.length.toLocaleString(locale()) }) }}
        </div>
      </el-form-item>

      <!-- per format -->
      <div class="nm-grid-2">
        <template v-if="isCsv || opts.format === 'xlsx'">
          <el-form-item><el-checkbox v-model="opts.header">{{ $t('results:exportDialog.header') }}</el-checkbox></el-form-item>
        </template>
        <template v-if="isCsv">
          <el-form-item :label="$t('results:exportDialog.delimiter')">
            <el-input v-model="opts.delimiter" maxlength="1" :placeholder="opts.format === 'tsv' ? $t('results:exportDialog.tab') : opts.format === 'csv' ? ',' : ';'" />
          </el-form-item>
          <el-form-item :label="$t('results:exportDialog.nullText')">
            <el-input v-model="opts.null_text" :placeholder="$t('results:exportDialog.empty')" />
          </el-form-item>
          <el-form-item><el-checkbox v-model="opts.quote_all">{{ $t('results:exportDialog.quoteAll') }}</el-checkbox></el-form-item>
          <el-form-item><el-checkbox v-model="opts.bom">{{ $t('results:exportDialog.bom') }}</el-checkbox></el-form-item>
          <el-form-item><el-checkbox v-model="opts.crlf">{{ $t('results:exportDialog.crlf') }}</el-checkbox></el-form-item>
        </template>
        <template v-if="opts.format === 'json'">
          <el-form-item><el-checkbox v-model="opts.pretty">{{ $t('results:exportDialog.pretty') }}</el-checkbox></el-form-item>
        </template>
        <template v-if="opts.format === 'sql'">
          <el-form-item :label="$t('results:exportDialog.targetTable')">
            <el-input v-model="opts.table" :placeholder="$t('results:exportDialog.targetTablePlaceholder')" />
          </el-form-item>
          <el-form-item :label="$t('results:exportDialog.rowsPerInsert')">
            <el-input-number v-model="opts.rows_per_insert" :min="1" :max="10000" controls-position="right" style="width: 100%" />
          </el-form-item>
          <el-form-item :label="$t('results:exportDialog.quote')">
            <el-select v-model="opts.quote" style="width: 100%">
              <el-option :label="$t('results:exportDialog.quoteDouble')" value="double" />
              <el-option :label="$t('results:exportDialog.quoteBracket')" value="bracket" />
              <el-option :label="$t('results:exportDialog.quoteBacktick')" value="backtick" />
            </el-select>
          </el-form-item>
        </template>
        <template v-if="opts.format === 'xlsx'">
          <el-form-item :label="$t('results:exportDialog.sheet')">
            <el-input v-model="opts.sheet" maxlength="31" />
          </el-form-item>
        </template>
        <template v-if="opts.format === 'xml'">
          <el-form-item :label="$t('results:exportDialog.xmlRoot')"><el-input v-model="opts.xml_root" /></el-form-item>
          <el-form-item :label="$t('results:exportDialog.xmlRow')"><el-input v-model="opts.xml_row" /></el-form-item>
        </template>
      </div>
    </el-form>

    <div v-if="running" class="ed-progress">
      <el-icon class="is-loading"><ei-loading /></el-icon>
      <span>{{ scope === 'all' ? $t('results:exportDialog.progressRows', { count: progress, rows: progress.toLocaleString(locale()) }) : $t('results:exportDialog.progress') }}</span>
    </div>

    <template #footer>
      <el-button @click="cancel">{{ running && scope === 'all' ? $t('results:exportDialog.cancelExport') : $t('common:cancel') }}</el-button>
      <el-button type="primary" :loading="running" @click="run">{{ $t('results:exportDialog.submit') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.ed-scope { display: flex; flex-direction: column; align-items: flex-start; gap: 4px; }
.ed-help { font-size: 11.5px; color: var(--nm-text-dim); line-height: 1.45; margin-top: 4px; }
.ed-help.warn { color: var(--nm-warning); }
.ed-progress { display: flex; align-items: center; gap: 8px; font-size: 12.5px; color: var(--nm-text); margin-top: 6px; }
</style>
