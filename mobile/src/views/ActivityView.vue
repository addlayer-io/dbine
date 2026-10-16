<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { useTranslation } from 'i18next-vue';
import { tb } from '../backend';
import { api, duration, errorText, type QueriesView } from '../api';
import TasksView from './TasksView.vue';

// "Actividad": what DBine is doing or did. Queries from the editor (running
// now, with how long, and the long ones that ended, with their result) and
// the scheduled tasks. Opening the app shows how a query ended even when
// its notification never arrived.

const { t } = useTranslation();
const part = ref<'queries' | 'tasks'>('queries');
const view = ref<QueriesView | null>(null);
const loadedAt = ref(Date.now());
const now = ref(Date.now());
const canCancel = ref(false);
const error = ref('');
const notice = ref('');
let timers: ReturnType<typeof setInterval>[] = [];

async function load() {
  try {
    canCancel.value = (await api.hello()).can_kill;
    view.value = await api.queries();
    loadedAt.value = Date.now();
    error.value = '';
  } catch (e) {
    error.value = errorText(e);
  }
}

/** A running query's time, ticking between refreshes. */
const live = (ms: number) => duration(ms + (now.value - loadedAt.value));

async function cancel(id: string, sql: string) {
  notice.value = '';
  if (!confirm(t('activity.cancelConfirm', { sql: sql.slice(0, 160) }))) return;
  try {
    const { authenticate } = await import('@tauri-apps/plugin-biometric');
    await authenticate(t('activity.cancelReason'), { allowDeviceCredential: true, title: 'DBine' });
  } catch {
    error.value = t('activity.notConfirmed');
    return;
  }
  try {
    await api.cancelQuery(id);
    notice.value = t('activity.cancelled');
    setTimeout(load, 1500);
  } catch (e) {
    error.value = errorText(e);
  }
}

const empty = computed(() => view.value && !view.value.running.length && !view.value.recent.length);

onMounted(() => {
  load();
  timers = [setInterval(load, 5000), setInterval(() => (now.value = Date.now()), 1000)];
});
onBeforeUnmount(() => timers.forEach(clearInterval));
</script>

<template>
  <div>
    <div class="bar"><h1>{{ $t('activity.title') }}</h1></div>
    <div class="tabs">
      <button :class="{ on: part === 'queries' }" @click="part = 'queries'">{{ $t('activity.queries') }}</button>
      <button :class="{ on: part === 'tasks' }" @click="part = 'tasks'">{{ $t('activity.tasks') }}</button>
    </div>

    <TasksView v-if="part === 'tasks'" embedded />
    <div v-else class="page">
      <p v-if="error" class="error">{{ error }}</p>
      <p v-if="notice" class="ok">{{ notice }}</p>
      <p v-if="empty" class="muted">{{ $t('activity.none') }}</p>

      <template v-if="view?.running.length">
        <h2>{{ $t('activity.running') }}</h2>
        <div v-for="q in view.running" :key="q.id" class="card q run">
          <div class="head">
            <span class="dot running" />
            <span class="place">{{ q.place }}</span>
            <span class="time">{{ live(q.elapsed_ms) }}</span>
          </div>
          <p class="mono sql">{{ q.sql }}</p>
          <button v-if="canCancel" class="danger" @click="cancel(q.id, q.sql)">{{ $t('activity.cancel') }}</button>
        </div>
      </template>

      <template v-if="view?.recent.length">
        <h2>{{ $t('activity.recent') }}</h2>
        <div v-for="q in view.recent" :key="q.id" class="card q">
          <div class="head">
            <span class="dot" :class="q.ok ? 'ok' : 'failed'" />
            <span class="place">{{ q.place }}</span>
            <span class="muted">{{ duration(q.elapsed_ms) }}</span>
          </div>
          <p class="mono sql">{{ q.sql }}</p>
          <p :class="q.ok ? 'muted' : 'error'">{{ q.ok ? (q.summary ? tb(q.summary) : $t('activity.done')) : tb(q.summary) }}</p>
          <p class="muted">{{ $t('activity.startedAt', { at: q.started_at }) }}</p>
        </div>
      </template>
      <p class="muted">{{ $t('activity.hint') }}</p>
    </div>
  </div>
</template>

<style scoped>
.tabs { display: flex; gap: 8px; padding: 10px 16px 0; }
.tabs button { flex: 1; padding: 10px; border: 1px solid var(--border); background: transparent; color: var(--text-dim); }
.tabs button.on { background: var(--bg-elev); color: var(--text-strong); border-color: var(--accent); }
h2 { margin: 6px 0 0; font-size: 13px; text-transform: uppercase; letter-spacing: .5px; color: var(--text-dim); }
.q { display: flex; flex-direction: column; gap: 6px; }
.q p { margin: 0; }
.head { display: flex; align-items: center; gap: 8px; }
.place { flex: 1; color: var(--text-strong); min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.time { color: var(--accent); font-variant-numeric: tabular-nums; }
.sql { font-size: 12px; color: var(--text); word-break: break-word; }
.dot { width: 10px; height: 10px; border-radius: 50%; flex: none; background: var(--text-dim); }
.dot.ok { background: var(--success); }
.dot.failed { background: var(--danger); }
.dot.running { background: var(--accent); animation: pulse 1.6s infinite; }
@keyframes pulse { 50% { opacity: .35; } }
.ok { color: var(--success); }
</style>
