<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import { save } from '@tauri-apps/plugin-dialog';
import { api, errorMessage } from '../api/client';
import type { Cell, ObjectRef, QueryOutcome } from '../api/types';
import { buildChanges, inferTarget, resolveEditing, type EditSetup, type Edits } from '../composables/gridEdit';
import ResultGrid from './ResultGrid.vue';
import PlanView from './PlanView.vue';
import ChartView from './ChartView.vue';
import ExportDialog from './ExportDialog.vue';
import type { FilterState } from '../composables/gridFilter';
import { EXPORT_FORMATS, defaultOptions, exportApi, fileName, type ExportFormat } from '../composables/export';

// What a run produced: one sub-tab per result set, "Plan de ejecución" when
// the run asked for plans, and "Mensajes" (server messages, affected rows,
// the error). Opens on the error, else the plan when there's no data.

const props = defineProps<{
  outcome: QueryOutcome | null;
  running: boolean;
  /** What produced the outcome, so an export can run it again for every row. */
  source?: { connectionId: string; database: string; sql: string } | null;
  /** Base name for exported files. */
  title?: string;
  /** Driver's SQL dialect (identifier quoting of SQL exports). */
  dialect?: string;
  /** The table the rows come from, when known (copy as SQL / Mongo). */
  table?: { schema: string | null; name: string } | null;
  /** Its key columns (copy as SQL UPDATEs). */
  keyColumns?: string[];
  /** Where edited cells' UPDATE code comes from: the tab's table, or the
   *  query (`script`) the rows came from. */
  editSource?: { connectionId: string; database: string; language: string; target?: ObjectRef | null; script?: string } | null;
  /** Column filters under the headers (a table's data). */
  filterable?: boolean;
  filters?: Record<string, FilterState>;
}>();
const emit = defineEmits<{
  /** "Agregar a la query": the generated UPDATE code. */
  script: [code: string];
  filter: [column: string, state: FilterState | null];
  /** The edits were saved (run) on the server: a table's data reloads. */
  applied: [];
}>();

const { t } = useTranslation();

const sets = computed(() => (props.outcome?.results ?? []).map((r, i) => ({ ...r, i })).filter((r) => r.columns.length));
const active = ref<number | 'messages' | 'plan'>(0);
/** How the current result set is shown. */
const show = ref<'table' | 'chart'>('table');

watch(
  () => props.outcome,
  (o) => {
    if (!o) return;
    active.value = o.error && !o.plans.length ? 'messages'
      : o.plans.length && !sets.value.length ? 'plan'
      : sets.value.length ? sets.value[0].i
      : 'messages';
  },
);

const current = computed(() => (typeof active.value === 'number' ? props.outcome?.results[active.value] ?? null : null));

// -- editing (the changes become UPDATE code; nothing runs) ------------------------------
const edits = reactive<Record<number, Edits>>({});
watch(() => props.outcome, () => { for (const k of Object.keys(edits)) delete edits[Number(k)]; });
const currentEdits = computed(() => (typeof active.value === 'number' ? edits[active.value] ?? {} : {}));
const editCount = computed(() => Object.values(currentEdits.value).reduce((n, r) => n + Object.keys(r).length, 0));
const editRows = computed(() => Object.keys(currentEdits.value).length);

/** How the current result can be edited, or why not. */
const setup = ref<EditSetup | string | null>(null);
let setupSeq = 0;
// Keyed by content: the parent builds `editSource` inline (a new object each
// render), watching its identity would loop.
const editKey = computed(() => {
  const e = props.editSource;
  return e ? JSON.stringify([e.connectionId, e.database, e.language, e.target ?? null, e.script ?? '']) : '';
});
watch(
  () => [editKey.value, active.value, props.outcome] as const,
  async () => {
    const seq = ++setupSeq;
    setup.value = null;
    const src = props.editSource;
    const r = current.value;
    if (!src || !r || typeof active.value !== 'number') return;
    let target = src.target ?? null;
    if (!target) {
      const pos = sets.value.findIndex((x) => x.i === active.value);
      const inferred = inferTarget(src.script ?? '', pos, src.language);
      if (!inferred.target) { setup.value = inferred.reason; return; }
      target = inferred.target;
    }
    try {
      const res = await resolveEditing(src.connectionId, src.database, target, r.columns.map((c) => c.name));
      if (seq === setupSeq) setup.value = res;
    } catch (e) {
      if (seq === setupSeq) setup.value = errorMessage(e);
    }
  },
  { immediate: true },
);
const canEdit = computed(() => !!setup.value && typeof setup.value !== 'string');
const noEditReason = computed(() => (typeof setup.value === 'string' ? setup.value : props.editSource ? t('results:edit.lookingUpTable') : null));

