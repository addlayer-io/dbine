<script setup lang="ts">
import { computed, markRaw, onBeforeUnmount, reactive, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { save } from '@tauri-apps/plugin-dialog';
import { errorKind, errorMessage } from '../api/client';
import { locale, t } from '../i18n';
import type { Cell, ResultColumn } from '../api/types';
import {
  EXPORT_FORMATS, defaultOptions, exportApi, fileName, type ExportFormat, type ExportOptions, type ExportResult,
} from '../composables/export';
import { startTask, useTasksStore, type TaskHandle } from '../stores/tasks';

// Advanced export: format + its options, and what to export — the rows
// already loaded, or every row (the script runs again, read-only, and
// streams to the file). Shows progress and can cancel. The run is a task
// (stores/tasks.ts): "Seguir en segundo plano" closes the dialog and the
// export goes on, with its progress in the Tareas panel.

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
const tasks = useTasksStore();

const opts = reactive<ExportOptions>(defaultOptions(props.initialFormat ?? 'csv', props.title, props.dialect));
const scope = ref<'loaded' | 'all'>(props.truncated && props.source ? 'all' : 'loaded');
const running = ref(false);
const cancelling = ref(false);
const progress = ref(0);
/** Rows the engine's estimated plan expects (streamed export only). */
const estimate = ref<number | null>(null);
let task: TaskHandle<ExportResult> | null = null;
let mounted = true;

onBeforeUnmount(() => {
  mounted = false;
  // Gone while exporting (its tab closed…): the export goes on and says so
  // when it ends; there's no dialog left for "Ver detalle" to show.
  if (running.value && task) {
    task.background();
    task.setReopen(undefined);
  }
});

watch(() => opts.format, (f) => {
  // Keep what's typed, reset what depends on the format.
  opts.delimiter = '';
  opts.bom = f === 'csv_excel';
  opts.crlf = f === 'csv_excel';
});

const isCsv = computed(() => ['csv', 'csv_semicolon', 'csv_excel', 'tsv'].includes(opts.format));

async function run() {
  if (running.value) return;
  const fmt = EXPORT_FORMATS.find((f) => f.id === opts.format)!;
  let path: string | null = null;
  try {
    path = await save({ defaultPath: fileName(props.title, opts.format), filters: [{ name: fmt.filter, extensions: [fmt.ext] }] });
  } catch { /* no dialog outside Tauri */ }
  if (!path) return;
  running.value = true;
  cancelling.value = false;
  progress.value = 0;
  estimate.value = null;
  const all = scope.value === 'all' && props.source ? { ...props.source } : null;
  const options = { ...opts };
  const exportId = crypto.randomUUID();
  const h = startTask<ExportResult>({
    kind: 'export',
    title: t('tasks:genImportExport.export', { file: path.split(/[\\/]/).pop() ?? path }),
    connectionId: all?.connectionId, database: all?.database,
    // Only the streamed export has a cancel path; the loaded rows are written in one go.
    // A cancel that doesn't land re-enables the dialog's button too.
    cancel: all ? () => { cancelling.value = true; return exportApi.cancel(exportId).catch((e) => { cancelling.value = false; throw e; }); } : undefined,
    // While the dialog is up, "Ver detalle" just brings it back into view.
    reopen: () => {},
  });
  task = markRaw(h);
  try {
    let result: ExportResult;
    if (all) {
      // `total`: the plan's row estimate, when the engine gives one.
      await h.listen<{ id: string; rows: number; total?: number }>('export-progress', (e) => {
        if (e.payload.id !== exportId) return;
        const done = Math.max(progress.value, e.payload.rows);
        progress.value = done;
        if (e.payload.total != null) estimate.value = e.payload.total;
        const total = estimate.value;
        // An estimate the export went past is no longer a total.
        h.progress({ done, total: total != null && done <= total ? total : undefined, unit: 'rows' });
      });
      result = await exportApi.query({ exportId, ...all, path, options });
    } else {
      h.progress({ total: props.rows.length, unit: 'rows' });
      result = await exportApi.rows(path, options, props.columns, props.rows, props.source?.connectionId);
    }
    const rows = result.rows.toLocaleString(locale());
    h.setReopen(undefined);
    h.finish(result, t('tasks:dialogs.rows', { count: result.rows, n: rows }));
    if (mounted) {
      ElMessage.success({ message: t('results:exportDialog.done', { count: result.rows, rows, seconds: (result.elapsed_ms / 1000).toLocaleString(locale(), { minimumFractionDigits: 1, maximumFractionDigits: 1 }) }), duration: 3000 });
      emit('close');
    }
  } catch (e) {
    h.setReopen(undefined);
    const stopped = errorKind(e) === 'cancelled' || h.isCancelling;
    if (stopped) h.cancelled(); else h.fail(e);
    if (mounted && !stopped) ElMessage.error({ message: errorMessage(e), duration: 6000 });
  } finally {
    running.value = false;
    cancelling.value = false;
  }
}

function cancel() {
  if (!running.value || !task) emit('close');
  else if (scope.value === 'all') tasks.cancel(task.id);
}

function toBackground() {
  task?.background();
  emit('close');
}
</script>

<template>
  <el-dialog
    :model-value="true" :title="$t('results:exportDialog.title')" width="560px" append-to-body
    :close-on-click-modal="false" :close-on-press-escape="!running" :show-close="!running" @close="cancel"
  >
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
      <span v-if="scope === 'all' && estimate != null && progress <= estimate" class="ed-estimate">{{ $t('tasks:importExport.estimated', { total: estimate.toLocaleString(locale()) }) }}</span>
    </div>

    <template #footer>
      <el-button v-if="running" @click="toBackground">{{ $t('tasks:panel.background') }}</el-button>
      <el-button v-if="!running || scope === 'all'" :disabled="cancelling" @click="cancel">{{ running ? $t('results:exportDialog.cancelExport') : $t('common:cancel') }}</el-button>
      <el-button type="primary" :loading="running" @click="run">{{ $t('results:exportDialog.submit') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.ed-scope { display: flex; flex-direction: column; align-items: flex-start; gap: 4px; }
.ed-help { font-size: 11.5px; color: var(--nm-text-dim); line-height: 1.45; margin-top: 4px; }
.ed-help.warn { color: var(--nm-warning); }
.ed-estimate { color: var(--nm-text-dim); }
.ed-progress { display: flex; align-items: center; gap: 8px; font-size: 12.5px; color: var(--nm-text); margin-top: 6px; }
</style>
