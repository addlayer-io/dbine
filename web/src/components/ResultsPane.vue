<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import { save } from '@tauri-apps/plugin-dialog';
import { api, errorMessage } from '../api/client';
import type { Cell, Message, MessageLevel, ObjectRef, QueryOutcome, StatementResult } from '../api/types';
import { buildChanges, inferTarget, resolveEditing, rowKey, skippedKeyColumns, type EditSetup, type Edits } from '../composables/gridEdit';
import ResultGrid from './ResultGrid.vue';
import PlanView from './PlanView.vue';
import ChartView from './ChartView.vue';
import ExportDialog from './ExportDialog.vue';
import type { FilterState } from '../composables/gridFilter';
import { EXPORT_FORMATS, defaultOptions, exportApi, fileName, type ExportFormat } from '../composables/export';
import { inCodeEditor, isSaveShortcut, modalOpen } from '../composables/shortcuts';

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
  /** The editor's line of the script's first line, minus one: messages then
   *  show "Línea N" and a click on one emits `goto`. Absent: no lines. */
  lineOffset?: number | null;
  /** The parent shows the run's status (time, rows) itself. */
  hideStatus?: boolean;
}>();
const emit = defineEmits<{
  /** "Agregar a la query": the generated DELETE / UPDATE code. */
  script: [code: string];
  filter: [column: string, state: FilterState | null];
  /** The edits were saved (run) on the server: a table's data reloads. */
  applied: [];
  /** A message's line was clicked: where it is in the script that ran
   *  (`offset`, a JS index, when known; `line` 1-based). */
  goto: [at: { offset: number | null; line: number | null }];
}>();

const { t } = useTranslation();

const sets = computed(() => (props.outcome?.results ?? []).map((r, i) => ({ ...r, i })).filter((r) => r.columns.length));
const active = ref<number | 'messages' | 'plan'>(0);
/** How the current result set is shown. */
const show = ref<'table' | 'chart'>('table');
/** The user picked a sub-tab during this outcome. */
const picked = ref(false);
function pick(v: number | 'messages' | 'plan') {
  active.value = v;
  picked.value = true;
}
// A live run (statement by statement) starts on the messages; the first
// result set to arrive takes over unless the user chose a sub-tab.
watch(() => sets.value.length, (n, old) => {
  if (props.running && !old && n && active.value === 'messages' && !picked.value) active.value = sets.value[0].i;
});

watch(
  () => props.outcome,
  (o) => {
    picked.value = false;
    if (!o) return;
    active.value = o.error && !o.plans.length ? 'messages'
      : o.plans.length && !sets.value.length ? 'plan'
      : sets.value.length ? sets.value[0].i
      : 'messages';
  },
);

const current = computed(() => (typeof active.value === 'number' ? props.outcome?.results[active.value] ?? null : null));

// -- editing (the changes become DELETE / UPDATE code; nothing runs) --------------------
const edits = reactive<Record<number, Edits>>({});
/** Rows marked for deletion, per result set. */
const deletes = reactive<Record<number, Set<number>>>({});
watch(() => props.outcome, () => {
  for (const k of Object.keys(edits)) delete edits[Number(k)];
  for (const k of Object.keys(deletes)) delete deletes[Number(k)];
});
const currentEdits = computed(() => (typeof active.value === 'number' ? edits[active.value] ?? {} : {}));
const currentDeletes = computed(() => (typeof active.value === 'number' ? deletes[active.value] ?? new Set<number>() : new Set<number>()));
/** The edits that count: a row marked for deletion drops its own. */
const liveEdits = computed(() => Object.entries(currentEdits.value).filter(([r]) => !currentDeletes.value.has(Number(r))));
const editCount = computed(() => liveEdits.value.reduce((n, [, r]) => n + Object.keys(r).length, 0));
const editRows = computed(() => liveEdits.value.length);
const deleteCount = computed(() => currentDeletes.value.size);
const pending = computed(() => editCount.value + deleteCount.value > 0);

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

