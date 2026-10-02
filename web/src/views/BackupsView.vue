<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, reactive, ref } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { invoke } from '@tauri-apps/api/core';
import { save } from '@tauri-apps/plugin-dialog';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import { backupApi, type BackupAction, type BackupCopy, type BackupEntry } from '../api/backup';
import type { Field } from '../api/types';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import { dbKey, useConnectionsStore } from '../stores/connections';
import type { BackupsTab } from '../stores/tabs';
import { startTask, useTasksStore, type TaskHandle } from '../stores/tasks';

// Backups (docs/backups.md): DBine's copies of the database (a script file
// with its structure and data, for every engine, restored by running it) and
// the engine's own backups (its history; the code for a backup, a restore or
// a delete is shown and runs only on the user's click).

const props = defineProps<{ tab: BackupsTab }>();
const conns = useConnectionsStore();
const { t } = useTranslation();
const driver = computed(() => conns.driverOf(props.tab.connectionId));
const spec = computed(() => driver.value?.backup ?? null);
const readOnly = computed(() => !!conns.byId(props.tab.connectionId)?.config.read_only);
/** Opened on the connection (a server-wide engine): no DBine copy of "the server". */
const serverView = computed(() => !props.tab.database);
const databases = computed(() => conns.live[props.tab.connectionId]?.databases ?? []);
/** Why the login can't make / restore native backups ('' when it can). */
const noPermission = (action: 'backup' | 'restore') => {
  const missing = conns.denied(props.tab.connectionId, props.tab.database, action);
  return missing ? t('common:noPermission', { missing }) : '';
};

const copies = ref<BackupCopy[]>([]);
const native = ref<BackupEntry[]>([]);
const nativeError = ref<string | null>(null);
const loading = ref(false);
const error = ref<string | null>(null);

async function load() {
  loading.value = true;
  error.value = null;
  try {
    if (!(await conns.ensureConnected(props.tab.connectionId))) return;
    conns.loadPermissions(props.tab.connectionId, props.tab.database);
    const l = await backupApi.list(props.tab.connectionId, props.tab.database);
    copies.value = l.copies;
    native.value = l.native;
    nativeError.value = l.native_error ? tb(l.native_error) : null;
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    loading.value = false;
  }
}
onMounted(load);

