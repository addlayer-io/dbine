<script lang="ts">
import type { TaskHandle } from '../stores/tasks';

interface RunResult { statements: number; errors: string[]; elapsed_ms: number }
interface RunProgress { id: string; statements: number; bytes: number; total_bytes: number }

/** A run's state. It lives outside the dialog so the run keeps going (and
 *  can be shown again) after "Seguir en segundo plano" closes it. */
interface FileRun {
  path: string;
  size: number | null;
  continueOnError: boolean;
  running: boolean;
  cancelling: boolean;
  cancelled: boolean;
  progress: { statements: number; bytes: number; total_bytes: number };
  elapsed: number;
  result: RunResult | null;
  runError: string;
  task: TaskHandle<RunResult> | null;
  /** Dialogs showing it right now. */
  viewers: number;
}

/** The run a reopened dialog shows (set right before it mounts). */
let adopt: FileRun | null = null;
</script>

<script setup lang="ts">
import { computed, defineComponent, getCurrentInstance, h, markRaw, onBeforeUnmount, reactive, render, toRefs, type Component, type VNode } from 'vue';
import { ElConfigProvider, ElMessage } from 'element-plus';
import elEn from 'element-plus/es/locale/lang/en.mjs';
import elEs from 'element-plus/es/locale/lang/es.mjs';
import elPt from 'element-plus/es/locale/lang/pt-br.mjs';
import elFr from 'element-plus/es/locale/lang/fr.mjs';
import elIt from 'element-plus/es/locale/lang/it.mjs';
import { invoke } from '@tauri-apps/api/core';
import { open } from '@tauri-apps/plugin-dialog';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { language, locale } from '../i18n';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { startTask, useTasksStore } from '../stores/tasks';

// Run a script file against a database (restore a dump): the backend reads
// it in chunks and splits statements the driver's way, so files of any size
// work. Shows progress by bytes and lists the errors at the end.
// Backend: `run_script_file` (docs/api-comandos.md). The run is a task
// (stores/tasks.ts): "Seguir en segundo plano" closes the dialog and the run
// goes on; "Ver detalle" in the Tareas panel shows this dialog again.

const props = withDefaults(defineProps<{
  connectionId: string;
  database: string;
  /** Opens with this file already chosen. */
  initialPath?: string;
  /** File size in bytes, when the host knows it (otherwise it shows once the run starts). */
  initialSize?: number | null;
}>(), { initialPath: '', initialSize: null });
const emit = defineEmits<{ close: [] }>();
const { t } = useTranslation();
const conns = useConnectionsStore();
const tasks = useTasksStore();
const inst = getCurrentInstance()!;

const s: FileRun = adopt ?? reactive<FileRun>({
  path: props.initialPath, size: props.initialSize, continueOnError: false, running: false, cancelling: false, cancelled: false,
  progress: { statements: 0, bytes: 0, total_bytes: 0 }, elapsed: 0, result: null, runError: '', task: null, viewers: 0,
});
adopt = null;
s.viewers++;
onBeforeUnmount(() => {
  s.viewers--;
  // Gone without "Seguir en segundo plano" (e.g. its host closed): the run
  // goes on, and says so when it ends.
  if (s.running && !s.viewers) s.task?.background();
});

const { path, size, continueOnError, running, cancelling, cancelled, elapsed, result, runError } = toRefs(s);
const progress = s.progress;
const fileName = computed(() => path.value.split(/[\\/]/).pop() ?? '');

const percent = computed(() =>
  progress.total_bytes ? Math.min(100, Math.floor((progress.bytes / progress.total_bytes) * 100)) : 0);
const finished = computed(() => !!result.value || !!runError.value);

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}
const seconds = (ms: number) => (ms / 1000).toFixed(1);

async function pickFile() {
  let picked: string | string[] | null = null;
  try {
    picked = await open({
      multiple: false,
      directory: false,
      filters: [
        { name: 'Scripts (SQL, CQL, JS, JSON, TXT)', extensions: ['sql', 'cql', 'js', 'json', 'txt'] },
        { name: t('scripts:run.allFiles'), extensions: ['*'] },
      ],
    });
  } catch { /* no dialog outside Tauri */ }
  if (typeof picked !== 'string') return;
  s.path = picked;
  s.size = null;
  reset();
}