// Whether the engine writes DELETEs (Drill, ksqlDB, InfluxDB with Flux or v3
// SQL don't): asked once per table with no rows, the error is the reason.
const deleteReason = ref<string | null>(null);
let deleteSeq = 0;
watch(
  () => (canEdit.value && props.editSource ? JSON.stringify([props.editSource.connectionId, (setup.value as EditSetup).target]) : ''),
  async (k) => {
    const seq = ++deleteSeq;
    deleteReason.value = null;
    const s = setup.value;
    const src = props.editSource;
    if (!k || !s || typeof s === 'string' || !src) return;
    try {
      await invoke<string>('update_script', { args: { connection_id: src.connectionId, target: s.target, changes: [], deletes: [] } });
    } catch (e) {
      if (seq === deleteSeq) deleteReason.value = errorMessage(e);
    }
  },
  { immediate: true },
);
const canDelete = computed(() => canEdit.value && !deleteReason.value);
const noDeleteReason = computed(() => deleteReason.value ?? noEditReason.value);
/** The edits bar's note: no primary key, and the columns left out of the WHERE. */
const editNote = computed(() => {
  const s = setup.value;
  const r = current.value;
  if (!s || typeof s === 'string' || !r) return null;
  const skipped = skippedKeyColumns(r.columns, s, props.dialect ?? '');
  const parts = [s.note, skipped.length ? t('core:gridEdit.skippedColumns', { columns: skipped.join(', ') }) : null].filter(Boolean);
  return parts.length ? parts.join(' ') : null;
});

function onDelete(rows: number[], mark: boolean) {
  if (typeof active.value !== 'number') return;
  const cur = deletes[active.value] ?? new Set<number>();
  const next = new Set(cur);
  for (const r of rows) {
    if (mark) next.add(r);
    else next.delete(r);
  }
  if (next.size) deletes[active.value] = next;
  else delete deletes[active.value];
}

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
  if (typeof active.value !== 'number') return;
  delete edits[active.value];
  delete deletes[active.value];
}

// The code, regenerated as the edits change. Not shown in the bar (it grows
// with every edit): "Guardar" shows it in full before running.
const code = ref('');
const codeError = ref<string | null>(null);
let codeTimer: ReturnType<typeof setTimeout> | undefined;
watch(
  () => [currentEdits.value, currentDeletes.value, setup.value] as const,
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
  if (!pending.value || !s || typeof s === 'string' || !r || !src) { code.value = ''; codeError.value = null; return; }
  const dialect = props.dialect ?? '';
  try {
    const changes = buildChanges(r.columns, r.rows, currentEdits.value, s, dialect, currentDeletes.value);
    // In row order, so the script reads like the grid.
    const keys = [...currentDeletes.value].sort((a, b) => a - b).map((i) => rowKey(r.columns, r.rows[i], s, dialect));
    // Without deletes the call stays as before (older behavior, same code).
    const args = { connection_id: src.connectionId, target: s.target, changes, ...(keys.length ? { deletes: keys } : {}) };
    code.value = await invoke<string>('update_script', { args });
    codeError.value = null;
  } catch (e) {
    code.value = '';
    codeError.value = errorMessage(e);
  }
}
/** "2 celdas modificadas en 1 fila, 3 filas para eliminar": what's pending. */
function pendingText(key: 'summary' | 'hint') {
  const cells = t('results:edit.cells', { count: editCount.value });
  const rows = t('results:edit.rows', { count: editRows.value });
  const dels = t('results:edit.deletes', { count: deleteCount.value });
  if (key === 'summary') {
    if (!deleteCount.value) return t('results:edit.summary', { cells, rows });
    return editCount.value ? t('results:edit.summaryBoth', { cells, rows, deletes: dels }) : t('results:edit.summaryDeletes', { deletes: dels });
  }
  if (!deleteCount.value) return t('applyEdits:hint', { cells, rows });
  return editCount.value ? t('applyEdits:hintBoth', { cells, rows, deletes: dels }) : t('applyEdits:hintDeletes', { deletes: dels });
}
const summary = computed(() => pendingText('summary'));
const applyHint = computed(() => pendingText('hint'));
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
    // The grid shows the saved values right away (a table's data also
    // reloads): edited cells take their values, deleted rows go.
    for (const [row, cols] of liveEdits.value) {
      for (const [col, value] of Object.entries(cols)) r.rows[Number(row)][Number(col)] = value as Cell;
    }
    const gone = [...currentDeletes.value].sort((a, b) => b - a);
    for (const i of gone) r.rows.splice(i, 1);
    r.total_rows = Math.max(0, r.total_rows - gone.length);
    const affected = o.results.reduce((n, x) => n + (x.rows_affected ?? 0), 0);
    discard();
    applyOpen.value = false;
    ElMessage.success(!affected ? t('applyEdits:done') : gone.length ? t('applyEdits:doneAffected', { count: affected }) : t('applyEdits:doneRows', { count: affected }));
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
  if (isSaveShortcut(e)) { saveShortcut(e); return; }
  if (!(e.metaKey || e.ctrlKey) || e.key.toLowerCase() !== 'e') return;
  if (!current.value || !root.value?.offsetParent) return;
  e.preventDefault();
  exporting.value = { format: 'csv' };
}
onMounted(() => window.addEventListener('keydown', onKey));
onBeforeUnmount(() => window.removeEventListener('keydown', onKey));

