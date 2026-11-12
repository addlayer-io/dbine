<script setup lang="ts">
import { onMounted, onUnmounted, ref } from 'vue';
import { useTranslation } from 'i18next-vue';
import type { JobOutcome, ServerJob } from '../api/jobs';
import type { SchedulerItem, SchedulerKind } from '../api/scheduler';
import { canCreate, schedulerItemMenu, schedulerKindMenu } from '../composables/schedulerActions';
import { serverJobMenu } from '../composables/serverJobActions';
import { vKeyActivate } from '../composables/keyActivate';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { useServerJobsStore } from '../stores/serverJobs';
import { readJson, writeJson } from '../stores/storage';
import { useTabsStore } from '../stores/tabs';
import ContextMenu, { type MenuItem } from './ContextMenu.vue';
import EngineIcon from './EngineIcon.vue';
import SchedulerDialogs from './SchedulerDialogs.vue';

// "Programadas › En los servidores" (docs/server-jobs.md): one group per
// connection whose engine has jobs. A group reads its jobs when it's opened
// and again every minute while it stays open; closed groups never connect.
// The jobs that come with the engine (`system`) are hidden until the group's
// toggle shows them, remembered per connection.
//
// Engines whose scheduler DBine manages whole (`capabilities.scheduler`)
// show it as a tree: its notes, one node per kind of object (Jobs first,
// holding the job rows; the others read their objects when opened), "+" on
// a kind to create one in the designer, and the scheduler's log.

const store = useServerJobsStore();
const conns = useConnectionsStore();
const tabs = useTabsStore();
const { t } = useTranslation();
const menu = ref<{ x: number; y: number; items: MenuItem[] } | null>(null);

let timer: ReturnType<typeof setInterval> | undefined;
onMounted(() => {
  store.refreshOpen({ auto: true });
  timer = setInterval(() => store.refreshOpen({ auto: true }), 60_000);
});
onUnmounted(() => clearInterval(timer));

/** The dot's class, shared with "En DBine": ok, failed, running, partial, none. */
function dot(job: ServerJob): string {
  if (job.running) return 'running';
  const o: JobOutcome | null = job.last_outcome;
  if (o === 'succeeded') return 'ok';
  if (o === 'failed') return 'failed';
  if (o === 'running' || o === 'retrying') return 'running';
  if (o === 'canceled' || o === 'skipped') return 'partial';
  return 'none';
}

const SHOW_SYSTEM = 'dbine.serverJobs.showSystem';
/** The connections whose system jobs are shown. */
const showSystem = ref<string[]>(readJson<string[]>(SHOW_SYSTEM, []));

function toggleSystem(connectionId: string) {
  showSystem.value = showSystem.value.includes(connectionId)
    ? showSystem.value.filter((id) => id !== connectionId)
    : [...showSystem.value, connectionId];
  writeJson(SHOW_SYSTEM, showSystem.value);
}

/** How many of the connection's jobs (and of its scheduler's objects read
 *  so far) are the engine's own. */
function systemCount(connectionId: string): number {
  const jobs = store.entries[connectionId]?.list?.jobs.filter((j) => j.system).length ?? 0;
  const items = Object.values(store.scheduler[connectionId]?.items ?? {})
    .reduce((n, k) => n + (k.list?.items.filter((i) => i.system).length ?? 0), 0);
  return jobs + items;
}

/** The jobs the group shows. */
function shown(connectionId: string): ServerJob[] {
  const all = store.entries[connectionId]?.list?.jobs ?? [];
  return showSystem.value.includes(connectionId) ? all : all.filter((j) => !j.system);
}

/** A kind's objects the tree shows. */
function shownItems(connectionId: string, kind: string): SchedulerItem[] {
  const all = store.scheduler[connectionId]?.items[kind]?.list?.items ?? [];
  return showSystem.value.includes(connectionId) ? all : all.filter((i) => !i.system);
}

function failed(connectionId: string): number {
  return shown(connectionId).filter((j) => j.last_outcome === 'failed').length;
}