function reset() {
  s.result = null;
  s.runError = '';
  s.cancelled = false;
  Object.assign(s.progress, { statements: 0, bytes: 0, total_bytes: 0 });
}

/** A reopened copy mounts outside App.vue's <el-config-provider>: it gets
 *  the same Element Plus locale (the app's language, as App.vue does). */
function withAppLocale(child: () => VNode): VNode {
  const locales = { en: elEn, es: elEs, pt: elPt, fr: elFr, it: elIt };
  return h(defineComponent({ setup: () => () => h(ElConfigProvider, { locale: locales[language.value] }, { default: child }) }));
}

/** "Ver detalle" from the Tareas panel: this dialog again, on the same run. */
function reopener(connectionId: string, database: string) {
  return () => {
    if (s.viewers) return;
    adopt = s;
    const el = document.createElement('div');
    document.body.appendChild(el);
    const onClose = () => { render(null, el); el.remove(); };
    const vnode = withAppLocale(() => h(inst.type as Component, { connectionId, database, onClose }));
    vnode.appContext = inst.appContext;
    render(vnode, el);
  };
}

async function run() {
  if (!s.path || s.running) return;
  // "Ejecutar de nuevo" is a new task: the earlier one's "Ver detalle" keeps
  // its own outcome (the panel's detail), not this run's state.
  s.task?.setReopen(undefined);
  reset();
  s.running = true;
  s.cancelling = false;
  const { connectionId, database } = props;
  const runId = crypto.randomUUID();
  const file = fileName.value;
  const started = Date.now();
  s.elapsed = 0;
  const timer = setInterval(() => { s.elapsed = Date.now() - started; }, 250);
  const task = startTask<RunResult>({
    kind: 'script-run',
    title: t('tasks:dialogs.runScriptFile', { file, db: database || conns.byId(connectionId)?.name || '' }),
    connectionId, database,
    cancel: () => { s.cancelling = true; return invoke('cancel_query', { args: { session_id: `run:${runId}` } }); },
    reopen: reopener(connectionId, database),
  });
  s.task = markRaw(task);
  try {
    await task.listen<RunProgress>('script-run-progress', (e) => {
      if (e.payload.id !== runId) return;
      Object.assign(s.progress, { statements: e.payload.statements, bytes: e.payload.bytes, total_bytes: e.payload.total_bytes });
      if (e.payload.total_bytes) s.size = e.payload.total_bytes;
      // Bytes read out of the file's size is the one count with a known total
      // (the ETA needs it); the statements run so far go in the phase.
      const { statements, bytes, total_bytes } = e.payload;
      task.progress(total_bytes
        ? { done: bytes, total: total_bytes, unit: 'bytes', phase: t('tasks:dialogs.statements', { count: statements, n: statements.toLocaleString(locale()) }) }
        : { done: statements, unit: 'statements', phase: undefined });
    });
    const r = await invoke<RunResult>('run_script_file', {
      args: {
        run_id: runId,
        connection_id: connectionId,
        database,
        path: s.path,
        continue_on_error: s.continueOnError,
      },
    });
    s.result = r;
    for (const e of r.errors) task.log(tb(e), 'error');
    const n = r.statements.toLocaleString(locale());
    task.finish(
      r,
      r.errors.length ? t('tasks:dialogs.statementsWithErrors', { count: r.statements, n, errors: r.errors.length }) : t('tasks:dialogs.statements', { count: r.statements, n }),
      r.errors.length ? 'error' : 'done',
    );
    if (!r.errors.length && s.viewers) {
      ElMessage.success({ message: t('scripts:run.done', { count: r.statements, n }), duration: 3000 });
    }
  } catch (e) {
    s.cancelled = s.cancelling || task.isCancelling;
    s.runError = s.cancelled ? t('scripts:run.cancelledMessage') : errorMessage(e);
    if (s.cancelled) task.cancelled(); else task.fail(e);
  } finally {
    s.running = false;
    s.cancelling = false;
    clearInterval(timer);
    // The file changed the database: the explorer shows it, once per run,
    // whether the dialog is open, closed or in the background.
    conns.loadObjects(connectionId, database, true).catch(() => {});
  }
}