// ⌘S (Ctrl+S on Windows / Linux) is "Guardar": the review dialog, when this
// pane is the visible one and its grid has pending edits. Inside the SQL
// editor ⌘S saves the query (CodeEditor), and with a dialog open (the
// review included) it does nothing: running is always the "Ejecutar" click.
async function saveShortcut(e: KeyboardEvent) {
  if (inCodeEditor(e)) return;
  e.preventDefault();
  if (applyOpen.value || modalOpen() || show.value !== 'table' || !current.value || !root.value?.offsetParent) return;
  // A cell being edited commits on the key (ResultGrid); its code is
  // generated now instead of after the debounce.
  await nextTick();
  if (!pending.value) return;
  clearTimeout(codeTimer);
  await generate();
  if (!applyOpen.value && !modalOpen()) reviewApply();
}

/** A line of "Mensajes": a statement's outcome, a server message or an error. */
interface Entry {
  level: MessageLevel;
  text: string;
  code?: string | null;
  /** 1-based line of the script that ran, and the error's position in it. */
  line?: number | null;
  offset?: number | null;
}

/** "INSERT 0 3 · 3 filas afectadas · 12 ms": a statement of a run statement by statement. */
function statementEntry(r: StatementResult): Entry {
  const parts: string[] = [];
  if (r.tag) parts.push(r.tag);
  if (r.columns.length) {
    const rows = t('results:messages.rows', { count: r.total_rows, rows: r.total_rows.toLocaleString(locale()) });
    parts.push(r.truncated ? `${rows}${t('results:messages.shown', { rows: r.rows.length.toLocaleString(locale()) })}` : rows);
  } else if (r.rows_affected != null) {
    parts.push(t('results:messages.affected', { count: r.rows_affected, rows: r.rows_affected.toLocaleString(locale()) }));
  } else if (!r.tag) {
    parts.push(t('results:messages.completed'));
  }
  if (r.elapsed_ms != null) parts.push(t('results:messages.elapsed', { ms: r.elapsed_ms.toLocaleString(locale()) }));
  return { level: 'info', text: parts.join(' · '), line: r.line ?? null, offset: r.offset ?? null };
}

/** Statement by statement (results carry their statement, messages a log):
 *  each statement's messages and errors, then its outcome, in script order. */
function scriptEntries(o: QueryOutcome): Entry[] {
  const log = o.log ?? [];
  const errors = [...(o.errors ?? [])];
  const fromLog = (m: Message): Entry => {
    let offset: number | null = null;
    if (m.level === 'error') {
      const k = errors.findIndex((e) => e.message === m.text && e.statement === m.statement);
      if (k >= 0) offset = errors.splice(k, 1)[0].offset;
    }
    return { level: m.level, text: tb(m.text), code: m.code, line: m.line, offset };
  };
  const out: Entry[] = [];
  const stmts = [...new Set([...o.results.map((r) => r.statement), ...log.map((m) => m.statement)])]
    .filter((s): s is number => s != null)
    .sort((a, b) => a - b);
  for (const s of stmts) {
    for (const m of log) if (m.statement === s) out.push(fromLog(m));
    for (const r of o.results) if (r.statement === s) out.push(statementEntry(r));
  }
  for (const r of o.results) if (r.statement == null) out.push(statementEntry(r));
  for (const m of log) if (m.statement == null) out.push(fromLog(m));
  // Messages a driver pushed as plain text after its last logged one.
  const logged = log.filter((m) => m.level !== 'error').length;
  for (const m of o.messages.slice(logged)) out.push({ level: 'info', text: tb(m) });
  if (o.error && !log.some((m) => m.level === 'error')) out.push({ level: 'error', text: tb(o.error) });
  return out;
}