const when = (iso: string) => new Date(iso).toLocaleString(locale(), { dateStyle: 'short', timeStyle: 'short' });

/** One line of a group: notes and errors, kind nodes, jobs, objects, the log. */
type Row =
  | { type: 'note' | 'empty'; key: string; text: string; nested: boolean }
  | { type: 'error'; key: string; text: string; nested: boolean; retry: () => void }
  | { type: 'kind'; key: string; kind: SchedulerKind; open: boolean; count: number | null; loading: boolean }
  | { type: 'job'; key: string; job: ServerJob; nested: boolean }
  | { type: 'item'; key: string; kind: SchedulerKind; item: SchedulerItem }
  | { type: 'log'; key: string };

/** The group's lines: the scheduler's tree when the engine has one (and
 *  it was read), else the jobs as a flat list. */
function rows(connectionId: string): Row[] {
  const out: Row[] = [];
  const e = store.entries[connectionId];
  const s = store.hasScheduler(connectionId) ? store.scheduler[connectionId] : undefined;
  const jobRows = (nested: boolean) => {
    (e?.list?.notes ?? []).forEach((n, i) => out.push({ type: 'note', key: `jn${i}`, text: tb(n), nested }));
    if (e?.list && !e.list.jobs.length && !e.list.notes?.length) out.push({ type: 'empty', key: 'jnone', text: t('scheduled:server.none'), nested });
    for (const job of shown(connectionId)) out.push({ type: 'job', key: `j:${job.id}`, job, nested });
  };
  if (s?.error) out.push({ type: 'error', key: 'serr', text: s.error, nested: false, retry: () => void store.load(connectionId) });
  const info = s?.info;
  if (!info) {
    jobRows(false);
    return out;
  }
  (info.notes ?? []).forEach((n, i) => out.push({ type: 'note', key: `in${i}`, text: tb(n), nested: false }));
  // Jobs outside every kind (a driver without a "job" kind) stay listed.
  if (!info.kinds.some((k) => k.key === 'job') && e?.list?.jobs.length) jobRows(false);
  for (const k of info.kinds) {
    const open = store.isKindOpen(connectionId, k.key);
    const items = s.items[k.key];
    const isJob = k.key === 'job';
    out.push({
      type: 'kind', key: `k:${k.key}`, kind: k, open,
      count: isJob ? (e?.list ? shown(connectionId).length : null) : items?.list ? shownItems(connectionId, k.key).length : null,
      loading: isJob ? !!e?.loading : !!items?.loading,
    });
    if (!open) continue;
    if (isJob) { jobRows(true); continue; }
    if (items?.error) out.push({ type: 'error', key: `ke:${k.key}`, text: items.error, nested: true, retry: () => void store.loadItems(connectionId, k.key) });
    (items?.list?.notes ?? []).forEach((n, i) => out.push({ type: 'note', key: `kn:${k.key}:${i}`, text: tb(n), nested: true }));
    const list = shownItems(connectionId, k.key);
    for (const item of list) out.push({ type: 'item', key: `i:${k.key}:${item.id}`, kind: k, item });
    if (items?.list && !list.length && !items.list.notes?.length) out.push({ type: 'empty', key: `kx:${k.key}`, text: t('scheduled:server.noItems'), nested: true });
  }
  if (info.logs) out.push({ type: 'log', key: 'log' });
  return out;
}

function openMenu(ev: MouseEvent, connectionId: string, job: ServerJob) {
  menu.value = { x: ev.clientX, y: ev.clientY, items: serverJobMenu(connectionId, job) };
}

function openKindMenu(ev: MouseEvent, connectionId: string, kind: SchedulerKind) {
  menu.value = { x: ev.clientX, y: ev.clientY, items: schedulerKindMenu(connectionId, kind) };
}

function openItemMenu(ev: MouseEvent, connectionId: string, kind: SchedulerKind, item: SchedulerItem) {
  menu.value = { x: ev.clientX, y: ev.clientY, items: schedulerItemMenu(connectionId, kind, item) };
}

