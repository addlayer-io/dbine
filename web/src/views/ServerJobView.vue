<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { jobsApi, type JobDetail, type JobOutcome, type JobRun } from '../api/jobs';
import ConnectionState from '../components/ConnectionState.vue';
import EngineIcon from '../components/EngineIcon.vue';
import { useConnectionGate } from '../composables/connectionGate';
import {
  openServerJobScript, runServerJob, serverJobActions, stopServerJob, toggleServerJob,
} from '../composables/serverJobActions';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { useServerJobsStore } from '../stores/serverJobs';
import type { ServerJobTab } from '../stores/tabs';
import { formatElapsed } from '../stores/tasks';

// A server job's tab (docs/server-jobs.md): what it is, what it runs, and
// its runs with each step's result and message. Read again every 30 s
// while the tab is shown, and right after an action on it.

const props = defineProps<{ tab: ServerJobTab; active: boolean }>();
const { t } = useTranslation();
const conns = useConnectionsStore();
const jobs = useServerJobsStore();
const { blocked: connBlocked, ensure: ensureConnection } = useConnectionGate(() => props.tab.connectionId);

const detail = ref<JobDetail | null>(null);
const runs = ref<JobRun[]>([]);
const error = ref<string | null>(null);
const runsError = ref<string | null>(null);
const loading = ref(false);
const limit = ref(50);
/** Runs whose steps and message are open, by index key. */
const open = ref(new Set<string>());

const conn = computed(() => conns.byId(props.tab.connectionId));
const driver = computed(() => conns.driverOf(props.tab.connectionId));
const job = computed(() => detail.value?.job ?? null);
const can = computed(() => (job.value ? serverJobActions(props.tab.connectionId, job.value) : null));
const groups = computed(() => {
  const by = new Map<string, { label: string; value: string }[]>();
  for (const p of detail.value?.properties ?? []) {
    const g = p.group || '';
    if (!by.has(g)) by.set(g, []);
    by.get(g)!.push(p);
  }
  return [...by.entries()];
});

async function load() {
  if (loading.value) return;
  loading.value = true;
  try {
    if (!(await ensureConnection({ auto: !!detail.value }))) return;
    detail.value = await jobsApi.detail(props.tab.connectionId, props.tab.jobId);
    error.value = null;
    jobs.put(props.tab.connectionId, detail.value.job);
    if (detail.value.job.has_history) {
      try {
        runs.value = await jobsApi.runs(props.tab.connectionId, props.tab.jobId, limit.value);
        runsError.value = null;
      } catch (e) {
        runsError.value = errorMessage(e);
      }
    }
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    loading.value = false;
  }
}

let timer: ReturnType<typeof setInterval> | undefined;
watch(() => props.active, (shown) => {
  clearInterval(timer);
  if (!shown) return;
  void load();
  timer = setInterval(() => void load(), 30_000);
}, { immediate: true });
onUnmounted(() => clearInterval(timer));

async function act(fn: typeof runServerJob) {
  if (job.value && (await fn(props.tab.connectionId, job.value))) void load();
}

function more() {
  limit.value = Math.min(limit.value * 4, 500);
  void load();
}

const when = (iso: string | null) => (iso ? new Date(iso).toLocaleString(locale(), { dateStyle: 'medium', timeStyle: 'medium' }) : '—');
const took = (ms: number | null) => (ms == null ? '—' : formatElapsed(ms));
const outcome = (o: JobOutcome | null) => (o ? t(`scheduled:server.outcome.${o}`) : t('scheduled:neverRan'));
const dot = (o: JobOutcome | null) =>
  o === 'succeeded' ? 'ok' : o === 'failed' ? 'failed' : o === 'running' || o === 'retrying' ? 'running' : o ? 'partial' : 'none';

function toggleRun(key: string) {
  const s = new Set(open.value);
  if (s.has(key)) s.delete(key);
  else s.add(key);
  open.value = s;
}
</script>

