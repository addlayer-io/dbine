<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from 'vue';
import { useTranslation } from 'i18next-vue';
import { tb } from '../backend';
import { api, errorText, type RunRow, type TaskItem } from '../api';

// The computer's scheduled tasks: how the last run went, when the next one
// is, the last runs of one, and "Ejecutar ahora" (allowed per phone on the
// computer, confirmed here with biometrics or PIN).

const canRun = ref(false);
const { t } = useTranslation();
const list = ref<TaskItem[] | null>(null);
const open = ref<TaskItem | null>(null);
const runs = ref<RunRow[] | null>(null);
const error = ref('');
const notice = ref('');
let timer: ReturnType<typeof setInterval> | null = null;

async function load() {
  try {
    canRun.value = (await api.hello()).can_run_tasks;
    list.value = await api.tasks();
    if (open.value) {
      open.value = list.value.find((x) => x.id === open.value!.id) ?? null;
      if (open.value) runs.value = await api.taskRuns(open.value.id);
    }
    error.value = '';
  } catch (e) {
    error.value = errorText(e);
  }
}

async function pick(task: TaskItem) {
  open.value = task;
  runs.value = null;
  notice.value = '';
  try {
    runs.value = await api.taskRuns(task.id);
  } catch (e) {
    error.value = errorText(e);
  }
}

async function runNow(task: TaskItem) {
  notice.value = '';
  if (!confirm(t('tasks.runConfirm', { name: task.name }))) return;
  try {
    const { authenticate } = await import('@tauri-apps/plugin-biometric');
    await authenticate(t('tasks.runReason', { name: task.name }), { allowDeviceCredential: true, title: 'DBine' });
  } catch {
    error.value = t('tasks.notConfirmed');
    return;
  }
  try {
    await api.runTask(task.id);
    notice.value = t('tasks.started');
    setTimeout(load, 1500);
  } catch (e) {
    error.value = errorText(e);
  }
}

const statusText = (s: string) => t(`tasks.status.${s}`, { defaultValue: s });

onMounted(() => {
  load();
  timer = setInterval(load, 10000);
});
onBeforeUnmount(() => timer && clearInterval(timer));
</script>

<template>
  <div>
    <div class="bar">
      <button v-if="open" @click="open = null">‹</button>
      <h1>{{ open ? open.name : $t('tasks.title') }}</h1>
    </div>
    <div class="page">
      <p v-if="error" class="error">{{ error }}</p>
      <p v-if="notice" class="ok">{{ notice }}</p>

      <template v-if="!open">
        <p v-if="list && !list.length" class="muted">{{ $t('tasks.none') }}</p>
        <button v-for="task in list ?? []" :key="task.id" class="task" @click="pick(task)">
          <span class="dot" :class="task.last?.status ?? 'none'" />
          <span class="info">
            <span class="name">{{ task.name }}</span>
            <span class="muted">
              <template v-if="task.last">{{ statusText(task.last.status) }} · {{ task.last.started_at }}</template>
              <template v-else>{{ $t('tasks.neverRan') }}</template>
            </span>
            <span v-if="task.enabled && task.next_run" class="muted">{{ $t('tasks.next', { at: task.next_run }) }}</span>
            <span v-else-if="!task.enabled" class="muted">{{ $t('tasks.disabled') }}</span>
          </span>
          <span v-if="task.last?.steps.some((s) => s.alert)" class="bell">●</span>
        </button>
      </template>

      <template v-else>
        <button v-if="canRun" class="primary" :disabled="open.needs_approval || runs?.[0]?.status === 'running'" @click="runNow(open)">{{ $t('tasks.runNow') }}</button>
        <p v-if="open.needs_approval" class="muted">{{ $t('tasks.needsApproval') }}</p>
        <p v-else-if="!canRun" class="muted">{{ $t('tasks.notAllowed') }}</p>
        <div v-for="r in runs ?? []" :key="r.id" class="card run">
          <div class="head">
            <span class="dot" :class="r.status" />
            <strong>{{ statusText(r.status) }}</strong>
            <span class="muted">{{ r.started_at }}</span>
          </div>
          <p v-if="r.error" class="error">{{ tb(r.error) }}</p>
          <div v-for="(s, i) in r.steps" :key="i" class="step">
            <span class="muted">{{ i + 1 }}.</span> {{ tb(s.summary) }}
            <p v-if="s.alert" class="alert">{{ s.alert }}</p>
          </div>
        </div>
        <p v-if="runs && !runs.length" class="muted">{{ $t('tasks.neverRan') }}</p>
      </template>
    </div>
  </div>
</template>

<style scoped>
.task { display: flex; align-items: center; gap: 12px; text-align: left; padding: 14px; border: 1px solid var(--border); }
.info { flex: 1; display: flex; flex-direction: column; gap: 2px; min-width: 0; }
.name { color: var(--text-strong); }
.dot { width: 10px; height: 10px; border-radius: 50%; flex: none; background: var(--text-dim); }
.dot.ok { background: var(--success); }
.dot.partial { background: var(--warning); }
.dot.failed { background: var(--danger); }
.dot.running { background: var(--accent); }
.bell { color: var(--warning); }
.run { display: flex; flex-direction: column; gap: 6px; }
.run .head { display: flex; align-items: center; gap: 8px; }
.run p { margin: 0; }
.step { font-size: 14px; }
.alert { white-space: pre-line; color: var(--warning); font-size: 13px; }
.ok { color: var(--success); }
</style>