/** The connection's menu: "Nuevo job" where the scheduler is managed. */
function openConnMenu(ev: MouseEvent, connectionId: string) {
  if (!store.hasScheduler(connectionId)) return;
  const items: MenuItem[] = [];
  const job = store.kind(connectionId, 'job');
  if (canCreate(connectionId, job)) items.push({ label: t('scheduled:server.newJob'), action: () => tabs.openSchedulerObject(connectionId, 'job', null) });
  if (store.scheduler[connectionId]?.info?.logs) items.push({ label: t('scheduled:server.log'), action: () => tabs.openSchedulerLog(connectionId) });
  items.push({ label: t('common:refresh'), divided: items.length > 0, action: () => void store.load(connectionId) });
  menu.value = { x: ev.clientX, y: ev.clientY, items };
}

function create(connectionId: string, kind: SchedulerKind) {
  tabs.openSchedulerObject(connectionId, kind.key, null);
}
</script>

<template>
  <div class="sj">
    <div v-if="!store.connections.length" class="sj-empty">{{ $t('scheduled:server.noConnections') }}</div>
    <template v-for="c in store.connections" :key="c.id">
      <div
        v-key-activate
        class="sj-conn"
        :title="conns.driverOf(c.id)?.name"
        @click="store.toggle(c.id)"
        @contextmenu.prevent="openConnMenu($event, c.id)"
      >
        <el-icon class="sj-chev" :class="{ open: store.isExpanded(c.id) }"><ei-arrow-right /></el-icon>
        <EngineIcon :id="c.config.driver" :name="conns.driverOf(c.id)?.name ?? ''" :size="14" />
        <span class="sj-conn-name">{{ c.name }}</span>
        <span v-if="failed(c.id)" class="sj-failed" :title="$t('scheduled:server.failedCount', { count: failed(c.id) })">{{ failed(c.id) }}</span>
        <el-icon v-if="store.entries[c.id]?.loading" class="is-loading sj-spin"><ei-loading /></el-icon>
        <button
          v-else-if="store.isExpanded(c.id)"
          class="sj-icon"
          :aria-label="$t('common:refresh')"
          :title="$t('common:refresh')"
          @click.stop="store.load(c.id)"
        ><el-icon><ei-refresh /></el-icon></button>
      </div>
      <template v-if="store.isExpanded(c.id)">
        <div v-if="store.entries[c.id]?.error" class="sj-error">
          <span>{{ store.entries[c.id]?.error }}</span>
          <el-button size="small" link type="primary" @click="store.load(c.id)">{{ $t('common:retry') }}</el-button>
        </div>
        <button
          v-if="systemCount(c.id)"
          class="sj-system"
          :title="store.scheduler[c.id]?.info ? $t('scheduled:server.systemObjectsHint') : $t('scheduled:server.systemHint')"
          @click="toggleSystem(c.id)"
        >{{ showSystem.includes(c.id)
          ? $t('scheduled:server.hideSystem', { count: systemCount(c.id) })
          : $t('scheduled:server.showSystem', { count: systemCount(c.id) }) }}</button>
        <template v-for="r in rows(c.id)" :key="r.key">
          <div v-if="r.type === 'note' || r.type === 'empty'" class="sj-note" :class="{ nested: r.nested }">{{ r.text }}</div>
          <div v-else-if="r.type === 'error'" class="sj-error" :class="{ nested: r.nested }">
            <span>{{ r.text }}</span>
            <el-button size="small" link type="primary" @click="r.retry()">{{ $t('common:retry') }}</el-button>
          </div>
          <div
            v-else-if="r.type === 'kind'"
            v-key-activate
            class="sj-node"
            :title="tb(r.kind.label)"
            @click="store.toggleKind(c.id, r.kind.key)"
            @contextmenu.prevent="openKindMenu($event, c.id, r.kind)"
          >
            <el-icon class="sj-chev" :class="{ open: r.open }"><ei-arrow-right /></el-icon>
            <el-icon class="sj-node-ic"><ei-folder-opened v-if="r.open" /><ei-folder v-else /></el-icon>
            <span class="sj-node-name">{{ tb(r.kind.label) }}</span>
            <span v-if="r.count !== null" class="sj-count">{{ r.count }}</span>
            <el-icon v-if="r.loading && r.kind.key !== 'job'" class="is-loading sj-spin"><ei-loading /></el-icon>
            <button
              v-if="canCreate(c.id, r.kind)"
              class="sj-icon"
              :aria-label="r.kind.key === 'job' ? $t('scheduled:server.newJob') : $t('scheduled:designer.newTitle', { kind: tb(r.kind.singular) })"
              :title="r.kind.key === 'job' ? $t('scheduled:server.newJob') : $t('scheduled:designer.newTitle', { kind: tb(r.kind.singular) })"
              @click.stop="create(c.id, r.kind)"
            ><el-icon><ei-plus /></el-icon></button>
          </div>
          <div
            v-else-if="r.type === 'job'"
            v-key-activate
            class="ss-item sj-job"
            :class="{ off: !r.job.enabled, nested: r.nested, [dot(r.job)]: true }"
            @click="tabs.openServerJob(c.id, r.job.id, r.job.name)"
            @contextmenu.prevent="openMenu($event, c.id, r.job)"
          >
            <div class="ss-row">
              <span
                class="ss-dot"
                :title="r.job.last_outcome ? $t(`scheduled:server.outcome.${r.job.last_outcome}`) : $t('scheduled:neverRan')"
              />
              <span class="ss-name" :title="r.job.description ?? r.job.name">{{ r.job.name }}</span>
              <span class="sj-kind">{{ r.job.kind }}</span>
            </div>
            <div class="ss-meta">
              <span class="sj-sched" :title="r.job.schedule ? tb(r.job.schedule) : ''">{{ r.job.group ? `${r.job.group} · ` : '' }}{{ r.job.schedule ? tb(r.job.schedule) : $t('scheduled:server.onDemand') }}</span>
              <span class="ss-sp" />
              <span v-if="!r.job.enabled">{{ $t('scheduled:server.disabled') }}</span>
              <span v-else-if="r.job.next_run" :title="$t('scheduled:nextRun')">→ {{ when(r.job.next_run) }}</span>
            </div>
          </div>
          <div
            v-else-if="r.type === 'item'"
            v-key-activate
            class="sj-item"
            :class="{ off: r.item.enabled === false }"
            :title="r.item.description || r.item.name"
            @click="tabs.openSchedulerObject(c.id, r.kind.key, r.item.id, r.item.name)"
            @contextmenu.prevent="openItemMenu($event, c.id, r.kind, r.item)"
          >
            <span class="sj-item-name">{{ r.item.name }}</span>
            <span v-if="r.item.system" class="sj-kind">{{ $t('scheduled:server.system') }}</span>
            <span v-if="r.item.enabled === false" class="sj-item-off">{{ $t('scheduled:server.disabled') }}</span>
          </div>
          <div v-else-if="r.type === 'log'" v-key-activate class="sj-node sj-log" @click="tabs.openSchedulerLog(c.id)">
            <span class="sj-chev-pad" />
            <el-icon class="sj-node-ic"><ei-tickets /></el-icon>
            <span class="sj-node-name">{{ $t('scheduled:server.log') }}</span>
          </div>
        </template>
      </template>
    </template>
    <ContextMenu v-if="menu" :x="menu.x" :y="menu.y" :items="menu.items" @close="menu = null" />
    <SchedulerDialogs />
  </div>