function onEdit(r: number, c: number, value: Cell | undefined) {
  if (typeof active.value !== 'number') return;
  const set = (edits[active.value] ??= {});
  if (value === undefined) {
    if (set[r]) {
      delete set[r][c];
      if (!Object.keys(set[r]).length) delete set[r];
    }
  } else {
    (set[r] ??= {})[c] = value;
  }
}
function discard() {
  if (typeof active.value === 'number') delete edits[active.value];
}

// The code, regenerated as the edits change. Not shown in the bar (it grows
// with every edit): "Guardar" shows it in full before running.
const code = ref('');
const codeError = ref<string | null>(null);
let codeTimer: ReturnType<typeof setTimeout> | undefined;
watch(
  () => [currentEdits.value, setup.value] as const,
  () => {
    clearTimeout(codeTimer);
    codeTimer = setTimeout(generate, 150);
  },
  { deep: true },
);
async function generate() {
  const s = setup.value;
  const r = current.value;
  const src = props.editSource;
  if (!editCount.value || !s || typeof s === 'string' || !r || !src) { code.value = ''; codeError.value = null; return; }
  const changes = buildChanges(r.columns.map((c) => c.name), r.rows, currentEdits.value, s.keyColumns);
  try {
    code.value = await invoke<string>('update_script', { args: { connection_id: src.connectionId, target: s.target, changes } });
    codeError.value = null;
  } catch (e) {
    code.value = '';
    codeError.value = errorMessage(e);
  }
}
function addToQuery() {
  if (!code.value) return;
  emit('script', code.value);
  discard();
}
// "Guardar": the code in a dialog to review (and copy), then run on its
// own session. Nothing runs without that click.
const applyOpen = ref(false);
const applying = ref(false);
const applyError = ref<string | null>(null);
function reviewApply() {
  if (!code.value) return;
  applyError.value = null;
  applyOpen.value = true;
}
async function applyEdits() {
  const src = props.editSource;
  const r = current.value;
  if (!src || !r || !code.value || typeof active.value !== 'number') return;
  applying.value = true;
  applyError.value = null;
  const sessionId = `apply:${Date.now()}:${Math.random().toString(36).slice(2)}`;
  try {
    const o = await api.executeQuery({ sessionId, connectionId: src.connectionId, database: src.database, sql: code.value, maxRows: 10, record: true });
    if (o.error) { applyError.value = tb(o.error); return; }
    // The grid shows the saved values right away (a table's data also reloads).
    for (const [row, cols] of Object.entries(currentEdits.value)) {
      for (const [col, value] of Object.entries(cols)) r.rows[Number(row)][Number(col)] = value as Cell;
    }
    const affected = o.results.reduce((n, x) => n + (x.rows_affected ?? 0), 0);
    discard();
    applyOpen.value = false;
    ElMessage.success(affected ? t('applyEdits:doneRows', { count: affected }) : t('applyEdits:done'));
    emit('applied');
  } catch (e) {
    applyError.value = errorMessage(e);
  } finally {
    applying.value = false;
    api.closeSession(sessionId).catch(() => {});
  }
}
async function copyCode() {
  try { await navigator.clipboard.writeText(code.value); ElMessage.success({ message: t('results:edit.codeCopied'), duration: 1200 }); } catch { /* ignore */ }
}

// -- export ---------------------------------------------------------------------------------
const exporting = ref<{ format: ExportFormat } | null>(null);
const exportSource = computed(() =>
  props.source && typeof active.value === 'number' ? { ...props.source, resultIndex: active.value } : null);

