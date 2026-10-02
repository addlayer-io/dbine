<script lang="ts">
import type { ClonePhase as Phase, CloneTableResult as Result } from '../api/cloneTable';
import type { TaskHandle } from '../stores/tasks';

/** A clone run's state. It lives outside the dialog so the run keeps going
 *  (and can be shown again) after "Seguir en segundo plano" closes it. */
interface CloneRun {
  name: string;
  withData: boolean;
  withIndexes: boolean;
  running: boolean;
  cancelling: boolean;
  phase: Phase | null;
  rowsDone: number;
  rowsTotal: number | null;
  error: string | null;
  warnings: string[];
  result: Result | null;
  task: TaskHandle<Result> | null;
  /** Dialogs showing it right now, and how to close the one that does. */
  viewers: number;
  close: (() => void) | null;
}

/** The run a reopened dialog shows (set right before it mounts). */
let adopt: CloneRun | null = null;
</script>

<script setup lang="ts">
import { computed, defineComponent, getCurrentInstance, h, markRaw, onBeforeUnmount, reactive, render, toRefs, type Component, type VNode } from 'vue';
import { ElConfigProvider, ElMessage } from 'element-plus';
import elEn from 'element-plus/es/locale/lang/en.mjs';
import elEs from 'element-plus/es/locale/lang/es.mjs';
import elPt from 'element-plus/es/locale/lang/pt-br.mjs';
import elFr from 'element-plus/es/locale/lang/fr.mjs';
import elIt from 'element-plus/es/locale/lang/it.mjs';
import { useTranslation } from 'i18next-vue';
import { errorKind, errorMessage } from '../api/client';
import { cloneTableApi, defaultCloneName, type CloneTableEvent, type CloneTableResult } from '../api/cloneTable';
import type { ObjectRef } from '../api/types';
import { language } from '../i18n';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { useUiStore } from '../stores/ui';
import { startTask, useTasksStore } from '../stores/tasks';

// "Clonar…" on a table (collection…): a copy next to it under a new name,
// proposed as <name>_yyyyMMdd_HHmmss, with its data and indexes. Whatever
// fails, the backend drops the clone; the original is only read. The run is
// a task (stores/tasks.ts): "Seguir en segundo plano" closes the dialog and
// the clone goes on; "Ver detalle" in the Tareas panel shows it again.

const props = defineProps<{ connectionId: string; database: string; object: ObjectRef }>();
const emit = defineEmits<{ close: [] }>();

const { t } = useTranslation();
const conns = useConnectionsStore();
const ui = useUiStore();
const tasks = useTasksStore();
const inst = getCurrentInstance()!;

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

const s: CloneRun = adopt ?? reactive<CloneRun>({
  name: proposedName(), withData: true, withIndexes: true, running: false, cancelling: false, phase: null,
  rowsDone: 0, rowsTotal: null, error: null, warnings: [], result: null, task: null, viewers: 0, close: null,
});
adopt = null;
s.viewers++;
const closeMe = markRaw(() => emit('close'));
s.close = closeMe;
onBeforeUnmount(() => {
  s.viewers--;
  if (s.close === closeMe) s.close = null;
  // Gone without "Seguir en segundo plano" (its tab or the explorer closed):
  // the clone goes on, and says so when it ends.
  if (s.running && !s.viewers) s.task?.background();
});

const { name, withData, withIndexes, running, cancelling, phase, rowsDone, rowsTotal, error, warnings, result } = toRefs(s);

const source = computed(() => (props.object.schema ? `${props.object.schema}.${props.object.name}` : props.object.name));
const valid = computed(() => name.value.trim().length > 0 && name.value.trim() !== props.object.name);
const percent = computed(() => (rowsTotal.value ? Math.min(100, Math.round((rowsDone.value / rowsTotal.value) * 100)) : 0));
const fmt = (n: number) => n.toLocaleString();

/** A reopened copy mounts outside App.vue's <el-config-provider>: it gets
 *  the same Element Plus locale (the app's language, as App.vue does). */
function withAppLocale(child: () => VNode): VNode {
  const locales = { en: elEn, es: elEs, pt: elPt, fr: elFr, it: elIt };
  return h(defineComponent({ setup: () => () => h(ElConfigProvider, { locale: locales[language.value] }, { default: child }) }));
}

