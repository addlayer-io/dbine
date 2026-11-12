<script setup lang="ts">
import { onMounted, onUnmounted, ref } from 'vue';
import type { JobOutcome, ServerJob } from '../api/jobs';
import { serverJobMenu } from '../composables/serverJobActions';
import { vKeyActivate } from '../composables/keyActivate';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { useServerJobsStore } from '../stores/serverJobs';
import { useTabsStore } from '../stores/tabs';
import ContextMenu, { type MenuItem } from './ContextMenu.vue';
import EngineIcon from './EngineIcon.vue';

// "Programadas › En los servidores" (docs/server-jobs.md): one group per
// connection whose engine has jobs. A group reads its jobs when it's opened
// and again every minute while it stays open; closed groups never connect.

const store = useServerJobsStore();
const conns = useConnectionsStore();
const tabs = useTabsStore();
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

function failed(connectionId: string): number {
  return store.entries[connectionId]?.list?.jobs.filter((j) => j.last_outcome === 'failed').length ?? 0;
}

const when = (iso: string) => new Date(iso).toLocaleString(locale(), { dateStyle: 'short', timeStyle: 'short' });

function openMenu(ev: MouseEvent, connectionId: string, job: ServerJob) {
  menu.value = { x: ev.clientX, y: ev.clientY, items: serverJobMenu(connectionId, job) };
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
        <div v-for="(n, i) in store.entries[c.id]?.list?.notes ?? []" :key="i" class="sj-note">{{ tb(n) }}</div>
        <div
          v-if="store.entries[c.id]?.list && !store.entries[c.id]?.list?.jobs.length && !store.entries[c.id]?.list?.notes?.length"
          class="sj-note"
        >{{ $t('scheduled:server.none') }}</div>
        <div
          v-for="job in store.entries[c.id]?.list?.jobs ?? []"
          :key="job.id"
          v-key-activate
          class="ss-item sj-job"
          :class="{ off: !job.enabled, [dot(job)]: true }"
          @click="tabs.openServerJob(c.id, job.id, job.name)"
          @contextmenu.prevent="openMenu($event, c.id, job)"
        >
          <div class="ss-row">
            <span
              class="ss-dot"
              :title="job.last_outcome ? $t(`scheduled:server.outcome.${job.last_outcome}`) : $t('scheduled:neverRan')"
            />
            <span class="ss-name" :title="job.description ?? job.name">{{ job.name }}</span>
            <span class="sj-kind">{{ job.kind }}</span>
          </div>
          <div class="ss-meta">
            <span class="sj-sched" :title="job.schedule ? tb(job.schedule) : ''">{{ job.group ? `${job.group} · ` : '' }}{{ job.schedule ? tb(job.schedule) : $t('scheduled:server.onDemand') }}</span>
            <span class="ss-sp" />
            <span v-if="!job.enabled">{{ $t('scheduled:server.disabled') }}</span>
            <span v-else-if="job.next_run" :title="$t('scheduled:nextRun')">→ {{ when(job.next_run) }}</span>
          </div>
        </div>
      </template>
    </template>
    <ContextMenu v-if="menu" :x="menu.x" :y="menu.y" :items="menu.items" @close="menu = null" />
  </div>
</template>

<style scoped>
.sj-empty { padding: 10px 16px 14px; font-size: 12px; color: var(--nm-text-dim); }
.sj-conn { display: flex; align-items: center; gap: 6px; padding: 4px 10px 4px 8px; cursor: pointer; font-size: 13px; color: var(--nm-text-strong); }
.sj-conn:hover { background: var(--ide-hover); }
.sj-chev { flex: none; font-size: 12px; color: var(--nm-text-dim); transition: transform .12s; }
.sj-chev.open { transform: rotate(90deg); }
.sj-conn-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sj-failed { flex: none; min-width: 16px; padding: 0 4px; border-radius: 8px; font-size: 10px; font-weight: 700; text-align: center; color: #fff; background: var(--nm-danger); }
.sj-spin { color: var(--nm-text-dim); }
.sj-icon { display: inline-flex; align-items: center; justify-content: center; width: 20px; height: 20px; border: 0; border-radius: 4px; background: none; color: var(--nm-text-dim); cursor: pointer; visibility: hidden; }
.sj-conn:hover .sj-icon { visibility: visible; }
.sj-icon:hover { background: var(--ide-hover); color: var(--nm-text-strong); }
.sj-note, .sj-error { padding: 3px 12px 5px 30px; font-size: 11.5px; color: var(--nm-text-dim); }
.sj-error { color: var(--nm-danger); display: flex; flex-wrap: wrap; align-items: baseline; gap: 6px; }
.sj-job { padding-left: 30px; }
.sj-kind { flex: none; max-width: 40%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 10px; padding: 0 4px; border-radius: 3px; color: var(--nm-text-dim); background: var(--ide-hover); }
.sj-sched { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

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