<template>
  <div class="sjv">
    <header class="sjv-head">
      <EngineIcon v-if="driver" :id="driver.id" :name="driver.name" :size="20" />
      <div class="sjv-title">
        <strong>{{ job?.name ?? tab.jobName }}</strong>
        <span class="nm-muted">{{ conn?.name }}<template v-if="job?.group"> · {{ job.group }}</template></span>
      </div>
      <span v-if="job" class="sjv-kind">{{ job.kind }}</span>
      <span v-if="job && !job.enabled" class="sjv-chip off">{{ $t('scheduled:server.disabled') }}</span>
      <span v-if="job?.running" class="sjv-chip running">{{ $t('scheduled:server.outcome.running') }}</span>
      <div class="sjv-spacer" />
      <template v-if="job && can">
        <el-button v-if="can.run" size="small" type="primary" @click="act(runServerJob)">{{ $t('scheduled:server.run') }}</el-button>
        <el-button v-if="can.stop" size="small" @click="act(stopServerJob)">{{ $t('scheduled:server.stop') }}</el-button>
        <el-button v-if="can.toggle" size="small" @click="act(toggleServerJob)">
          {{ job.enabled ? $t('scheduled:server.disable') : $t('scheduled:server.enable') }}
        </el-button>
        <el-button v-if="can.script" size="small" @click="openServerJobScript(tab.connectionId, job)">{{ $t('scheduled:server.script') }}</el-button>
      </template>
      <el-button size="small" :loading="loading" :aria-label="$t('common:refresh')" :title="$t('common:refresh')" @click="load()">
        <el-icon><ei-refresh /></el-icon>
      </el-button>
    </header>
    <ConnectionState v-if="connBlocked" :connection-id="tab.connectionId" @retry="load" />
    <el-alert v-if="error" type="error" :title="error" :closable="false" show-icon class="sjv-alert" />
    <div v-if="loading && !detail && !error" class="sjv-empty"><el-icon class="is-loading" :size="22"><ei-loading /></el-icon></div>

    <div v-if="detail && job" class="sjv-body">
      <section class="sjv-facts">
        <div class="sjv-fact"><span>{{ $t('scheduled:server.schedule') }}</span><code>{{ job.schedule ?? $t('scheduled:server.onDemand') }}</code></div>
        <div v-for="(s, i) in detail.schedules" :key="i" class="sjv-fact"><span /><code>{{ s }}</code></div>
        <div class="sjv-fact"><span>{{ $t('scheduled:nextRun') }}</span>{{ job.enabled ? when(job.next_run) : $t('scheduled:server.disabled') }}</div>
        <div class="sjv-fact">
          <span>{{ $t('scheduled:server.lastRun') }}</span>
          <span class="sjv-out" :class="dot(job.last_outcome)"><i />{{ outcome(job.last_outcome) }}</span>
          <template v-if="job.last_started"> · {{ when(job.last_started) }} · {{ took(job.last_duration_ms) }}</template>
        </div>
        <div v-if="job.owner" class="sjv-fact"><span>{{ $t('scheduled:server.owner') }}</span>{{ job.owner }}</div>
        <div v-if="job.description" class="sjv-fact"><span>{{ $t('scheduled:server.description') }}</span>{{ job.description }}</div>
        <div v-if="conn?.config.read_only" class="sjv-fact nm-muted"><span />{{ $t('scheduled:server.readOnly') }}</div>
      </section>

      <section v-if="detail.steps.length">
        <h3 class="nm-section-title">{{ $t('scheduled:server.steps') }}</h3>
        <table class="sjv-table">
          <thead>
            <tr><th>#</th><th>{{ $t('scheduled:server.stepName') }}</th><th>{{ $t('scheduled:server.stepKind') }}</th><th>{{ $t('scheduled:server.stepTarget') }}</th><th>{{ $t('scheduled:server.stepCommand') }}</th></tr>
          </thead>
          <tbody>
            <tr v-for="(s, i) in detail.steps" :key="i">
              <td class="num">{{ i + 1 }}</td>
              <td>{{ s.name }}</td>
              <td>{{ s.kind ? tb(s.kind) : '' }}</td>
              <td>{{ s.target ?? '' }}</td>
              <td class="code" :title="s.command ?? ''">{{ s.command ?? '' }}</td>
            </tr>
          </tbody>
        </table>
      </section>

      <section v-for="[g, props2] in groups" :key="g">
        <h3 class="nm-section-title">{{ g ? tb(g) : $t('scheduled:server.properties') }}</h3>
        <div v-for="(p, i) in props2" :key="i" class="sjv-fact"><span>{{ tb(p.label) }}</span>{{ p.value }}</div>
      </section>

      <section>
        <h3 class="nm-section-title">{{ $t('scheduled:server.history') }}</h3>
        <div v-if="!job.has_history" class="nm-muted sjv-note">{{ $t('scheduled:server.noHistory') }}</div>
        <el-alert v-else-if="runsError" type="error" :title="runsError" :closable="false" show-icon />
        <div v-else-if="!runs.length" class="nm-muted sjv-note">{{ $t('scheduled:neverRan') }}</div>
        <table v-else class="sjv-table runs">
          <thead>
            <tr><th>{{ $t('scheduled:server.started') }}</th><th>{{ $t('scheduled:server.duration') }}</th><th>{{ $t('scheduled:server.result') }}</th><th>{{ $t('scheduled:server.trigger') }}</th><th>{{ $t('scheduled:server.message') }}</th></tr>
          </thead>
          <tbody>
            <template v-for="(r, i) in runs" :key="r.id ?? i">
              <tr :class="{ expandable: r.steps.length || r.message }" @click="toggleRun(r.id ?? String(i))">
                <td>{{ when(r.started) }}</td>
                <td class="num">{{ took(r.duration_ms) }}</td>
                <td><span class="sjv-out" :class="dot(r.outcome)"><i />{{ outcome(r.outcome) }}</span></td>
                <td>{{ r.trigger ? tb(r.trigger) : '' }}</td>
                <td class="msg" :title="r.message ?? ''">{{ r.message ?? '' }}</td>
              </tr>
              <tr v-if="open.has(r.id ?? String(i))" class="sjv-run-detail">
                <td colspan="5">
                  <pre v-if="r.message" class="sjv-msg">{{ r.message }}</pre>
                  <table v-if="r.steps.length" class="sjv-table">
                    <tbody>
                      <tr v-for="(s, j) in r.steps" :key="j">
                        <td>{{ s.name }}</td>
                        <td><span class="sjv-out" :class="dot(s.outcome)"><i />{{ outcome(s.outcome) }}</span></td>
                        <td>{{ when(s.started) }}</td>
                        <td class="num">{{ took(s.duration_ms) }}</td>
                        <td class="msg wrap">{{ s.message ?? '' }}</td>
                      </tr>
                    </tbody>
                  </table>
                </td>
              </tr>
            </template>
          </tbody>
        </table>
        <el-button v-if="job.has_history && runs.length >= limit && limit < 500" size="small" link type="primary" @click="more">
          {{ $t('scheduled:server.moreRuns') }}
        </el-button>
      </section>
    </div>
  </div>
