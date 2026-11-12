<script setup lang="ts">
import { onMounted, onUnmounted, ref } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { scheduledApi, type TaskItem } from '../api/scheduled';
import { tb } from '../i18n/backend';
import { confirmNative } from '../native';
import { useScheduledStore } from '../stores/scheduled';
import { useServerJobsStore } from '../stores/serverJobs';
import { readJson, writeJson } from '../stores/storage';
import { useTabsStore } from '../stores/tabs';
import ContextMenu, { type MenuItem } from './ContextMenu.vue';
import ServerJobsSection from './ServerJobsSection.vue';
import { vKeyActivate } from '../composables/keyActivate';

// "Programadas" (the ⏰ in the activity bar): "En DBine", the tasks DBine
// runs on this computer (docs/scheduled-tasks.md), each with its schedule,
// its last run and the next one; and "En los servidores", the jobs each
// database server runs on its own (docs/server-jobs.md).

const { t } = useTranslation();
const store = useScheduledStore();
const tabs = useTabsStore();
const serverJobs = useServerJobsStore();

/** Closed sections, kept between sessions. */
const closed = ref<string[]>(readJson<string[]>('dbine.scheduled.closed', []));
function toggleSection(name: string) {
  closed.value = closed.value.includes(name) ? closed.value.filter((n) => n !== name) : [...closed.value, name];
  writeJson('dbine.scheduled.closed', closed.value);
}
function refreshAll() {
  store.load();
  serverJobs.refreshOpen();
}
const menu = ref<{ x: number; y: number; items: MenuItem[] } | null>(null);

// A run the OS started is written by another process: look again now and then.
let timer: ReturnType<typeof setInterval> | undefined;
onMounted(() => {
  store.load();
  timer = setInterval(() => store.load(), 60_000);
});
onUnmounted(() => clearInterval(timer));

function when(item: TaskItem): string {
  const s = item.schedule;
  if (s.type === 'daily') return t('scheduled:when.daily', { time: s.time });
  if (s.type === 'weekly') return t('scheduled:when.weekly', { days: s.days.map((d) => t(`scheduled:days.${d}`)).join(', '), time: s.time });
  if (s.type === 'monthly') return t('scheduled:when.monthly', { day: s.day, time: s.time });
  return t('scheduled:when.interval', { count: s.minutes });
}