/** Quick export: the rows already loaded, straight to a file. */
async function quickExport(format: ExportFormat) {
  const r = current.value;
  if (!r) return;
  const fmt = EXPORT_FORMATS.find((f) => f.id === format)!;
  let path: string | null = null;
  try {
    path = await save({ defaultPath: fileName(props.title ?? t('results:defaultFileName'), format), filters: [{ name: fmt.filter, extensions: [fmt.ext] }] });
  } catch { /* outside Tauri */ }
  if (!path) return;
  try {
    const res = await exportApi.rows(path, defaultOptions(format, props.title ?? t('results:defaultTableName'), props.dialect ?? ''), r.columns, r.rows);
    ElMessage.success({
      message: r.truncated
        ? t('results:export.doneLoaded', { count: res.rows, rows: res.rows.toLocaleString(locale()) })
        : t('results:export.done', { count: res.rows, rows: res.rows.toLocaleString(locale()) }),
      duration: r.truncated ? 5000 : 2500,
    });
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

function onExportCommand(cmd: string) {
  if (cmd === 'advanced') exporting.value = { format: 'csv' };
  else quickExport(cmd as ExportFormat);
}

// ⌘E opens the advanced export, when this pane is the visible one.
const root = ref<HTMLDivElement | null>(null);
function onKey(e: KeyboardEvent) {
  if (!(e.metaKey || e.ctrlKey) || e.key.toLowerCase() !== 'e') return;
  if (!current.value || !root.value?.offsetParent) return;
  e.preventDefault();
  exporting.value = { format: 'csv' };
}
onMounted(() => window.addEventListener('keydown', onKey));
onBeforeUnmount(() => window.removeEventListener('keydown', onKey));

const messages = computed(() => {
  const o = props.outcome;
  if (!o) return [];
  const out: { level: 'info' | 'error'; text: string }[] = [];
  o.results.forEach((r, i) => {
    if (r.columns.length) {
      const rows = t('results:messages.rows', { count: r.total_rows, rows: r.total_rows.toLocaleString(locale()) });
      const shown = r.truncated ? t('results:messages.shown', { rows: r.rows.length.toLocaleString(locale()) }) : '';
      out.push({ level: 'info', text: t('results:messages.statement', { n: i + 1, text: `${rows}${shown}` }) });
    } else if (r.rows_affected !== null) {
      out.push({ level: 'info', text: t('results:messages.statement', { n: i + 1, text: t('results:messages.affected', { count: r.rows_affected, rows: r.rows_affected.toLocaleString(locale()) }) }) });
    } else {
      out.push({ level: 'info', text: t('results:messages.statement', { n: i + 1, text: t('results:messages.completed') }) });
    }
  });
  for (const m of o.messages) out.push({ level: 'info', text: tb(m) });
  if (o.error) out.push({ level: 'error', text: tb(o.error) });
  return out;
});
</script>

<template>
  <div ref="root" class="rp">
    <div class="nm-subtabs">
      <button
        v-for="s in sets"
        :key="s.i"
        class="nm-subtab"
        :class="{ active: active === s.i }"
        @click="active = s.i"
      >
        {{ $t('results:tabs.result') }} {{ sets.length > 1 ? s.i + 1 : '' }}
        <span class="rp-count">{{ s.total_rows.toLocaleString(locale()) }}{{ s.truncated ? '+' : '' }}</span>
      </button>
      <button v-if="outcome?.plans.length" class="nm-subtab" :class="{ active: active === 'plan' }" @click="active = 'plan'">
        <el-icon><ei-share /></el-icon> {{ $t('results:tabs.plan') }}
      </button>
      <button class="nm-subtab" :class="{ active: active === 'messages' }" @click="active = 'messages'">
        {{ $t('results:tabs.messages') }}
        <el-icon v-if="outcome?.error" color="var(--nm-danger)"><ei-circle-close-filled /></el-icon>
      </button>
      <div class="nm-spacer" />
      <el-dropdown v-if="current" trigger="click" @command="onExportCommand">
        <button class="rp-export" :title="$t('results:export.title')">
          <el-icon><ei-download /></el-icon> {{ $t('common:export') }} <el-icon><ei-arrow-down /></el-icon>
        </button>
        <template #dropdown>
          <el-dropdown-menu>
            <el-dropdown-item command="advanced">
              <span class="rp-menu-row"><span>{{ $t('results:export.advanced') }}</span><kbd>⌘E</kbd></span>
            </el-dropdown-item>
            <el-dropdown-item v-for="(f, i) in EXPORT_FORMATS" :key="f.id" :command="f.id" :divided="i === 0">{{ $t(`results:export.format.${f.id}`, f.label) }}</el-dropdown-item>
          </el-dropdown-menu>
        </template>
      </el-dropdown>
      <div v-if="current" class="rp-mode" role="group" :aria-label="$t('results:view.label')">
        <button :class="{ on: show === 'table' }" :title="$t('results:view.table')" @click="show = 'table'"><el-icon><ei-grid /></el-icon></button>
        <button :class="{ on: show === 'chart' }" :title="$t('results:view.chart')" @click="show = 'chart'"><el-icon><ei-histogram /></el-icon></button>
      </div>
      <span v-if="running" class="rp-status"><el-icon class="is-loading"><ei-loading /></el-icon> {{ $t('results:running') }}</span>
      <span v-else-if="outcome" class="rp-status">{{ outcome.elapsed_ms.toLocaleString(locale()) }} ms</span>
    </div>

    <div v-if="!outcome && !running" class="rp-empty nm-muted">
      <i18next :translation="$t('results:empty')"><template #key><kbd>⌘↵</kbd></template></i18next>
    </div>
    <PlanView v-else-if="active === 'plan' && outcome?.plans.length" :plans="outcome.plans" />
    <template v-else-if="current">
      <div v-if="current.truncated" class="rp-trunc">
        {{ $t('results:truncated', { shown: current.rows.length.toLocaleString(locale()), total: current.total_rows.toLocaleString(locale()) }) }}
      </div>
      <ChartView v-if="show === 'chart'" :key="active" :columns="current.columns" :rows="current.rows" />
      <template v-else>
      <div v-if="editCount" class="rp-edits">
        <div class="rp-edits-bar">
          <el-icon><ei-edit /></el-icon>
          <span>{{ $t('results:edit.summary', { cells: $t('results:edit.cells', { count: editCount }), rows: $t('results:edit.rows', { count: editRows }) }) }}</span>
          <div class="nm-spacer" />
          <el-button size="small" type="primary" :disabled="!code" @click="reviewApply">{{ $t('applyEdits:save') }}</el-button>
          <el-button size="small" :disabled="!code" @click="addToQuery">{{ $t('results:edit.addToQuery') }}</el-button>
          <el-button size="small" :disabled="!code" @click="copyCode">{{ $t('common:copy') }}</el-button>
          <el-button size="small" text @click="discard">{{ $t('results:edit.discard') }}</el-button>
        </div>
        <div v-if="setup && typeof setup !== 'string' && setup.note" class="rp-edits-note">{{ setup.note }}</div>
        <div v-if="codeError" class="rp-edits-note err">{{ codeError }}</div>
      </div>
      <ResultGrid
        :columns="current.columns"
        :rows="current.rows"
        :copy-context="{ table: table ?? null, dialect: dialect ?? '', keyColumns }"
        :editable="canEdit"
        :no-edit-reason="noEditReason"
        :edits="currentEdits"
        :filterable="filterable"
        :filters="filters"
        @edit="onEdit"
        @filter="(c, st) => emit('filter', c, st)"
      />
      </template>
    </template>
    <div v-else class="rp-messages nm-selectable">
      <div v-for="(m, i) in messages" :key="i" :class="['rp-msg', m.level]">{{ m.text }}</div>
    </div>
    <ExportDialog
      v-if="exporting && current"
      :columns="current.columns"
      :rows="current.rows"
      :total-rows="current.total_rows"
      :truncated="current.truncated"
      :source="exportSource"
      :title="title ?? $t('results:defaultFileName')"
      :dialect="dialect ?? ''"
      :initial-format="exporting.format"
      @close="exporting = null"
    />
      <el-dialog v-model="applyOpen" :title="$t('applyEdits:title')" width="720px" append-to-body :close-on-click-modal="!applying">
      <p class="rp-apply-hint">{{ $t('applyEdits:hint', { cells: $t('results:edit.cells', { count: editCount }), rows: $t('results:edit.rows', { count: editRows }) }) }}</p>
      <pre class="rp-apply-code nm-selectable">{{ code }}</pre>
      <div v-if="applyError" class="rp-apply-error" role="alert"><el-icon><ei-circle-close-filled /></el-icon><span>{{ applyError }}</span></div>
      <template #footer>
        <div class="rp-apply-foot">
          <el-button @click="copyCode">{{ $t('common:copy') }}</el-button>
          <el-button @click="applyOpen = false; addToQuery()">{{ $t('results:edit.addToQuery') }}</el-button>
          <span class="rp-apply-sp" />
          <el-button :disabled="applying" @click="applyOpen = false">{{ $t('common:cancel') }}</el-button>
          <el-button type="primary" :loading="applying" @click="applyEdits">{{ $t('applyEdits:run') }}</el-button>
        </div>
      </template>
    </el-dialog>
</div>
</template>

<style scoped>
.rp { display: flex; flex-direction: column; min-height: 0; height: 100%; }
.rp-count {
  font-size: 10.5px;
  padding: 0 5px;
  border-radius: 8px;
  background: var(--ide-button-2);
  color: var(--nm-text);
  letter-spacing: 0;
}
.rp-export {
  display: inline-flex; align-items: center; gap: 4px; margin: 0 2px; padding: 2px 8px; align-self: center;
  font: inherit; font-size: 11.5px; color: var(--nm-text); background: transparent;
  border: 1px solid var(--nm-border); border-radius: 3px; cursor: pointer;
}
.rp-export:hover { color: var(--nm-text-strong); border-color: #5a5a5a; }
.rp-menu-row { display: flex; justify-content: space-between; gap: 24px; width: 100%; }
.rp-menu-row kbd { font-family: var(--nm-mono); font-size: 11px; color: var(--nm-text-dim); }
.rp-mode { display: inline-flex; margin: 0 6px; border: 1px solid var(--nm-border); border-radius: 3px; overflow: hidden; align-self: center; }
.rp-mode button { display: inline-flex; align-items: center; padding: 2px 7px; border: none; background: transparent; color: var(--nm-text-dim); cursor: pointer; }
.rp-mode button.on { background: var(--ide-selection); color: var(--nm-text-strong); }
.rp-mode button:hover { color: var(--nm-text-strong); }
.rp-status { display: inline-flex; align-items: center; gap: 4px; padding: 0 10px; font-size: 11.5px; color: var(--nm-text-dim); }
.rp-empty { padding: 16px; }
.rp-empty kbd { font-family: var(--nm-mono); padding: 0 4px; border: 1px solid var(--nm-border); border-radius: 3px; }
.rp-trunc { padding: 3px 10px; font-size: 11.5px; color: var(--nm-warning); border-bottom: 1px solid var(--nm-border-soft); }
.rp-messages { flex: 1; overflow: auto; padding: 8px 12px; font-family: var(--nm-mono); font-size: 12px; }
.rp-msg { padding: 2px 0; white-space: pre-wrap; }
.rp-msg.error { color: var(--nm-danger); }
.rp-edits { border-bottom: 1px solid var(--nm-border); background: color-mix(in srgb, var(--nm-warning) 6%, transparent); }
.rp-edits-bar { display: flex; align-items: center; gap: 6px; padding: 4px 8px; font-size: 12px; color: var(--nm-text); }
.rp-edits-bar .el-icon { color: var(--nm-warning); }
.rp-edits-note { padding: 0 10px 4px; font-size: 11.5px; color: var(--nm-text-dim); }
.rp-edits-note.err { color: var(--nm-danger); }
.rp-apply-foot { display: flex; align-items: center; gap: 8px; }
.rp-apply-foot .el-button { margin: 0; }
.rp-apply-sp { flex: 1; }
.rp-apply-hint { margin: 0 0 8px; color: var(--nm-text-dim); font-size: 12.5px; }
.rp-apply-code { margin: 0; padding: 10px 12px; max-height: 50vh; overflow: auto; border: 1px solid var(--nm-border); border-radius: 4px; background: var(--ide-editor, var(--nm-bg-elev)); font-family: var(--nm-mono); font-size: 12.5px; color: var(--nm-text-strong); white-space: pre-wrap; }
.rp-apply-error { display: flex; gap: 6px; margin-top: 10px; padding: 8px 10px; border-radius: 3px; color: var(--nm-text); border: 1px solid color-mix(in srgb, var(--nm-danger) 50%, transparent); background: color-mix(in srgb, var(--nm-danger) 10%, transparent); white-space: pre-wrap; word-break: break-word; }
.rp-apply-error .el-icon { color: var(--nm-danger); margin-top: 2px; flex: none; }
</style>