</template>

<style scoped>
.sj-empty { padding: 10px 16px 14px; font-size: 12px; color: var(--nm-text-dim); }
.sj-conn { display: flex; align-items: center; gap: 6px; padding: 4px 10px 4px 8px; cursor: pointer; font-size: 13px; color: var(--nm-text-strong); }
.sj-conn:hover { background: var(--ide-hover); }
.sj-chev { flex: none; font-size: 12px; color: var(--nm-text-dim); transition: transform .12s; }
.sj-chev.open { transform: rotate(90deg); }
.sj-chev-pad { flex: none; width: 12px; }
.sj-conn-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sj-failed { flex: none; min-width: 16px; padding: 0 4px; border-radius: 8px; font-size: 10px; font-weight: 700; text-align: center; color: #fff; background: var(--nm-danger); }
.sj-spin { color: var(--nm-text-dim); }
.sj-icon { display: inline-flex; align-items: center; justify-content: center; width: 20px; height: 20px; border: 0; border-radius: 4px; background: none; color: var(--nm-text-dim); cursor: pointer; visibility: hidden; }
.sj-conn:hover .sj-icon, .sj-node:hover .sj-icon, .sj-node:focus-within .sj-icon { visibility: visible; }
.sj-icon:hover { background: var(--ide-hover); color: var(--nm-text-strong); }
.sj-note, .sj-error { padding: 3px 12px 5px 30px; font-size: 11.5px; color: var(--nm-text-dim); }
.sj-note.nested, .sj-error.nested { padding-left: 44px; }
.sj-error { color: var(--nm-danger); display: flex; flex-wrap: wrap; align-items: baseline; gap: 6px; }
.sj-job { padding-left: 30px; }
.sj-job.nested { padding-left: 44px; }
.sj-system { display: block; margin: 0 0 2px 30px; padding: 1px 0; border: 0; background: none; font: inherit; font-size: 11.5px; color: var(--nm-link); cursor: pointer; }
.sj-system:hover { text-decoration: underline; }
.sj-kind { flex: none; max-width: 40%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 10px; padding: 0 4px; border-radius: 3px; color: var(--nm-text-dim); background: var(--ide-hover); }
.sj-sched { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

/* The scheduler's tree: kind nodes, their objects, the log. */
.sj-node { display: flex; align-items: center; gap: 6px; height: 24px; padding: 0 10px 0 24px; cursor: pointer; font-size: 12.5px; color: var(--nm-text); }
.sj-node:hover { background: var(--ide-hover); }
.sj-node-ic { flex: none; font-size: 13px; color: var(--nm-text-dim); }
.sj-node-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sj-count { flex: none; font-size: 10px; min-width: 16px; padding: 0 5px; border-radius: 8px; text-align: center; color: var(--nm-text-dim); background: var(--ide-hover); font-variant-numeric: tabular-nums; }
.sj-item { display: flex; align-items: center; gap: 6px; height: 24px; padding: 0 12px 0 44px; cursor: pointer; font-size: 12.5px; color: var(--nm-text-strong); }
.sj-item:hover { background: var(--ide-hover); }
.sj-item.off { opacity: .55; }
.sj-item-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sj-item-off { flex: none; font-size: 10.5px; color: var(--nm-text-dim); }

/* The job rows reuse "En DBine"'s look (ScheduledSidebar.vue). */
.ss-item { padding-top: 6px; padding-right: 12px; padding-bottom: 6px; cursor: pointer; }
.ss-item:hover { background: var(--ide-hover); }
.ss-item.off { opacity: .55; }
.ss-row { display: flex; align-items: center; gap: 7px; }
.ss-dot { width: 8px; height: 8px; border-radius: 50%; background: var(--nm-text-muted); flex: none; }
.ss-item.ok .ss-dot { background: var(--nm-success); }
.ss-item.partial .ss-dot { background: var(--nm-warning); }
.ss-item.failed .ss-dot { background: var(--nm-danger); }
.ss-item.running .ss-dot { background: var(--nm-info); }
.ss-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 13px; color: var(--nm-text-strong); }
.ss-meta { display: flex; gap: 6px; padding-left: 15px; margin-top: 2px; font-size: 11px; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; }
.ss-sp { flex: 1; }
</style>