async function runNow(item: TaskItem) {
  try {
    await scheduledApi.runNow(item.id);
    ElMessage.success(t('scheduled:started', { name: item.name }));
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

async function toggle(item: TaskItem) {
  try {
    const r = await scheduledApi.enable(item.id, !item.enabled);
    store.upsert(r.item);
    if (r.schedule_error) ElMessage.warning(tb(r.schedule_error));
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

async function remove(item: TaskItem) {
  if (!(await confirmNative(t('scheduled:deleteConfirm', { name: item.name }), { title: t('scheduled:delete'), okLabel: t('scheduled:delete'), kind: 'warning' }))) return;
  try {
    await scheduledApi.remove(item.id);
    store.drop(item.id);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

function openMenu(ev: MouseEvent, item: TaskItem) {
  menu.value = {
    x: ev.clientX,
    y: ev.clientY,
    items: [
      { label: t('scheduled:open'), action: () => tabs.openScheduledTask(item.id) },
      { label: t('scheduled:runNow'), action: () => runNow(item) },
      { label: item.enabled ? t('scheduled:disable') : t('scheduled:enable'), action: () => toggle(item) },
      { label: t('scheduled:delete'), danger: true, divided: true, action: () => remove(item) },
    ],
  };
}
</script>

<template>
  <div class="ss">
    <div class="ss-header">
      <span class="ss-title">{{ $t('scheduled:title') }}</span>
      <span style="flex: 1" />
      <button :aria-label="$t('common:refresh')" class="ss-icon" :title="$t('common:refresh')" @click="refreshAll()"><el-icon><ei-refresh /></el-icon></button>
    </div>
    <div class="ss-list">
      <div v-key-activate class="ss-section" @click="toggleSection('dbine')">
        <el-icon class="ss-chev" :class="{ open: !closed.includes('dbine') }"><ei-arrow-right /></el-icon>
        <span class="ss-section-title" :title="$t('scheduled:sections.dbineHint')">{{ $t('scheduled:sections.dbine') }}</span>
        <button :aria-label="$t('scheduled:newTask')" class="ss-icon" :title="$t('scheduled:newTask')" @click.stop="tabs.openScheduledTask(null)"><el-icon><ei-plus /></el-icon></button>
      </div>
      <template v-if="!closed.includes('dbine')">
      <div v-if="store.loaded && !store.items.length" class="ss-empty">
        <el-icon :size="22"><ei-alarm-clock /></el-icon>
        <span>{{ $t('scheduled:empty') }}</span>
        <el-button size="small" @click="tabs.openScheduledTask(null)">{{ $t('scheduled:newTask') }}</el-button>
      </div>
      <div
        v-for="item in store.items"
        :key="item.id"
        v-key-activate
        class="ss-item"
        :class="{ off: !item.enabled, [item.last_run?.status ?? 'none']: true }"
        @click="tabs.openScheduledTask(item.id)"
        @contextmenu.prevent="openMenu($event, item)"
      >
        <div class="ss-row">
          <span class="ss-dot" :title="item.last_run ? $t(`scheduled:status.${item.last_run.status}`) : $t('scheduled:neverRan')" />
          <span class="ss-name">{{ item.name }}</span>
          <span v-if="item.writes.some((w) => w.production)" class="ss-prod" :title="$t('scheduled:prodHint')">PROD</span>
          <el-icon v-if="item.needs_approval || (item.enabled && !item.registered)" class="ss-warn" :title="item.needs_approval ? $t('scheduled:needsApproval') : $t('scheduled:notRegistered')"><ei-warning-filled /></el-icon>
        </div>
        <div class="ss-meta">
          <span>{{ when(item) }}</span>
          <span class="ss-sp" />
          <span v-if="!item.enabled">{{ $t('scheduled:disabled') }}</span>
          <span v-else-if="item.next_run" :title="$t('scheduled:nextRun')">→ {{ item.next_run.slice(5) }}</span>
        </div>
      </div>
      </template>
      <div v-key-activate class="ss-section" @click="toggleSection('servers')">
        <el-icon class="ss-chev" :class="{ open: !closed.includes('servers') }"><ei-arrow-right /></el-icon>
        <span class="ss-section-title" :title="$t('scheduled:sections.serversHint')">{{ $t('scheduled:sections.servers') }}</span>
      </div>
      <ServerJobsSection v-if="!closed.includes('servers')" />
    </div>
    <ContextMenu v-if="menu" :x="menu.x" :y="menu.y" :items="menu.items" @close="menu = null" />
  </div>
</template>

<style scoped>
.ss { display: flex; flex-direction: column; height: 100%; min-height: 0; }
.ss-header { display: flex; align-items: center; gap: 2px; padding: 0 8px 0 16px; height: 35px; flex: none; }
.ss-title { font-size: 11px; font-weight: 600; letter-spacing: .06em; text-transform: uppercase; color: var(--nm-text-dim); }
.ss-icon { display: inline-flex; align-items: center; justify-content: center; width: 24px; height: 24px; border: 0; border-radius: 4px; background: none; color: var(--nm-text-dim); cursor: pointer; }
.ss-icon:hover { background: var(--ide-hover); color: var(--nm-text-strong); }
.ss-list { flex: 1; min-height: 0; overflow: auto; padding-bottom: 12px; }
.ss-section { display: flex; align-items: center; gap: 4px; height: 24px; padding: 0 8px 0 6px; cursor: pointer; border-top: 1px solid var(--nm-border-soft); }
.ss-section:first-child { border-top: 0; }
.ss-section-title { flex: 1; font-size: 11px; font-weight: 700; letter-spacing: .04em; text-transform: uppercase; color: var(--nm-text); }
.ss-chev { flex: none; font-size: 12px; color: var(--nm-text-dim); transition: transform .12s; }
.ss-chev.open { transform: rotate(90deg); }
.ss-section .ss-icon { visibility: hidden; }
.ss-section:hover .ss-icon { visibility: visible; }
.ss-empty { display: flex; flex-direction: column; align-items: center; gap: 10px; padding: 40px 16px; color: var(--nm-text-dim); font-size: 12px; text-align: center; }
.ss-item { padding: 6px 12px 6px 14px; cursor: pointer; }
.ss-item:hover { background: var(--ide-hover); }
.ss-item.off { opacity: .55; }
.ss-row { display: flex; align-items: center; gap: 7px; }
.ss-dot { width: 8px; height: 8px; border-radius: 50%; background: var(--nm-text-muted); flex: none; }
.ss-item.ok .ss-dot { background: var(--nm-success); }
.ss-item.partial .ss-dot { background: var(--nm-warning); }
.ss-item.failed .ss-dot { background: var(--nm-danger); }
.ss-item.running .ss-dot { background: var(--nm-info); }
.ss-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 13px; color: var(--nm-text-strong); }
.ss-prod { font-size: 9.5px; font-weight: 700; padding: 0 4px; border-radius: 3px; color: #fff; background: var(--nm-danger); }
.ss-warn { color: var(--nm-warning); }
.ss-meta { display: flex; gap: 6px; padding-left: 15px; margin-top: 2px; font-size: 11px; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; }
.ss-sp { flex: 1; }
</style>
