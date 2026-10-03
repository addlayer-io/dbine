<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import type { SyncScript } from '../api/compare';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import { newQuery } from '../composables/actions';
import { dropIndexScript, indexUsageOf, runDropIndex, toggleIndexScript, type DropIndexTarget } from '../composables/dropIndex';
import { indexUsageEntry, sharePct } from '../composables/indexUsage';
import { startTask, useTasksStore, type TaskHandle } from '../stores/tasks';

// "Eliminar índice…": the index, how much it's used, and the engine's script
// to drop it (read-only, copyable, or opened as a query). It runs only on
// "Eliminar". The run is a task (stores/tasks.ts): "Seguir en segundo plano"
// closes the dialog and it goes on; the Tareas panel shows its script and
// outcome, and cancels it the way a schema sync is (`sync:<runId>`): the
// statement is interrupted where the engine allows it, or the next one
// doesn't start.

const props = defineProps<{ target: DropIndexTarget }>();
const emit = defineEmits<{ close: [] }>();
const { t } = useTranslation();

// "Deshabilitar / Habilitar índice…" use the same dialog: their texts live
// under explorer:indexes.disable / .enable, the script comes from the engine.
const mode = computed(() => props.target.mode ?? 'drop');
const k = (key: string) => `explorer:indexes.${mode.value}.${key}`;

const qualified = computed(() => (props.target.table.schema ? `${props.target.table.schema}.${props.target.table.name}` : props.target.table.name));

const num = (n: number) => n.toLocaleString(locale());
const usageLine = computed(() => {
  const report = indexUsageEntry(props.target.connectionId, props.target.database, props.target.table)?.report;
  const i = indexUsageOf(props.target);
  if (!i || !report?.stats_available || mode.value === 'enable') return null;
  if (i.unused) return t('explorer:indexes.drop.usageUnused', { writes: num(i.updates) });
  if (i.read_share == null) return t('explorer:indexes.drop.usageNone');
  return t('explorer:indexes.drop.usageShare', { share: sharePct(i.read_share), seeks: num(i.seeks), scans: num(i.scans), lookups: num(i.lookups) });
});

const script = ref<SyncScript | null>(null);
const text = computed(() => script.value?.statements.join('\n') ?? '');
const scriptError = ref<string | null>(null);
const running = ref(false);
const runError = ref<string | null>(null);
let task: TaskHandle | null = null;
const taskId = ref<string | null>(null);
const tasks = useTasksStore();
const cancelling = computed(() => !!(taskId.value && tasks.byId(taskId.value)?.cancelling));
/** "Cancelar": closes the dialog, or stops the drop while it runs. */
function cancel() {
  if (running.value && taskId.value) tasks.cancel(taskId.value);
  else emit('close');
}
let alive = true;
onBeforeUnmount(() => {
  alive = false;
  // Closed while running (its host went away): the run goes on and says so when it ends.
  if (running.value) task?.background();
});

onMounted(async () => {
  try {
    script.value = mode.value === 'drop' ? await dropIndexScript(props.target) : await toggleIndexScript(props.target);
  } catch (e) {
    scriptError.value = errorMessage(e);
  }
});

async function copyScript() {
  try {
    await navigator.clipboard.writeText(text.value);
    ElMessage.success({ message: t('common:copied'), duration: 1200 });
  } catch { /* no clipboard */ }
}
function openInQuery() {
  newQuery(props.target.connectionId, props.target.database, text.value, t(k('scriptName'), { name: props.target.index }));
  emit('close');
}