/** "Ver detalle" from the Tareas panel: this dialog again, on the same run. */
function reopener(connectionId: string, database: string, object: ObjectRef) {
  return () => {
    if (s.viewers) return;
    adopt = s;
    const el = document.createElement('div');
    document.body.appendChild(el);
    const onClose = () => { render(null, el); el.remove(); };
    const vnode = withAppLocale(() => h(inst.type as Component, { connectionId, database, object, onClose }));
    vnode.appContext = inst.appContext;
    render(vnode, el);
  };
}

async function start() {
  if (!valid.value || s.running) return;
  // A retry (after an error or a stop) from this dialog is a new task: the
  // earlier one's "Ver detalle" keeps its own outcome (the panel's detail),
  // not this run's state.
  s.task?.setReopen(undefined);
  Object.assign(s, { running: true, cancelling: false, error: null, warnings: [], phase: null, rowsDone: 0, rowsTotal: null, result: null });
  const { connectionId, database, object } = props;
  const newName = s.name.trim();
  const runId = `clone-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  const task = startTask<CloneTableResult>({
    kind: 'clone-table',
    title: t('tasks:dialogs.cloneTable', { table: source.value, name: newName }),
    connectionId, database,
    cancel: () => { s.cancelling = true; return cloneTableApi.cancel(runId); },
    reopen: reopener(connectionId, database, object),
  });
  s.task = markRaw(task);
  const onEvent = (e: CloneTableEvent) => {
    if (e.event === 'phase') {
      s.phase = e.phase;
      task.progress({ phase: t(`cloneTable:phase.${e.phase}`) });
    } else if (e.event === 'progress') {
      // Some engines only estimate the total (DynamoDB's ItemCount): once
      // the copy passes it, it's no total at all.
      const total = e.rows_total != null && e.rows_total >= e.rows_done ? e.rows_total : null;
      s.rowsDone = e.rows_done;
      s.rowsTotal = total;
      task.progress({ done: e.rows_done, total: total ?? undefined, unit: 'rows' });
    } else if (e.level !== 'info') {
      // Raw, like the notes: the template translates both once.
      s.warnings.push(e.text);
      task.log(tb(e.text), e.level);
    }
  };
  try {
    await task.listen<CloneTableEvent>('clone-table-progress', (e) => { if (e.payload.runId === runId) onEvent(e.payload); });
    const r = await cloneTableApi.run({
      runId, connectionId, database, object, newName, withData: s.withData, withIndexes: s.withIndexes,
    });
    for (const n of r.notes) task.log(tb(n), 'warn');
    task.finish(r, t('tasks:dialogs.rows', { count: r.rows, n: fmt(r.rows) }));
    // The clone is done whatever the explorer reload does.
    await conns.loadObjects(connectionId, database, true).catch(() => {});
    // Shown: the tree jumps to the clone. In the background, the notice says it.
    if (s.viewers) {
      ui.revealInExplorer({ connectionId, database, object: r.table });
      ElMessage.success(t('cloneTable:done', { name: r.table.name, rows: fmt(r.rows) }));
    }
    // Kept either way: "Ver detalle" then shows the outcome, not the form
    // (which would invite a second clone).
    s.result = r;
    if (!r.notes.length && !s.warnings.length) s.close?.();
  } catch (e) {
    const stopped = errorKind(e) === 'cancelled' || task.isCancelling;
    s.error = stopped ? t('cloneTable:cancelled') : errorMessage(e);
    if (stopped) task.cancelled(); else task.fail(e);
  } finally {
    s.running = false;
    s.cancelling = false;
  }
}

function cancel() {
  if (!s.running || !s.task) return emit('close');
  tasks.cancel(s.task.id);
}

function toBackground() {
  s.task?.background();
  emit('close');
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
      <template v-if="result.notes.length || warnings.length">
      <div class="ct-notes-title">{{ $t('cloneTable:notes') }}</div>
      <ul class="ct-notes">
        <li v-for="(n, i) in [...result.notes, ...warnings]" :key="i">{{ tb(n) }}</li>
      </ul>
      </template>
    </template>
    <template #footer>
      <template v-if="result">
        <el-button type="primary" @click="emit('close')">{{ $t('common:close') }}</el-button>
      </template>
      <template v-else>
        <el-button v-if="running" @click="toBackground">{{ $t('tasks:panel.background') }}</el-button>
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
