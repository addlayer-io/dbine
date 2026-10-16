<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { useTranslation } from 'i18next-vue';
import { api, duration, errorText, type ConnectionItem, type LockRow, type MonitorView, type ProcessRow } from '../api';
import MonitorPanel from './MonitorPanel.vue';
import HealthPanel from './HealthPanel.vue';

// One connection: who blocks whom (the heads first, with what waits on
// them) and the statements running now, refreshed every few seconds. A
// session ends only with the phone's biometrics or PIN, and only when the
// computer allows this phone to.

const props = defineProps<{ conn: ConnectionItem; canKill: boolean }>();
const emit = defineEmits<{ back: [] }>();
const { t } = useTranslation();

type Tab = 'monitor' | 'locks' | 'processes' | 'health';
const tab = ref<Tab>(props.conn.monitor ? 'monitor' : props.conn.locks ? 'locks' : 'processes');
const monitor = ref<MonitorView | null>(null);
const monitorAt = ref(0);
const locks = ref<LockRow[] | null>(null);
const procs = ref<ProcessRow[] | null>(null);
const error = ref('');
const notice = ref('');
const busy = ref(false);
let timer: ReturnType<typeof setInterval> | null = null;

const killable = computed(() => props.canKill && props.conn.kill);

/** Heads (they block someone and wait for no one), each with its waiters. */
const chains = computed(() => {
  const rows = locks.value ?? [];
  const byId = new Map(rows.map((r) => [r.id, r]));
  const waiters = (id: string): LockRow[] => rows.filter((r) => r.blocked_by === id);
  const heads = rows.filter((r) => (!r.blocked_by || !byId.has(r.blocked_by)) && waiters(r.id).length);
  const flat = (id: string, depth: number, seen: Set<string>): { row: LockRow; depth: number }[] =>
    waiters(id).flatMap((w) => (seen.has(w.id) ? [] : [{ row: w, depth }, ...flat(w.id, depth + 1, new Set([...seen, w.id]))]));
  return heads.map((h) => ({ head: h, below: flat(h.id, 1, new Set([h.id])) }));
});

async function load() {
  if (busy.value || tab.value === 'health') return;
  busy.value = true;
  try {
    if (tab.value === 'monitor') {
      monitor.value = await api.monitor(props.conn.id);
      monitorAt.value = Date.now();
    } else if (tab.value === 'locks') locks.value = await api.locks(props.conn.id);
    else procs.value = await api.processes(props.conn.id);
    error.value = '';
  } catch (e) {
    error.value = errorText(e);
  } finally {
    busy.value = false;
  }
}

function pick(t2: Tab) {
  tab.value = t2;
  load();
}

async function kill(id: string, sql: string | null) {
  notice.value = '';
  if (!confirm(t('conn.killConfirm', { id, sql: (sql ?? '').slice(0, 160) }))) return;
  try {
    const { authenticate } = await import('@tauri-apps/plugin-biometric');
    await authenticate(t('conn.killReason', { id }), { allowDeviceCredential: true, title: 'DBine' });
  } catch (e) {
    error.value = t('conn.notConfirmed');
    return;
  }
  try {
    await api.kill(props.conn.id, id);
    notice.value = t('conn.killed', { id });
  } catch (e) {
    error.value = errorText(e);
  }
  load();
}

onMounted(() => {
  load();
  timer = setInterval(load, 5000);
});
onBeforeUnmount(() => {
  if (timer) clearInterval(timer);
});
</script>