async function run() {
  if (!script.value?.statements.length) return;
  running.value = true;
  runError.value = null;
  const target = props.target;
  const statements = script.value.statements;
  const runId = crypto.randomUUID();
  // The run's session exists once it has connected (its first progress event);
  // a cancel before that is a no-op, so it's re-sent every 500 ms until one
  // goes out after it, or the run ends.
  let ready = false;
  let settled = false;
  const current = task = startTask({
    kind: 'drop-index',
    title: t(mode.value === 'drop' ? 'tasks:dialogs.dropIndex' : k('task'), { name: target.index, table: qualified.value }),
    connectionId: target.connectionId, database: target.database,
    cancel: () => {
      const send = () => api.cancelQuery(`sync:${runId}`).catch(() => {});
      if (!ready) {
        const timer = setInterval(() => {
          if (settled) clearInterval(timer);
          else { if (ready) clearInterval(timer); send(); }
        }, 500);
      }
      return send();
    },
  });
  taskId.value = current.id;
  current.log(statements.join('\n'));
  current.progress({ done: 0, total: statements.length, unit: 'statements' });
  try {
    await current.listen<{ run_id: string; done: number }>('schema-sync-progress', ({ payload }) => {
      if (payload.run_id !== runId || settled) return;
      ready = true;
      current.progress({ done: payload.done });
    });
  } catch { /* no live progress: the run still goes */ }
  // runDropIndex refreshes the tree and the "Índices" tab itself, open or not.
  const error = await runDropIndex(target, statements, runId);
  settled = true;
  running.value = false;
  if (error) {
    runError.value = error;
    if (current.isCancelling) current.cancelled(); else current.fail(error);
    return;
  }
  current.finish(undefined, t(k('done')));
  if (!alive) return;
  ElMessage.success(t(k('done')));
  emit('close');
}
</script>

<template>
  <el-dialog
    :model-value="true" width="600px" append-to-body :title="$t(k('title'), { name: target.index })"
    :close-on-click-modal="false" :close-on-press-escape="!running" :show-close="!running" @close="emit('close')"
  >
    <p class="di-p">{{ $t(k('intro'), { name: target.index, table: qualified }) }}</p>
    <p v-if="usageLine" class="di-p di-usage">{{ usageLine }}</p>
    <el-alert v-for="w in script?.warnings ?? []" :key="w" type="warning" :title="tb(w)" :closable="false" show-icon class="di-warn" />

    <div class="di-script-head">{{ $t('explorer:indexes.drop.script') }}</div>
    <div v-if="scriptError" class="di-error">{{ scriptError }}</div>
    <pre v-else v-loading="!script" class="di-script nm-selectable">{{ script ? text || $t('explorer:indexes.drop.empty') : '' }}</pre>
    <div v-if="runError" class="di-error">{{ runError }}</div>

    <template #footer>
      <div class="di-foot">
        <el-button :disabled="!text" @click="copyScript">{{ $t('common:copy') }}</el-button>
        <el-button :disabled="!text || running" @click="openInQuery">{{ $t('explorer:indexes.drop.openInQuery') }}</el-button>
        <span style="flex: 1" />
        <el-button v-if="running" @click="task?.background(); emit('close')">{{ $t('tasks:panel.background') }}</el-button>
        <el-button :disabled="cancelling" @click="cancel">{{ $t('common:cancel') }}</el-button>
        <el-button :type="mode === 'enable' ? 'primary' : 'danger'" :loading="running" :disabled="!text || !!scriptError" @click="run">{{ $t(k('run')) }}</el-button>
      </div>
    </template>
  </el-dialog>
</template>

<style scoped lang="scss">
.di-p { margin: 0 0 8px; }
.di-usage { color: var(--nm-text-dim); font-size: 12.5px; }
.di-warn { margin-bottom: 6px; }
.di-script-head { margin-top: 12px; font-weight: 600; font-size: 13px; color: var(--nm-text-strong); }
.di-script {
  margin: 8px 0 0; padding: 10px 12px; min-height: 48px; max-height: 32vh; overflow: auto; border: 1px solid var(--nm-border); border-radius: 4px;
  background: var(--ide-editor, var(--nm-bg-elev)); font-family: var(--nm-mono); font-size: 12.5px; color: var(--nm-text-strong); white-space: pre-wrap;
}
.di-error {
  margin-top: 8px; padding: 8px 10px; border-radius: 3px; border: 1px solid color-mix(in srgb, var(--nm-danger) 50%, transparent);
  background: color-mix(in srgb, var(--nm-danger) 10%, transparent); color: var(--nm-text); white-space: pre-wrap; font-size: 12px;
}
.di-foot { display: flex; align-items: center; gap: 8px; }
.di-foot .el-button { margin: 0; }
</style>
