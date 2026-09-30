<script setup lang="ts">
import { computed, onBeforeUnmount, reactive, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { open } from '@tauri-apps/plugin-dialog';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { locale } from '../i18n';
import type { Cell, ObjectRef } from '../api/types';
import type { TableSchema } from '../api/schema-types';

// Import a data file into a table: file + format options, a preview with the
// inferred types, the target (an existing table or a new one) with the column
// mapping, and the run with progress. Backend: `preview_import_file` and
// `import_file` (docs/api-comandos.md).

type ImportFormat = 'auto' | 'csv' | 'csv_semicolon' | 'tsv' | 'json' | 'json_lines' | 'xlsx' | 'xml';
type InferredType = 'integer' | 'number' | 'boolean' | 'date' | 'datetime' | 'text';
interface ImportTable { schema: string | null; name: string; columns: string[] }
interface PreviewColumn { name: string; inferred_type: InferredType }
interface ImportPreview { format: ImportFormat; columns: PreviewColumn[]; rows: Cell[][]; sheets: string[] }
interface ImportResult { rows: number; elapsed_ms: number }

const props = withDefaults(defineProps<{
  connectionId: string;
  database: string;
  tables: ImportTable[];
  designerAvailable: boolean;
  defaultSchema: string | null;
  /** Engine dialect, to suggest native types for a new table (`mssql`, `oracle`, `mysql`…). */
  dialect?: string;
  /** Type suggestions for new columns (the designer's `data_types`). */
  dataTypes?: string[];
  /** Opens with this file already chosen (drag & drop, recent files…). */
  initialPath?: string;
}>(), { dialect: '', dataTypes: () => [], initialPath: '' });
const emit = defineEmits<{ close: []; imported: [target: ObjectRef] }>();
const { t } = useTranslation();

const FORMATS = computed((): { id: ImportFormat; label: string }[] => [
  { id: 'auto', label: t('importData:formats.auto') },
  { id: 'csv', label: t('importData:formats.csv') },
  { id: 'csv_semicolon', label: t('importData:formats.csvSemicolon') },
  { id: 'tsv', label: t('importData:formats.tsv') },
  { id: 'json', label: t('importData:formats.json') },
  { id: 'json_lines', label: t('importData:formats.jsonLines') },
  { id: 'xlsx', label: 'Excel (XLSX)' },
  { id: 'xml', label: 'XML' },
]);
const TYPE_LABELS = computed((): Record<InferredType, string> => ({
  integer: t('importData:types.integer'), number: t('importData:types.number'), boolean: t('importData:types.boolean'),
  date: t('importData:types.date'), datetime: t('importData:types.datetime'), text: t('importData:types.text'),
}));

function detectFormat(path: string): ImportFormat {
  const ext = path.split('.').pop()?.toLowerCase() ?? '';
  const map: Record<string, ImportFormat> = {
    csv: 'csv', tsv: 'tsv', tab: 'tsv', txt: 'csv', json: 'json', jsonl: 'json_lines', ndjson: 'json_lines',
    xlsx: 'xlsx', xlsm: 'xlsx', xml: 'xml',
  };
  return map[ext] ?? 'auto';
}

/** A type the engine understands for an inferred one (editable by the user). */
function suggestType(t: InferredType): string {
  const d = props.dialect.toLowerCase();
  if (d.includes('mssql') || d.includes('sqlserver') || d === 'tsql') {
    return { integer: 'bigint', number: 'float', boolean: 'bit', date: 'date', datetime: 'datetime2', text: 'nvarchar(max)' }[t];
  }
  if (d.includes('oracle')) {
    return { integer: 'number(19)', number: 'number', boolean: 'number(1)', date: 'date', datetime: 'timestamp', text: 'varchar2(4000)' }[t];
  }
  if (d.includes('mysql') || d.includes('mariadb')) {
    return { integer: 'bigint', number: 'double', boolean: 'boolean', date: 'date', datetime: 'datetime', text: 'text' }[t];
  }
  return { integer: 'bigint', number: 'double precision', boolean: 'boolean', date: 'date', datetime: 'timestamp', text: 'text' }[t];
}
const typeOptions = computed(() => {
  const own = props.dataTypes.length ? props.dataTypes : (['integer', 'number', 'boolean', 'date', 'datetime', 'text'] as InferredType[]).map(suggestType);
  return [...new Set(own)];
});

// ---- step 1: file --------------------------------------------------------
const step = ref(0);
const path = ref('');
const format = ref<ImportFormat>('auto');
const options = reactive<{ delimiter: string; header: boolean; sheet: string | null }>({ delimiter: '', header: true, sheet: null });
const fileName = computed(() => path.value.split(/[\\/]/).pop() ?? '');
const effectiveFormat = computed<ImportFormat>(() => (format.value === 'auto' ? preview.value?.format ?? detectFormat(path.value) : format.value));
const isDelimited = computed(() => ['csv', 'csv_semicolon', 'tsv', 'auto'].includes(effectiveFormat.value));
const hasHeaderOption = computed(() => isDelimited.value || effectiveFormat.value === 'xlsx');

async function pickFile() {
  let picked: string | string[] | null = null;
  try {
    picked = await open({
      multiple: false,
      directory: false,
      filters: [
        { name: t('importData:file.filterData'), extensions: ['csv', 'tsv', 'tab', 'txt', 'json', 'jsonl', 'ndjson', 'xlsx', 'xlsm', 'xml'] },
        { name: t('scripts:run.allFiles'), extensions: ['*'] },
      ],
    });
  } catch { /* no dialog outside Tauri */ }
  if (typeof picked !== 'string') return;
  setFile(picked);
  await loadPreview();
}

function setFile(p: string) {
  preview.value = null; // first: the option watcher only reloads an existing preview
  path.value = p;
  format.value = detectFormat(p);
  options.sheet = null;
}

// ---- step 2: preview ----------------------------------------------------
const preview = ref<ImportPreview | null>(null);
const loading = ref(false);
const previewError = ref('');

async function loadPreview() {
  if (!path.value) return;
  loading.value = true;
  previewError.value = '';
  try {
    const p = await invoke<ImportPreview>('preview_import_file', {
      args: { path: path.value, format: format.value, options: { ...options } },
    });
    preview.value = p;
    if (p.sheets.length && !options.sheet) options.sheet = p.sheets[0];
    resetTarget();
  } catch (e) {
    preview.value = null;
    previewError.value = errorMessage(e);
  } finally {
    loading.value = false;
  }
}

function cellText(v: Cell): string {
  if (v === null) return 'NULL';
  return typeof v === 'string' ? v : String(v);
}

// ---- step 3: target -----------------------------------------------------
const targetMode = ref<'existing' | 'new'>('existing');
const targetKey = ref('');
const newTable = reactive({ schema: props.defaultSchema ?? '', name: '' });
interface NewColumn { include: boolean; source: string; name: string; data_type: string; nullable: boolean; inferred: InferredType }
const newColumns = ref<NewColumn[]>([]);
/** source column → target column ('' = ignore), for an existing table. */
const mapping = reactive<Record<string, string>>({});
const batch = ref(1000);

const tableKey = (t: ImportTable) => `${t.schema ?? ''}\u0000${t.name}`;
const tableLabel = (t: ImportTable) => (t.schema ? `${t.schema}.${t.name}` : t.name);
const targetTable = computed(() => props.tables.find((t) => tableKey(t) === targetKey.value) ?? null);

function baseName(): string {
  return fileName.value.replace(/\.[^.]+$/, '').replace(/[^\p{L}\p{N}_]+/gu, '_').replace(/^_+|_+$/g, '').toLowerCase() || 'importado';
}

function resetTarget() {
  const cols = preview.value?.columns ?? [];
  newColumns.value = cols.map((c) => ({
    include: true, source: c.name, name: c.name, data_type: suggestType(c.inferred_type), nullable: true, inferred: c.inferred_type,
  }));
  if (!newTable.name) newTable.name = baseName();
  if (!props.tables.length && props.designerAvailable) targetMode.value = 'new';
  if (!targetKey.value) {
    const guess = props.tables.find((t) => t.name.toLowerCase() === baseName());
    if (guess) targetKey.value = tableKey(guess);
  }
  autoMap();
}

function autoMap() {
  const targetCols = targetTable.value?.columns ?? [];
  for (const k of Object.keys(mapping)) delete mapping[k];
  for (const c of preview.value?.columns ?? []) {
    mapping[c.name] = targetCols.find((t) => t.toLowerCase() === c.name.toLowerCase()) ?? '';
  }
}
watch(targetKey, autoMap);

const mappedCount = computed(() => {
  if (targetMode.value === 'new') return newColumns.value.filter((c) => c.include).length;
  return Object.values(mapping).filter(Boolean).length;
});

const targetError = computed((): string => {
  if (targetMode.value === 'existing') {
    if (!targetTable.value) return t('importData:target.errors.pickTable');
    if (!mappedCount.value) return t('importData:target.errors.mapOne');
    const used = Object.values(mapping).filter(Boolean);
    if (new Set(used).size !== used.length) return t('importData:target.errors.mappedTwice');
    return '';
  }
  if (!newTable.name.trim()) return t('importData:target.errors.newName');
  if (props.tables.some((t) => t.name.toLowerCase() === newTable.name.trim().toLowerCase() && (t.schema ?? '') === newTable.schema.trim()))
    return t('importData:target.errors.exists');
  const included = newColumns.value.filter((c) => c.include);
  if (!included.length) return t('importData:target.errors.includeOne');
  if (included.some((c) => !c.name.trim() || !c.data_type.trim())) return t('importData:target.errors.nameAndType');
  const names = included.map((c) => c.name.trim().toLowerCase());
  if (new Set(names).size !== names.length) return t('importData:target.errors.duplicateNames');
  return '';
});

function targetRef(): ObjectRef {
  if (targetMode.value === 'new') {
    return { kind: 'table', schema: newTable.schema.trim() || null, name: newTable.name.trim() };
  }
  const t = targetTable.value!;
  return { kind: 'table', schema: t.schema, name: t.name };
}

function createTableSchema(): TableSchema | null {
  if (targetMode.value !== 'new') return null;
  const t = targetRef();
  return {
    kind: 'table', schema: t.schema, name: t.name,
    columns: newColumns.value.filter((c) => c.include).map((c) => ({
      name: c.name.trim(), data_type: c.data_type.trim(), nullable: c.nullable,
      default_value: null, auto_increment: false, comment: null, options: {},
    })),
    primary_key: null, foreign_keys: [], indexes: [], comment: null, options: {},
  };
}

function mappingList(): { source: string; target: string }[] {
  if (targetMode.value === 'new') {
    return newColumns.value.filter((c) => c.include).map((c) => ({ source: c.source, target: c.name.trim() }));
  }
  return Object.entries(mapping).filter(([, t]) => t).map(([source, target]) => ({ source, target }));
}

// ---- step 4: run --------------------------------------------------------
const running = ref(false);
const cancelling = ref(false);
const progressRows = ref(0);
const elapsed = ref(0);
const result = ref<ImportResult | null>(null);
const runError = ref('');
const importId = crypto.randomUUID();
let unlisten: UnlistenFn | null = null;
let timer: ReturnType<typeof setInterval> | null = null;

async function runImport() {
  step.value = 3;
  running.value = true;
  cancelling.value = false;
  progressRows.value = 0;
  result.value = null;
  runError.value = '';
  const started = Date.now();
  elapsed.value = 0;
  timer = setInterval(() => { elapsed.value = Date.now() - started; }, 250);
  const target = targetRef();
  try {
    unlisten = await listen<{ id: string; rows: number }>('import-progress', (e) => {
      if (e.payload.id === importId) progressRows.value = e.payload.rows;
    });
    result.value = await invoke<ImportResult>('import_file', {
      args: {
        import_id: importId,
        connection_id: props.connectionId,
        database: props.database,
        path: path.value,
        format: format.value,
        options: { ...options },
        target,
        create_table: createTableSchema(),
        mapping: mappingList(),
        batch: batch.value,
      },
    });
    emit('imported', target);
  } catch (e) {
    runError.value = cancelling.value ? t('importData:run.cancelledMessage') : errorMessage(e);
  } finally {
    running.value = false;
    if (timer) clearInterval(timer);
    timer = null;
    unlisten?.();
    unlisten = null;
  }
}

const rate = computed(() => (elapsed.value > 500 ? Math.round(progressRows.value / (elapsed.value / 1000)) : 0));
const seconds = (ms: number) => (ms / 1000).toFixed(1);

// ---- navigation ---------------------------------------------------------
const canNext = computed(() => {
  if (step.value === 0) return !!preview.value && !loading.value;
  if (step.value === 1) return !!preview.value?.columns.length;
  if (step.value === 2) return !targetError.value;
  return false;
});
function next() {
  if (step.value === 2) runImport();
  else if (canNext.value) step.value++;
}
function back() {
  if (step.value === 3) {
    result.value = null;
    runError.value = '';
    step.value = 2;
  } else if (step.value > 0) step.value--;
}

function cancel() {
  if (running.value) {
    cancelling.value = true;
    invoke('cancel_query', { args: { session_id: `import:${importId}` } }).catch(() => {});
  } else emit('close');
}

// option changes on step 1 refresh the preview
watch(() => [format.value, options.delimiter, options.header, options.sheet], () => {
  if (path.value && preview.value && !loading.value) loadPreview();
}, { flush: 'sync' });

if (props.initialPath) {
  setFile(props.initialPath);
  loadPreview().then(() => { if (preview.value) step.value = 1; });
}

onBeforeUnmount(() => {
  unlisten?.();
  if (timer) clearInterval(timer);
});
defineExpose({ step, targetMode });
</script>

<template>
  <el-dialog
    :model-value="true" :title="$t('importData:title')" width="880px" append-to-body align-center
    :close-on-click-modal="false" :close-on-press-escape="!running" :show-close="!running"
    class="im-dialog" @close="cancel"
  >
    <el-steps :active="step" finish-status="success" simple class="im-steps">
      <el-step :title="$t('importData:steps.file')" />
      <el-step :title="$t('importData:steps.preview')" />
      <el-step :title="$t('importData:steps.target')" />
      <el-step :title="$t('importData:steps.import')" />
    </el-steps>

    <div class="im-body">
      <!-- 1: file -->
      <div v-if="step === 0" class="im-pane">
        <div class="im-file">
          <el-icon class="im-file-icon"><ei-document /></el-icon>
          <div class="im-file-text">
            <template v-if="path">
              <div class="im-file-name">{{ fileName }}</div>
              <div class="im-file-path nm-selectable" :title="path">{{ path }}</div>
            </template>
            <div v-else class="nm-muted">{{ $t('importData:file.pickHint') }}</div>
          </div>
          <el-button :type="path ? 'default' : 'primary'" :loading="loading" @click="pickFile">
            {{ path ? $t('scripts:run.change') : $t('scripts:run.pickFile') }}
          </el-button>
        </div>

        <el-form label-position="top" class="im-opts" :disabled="!path || loading" @submit.prevent>
          <el-form-item :label="$t('importData:file.format')">
            <el-select v-model="format">
              <el-option v-for="f in FORMATS" :key="f.id" :label="f.label" :value="f.id" />
            </el-select>
          </el-form-item>
          <el-form-item v-if="isDelimited" :label="$t('importData:file.delimiter')">
            <el-input
              v-model="options.delimiter" maxlength="1"
              :placeholder="effectiveFormat === 'tsv' ? $t('importData:file.tab') : effectiveFormat === 'csv_semicolon' ? ';' : $t('importData:file.commaDefault')"
            />
          </el-form-item>
          <el-form-item v-if="effectiveFormat === 'xlsx'" :label="$t('importData:file.sheet')">
            <el-select v-model="options.sheet" :disabled="!preview?.sheets.length" :placeholder="$t('importData:file.firstSheet')">
              <el-option v-for="s in preview?.sheets ?? []" :key="s" :label="s" :value="s" />
            </el-select>
          </el-form-item>
          <el-form-item v-if="hasHeaderOption" label=" ">
            <el-checkbox v-model="options.header">{{ $t('importData:file.header') }}</el-checkbox>
          </el-form-item>
        </el-form>

        <el-alert v-if="previewError" type="error" :closable="false" show-icon :title="previewError" class="nm-selectable" />
        <template v-else-if="preview">
          <div class="im-ok">
            <el-icon><ei-circle-check /></el-icon>
            {{ $t('importData:file.detected', { count: preview.columns.length, format: FORMATS.find((f) => f.id === preview!.format)?.label ?? preview.format }) }}
          </div>
          <div class="im-chips">
            <span v-for="c in preview.columns" :key="c.name" class="im-chip">
              {{ c.name }} <span class="im-col-type" :class="`t-${c.inferred_type}`">{{ TYPE_LABELS[c.inferred_type] ?? c.inferred_type }}</span>
            </span>
          </div>
        </template>
      </div>

      <!-- 2: preview -->
      <div v-else-if="step === 1 && preview" class="im-pane">
        <div class="im-caption nm-muted">
          <i18next :translation="$t('importData:preview.caption', { rows: preview.rows.length, count: preview.columns.length })"><template #file><b>{{ fileName }}</b></template></i18next>
        </div>
        <div class="im-grid-wrap">
          <table class="im-grid">
            <thead>
              <tr>
                <th class="im-rn">#</th>
                <th v-for="c in preview.columns" :key="c.name">
                  <div class="im-col-name">{{ c.name }}</div>
                  <div class="im-col-type" :class="`t-${c.inferred_type}`">{{ TYPE_LABELS[c.inferred_type] ?? c.inferred_type }}</div>
                </th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="(r, i) in preview.rows" :key="i">
                <td class="im-rn">{{ i + 1 }}</td>
                <td
                  v-for="(v, j) in r" :key="j"
                  :class="{ 'nm-null': v === null, 'nm-num': typeof v === 'number', 'nm-bool': typeof v === 'boolean' }"
                >{{ cellText(v) }}</td>
              </tr>
            </tbody>
          </table>
          <div v-if="!preview.rows.length" class="im-empty nm-muted">{{ $t('importData:preview.empty') }}</div>
        </div>
      </div>

      <!-- 3: target -->
      <div v-else-if="step === 2 && preview" class="im-pane">
        <div class="im-target-head">
          <el-radio-group v-model="targetMode">
            <el-radio-button value="existing" :disabled="!tables.length">{{ $t('importData:target.existing') }}</el-radio-button>
            <el-radio-button value="new" :disabled="!designerAvailable">{{ $t('importData:target.new') }}</el-radio-button>
          </el-radio-group>
          <template v-if="targetMode === 'existing'">
            <el-select v-model="targetKey" filterable :placeholder="$t('importData:target.pickTable')" class="im-target-select">
              <el-option v-for="t in tables" :key="tableKey(t)" :label="tableLabel(t)" :value="tableKey(t)" />
            </el-select>
          </template>
          <template v-else>
            <el-input v-if="defaultSchema !== null" v-model="newTable.schema" :placeholder="$t('importData:target.schema')" class="im-schema" />
            <el-input v-model="newTable.name" :placeholder="$t('importData:target.tableName')" class="im-name" />
          </template>
          <span class="nm-spacer" />
          <span class="im-batch-label">{{ $t('importData:target.batch') }}</span>
          <el-input-number v-model="batch" :min="1" :max="100000" :step="500" controls-position="right" class="im-batch" />
        </div>

        <div class="im-map-wrap">
          <!-- existing table: source → target -->
          <table v-if="targetMode === 'existing'" class="im-map">
            <thead>
              <tr><th>{{ $t('importData:target.sourceColumn') }}</th><th class="im-w-type">{{ $t('importData:target.detectedType') }}</th><th class="im-w-arrow" /><th>{{ $t('importData:target.targetColumn') }}</th></tr>
            </thead>
            <tbody>
              <tr v-for="c in preview.columns" :key="c.name" :class="{ off: !mapping[c.name] }">
                <td class="im-src">{{ c.name }}</td>
                <td><span class="im-col-type" :class="`t-${c.inferred_type}`">{{ TYPE_LABELS[c.inferred_type] }}</span></td>
                <td class="im-arrow"><el-icon><ei-right /></el-icon></td>
                <td>
                  <el-select v-model="mapping[c.name]" :disabled="!targetTable" filterable :placeholder="$t('importData:target.ignore')" class="im-map-select">
                    <el-option :label="$t('importData:target.ignore')" value="" />
                    <el-option v-for="t in targetTable?.columns ?? []" :key="t" :label="t" :value="t" />
                  </el-select>
                </td>
              </tr>
            </tbody>
          </table>

          <!-- new table: editable columns -->
          <table v-else class="im-map">
            <thead>
              <tr>
                <th class="im-w-check" /><th>{{ $t('importData:target.sourceColumn') }}</th><th class="im-w-arrow" />
                <th>{{ $t('importData:target.name') }}</th><th>{{ $t('importData:target.type') }}</th><th class="im-w-null">{{ $t('importData:target.nulls') }}</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="c in newColumns" :key="c.source" :class="{ off: !c.include }">
                <td><el-checkbox v-model="c.include" /></td>
                <td class="im-src">
                  {{ c.source }}
                  <span class="im-col-type" :class="`t-${c.inferred}`">{{ TYPE_LABELS[c.inferred] }}</span>
                </td>
                <td class="im-arrow"><el-icon><ei-right /></el-icon></td>
                <td><el-input v-model="c.name" :disabled="!c.include" /></td>
                <td>
                  <el-select v-model="c.data_type" :disabled="!c.include" filterable allow-create default-first-option class="im-map-select">
                    <el-option v-for="t in typeOptions" :key="t" :label="t" :value="t" />
                  </el-select>
                </td>
                <td class="im-center"><el-checkbox v-model="c.nullable" :disabled="!c.include" /></td>
              </tr>
            </tbody>
          </table>
        </div>
      </div>

      <!-- 4: run / result -->
      <div v-else-if="step === 3" class="im-pane im-run">
        <template v-if="running">
          <div class="im-run-title">
            <el-icon class="is-loading"><ei-loading /></el-icon>
            {{ cancelling ? $t('scripts:run.cancelling') : $t('importData:run.importing', { file: fileName }) }}
          </div>
          <el-progress :percentage="100" :indeterminate="true" :duration="2" :show-text="false" :stroke-width="4" class="im-bar" />
          <div class="im-stats">
            <div><span class="im-stat">{{ progressRows.toLocaleString(locale()) }}</span><span class="nm-muted">{{ $t('importData:run.rows', { count: progressRows }) }}</span></div>
            <div><span class="im-stat">{{ rate.toLocaleString(locale()) }}</span><span class="nm-muted">{{ $t('importData:run.rowsPerSecond') }}</span></div>
            <div><span class="im-stat">{{ seconds(elapsed) }}</span><span class="nm-muted">{{ $t('scripts:run.seconds') }}</span></div>
          </div>
        </template>
        <template v-else-if="result">
          <div class="im-done">
            <el-icon class="im-done-icon"><ei-circle-check-filled /></el-icon>
            <div>
              <div class="im-done-title">{{ $t('importData:run.done') }}</div>
              <div class="nm-muted">
                <i18next :translation="$t('importData:run.doneDetail', { count: result.rows, n: result.rows.toLocaleString(locale()), seconds: seconds(result.elapsed_ms) })"><template #table><b>{{ targetRef().schema ? `${targetRef().schema}.` : '' }}{{ targetRef().name }}</b></template></i18next>
              </div>
            </div>
          </div>
        </template>
        <template v-else-if="runError">
          <el-alert :type="cancelling || runError === $t('importData:run.cancelledMessage') ? 'warning' : 'error'" :closable="false" show-icon :title="runError" class="nm-selectable" />
          <div v-if="progressRows" class="nm-muted im-partial">
            {{ $t('importData:run.partial', { count: progressRows, n: progressRows.toLocaleString(locale()) }) }}
          </div>
        </template>
      </div>
    </div>

    <template #footer>
      <div class="im-footer">
        <span v-if="step === 2 && targetError" class="im-warn">{{ targetError }}</span>
        <span v-else-if="step === 2" class="nm-muted">{{ $t('importData:target.toImport', { count: mappedCount }) }}</span>
        <span class="nm-spacer" />
        <template v-if="step === 3 && !running">
          <el-button v-if="!result" @click="back">{{ $t('common:back') }}</el-button>
          <el-button type="primary" @click="emit('close')">{{ $t('common:close') }}</el-button>
        </template>
        <template v-else>
          <el-button :disabled="cancelling" @click="cancel">{{ running ? $t('importData:run.cancelImport') : $t('common:cancel') }}</el-button>
          <el-button v-if="step > 0 && step < 3" @click="back">{{ $t('common:back') }}</el-button>
          <el-button v-if="step < 3" type="primary" :disabled="!canNext" @click="next">
            {{ step === 2 ? $t('common:import') : $t('common:next') }}
          </el-button>
        </template>
      </div>
    </template>
  </el-dialog>
</template>

<style scoped>
.im-steps { margin: -4px 0 14px; padding: 8px 14px !important; background: var(--ide-editor) !important; border: 1px solid var(--nm-border-soft); border-radius: var(--nm-radius); }
.im-steps :deep(.el-step__title) { font-size: 12.5px; }
.im-body { height: 430px; display: flex; flex-direction: column; }
.im-pane { flex: 1; min-height: 0; display: flex; flex-direction: column; gap: 12px; }

/* file */
.im-file { display: flex; align-items: center; gap: 12px; padding: 14px 16px; border: 1px dashed var(--nm-border); border-radius: var(--nm-radius); background: var(--ide-editor); }
.im-file-icon { font-size: 26px; color: var(--nm-accent); }
.im-file-text { flex: 1; min-width: 0; }
.im-file-name { color: var(--nm-text-strong); font-weight: 600; }
.im-file-path { font-family: var(--nm-mono); font-size: 11.5px; color: var(--nm-text-dim); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; margin-top: 2px; }
.im-opts { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); column-gap: 16px; }
.im-opts .el-select { width: 100%; }
.im-chips { display: flex; flex-wrap: wrap; gap: 6px; }
.im-chip { display: inline-flex; align-items: baseline; gap: 6px; padding: 2px 8px; font-size: 12px; color: var(--nm-text); border: 1px solid var(--nm-border-soft); background: var(--ide-editor); border-radius: var(--nm-radius); }
.im-body :deep(.el-checkbox__input.is-checked + .el-checkbox__label) { color: var(--nm-text); }
.im-ok { display: flex; align-items: center; gap: 6px; font-size: 12.5px; color: var(--nm-success); }