const when = (iso: string | null) => (iso ? new Date(iso).toLocaleString(locale()) : '—');
function size(n: number | null) {
  if (n == null) return '—';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(v < 10 && i ? 1 : 0)} ${units[i]}`;
}
const num = (n: number) => n.toLocaleString(locale());

// -- background tasks ------------------------------------------------------------
// Copies, restores and native runs are registered in the tasks store, so they
// keep going (and can be cancelled from the Tareas panel) after their dialog
// closes or this tab does. The dialogs below are only a view of the task while
// this component lives.
const tasks = useTasksStore();
let alive = true;
const owned = new Set<TaskHandle<any>>();
onBeforeUnmount(() => {
  alive = false;
  // The dialogs are gone: "Ver detalle" falls back to the panel's own detail,
  // and a run whose dialog was still open notifies when it ends (the tab can
  // close with ⌘W while the modal is open, which emits no close).
  for (const h of owned) {
    h.setReopen(undefined);
    h.background();
  }
});
/** "conexión / base" for the task's title. */
const where = () => {
  const name = conns.byId(props.tab.connectionId)?.name ?? '';
  return props.tab.database ? `${name} / ${props.tab.database}` : name;
};
const isBackground = (h: TaskHandle<any> | null) => !!h && !!tasks.byId(h.id)?.background;
const isRunning = (h: TaskHandle<any> | null) => !!h && tasks.byId(h.id)?.state === 'running';
/** "Seguir en segundo plano", or the dialog closed (Esc) while its run goes on. */
function sendToBackground(h: TaskHandle<any> | null, close?: () => void) {
  if (isRunning(h)) h!.background();
  close?.();
}

// -- DBine copy ----------------------------------------------------------------
const copy = reactive({ open: false, path: '', data: true, running: false, cancelling: false, done: 0, total: 0, current: '' });
let copyTask: TaskHandle<any> | null = null;

async function openCopy() {
  if (copy.running) { copy.open = true; return; }
  try {
    copy.path = await backupApi.defaultPath(props.tab.connectionId, props.tab.database);
  } catch (e) {
    ElMessage.error(errorMessage(e));
    return;
  }
  Object.assign(copy, { open: true, data: true, running: false, cancelling: false, done: 0, total: 0, current: '' });
}
async function pickCopyPath() {
  try {
    const ext = copy.path.split('.').pop() || 'sql';
    const p = await save({ defaultPath: copy.path, filters: [{ name: 'Script', extensions: [ext] }] });
    if (p) copy.path = p;
  } catch { /* no dialog outside Tauri */ }
}
async function runCopy() {
  if (!copy.path || copy.running) return;
  copy.running = true;
  copy.cancelling = false;
  const id = crypto.randomUUID();
  const { connectionId, database } = props.tab;
  const path = copy.path;
  const data = copy.data;
  // The task exists from the first click, so Cancelar and "Seguir en segundo
  // plano" work while the object list loads too.
  const task: TaskHandle<any> = startTask({
    kind: 'backup-copy', title: t('tasks:backups.copy', { where: where() }), connectionId, database,
    cancel: () => { copy.cancelling = true; return api.cancelQuery(`script:${id}`); },
    reopen: () => { copy.open = true; },
  });
  copyTask = task;
  owned.add(task);
  const h = task;
  Object.assign(copy, { done: 0, total: 0, current: '' });
  try {
    await conns.loadObjects(connectionId, database);
    if (h.isCancelling) throw new Error('cancelled');
    const objects = (conns.objects[dbKey(connectionId, database)]?.items ?? []).map((o) => ({ kind: o.kind, schema: o.schema, name: o.name }));
    if (!objects.length) {
      h.cancelled(t('backups:copy.empty'));
      if (!isBackground(h)) ElMessage.warning(t('backups:copy.empty'));
      return;
    }
    Object.assign(copy, { done: 0, total: objects.length, current: '' });
    h.progress({ done: 0, total: objects.length, unit: 'objects' });
    await h.listen<{ id: string; done: number; total: number; current: string }>('script-progress', (e) => {
      if (e.payload.id !== id) return;
      Object.assign(copy, { done: e.payload.done, total: e.payload.total, current: e.payload.current });
      h.progress({ done: e.payload.done, total: e.payload.total, phase: e.payload.current });
    });
    const c = await backupApi.copy(id, connectionId, database, objects, data, path);
    h.finish(c, t('tasks:backups.copyDone', { size: size(c.size), path }));
    const quiet = isBackground(h);
    copy.open = false;
    if (!quiet) ElMessage.success(t('backups:copy.done', { size: size(c.size) }));
    if (alive) await load();
  } catch (e) {
    const quiet = isBackground(task);
    if (copy.cancelling) {
      task.cancelled();
      if (!quiet) ElMessage.info(t('backups:copy.cancelled'));
    } else {
      task.fail(e);
      if (!quiet) ElMessage.error(errorMessage(e));
    }
  } finally {
    copy.running = false;
    // Finished: "Ver detalle" shows the panel's detail, not a fresh form.
    task.setReopen(undefined);
    owned.delete(task);
  }
}
function cancelCopy() {
  if (copyTask) tasks.cancel(copyTask.id);
}

// -- restore a DBine copy (run its script) ------------------------------------
const restoreCopy = reactive({ open: false, copy: null as BackupCopy | null, database: '', continueOnError: false, running: false, cancelling: false, bytes: 0, total: 0, errors: [] as string[] });
let restoreTask: TaskHandle<any> | null = null;
function openRestoreCopy(c: BackupCopy) {
  if (restoreCopy.running) { restoreCopy.open = true; return; }
  Object.assign(restoreCopy, { open: true, copy: c, database: c.database, continueOnError: false, running: false, cancelling: false, bytes: 0, total: c.size, errors: [] });
}
async function runRestoreCopy() {
  const c = restoreCopy.copy;
  if (!c) return;
  try {
    await ElMessageBox.confirm(t('backups:restoreCopy.confirm', { db: restoreCopy.database || c.database }), t('backups:restoreCopy.title'), {
      type: 'warning', confirmButtonText: t('backups:restore'), cancelButtonText: t('common:cancel'),
    });
  } catch { return; }
  if (restoreCopy.running) return;
  restoreCopy.running = true;
  restoreCopy.cancelling = false;
  restoreCopy.errors = [];
  const runId = crypto.randomUUID();
  const connectionId = props.tab.connectionId;
  const database = restoreCopy.database;
  const name = conns.byId(connectionId)?.name ?? '';
  const task = startTask<{ statements: number; errors: string[] }>({
    kind: 'restore', title: t('tasks:backups.restoreCopy', { where: database ? `${name} / ${database}` : name }), connectionId, database,
    cancel: () => { restoreCopy.cancelling = true; return api.cancelQuery(`run:${runId}`); },
    reopen: () => { restoreCopy.open = true; },
  });
  restoreTask = task;
  owned.add(task);
  task.progress({ done: 0, total: c.size, unit: 'bytes' });
  try {
    await task.listen<{ id: string; statements: number; bytes: number; total_bytes: number }>('script-run-progress', (e) => {
      if (e.payload.id !== runId) return;
      restoreCopy.bytes = e.payload.bytes;
      if (e.payload.total_bytes) restoreCopy.total = e.payload.total_bytes;
      task.progress({
        // No total in the event: keep the copy's size, so the estimate survives.
        done: e.payload.bytes, total: e.payload.total_bytes || restoreCopy.total || undefined,
        phase: t('tasks:backups.statements', { n: num(e.payload.statements) }),
      });
    });
    const r = await invoke<{ statements: number; errors: string[] }>('run_script_file', {
      args: { run_id: runId, connection_id: connectionId, database, path: c.path, continue_on_error: restoreCopy.continueOnError },
    });
    restoreCopy.errors = r.errors;
    for (const err of r.errors) task.log(err, 'error');
    task.finish(r, r.errors.length
      ? t('tasks:backups.statementsWithErrors', { n: num(r.statements), errors: num(r.errors.length) })
      : t('tasks:backups.statements', { n: num(r.statements) }), r.errors.length ? 'error' : 'done');
    if (!r.errors.length) {
      const quiet = isBackground(task);
      restoreCopy.open = false;
      if (!quiet) ElMessage.success(t('backups:restoreCopy.done', { n: num(r.statements) }));
    }
    conns.loadObjects(connectionId, database, true).catch(() => {});
  } catch (e) {
    const quiet = isBackground(task);
    if (restoreCopy.cancelling) {
      task.cancelled();
      if (!quiet) ElMessage.info(t('backups:restoreCopy.cancelled'));
    } else {
      task.fail(e);
      restoreCopy.errors = [errorMessage(e)];
    }
  } finally {
    restoreCopy.running = false;
    // Finished: "Ver detalle" shows the panel's detail (summary, log of the
    // statement errors), not the dialog ready to restore again.
    task.setReopen(undefined);
    owned.delete(task);
  }
}
function cancelRestoreCopy() {
  if (restoreTask) tasks.cancel(restoreTask.id);
}

async function deleteCopy(c: BackupCopy) {
  let deleteFile = true;
  try {
    await ElMessageBox.confirm(t('backups:deleteCopy.message', { path: c.path }), t('backups:deleteCopy.title'), {
      type: 'warning',
      distinguishCancelAndClose: true,
      confirmButtonText: t('backups:deleteCopy.withFile'),
      cancelButtonText: t('backups:deleteCopy.listOnly'),
    });
  } catch (action) {
    if (action !== 'cancel') return;
    deleteFile = false;
  }
  try {
    await backupApi.deleteCopy(c.id, deleteFile);
    await load();
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

// -- the engine's own backups: options → script → run -------------------------
const form = reactive({ open: false, mode: 'backup' as 'backup' | 'restore', fields: [] as Field[], values: {} as Record<string, string>, source: '', database: '' });
function visible(f: Field): boolean {
  if (!f.when) return true;
  return f.when.values.includes(form.values[f.when.key] ?? '');
}
function openNative(mode: 'backup' | 'restore', entry: BackupEntry | null = null) {
  const s = spec.value;
  if (!s) return;
  const fields = mode === 'backup' ? s.backup_options : s.restore_options;
  Object.assign(form, {
    open: true,
    mode,
    fields,
    values: Object.fromEntries(fields.map((f) => [f.key, f.default ?? ''])),
    source: entry?.id ?? '',
    database: entry?.database ?? props.tab.database,
  });
}
const formReady = computed(() => (form.mode === 'backup' || !!form.source.trim()) && form.fields.every((f) => !f.required || !visible(f) || !!form.values[f.key]));
function submitForm() {
  if (!formReady.value) return;
  const options = Object.fromEntries(form.fields.filter(visible).map((f) => [f.key, form.values[f.key] ?? '']));
  const database = form.database.trim() || null;
  form.open = false;
  propose(form.mode === 'backup'
    ? { action: 'backup', database: serverView.value || spec.value?.server_wide ? null : props.tab.database || null, options }
    : { action: 'restore', source: form.source.trim(), database, options });
}
async function pickFile(f: Field) {
  try {
    const p = await save({ defaultPath: form.values[f.key] || undefined });
    if (p) form.values[f.key] = p;
  } catch { /* no dialog outside Tauri */ }
}

const review = reactive({ open: false, action: null as BackupAction | null, script: '', shown: '', error: null as string | null, running: false });
let reviewTask: TaskHandle<any> | null = null;
async function propose(action: BackupAction) {
  if (review.running) { review.open = true; return; }
  try {
    const s = await backupApi.script(props.tab.connectionId, action);
    Object.assign(review, { open: true, action, script: s.script, shown: s.shown, error: null, running: false });
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}
async function copyScript() {
  // The copy never carries the secrets.
  await navigator.clipboard.writeText(review.shown);
  ElMessage.success(t('backups:copied'));
}
async function runScript() {
  if (review.running) return;
  review.running = true;
  review.error = null;
  const session = `backup-run:${Date.now()}`;
  const connectionId = props.tab.connectionId;
  const database = spec.value?.script_database || props.tab.database;
  const action = review.action?.action ?? 'backup';
  const task = startTask({
    kind: action === 'restore' ? 'restore' : 'backup', title: t(`tasks:backups.native.${action}`, { where: where() }), connectionId, database,
    cancel: () => api.cancelQuery(session),
    reopen: () => { review.open = true; },
  });
  reviewTask = task;
  owned.add(task);
  try {
    const o = await api.executeQuery({ sessionId: session, connectionId, database, sql: review.script, maxRows: 10, record: false });
    if (o.error) {
      review.error = tb(o.error);
      if (task.isCancelling) task.cancelled(); else task.fail(review.error);
      return;
    }
    task.finish();
    const quiet = isBackground(task);
    review.open = false;
    if (!quiet) ElMessage.success(t(`backups:done.${action}`));
    if (alive) await load();
  } catch (e) {
    review.error = errorMessage(e);
    if (task.isCancelling) task.cancelled(); else task.fail(e);
  } finally {
    review.running = false;
    // Finished: "Ver detalle" shows the panel's detail, not the script ready to run again.
    task.setReopen(undefined);
    owned.delete(task);
    api.closeSession(session).catch(() => {});
  }
}
function cancelScript() {
  if (reviewTask) tasks.cancel(reviewTask.id);
}
</script>

<template>
  <div class="bv" v-loading="loading">
    <header class="bv-head">
      <h2>{{ $t('backups:title') }} · {{ tab.database || conns.byId(tab.connectionId)?.name }}</h2>
      <span style="flex: 1" />
      <el-button size="small" text :title="$t('common:refresh')" @click="load"><el-icon><ei-refresh /></el-icon></el-button>
    </header>
    <div v-if="error" class="bv-error">{{ error }}</div>

    <!-- DBine's copies: every engine -->
    <section v-if="!serverView" class="bv-section">
      <div class="bv-section-head">
        <h3>{{ $t('backups:copies') }}</h3>
        <span class="bv-dim">{{ $t('backups:copiesHint') }}</span>
        <span style="flex: 1" />
        <el-button size="small" type="primary" @click="openCopy">{{ $t('backups:copy.new') }}</el-button>
      </div>
      <table class="bv-table">
        <thead><tr>
          <th>{{ $t('backups:col.date') }}</th><th>{{ $t('backups:col.content') }}</th><th class="n">{{ $t('backups:col.objects') }}</th>
          <th class="n">{{ $t('backups:col.rows') }}</th><th class="n">{{ $t('backups:col.size') }}</th><th>{{ $t('backups:col.file') }}</th><th />
        </tr></thead>
        <tbody>
          <tr v-for="c in copies" :key="c.id">
            <td>{{ when(c.created_at) }}</td>
            <td>{{ c.data ? $t('backups:withData') : $t('backups:structureOnly') }}</td>
            <td class="n">{{ num(c.objects) }}</td>
            <td class="n">{{ c.data ? num(c.rows) : '—' }}</td>
            <td class="n">{{ size(c.size) }}</td>
            <td class="bv-path nm-selectable" :title="c.path">{{ c.path }}</td>
            <td class="bv-act">
              <el-button v-if="!readOnly" size="small" text @click="openRestoreCopy(c)">{{ $t('backups:restore') }}</el-button>
              <el-button size="small" text type="danger" @click="deleteCopy(c)">{{ $t('common:delete') }}</el-button>
            </td>
          </tr>
          <tr v-if="!copies.length"><td colspan="7" class="bv-dim">{{ $t('backups:noCopies') }}</td></tr>
        </tbody>
      </table>
    </section>

    <!-- The engine's own backups -->
    <section class="bv-section">
      <div class="bv-section-head">
        <h3>{{ $t('backups:server') }}</h3>
        <span style="flex: 1" />
        <template v-if="spec && !readOnly">
          <span v-if="spec.restore" :title="noPermission('restore')">
            <el-button size="small" :disabled="!!noPermission('restore')" @click="openNative('restore')">{{ $t('backups:restoreFrom') }}</el-button>
          </span>
          <span :title="noPermission('backup')">
            <el-button size="small" type="primary" :disabled="!!noPermission('backup')" @click="openNative('backup')">{{ $t('backups:native.new') }}</el-button>
          </span>
        </template>
      </div>
      <p v-if="!spec" class="bv-dim">{{ $t('backups:noNative') }}</p>
      <template v-else>
        <p v-if="spec.note" class="bv-note">{{ tb(spec.note) }}</p>
        <div v-if="nativeError" class="bv-error">{{ nativeError }}</div>
        <table v-if="spec.history" class="bv-table">
          <thead><tr>
            <th>{{ $t('backups:col.date') }}</th><th>{{ $t('backups:col.kind') }}</th><th>{{ $t('backups:col.database') }}</th>
            <th class="n">{{ $t('backups:col.size') }}</th><th>{{ $t('backups:col.location') }}</th><th>{{ $t('backups:col.status') }}</th><th />
          </tr></thead>
          <tbody>
            <tr v-for="b in native" :key="b.id" :title="b.details.map(([k, v]) => `${tb(k)}: ${tb(v)}`).join('\n')">
              <td>{{ when(b.finished ?? b.started) }}</td>
              <td>{{ b.kind ? tb(b.kind) : '—' }}</td>
              <td>{{ b.database ?? $t('backups:wholeServer') }}</td>
              <td class="n">{{ size(b.size) }}</td>
              <td class="bv-path nm-selectable" :title="b.location ?? ''">{{ b.location ?? b.id }}</td>
              <td>{{ b.status ? tb(b.status) : '—' }}</td>
              <td class="bv-act">
                <span v-if="!readOnly && spec.restore && b.restorable" :title="noPermission('restore')">
                  <el-button size="small" text :disabled="!!noPermission('restore')" @click="openNative('restore', b)">{{ $t('backups:restore') }}</el-button>
                </span>
                <span v-if="!readOnly && spec.delete" :title="noPermission('backup')">
                  <el-button size="small" text type="danger" :disabled="!!noPermission('backup')" @click="propose({ action: 'delete', source: b.id })">{{ $t('common:delete') }}</el-button>
                </span>
              </td>
            </tr>
            <tr v-if="!native.length && !nativeError"><td colspan="7" class="bv-dim">{{ $t('backups:noNativeHistory') }}</td></tr>
          </tbody>
        </table>
        <p v-else class="bv-dim">{{ $t('backups:noHistory') }}</p>
      </template>
    </section>

    <!-- new DBine copy -->
    <el-dialog v-model="copy.open" :title="$t('backups:copy.new')" width="560px" append-to-body :close-on-click-modal="!copy.running" :show-close="!copy.running" @close="copy.running && sendToBackground(copyTask)">
      <el-form label-position="top" @submit.prevent="runCopy">
        <el-form-item :label="$t('backups:copy.file')">
          <div class="bv-row">
            <el-input v-model="copy.path" :disabled="copy.running" />
            <el-button :disabled="copy.running" @click="pickCopyPath">{{ $t('backups:pick') }}</el-button>
          </div>
        </el-form-item>
        <el-checkbox v-model="copy.data" :disabled="copy.running">{{ $t('backups:copy.data') }}</el-checkbox>
        <p class="bv-dim">{{ $t('backups:copy.hint') }}</p>
      </el-form>
      <div v-if="copy.running" class="bv-progress">
        <el-progress :percentage="copy.total ? Math.round((copy.done / copy.total) * 100) : 0" :stroke-width="6" />
        <span class="bv-dim">{{ copy.current }}</span>
      </div>
      <template #footer>
        <template v-if="copy.running">
          <el-button :disabled="copy.cancelling" @click="cancelCopy">{{ $t('common:cancel') }}</el-button>
          <el-button type="primary" @click="sendToBackground(copyTask, () => (copy.open = false))">{{ $t('tasks:panel.background') }}</el-button>
        </template>
        <template v-else>
          <el-button @click="copy.open = false">{{ $t('common:cancel') }}</el-button>
          <el-button type="primary" :disabled="!copy.path" @click="runCopy">{{ $t('backups:copy.run') }}</el-button>
        </template>
      </template>
    </el-dialog>

    <!-- restore a DBine copy -->
    <el-dialog v-model="restoreCopy.open" :title="$t('backups:restoreCopy.title')" width="560px" append-to-body :close-on-click-modal="!restoreCopy.running" :show-close="!restoreCopy.running" @close="restoreCopy.running && sendToBackground(restoreTask)">
      <el-form label-position="top">
        <el-form-item :label="$t('backups:restoreCopy.into')">
          <el-select v-model="restoreCopy.database" filterable allow-create :disabled="restoreCopy.running" style="width: 100%">
            <el-option v-for="d in databases" :key="d" :label="d" :value="d" />
          </el-select>
        </el-form-item>
        <el-checkbox v-model="restoreCopy.continueOnError" :disabled="restoreCopy.running">{{ $t('backups:restoreCopy.continueOnError') }}</el-checkbox>
        <p class="bv-dim">{{ $t('backups:restoreCopy.hint') }}</p>
      </el-form>
      <el-progress v-if="restoreCopy.running" :percentage="restoreCopy.total ? Math.min(100, Math.round((restoreCopy.bytes / restoreCopy.total) * 100)) : 0" :stroke-width="6" />
      <div v-if="restoreCopy.errors.length" class="bv-error">{{ restoreCopy.errors.join('\n') }}</div>
      <template #footer>
        <template v-if="restoreCopy.running">
          <el-button :disabled="restoreCopy.cancelling" @click="cancelRestoreCopy">{{ $t('common:cancel') }}</el-button>
          <el-button type="primary" @click="sendToBackground(restoreTask, () => (restoreCopy.open = false))">{{ $t('tasks:panel.background') }}</el-button>
        </template>
        <template v-else>
          <el-button @click="restoreCopy.open = false">{{ $t('common:close') }}</el-button>
          <el-button type="primary" :disabled="!restoreCopy.database" @click="runRestoreCopy">{{ $t('backups:restore') }}</el-button>
        </template>
      </template>
    </el-dialog>

    <!-- native backup / restore options -->
    <el-dialog v-model="form.open" :title="form.mode === 'backup' ? $t('backups:native.new') : $t('backups:restoreFrom')" width="560px" append-to-body>
      <el-form label-position="top" @submit.prevent="submitForm">
        <template v-if="form.mode === 'restore'">
          <el-form-item :label="$t('backups:native.source')" required>
            <el-input v-model="form.source" :placeholder="$t('backups:native.sourceHint')" />
          </el-form-item>
          <el-form-item v-if="!serverView || databases.length" :label="$t('backups:native.into')">
            <el-select v-model="form.database" filterable allow-create clearable style="width: 100%">
              <el-option v-for="d in databases" :key="d" :label="d" :value="d" />
            </el-select>
          </el-form-item>
        </template>
        <template v-for="f in form.fields" :key="f.key">
          <el-form-item v-if="visible(f)" :label="tb(f.label)" :required="f.required">
            <el-checkbox v-if="f.kind.type === 'bool'" :model-value="form.values[f.key] === 'true'" @update:model-value="(v: string | number | boolean) => (form.values[f.key] = v ? 'true' : 'false')" />
            <el-select v-else-if="f.kind.type === 'select'" v-model="form.values[f.key]" style="width: 100%">
              <el-option v-for="[v, l] in f.kind.options" :key="v" :label="tb(l)" :value="v" />
            </el-select>
            <el-input v-else-if="f.kind.type === 'textarea'" v-model="form.values[f.key]" type="textarea" :rows="4" :placeholder="tb(f.placeholder)" />
            <div v-else-if="f.kind.type === 'file'" class="bv-row">
              <el-input v-model="form.values[f.key]" :placeholder="tb(f.placeholder)" />
              <el-button @click="pickFile(f)">{{ $t('backups:pick') }}</el-button>
            </div>
            <el-input v-else v-model="form.values[f.key]" :type="f.kind.type === 'password' ? 'password' : 'text'" :show-password="f.kind.type === 'password'" :placeholder="tb(f.placeholder)" />
            <div v-if="f.help" class="bv-help">{{ tb(f.help) }}</div>
          </el-form-item>
        </template>
      </el-form>
      <template #footer>
        <el-button @click="form.open = false">{{ $t('common:cancel') }}</el-button>
        <el-button type="primary" :disabled="!formReady" @click="submitForm">{{ $t('backups:seeScript') }}</el-button>
      </template>
    </el-dialog>

    <!-- the script, reviewed, then run -->
    <el-dialog v-model="review.open" :title="$t('backups:reviewTitle')" width="680px" append-to-body :close-on-click-modal="!review.running" :show-close="!review.running" @close="review.running && sendToBackground(reviewTask)">
      <p class="bv-dim">{{ $t('backups:reviewHint', { db: spec?.script_database || tab.database || conns.byId(tab.connectionId)?.name }) }}</p>
      <pre class="bv-script nm-selectable">{{ review.shown }}</pre>
      <div v-if="review.error" class="bv-error">{{ review.error }}</div>
      <template #footer>
        <div class="bv-foot">
          <el-button @click="copyScript">{{ $t('common:copy') }}</el-button>
          <span style="flex: 1" />
          <template v-if="review.running">
            <el-button :disabled="!!reviewTask?.isCancelling" @click="cancelScript">{{ $t('common:cancel') }}</el-button>
            <el-button @click="sendToBackground(reviewTask, () => (review.open = false))">{{ $t('tasks:panel.background') }}</el-button>
          </template>
          <el-button v-else @click="review.open = false">{{ $t('common:cancel') }}</el-button>
          <el-button type="primary" :loading="review.running" @click="runScript">{{ $t('backups:run') }}</el-button>
        </div>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.bv { height: 100%; min-height: 0; overflow: auto; padding: 16px 22px 24px; background: var(--ide-editor); }
.bv-head { display: flex; align-items: center; gap: 8px; }
.bv-head h2 { margin: 0; font-size: 17px; color: var(--nm-text-strong); }
.bv-section { margin-top: 18px; }
.bv-section-head { display: flex; align-items: baseline; gap: 10px; margin-bottom: 8px; }
.bv-section-head h3 { margin: 0; font-size: 13px; color: var(--nm-text-strong); }
.bv-section-head .el-button { margin: 0; }
.bv-dim { color: var(--nm-text-dim); font-size: 12px; }
.bv-note { margin: 0 0 8px; font-size: 12px; color: var(--nm-text); }
.bv-help { margin-top: 2px; font-size: 11.5px; line-height: 1.4; color: var(--nm-text-dim); }
.bv-table { width: 100%; border-collapse: collapse; font-size: 12.5px; }
.bv-table th { text-align: left; font-weight: 500; color: var(--nm-text-dim); padding: 5px 8px; border-bottom: 1px solid var(--nm-border); white-space: nowrap; }
.bv-table td { padding: 4px 8px; border-bottom: 1px solid var(--nm-border-soft); color: var(--nm-text); white-space: nowrap; }
.bv-table .n { text-align: right; font-variant-numeric: tabular-nums; }
.bv-path { max-width: 360px; overflow: hidden; text-overflow: ellipsis; direction: rtl; text-align: left; }
.bv-act { text-align: right; }
.bv-act .el-button { margin: 0; }
.bv-row { display: flex; gap: 6px; width: 100%; }
.bv-row .el-button { margin: 0; }
.bv-progress { display: flex; flex-direction: column; gap: 4px; margin-top: 10px; }
.bv-error { margin: 8px 0; padding: 8px 10px; border-radius: 3px; border: 1px solid color-mix(in srgb, var(--nm-danger) 50%, transparent); background: color-mix(in srgb, var(--nm-danger) 10%, transparent); color: var(--nm-text); white-space: pre-wrap; font-size: 12px; max-height: 200px; overflow: auto; }
.bv-script { margin: 0; padding: 10px 12px; max-height: 45vh; overflow: auto; border: 1px solid var(--nm-border); border-radius: 4px; background: var(--ide-editor, var(--nm-bg-elev)); font-family: var(--nm-mono); font-size: 12.5px; color: var(--nm-text-strong); white-space: pre-wrap; }
.bv-foot { display: flex; align-items: center; gap: 8px; }
.bv-foot .el-button { margin: 0; }
</style>