const messages = computed<Entry[]>(() => {
  const o = props.outcome;
  if (!o) return [];
  if (o.log?.length || o.results.some((r) => r.statement != null)) return scriptEntries(o);
  const out: Entry[] = [];
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

const showLines = computed(() => props.lineOffset != null);
/** "Completado" / "Completado con errores" and the total time (as SSMS). */
const statusText = computed(() => {
  const o = props.outcome;
  if (!o || props.running) return '';
  const failed = !!o.error || !!o.errors?.length;
  return `${failed ? t('results:status.doneWithErrors') : t('results:status.done')} · ${t('results:messages.elapsed', { ms: o.elapsed_ms.toLocaleString(locale()) })}`;
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
        @click="pick(s.i)"
      >
        {{ $t('results:tabs.result') }} {{ sets.length > 1 ? s.i + 1 : '' }}
        <span class="rp-count">{{ s.total_rows.toLocaleString(locale()) }}{{ s.truncated ? '+' : '' }}</span>
      </button>
      <button v-if="outcome?.plans.length" class="nm-subtab" :class="{ active: active === 'plan' }" @click="pick('plan')">
        <el-icon><ei-share /></el-icon> {{ $t('results:tabs.plan') }}
      </button>
      <button class="nm-subtab" :class="{ active: active === 'messages' }" @click="pick('messages')">
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
      <template v-if="!hideStatus">
        <span v-if="running" class="rp-status"><el-icon class="is-loading"><ei-loading /></el-icon> {{ $t('results:running') }}</span>
        <span v-else-if="outcome" class="rp-status">{{ outcome.elapsed_ms.toLocaleString(locale()) }} ms</span>
      </template>
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
      <div v-if="pending" class="rp-edits">
        <div class="rp-edits-bar">
          <el-icon><ei-edit /></el-icon>
          <span>{{ summary }}</span>
          <div class="nm-spacer" />
          <el-button size="small" type="primary" :disabled="!code" @click="reviewApply">{{ $t('applyEdits:save') }}</el-button>
          <el-button size="small" :disabled="!code" @click="addToQuery">{{ $t('results:edit.addToQuery') }}</el-button>
          <el-button size="small" :disabled="!code" @click="copyCode">{{ $t('common:copy') }}</el-button>
          <el-button size="small" text @click="discard">{{ $t('results:edit.discard') }}</el-button>
        </div>
        <div v-if="editNote" class="rp-edits-note">{{ editNote }}</div>
        <div v-if="codeError" class="rp-edits-note err">{{ codeError }}</div>
      </div>
      <ResultGrid
        :columns="current.columns"
        :rows="current.rows"
        :copy-context="{ table: table ?? null, dialect: dialect ?? '', keyColumns }"
        :editable="canEdit"
        :no-edit-reason="noEditReason"
        :edits="currentEdits"
        :deleted="currentDeletes"
        :deletable="canDelete"
        :no-delete-reason="noDeleteReason"
        :filterable="filterable"
        :filters="filters"
        @edit="onEdit"
        @delete="onDelete"
        @filter="(c, st) => emit('filter', c, st)"
      />
      </template>
    </template>
    <div v-else class="rp-messages nm-selectable">
      <div v-for="(m, i) in messages" :key="i" :class="['rp-msg', m.level]">
        <button
          v-if="showLines && m.line != null"
          class="rp-msg-line"
          :title="$t('results:messages.goToLine')"
          @click="emit('goto', { offset: m.offset ?? null, line: m.line ?? null })"
        >{{ $t('results:messages.line', { line: m.line + (lineOffset ?? 0) }) }}</button>
        <span v-if="m.code" class="rp-msg-code">{{ m.code }}</span>
        <span>{{ m.text }}</span>
      </div>
      <div v-if="running" class="rp-msg rp-msg-status"><el-icon class="is-loading"><ei-loading /></el-icon> {{ $t('results:running') }}</div>
      <div v-else-if="statusText" class="rp-msg rp-msg-status">{{ statusText }}</div>
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
      <p class="rp-apply-hint">{{ applyHint }}</p>
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
.rp-msg.warning { color: var(--nm-warning); }
.rp-msg-line {
  margin-right: 6px; padding: 0; font: inherit; color: var(--ide-focus, var(--nm-text-dim));
  background: transparent; border: none; cursor: pointer; text-decoration: underline dotted;
}
.rp-msg-line:hover { text-decoration: underline; }
.rp-msg-code { margin-right: 6px; padding: 0 4px; border-radius: 3px; background: var(--ide-button-2); color: var(--nm-text); }
.rp-msg-status { margin-top: 6px; color: var(--nm-text-dim); }
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