/* preview grid */
.im-caption b { color: var(--nm-text-strong); font-weight: 600; }
.im-grid-wrap { flex: 1; min-height: 0; overflow: auto; border: 1px solid var(--nm-border-soft); background: var(--ide-editor); }
.im-grid { border-collapse: separate; border-spacing: 0; font-size: 12px; min-width: 100%; }
.im-grid th, .im-grid td { padding: 3px 10px; border-right: 1px solid var(--nm-border-soft); border-bottom: 1px solid var(--nm-border-soft); white-space: nowrap; max-width: 260px; overflow: hidden; text-overflow: ellipsis; text-align: left; }
.im-grid td { font-family: var(--nm-mono); font-size: 11.5px; user-select: text; }
.im-grid th { position: sticky; top: 0; background: var(--ide-sidebar); z-index: 1; font-weight: 600; }
.im-grid .im-rn { color: var(--nm-text-muted); text-align: right; width: 1%; font-family: var(--nm-mono); font-size: 11px; position: sticky; left: 0; background: var(--ide-sidebar); z-index: 2; }
.im-grid thead .im-rn { z-index: 3; }
.im-grid tbody tr:hover td { background: var(--ide-hover); }
.im-col-name { color: var(--nm-text-strong); font-size: 12px; }
.im-col-type { display: inline-block; font-size: 10.5px; font-weight: 400; color: var(--nm-text-dim); font-family: var(--nm-mono); }
.im-col-type.t-integer, .im-col-type.t-number { color: #b5cea8; }
.im-col-type.t-boolean { color: #569cd6; }
.im-col-type.t-date, .im-col-type.t-datetime { color: #d7ba7d; }
.im-col-type.t-text { color: #ce9178; }
.im-empty { padding: 24px; text-align: center; }

/* target */
.im-target-head { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.im-target-select { width: 260px; }
.im-schema { width: 110px; }
.im-name { width: 200px; }
.im-batch-label { font-size: 12px; color: var(--nm-text-dim); }
.im-batch { width: 110px; }
.im-map-wrap { flex: 1; min-height: 0; overflow: auto; border: 1px solid var(--nm-border-soft); background: var(--ide-editor); }
.im-map { width: 100%; border-collapse: collapse; font-size: 12.5px; }
.im-map th { position: sticky; top: 0; z-index: 1; background: var(--ide-sidebar); text-align: left; font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.04em; color: var(--nm-text-dim); padding: 5px 8px; }
.im-map td { padding: 3px 8px; border-bottom: 1px solid var(--nm-border-soft); }
.im-map tr.off .im-src { color: var(--nm-text-muted); }
.im-src { color: var(--nm-text-strong); white-space: nowrap; }
.im-src .im-col-type { margin-left: 6px; }
.im-arrow { color: var(--nm-text-muted); text-align: center; }
.im-center { text-align: center; }
.im-w-type { width: 130px; white-space: nowrap; }
.im-w-arrow { width: 28px; }
.im-w-check { width: 30px; }
.im-w-null { width: 56px; text-align: center !important; }
.im-map-select { width: 100%; }

/* run */
.im-run { justify-content: center; align-items: stretch; padding: 0 60px; gap: 16px; }
.im-run-title { display: flex; align-items: center; gap: 8px; font-size: 13px; color: var(--nm-text-strong); }
.im-stats { display: grid; grid-template-columns: repeat(3, 1fr); gap: 12px; }
.im-stats > div { display: flex; flex-direction: column; gap: 2px; padding: 10px 12px; border: 1px solid var(--nm-border-soft); background: var(--ide-editor); }
.im-stat { font-size: 20px; color: var(--nm-text-strong); font-variant-numeric: tabular-nums; }
.im-done { display: flex; align-items: center; gap: 14px; padding: 18px 20px; border: 1px solid var(--nm-border-soft); background: var(--ide-editor); }
.im-done-icon { font-size: 30px; color: var(--nm-success); }
.im-done-title { font-size: 14px; color: var(--nm-text-strong); font-weight: 600; margin-bottom: 3px; }
.im-done b { color: var(--nm-text); font-weight: 600; }
.im-partial { font-size: 12px; }

.im-footer { display: flex; align-items: center; gap: 8px; font-size: 12px; }
.im-warn { color: var(--nm-warning); }
</style>