<template>
  <div>
    <div class="bar">
      <button @click="emit('back')">‹</button>
      <h1>{{ conn.name }}</h1>
      <span v-if="busy" class="muted">…</span>
    </div>
    <div class="tabs">
      <button :class="{ on: tab === 'monitor' }" :disabled="!conn.monitor" @click="pick('monitor')">{{ $t('conn.monitor') }}</button>
      <button :class="{ on: tab === 'locks' }" :disabled="!conn.locks" @click="pick('locks')">{{ $t('conn.locks') }}</button>
      <button :class="{ on: tab === 'processes' }" :disabled="!conn.processes" @click="pick('processes')">{{ $t('conn.processes') }}</button>
      <button :class="{ on: tab === 'health' }" @click="pick('health')">{{ $t('conn.health') }}</button>
    </div>
    <div class="page">
      <p v-if="error" class="error">{{ error }}</p>
      <p v-if="notice" class="ok">{{ notice }}</p>
      <p v-if="!conn.monitor && !conn.locks && !conn.processes" class="muted">{{ $t('conn.unsupported', { engine: conn.engine }) }}</p>

      <MonitorPanel v-if="tab === 'monitor'" :view="monitor" :at="monitorAt" />
      <HealthPanel v-if="tab === 'health'" :connection-id="conn.id" />

      <template v-if="tab === 'locks' && locks">
        <p v-if="!chains.length" class="muted">{{ $t('conn.noLocks') }}</p>
        <div v-for="c in chains" :key="c.head.id" class="card chain">
          <div class="head">
            <div class="who">
              <strong>{{ $t('conn.session', { id: c.head.id }) }}</strong>
              <span class="muted">{{ [c.head.user, c.head.client].filter(Boolean).join(' · ') }}</span>
              <span class="badge">{{ $t('conn.blocks', { count: c.below.length }) }}</span>
            </div>
            <button v-if="killable" class="danger" @click="kill(c.head.id, c.head.sql)">{{ $t('conn.kill') }}</button>
          </div>
          <p v-if="c.head.sql" class="mono sql">{{ c.head.sql }}</p>
          <p v-if="c.head.wait" class="muted">{{ c.head.wait }}<template v-if="c.head.waited_ms"> · {{ duration(c.head.waited_ms) }}</template></p>
          <div v-for="w in c.below" :key="w.row.id" class="waiter" :style="{ marginLeft: `${(w.depth - 1) * 12}px` }">
            <div class="who">
              <span>↳ {{ $t('conn.session', { id: w.row.id }) }}</span>
              <span class="muted">{{ w.row.user }}</span>
              <span class="wait">{{ duration(w.row.waited_ms) }}</span>
            </div>
            <p v-if="w.row.object" class="muted">{{ w.row.object }}</p>
          </div>
        </div>
        <p v-if="!killable && chains.length" class="muted">{{ canKill ? $t('conn.engineCantKill') : $t('conn.notAllowed') }}</p>
      </template>

      <template v-if="tab === 'processes' && procs">
        <p v-if="!procs.length" class="muted">{{ $t('conn.noProcesses') }}</p>
        <div v-for="p in [...procs].sort((a, b) => (b.elapsed_ms ?? 0) - (a.elapsed_ms ?? 0))" :key="p.id" class="card">
          <div class="head">
            <div class="who">
              <strong>{{ $t('conn.session', { id: p.id }) }}</strong>
              <span class="muted">{{ [p.user, p.host, p.database].filter(Boolean).join(' · ') }}</span>
              <span class="wait">{{ duration(p.elapsed_ms) }}<template v-if="p.blocked_by"> · {{ $t('conn.waitsFor', { id: p.blocked_by }) }}</template></span>
            </div>
            <button v-if="killable" class="danger" @click="kill(p.id, p.sql)">{{ $t('conn.kill') }}</button>
          </div>
          <p v-if="p.sql" class="mono sql">{{ p.sql }}</p>
        </div>
      </template>
    </div>
  </div>
</template>

<style scoped>
.tabs { display: flex; gap: 8px; padding: 10px 16px 0; }
.tabs { overflow-x: auto; }
.tabs button { flex: 1; padding: 10px 8px; white-space: nowrap; border: 1px solid var(--border); background: transparent; color: var(--text-dim); }
.tabs button.on { background: var(--bg-elev); color: var(--text-strong); border-color: var(--accent); }
.chain { display: flex; flex-direction: column; gap: 6px; }
.head { display: flex; align-items: flex-start; gap: 10px; }
.who { flex: 1; display: flex; flex-direction: column; gap: 2px; min-width: 0; }
.badge { align-self: flex-start; font-size: 12px; padding: 2px 8px; border-radius: 10px; background: color-mix(in srgb, var(--danger) 20%, transparent); color: var(--danger); }
.sql { margin: 0; font-size: 12px; color: var(--text); white-space: pre-wrap; word-break: break-word; max-height: 120px; overflow: hidden; }
.waiter { border-left: 2px solid var(--border); padding: 4px 0 4px 10px; }
.waiter .who { flex-direction: row; flex-wrap: wrap; gap: 8px; }
.wait { color: var(--warning); font-size: 13px; }
.ok { color: var(--success); }
.card p { margin: 0; }
</style>