</template>

<style scoped>
.sjv { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--nm-bg); }
.sjv-head { display: flex; align-items: center; gap: 10px; padding: 10px 16px; border-bottom: 1px solid var(--nm-border-soft); flex-wrap: wrap; }
.sjv-head .el-button { margin: 0; }
.sjv-title { display: flex; flex-direction: column; min-width: 0; }
.sjv-title strong { color: var(--nm-text-strong); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sjv-title span { font-size: 11.5px; }
.sjv-kind { font-size: 10.5px; padding: 1px 6px; border-radius: 3px; color: var(--nm-text-dim); background: var(--ide-hover); }
.sjv-chip { font-size: 10.5px; padding: 1px 6px; border-radius: 8px; }
.sjv-chip.off { color: var(--nm-text-dim); border: 1px solid var(--nm-border); }
.sjv-chip.running { color: #fff; background: var(--nm-info); }
.sjv-spacer { flex: 1; }
.sjv-alert { margin: 10px 16px 0; width: auto; }
.sjv-empty { display: flex; justify-content: center; padding: 40px; color: var(--nm-text-dim); }
.sjv-body { flex: 1; min-height: 0; overflow: auto; padding: 8px 16px 24px; display: flex; flex-direction: column; gap: 14px; }
.sjv-facts { display: flex; flex-direction: column; gap: 4px; }
.sjv-fact { display: flex; gap: 10px; font-size: 12.5px; color: var(--nm-text); align-items: baseline; }
.sjv-fact > span:first-child { flex: none; width: 150px; color: var(--nm-text-dim); }
.sjv-fact code { font-family: var(--nm-mono); font-size: 12px; }
.sjv-note { font-size: 12px; padding: 4px 0; }
.sjv-out { display: inline-flex; align-items: center; gap: 5px; }
.sjv-out i { width: 8px; height: 8px; border-radius: 50%; background: var(--nm-text-muted); }
.sjv-out.ok i { background: var(--nm-success); }
.sjv-out.failed i { background: var(--nm-danger); }
.sjv-out.running i { background: var(--nm-info); }
.sjv-out.partial i { background: var(--nm-warning); }
.sjv-table { width: 100%; border-collapse: collapse; font-size: 12px; }
.sjv-table th { text-align: left; font-weight: 600; padding: 5px 10px; color: var(--nm-text-dim); border-bottom: 1px solid var(--nm-border); white-space: nowrap; }
.sjv-table td { padding: 4px 10px; border-bottom: 1px solid var(--nm-border-soft); color: var(--nm-text); max-width: 360px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-variant-numeric: tabular-nums; }
.sjv-table .num { text-align: right; }
.sjv-table td.code { font-family: var(--nm-mono); max-width: 520px; }
.sjv-table td.msg { max-width: 520px; }
.sjv-table td.wrap { white-space: pre-wrap; }
.sjv-table.runs tbody tr.expandable { cursor: pointer; }
.sjv-table.runs tbody tr.expandable:hover td { background: var(--ide-hover); }
.sjv-run-detail > td { background: var(--nm-bg-elev); white-space: normal; max-width: none; }
.sjv-msg { margin: 4px 0 8px; font-family: var(--nm-mono); font-size: 11.5px; white-space: pre-wrap; color: var(--nm-text); }
</style>