async function copyErrors() {
  const text = [...(result.value?.errors ?? []), ...(runError.value && !cancelled.value ? [runError.value] : [])].join('\n\n');
  try {
    await navigator.clipboard.writeText(text);
    ElMessage.success({ message: t('scripts:run.errorsCopied'), duration: 1500 });
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

function cancel() {
  if (s.running && s.task) tasks.cancel(s.task.id);
  else emit('close');
}

function toBackground() {
  s.task?.background();
  emit('close');
}

defineExpose({ run });
</script>

<template>
  <el-dialog
    :model-value="true" :title="$t('scripts:run.title')" class="rs-dialog" width="720px" append-to-body align-center
    :close-on-click-modal="false" :close-on-press-escape="!running" :show-close="!running"
    @close="cancel"
  >
    <div class="rs-file">
      <el-icon class="rs-file-icon"><ei-document /></el-icon>
      <div class="rs-file-text">
        <template v-if="path">
          <div class="rs-file-name">
            {{ fileName }}
            <span v-if="size != null" class="rs-size">{{ formatBytes(size) }}</span>
          </div>
          <div class="rs-file-path nm-selectable" :title="path">{{ path }}</div>
        </template>
        <div v-else class="nm-muted">{{ $t('scripts:run.pickHint') }}</div>
      </div>
      <el-button :type="path ? 'default' : 'primary'" :disabled="running" @click="pickFile">
        {{ path ? $t('scripts:run.change') : $t('scripts:run.pickFile') }}
      </el-button>
    </div>

    <div class="rs-opts">
      <el-checkbox v-model="continueOnError" :disabled="running">{{ $t('scripts:run.continueOnError') }}</el-checkbox>
      <span class="rs-help">
        {{ continueOnError
          ? $t('scripts:run.continueOnErrorHelp')
          : $t('scripts:run.stopOnErrorHelp') }}
      </span>
    </div>
    <el-alert v-if="!finished && !running" type="warning" :closable="false" show-icon class="rs-alert">
      <template #title>
        <i18next :translation="$t('scripts:run.warning')"><template #database><b>{{ database }}</b></template></i18next>
      </template>
    </el-alert>

    <div v-if="running || finished" class="rs-progress">
      <div class="rs-progress-head">
        <el-icon v-if="running" class="is-loading"><ei-loading /></el-icon>
        <span v-if="running">{{ cancelling ? $t('scripts:run.cancelling') : $t('scripts:run.running') }}</span>
        <span v-else-if="result && !result.errors.length" class="rs-ok"><el-icon><ei-circle-check-filled /></el-icon> {{ $t('scripts:run.finished') }}</span>
        <span v-else-if="result" class="rs-warn"><el-icon><ei-warning-filled /></el-icon> {{ $t('scripts:run.finishedWithErrors') }}</span>
        <span v-else-if="cancelled" class="rs-warn"><el-icon><ei-warning-filled /></el-icon> {{ $t('scripts:run.cancelled') }}</span>
        <span v-else class="rs-err"><el-icon><ei-circle-close-filled /></el-icon> {{ $t('scripts:run.stoppedByError') }}</span>
        <span class="nm-spacer" />
        <span v-if="progress.total_bytes" class="rs-counter">
          {{ formatBytes(progress.bytes) }} / {{ formatBytes(progress.total_bytes) }} · {{ percent }}%
        </span>
      </div>
      <el-progress
        :percentage="result ? 100 : percent" :show-text="false" :stroke-width="4"
        :status="result && !result.errors.length ? 'success' : undefined"
        :indeterminate="running && !progress.total_bytes" :duration="2"
      />
      <div class="rs-stats">
        <div><span class="rs-stat">{{ (result?.statements ?? progress.statements).toLocaleString(locale()) }}</span><span class="nm-muted">{{ $t('scripts:run.statements', { count: result?.statements ?? progress.statements }) }}</span></div>
        <div>
          <span class="rs-stat" :class="{ bad: (result?.errors.length ?? 0) > 0 }">{{ (result?.errors.length ?? 0).toLocaleString(locale()) }}</span>
          <span class="nm-muted">{{ $t('scripts:run.errorsUnit', { count: result?.errors.length ?? 0 }) }}</span>
        </div>
        <div><span class="rs-stat">{{ seconds(result?.elapsed_ms ?? elapsed) }}</span><span class="nm-muted">{{ $t('scripts:run.seconds') }}</span></div>
      </div>
    </div>

    <div v-if="(result && result.errors.length) || (runError && !cancelled)" class="rs-errors">
      <div class="rs-errors-head">
        <span class="nm-section-title">{{ $t('scripts:run.errors') }}</span>
        <span class="nm-spacer" />
        <el-button link type="primary" @click="copyErrors"><el-icon><ei-copy-document /></el-icon>&nbsp;{{ $t('common:copy') }}</el-button>
      </div>
      <div class="rs-errors-list nm-selectable">
        <div v-for="(e, i) in result?.errors ?? [runError]" :key="i" class="rs-error">
          <span class="rs-error-n">{{ i + 1 }}</span><pre>{{ tb(e) }}</pre>
        </div>
      </div>
    </div>

    <template #footer>
      <el-button v-if="running" @click="toBackground">{{ $t('tasks:panel.background') }}</el-button>
      <el-button :disabled="cancelling" @click="cancel">
        {{ running ? $t('scripts:run.cancelRun') : finished ? $t('common:close') : $t('common:cancel') }}
      </el-button>
      <el-button type="primary" :loading="running" :disabled="!path" @click="run">
        {{ finished ? $t('scripts:run.runAgain') : $t('common:run') }}
      </el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.rs-opts :deep(.el-checkbox__input.is-checked + .el-checkbox__label) { color: var(--nm-text); }
.rs-file { display: flex; align-items: center; gap: 12px; padding: 14px 16px; border: 1px dashed var(--nm-border); border-radius: var(--nm-radius); background: var(--ide-editor); }
.rs-file-icon { font-size: 26px; color: var(--nm-accent); }
.rs-file-text { flex: 1; min-width: 0; }
.rs-file-name { color: var(--nm-text-strong); font-weight: 600; display: flex; align-items: baseline; gap: 8px; }
.rs-size { font-weight: 400; font-size: 11.5px; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; }
.rs-file-path { font-family: var(--nm-mono); font-size: 11.5px; color: var(--nm-text-dim); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; margin-top: 2px; }
.rs-opts { display: flex; align-items: center; gap: 10px; margin: 12px 0 10px; }
.rs-help { font-size: 11.5px; color: var(--nm-text-dim); }
.rs-alert b { color: var(--nm-text-strong); }

.rs-progress { margin-top: 12px; display: flex; flex-direction: column; gap: 8px; }
.rs-progress-head { display: flex; align-items: center; gap: 8px; font-size: 12.5px; }
.rs-progress-head .el-icon { vertical-align: -2px; }
.rs-ok { color: var(--nm-success); }
.rs-warn { color: var(--nm-warning); }
.rs-err { color: var(--nm-danger); }
.rs-counter { color: var(--nm-text-dim); font-variant-numeric: tabular-nums; font-size: 12px; }
.rs-stats { display: grid; grid-template-columns: repeat(3, 1fr); gap: 10px; }
.rs-stats > div { display: flex; align-items: baseline; gap: 6px; padding: 8px 12px; border: 1px solid var(--nm-border-soft); background: var(--ide-editor); }
.rs-stat { font-size: 18px; color: var(--nm-text-strong); font-variant-numeric: tabular-nums; }
.rs-stat.bad { color: var(--nm-danger); }

.rs-errors { margin-top: 12px; }
.rs-errors-head { display: flex; align-items: center; }
.rs-errors-head .nm-section-title { margin-bottom: 4px; }
.rs-errors-list { max-height: 220px; overflow: auto; border: 1px solid var(--nm-border-soft); background: var(--ide-editor); }
.rs-error { display: flex; gap: 10px; padding: 6px 10px; border-bottom: 1px solid var(--nm-border-soft); }
.rs-error:last-child { border-bottom: none; }
.rs-error-n { color: var(--nm-text-muted); font-family: var(--nm-mono); font-size: 11px; min-width: 18px; text-align: right; padding-top: 1px; }
.rs-error pre { margin: 0; font-family: var(--nm-mono); font-size: 11.5px; color: #f48771; white-space: pre-wrap; word-break: break-word; user-select: text; cursor: text; }
</style>
