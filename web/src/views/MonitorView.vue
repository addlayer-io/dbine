<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import type { Metric, MonitorSnapshot, MonitorTable } from '../api/types';
import EngineIcon from '../components/EngineIcon.vue';
import Sparkline from '../components/Sparkline.vue';
import BlockingPanel from '../components/BlockingPanel.vue';
import { locksApi, type BlockedSession } from '../api/locks';
import { useConnectionsStore } from '../stores/connections';
import type { MonitorTab } from '../stores/tabs';

// The server monitor: polls the driver's snapshot every few seconds while
// the tab is visible and keeps a rolling history per metric. Running totals
// (`counter`) are charted as a rate per second between two snapshots.

const props = defineProps<{ tab: MonitorTab; active: boolean }>();
const conns = useConnectionsStore();
const { t } = useTranslation();

const HISTORY = 120;
const INTERVALS = [2, 5, 10, 30];

const conn = computed(() => conns.byId(props.tab.connectionId));
const driver = computed(() => conns.driverOf(props.tab.connectionId));

const snap = ref<MonitorSnapshot | null>(null);
const error = ref<string | null>(null);
/** The locks panel: engines that report who blocks whom (docs/bloqueos.md). */
const canBlocking = computed(() => !!driver.value?.capabilities.blocking);
const canKill = computed(() => !!driver.value?.capabilities.kill_session && !conn.value?.config.read_only);
/** Why the login can't end other sessions ('' when it can). */
const killDenied = computed(() => {
  const missing = conns.denied(props.tab.connectionId, '', 'kill_session');
  return missing ? t('common:noPermission', { missing }) : '';
});
const blocking = ref<BlockedSession[]>([]);
const blockingError = ref<string | null>(null);
async function pollBlocking() {
  if (!canBlocking.value) return;
  try {
    blocking.value = await locksApi.blocking(props.tab.connectionId);
    blockingError.value = null;
  } catch (e) {
    blockingError.value = errorMessage(e);
  }
}
/** The driver doesn't report a snapshot (known before connecting). */
const unsupported = computed(() => !!driver.value && !driver.value.capabilities.monitor);
const loading = ref(false);
const paused = ref(false);
const every = ref(5);
const lastAt = ref<number | null>(null);
const now = ref(Date.now());

/** key → recent points (already rates for counters). */
const history = ref<Record<string, { t: number; v: number | null }[]>>({});
/** Previous raw value of each counter, to compute rates. */
let prevRaw: Record<string, { t: number; v: number }> = {};

function record(s: MonitorSnapshot, t: number) {
  const h = history.value;
  for (const m of s.metrics) {
    let v = m.value;
    if (m.counter) {
      const p = prevRaw[m.key];
      if (v !== null) prevRaw[m.key] = { t, v };
      // A counter that went down means the server restarted: no rate.
      v = p && v !== null && v >= p.v && t > p.t ? (v - p.v) / ((t - p.t) / 1000) : null;
      // The first snapshot has nothing to compare with: don't plot a gap.
      if (!p) continue;
    }
    (h[m.key] ??= []).push({ t, v });
    if (h[m.key].length > HISTORY) h[m.key].shift();
  }
}

async function poll() {
  if (loading.value) return;
  loading.value = true;
  try {
    if (!(await conns.ensureConnected(props.tab.connectionId))) {
      error.value = t('monitor:couldNotConnect');
      return;
    }
    if (canKill.value) conns.loadPermissions(props.tab.connectionId, '');
    const [s] = await Promise.all([api.monitorSnapshot(props.tab.connectionId), pollBlocking()]);
    const at = Date.now();
    record(s, at);
    snap.value = s;
    lastAt.value = at;
    error.value = null;
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    loading.value = false;
  }
}

let timer: ReturnType<typeof setInterval> | undefined;
let clock: ReturnType<typeof setInterval> | undefined;
function schedule() {
  clearInterval(timer);
  timer = undefined;
  if (!props.active || paused.value || unsupported.value) return;
  timer = setInterval(poll, every.value * 1000);
}
watch(
  () => [props.active, paused.value, every.value] as const,
  ([active]) => {
    if (active && !paused.value && !unsupported.value && (!lastAt.value || Date.now() - lastAt.value > every.value * 1000)) poll();
    schedule();
  },
  { immediate: true },
);
clock = setInterval(() => (now.value = Date.now()), 1000);
onUnmounted(() => {
  clearInterval(timer);
  clearInterval(clock);
});

// -- formatting --------------------------------------------------------------------------

function bytes(v: number) {
  const u = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
  let i = 0;
  let x = Math.abs(v);
  while (x >= 1024 && i < u.length - 1) { x /= 1024; i++; }
  return `${(v < 0 ? -x : x).toLocaleString(locale(), { maximumFractionDigits: x < 10 && i ? 1 : 0 })} ${u[i]}`;
}
function duration(s: number) {
  if (s < 60) return t('monitor:duration.s', { s: s.toLocaleString(locale(), { maximumFractionDigits: s < 10 ? 1 : 0 }) });
  const d = Math.floor(s / 86400), h = Math.floor((s % 86400) / 3600), m = Math.floor((s % 3600) / 60);
  return d ? t('monitor:duration.dh', { d, h }) : h ? t('monitor:duration.hm', { h, m }) : t('monitor:duration.ms', { m, s: Math.floor(s % 60) });
}
function fmt(m: Metric, v: number): string {
  const rate = m.counter ? '/s' : '';
  switch (m.unit) {
    case 'percent': return `${v.toLocaleString(locale(), { maximumFractionDigits: 1 })} %`;
    case 'bytes': return bytes(v) + rate;
    case 'millis': return v >= 1000 ? duration(v / 1000) : `${v.toLocaleString(locale(), { maximumFractionDigits: 1 })} ms`;
    case 'seconds': return m.counter ? `${v.toLocaleString(locale(), { maximumFractionDigits: 2 })} s/s` : duration(v);
    default: return v.toLocaleString(locale(), { maximumFractionDigits: m.counter || v % 1 ? 1 : 0 }) + rate;
  }
}

/** What the tile shows: the latest point (a rate for counters). */
function current(m: Metric): number | null {
  const h = history.value[m.key];
  return m.counter ? h?.[h.length - 1]?.v ?? null : m.value;
}

function ceiling(m: Metric): number | null {
  if (m.counter) return null;
  return m.max ?? (m.unit === 'percent' ? 100 : null);
}
/** Share of the ceiling (0–1), or null when unbounded. */
function share(m: Metric): number | null {
  const c = ceiling(m);
  const v = current(m);
  return c && v !== null ? Math.min(1, v / c) : null;
}
/** Status of a bounded metric. Always shown with a text label too. */
function level(m: Metric): 'ok' | 'warn' | 'crit' {
  const s = share(m);
  if (s === null || m.key === 'cache_hit') return 'ok';
  return s >= 0.9 ? 'crit' : s >= 0.75 ? 'warn' : 'ok';
}
const LEVEL_TEXT = { ok: '', warn: 'monitor:level.warn', crit: 'monitor:level.crit' };

const groups = computed(() => {
  const out: { name: string; metrics: Metric[] }[] = [];
  for (const m of snap.value?.metrics ?? []) {
    let g = out.find((x) => x.name === m.group);
    if (!g) out.push((g = { name: m.group, metrics: [] }));
    g.metrics.push(m);
  }
  return out;
});

// -- tables ------------------------------------------------------------------------------

const tableKey = ref<string>('');
const filter = ref('');
const tables = computed(() => snap.value?.tables ?? []);
watch(tables, (ts) => {
  if (!ts.some((t) => t.key === tableKey.value)) tableKey.value = ts[0]?.key ?? '';
});
const table = computed<MonitorTable | undefined>(() => tables.value.find((t) => t.key === tableKey.value));

function cell(v: unknown): string {
  if (v === null || v === undefined) return '';
  // Integers are often ids (PIDs, ports): no thousands separator.
  if (typeof v === 'number') return Number.isInteger(v) ? String(v) : v.toLocaleString(locale(), { maximumFractionDigits: 3 });
  if (typeof v === 'object') return JSON.stringify(v);
  return String(v);
}
const rows = computed(() => {
  const t = table.value;
  if (!t) return [];
  const q = filter.value.trim().toLowerCase();
  const all = t.rows.map((r) => r.map(cell));
  return q ? all.filter((r) => r.some((c) => c.toLowerCase().includes(q))) : all;
});

const ago = computed(() => {
  if (!lastAt.value) return '';
  const s = Math.max(0, Math.round((now.value - lastAt.value) / 1000));
  return s < 2 ? t('monitor:justNow') : t('monitor:secondsAgo', { s });
});
</script>

<template>
  <div class="mv">
    <header class="mv-head">
      <EngineIcon v-if="driver" :id="driver.id" :name="driver.name" :size="22" />
      <div class="mv-title">
        <strong>{{ conn?.name ?? $t('monitor:connection') }}</strong>
        <span class="mv-sub">{{ driver?.name }} · Monitor</span>
      </div>
      <div class="mv-spacer" />
      <span class="mv-ago" :class="{ busy: loading }">
        <el-icon v-if="loading" class="is-loading"><ei-loading /></el-icon>
        {{ paused ? $t('monitor:paused') : ago }}
      </span>
      <el-select v-model="every" size="small" style="width: 92px" :disabled="unsupported">
        <el-option v-for="s in INTERVALS" :key="s" :label="$t('monitor:every', { s })" :value="s" />
      </el-select>
      <el-button size="small" :disabled="unsupported" @click="paused = !paused">
        <el-icon><ei-video-play v-if="paused" /><ei-video-pause v-else /></el-icon>
        <span>{{ paused ? $t('monitor:resume') : $t('monitor:pause') }}</span>
      </el-button>
      <el-button size="small" :disabled="loading || unsupported" @click="poll">
        <el-icon><ei-refresh /></el-icon><span>{{ $t('monitor:refresh') }}</span>
      </el-button>
    </header>

    <div class="mv-body">
      <div v-if="unsupported" class="mv-empty">
        <el-icon :size="28"><ei-odometer /></el-icon>
        <p>{{ $t('monitor:unsupported', { engine: driver?.name ?? $t('monitor:thisEngine') }) }}</p>
      </div>

      <template v-else>
        <el-alert v-if="error" type="error" :title="error" :closable="false" show-icon class="mv-alert" />
        <div v-if="!snap && !error" class="mv-empty"><el-icon class="is-loading" :size="22"><ei-loading /></el-icon></div>

        <template v-if="snap">
          <section v-if="snap.info.length" class="mv-info">
            <div v-for="[k, v] in snap.info" :key="k" class="mv-info-item">
              <span class="mv-info-k">{{ tb(k) }}</span>
              <span class="mv-info-v" :title="tb(v)">{{ tb(v) }}</span>
            </div>
          </section>

          <div class="mv-groups">
          <section v-for="g in groups" :key="g.name" class="mv-group">
            <h3 class="nm-section-title">{{ tb(g.name) }}</h3>
            <div class="mv-tiles">
              <div v-for="m in g.metrics" :key="m.key" class="mv-tile" :class="level(m)">
                <div class="mv-tile-label" :title="tb(m.label)">{{ tb(m.label) }}</div>
                <div class="mv-tile-value">
                  <template v-if="current(m) !== null">{{ fmt(m, current(m)!) }}</template>
                  <span v-else class="nm-muted">{{ m.counter && !history[m.key] ? $t('monitor:measuring') : '—' }}</span>
                  <span v-if="level(m) !== 'ok'" class="mv-level">{{ $t(LEVEL_TEXT[level(m)]) }}</span>
                </div>
                <template v-if="share(m) !== null">
                  <div class="mv-gauge"><div class="mv-gauge-fill" :style="{ width: `${share(m)! * 100}%` }" /></div>
                  <div class="mv-tile-max">{{ $t('monitor:of', { max: fmt({ ...m, counter: false }, ceiling(m)!) }) }}</div>
                </template>
                <Sparkline
                  v-if="(history[m.key]?.length ?? 0) > 1"
                  :points="history[m.key]"
                  :max="ceiling(m)"
                  :format="(v: number) => fmt(m, v)"
                />
              </div>
            </div>
          </section>
          </div>

          <BlockingPanel v-if="canBlocking" :connection-id="tab.connectionId" :sessions="blocking" :can-kill="canKill" :kill-denied="killDenied" :error="blockingError" @killed="pollBlocking" />

          <section v-if="tables.length" class="mv-tables">
            <div class="mv-table-bar">
              <div class="mv-table-tabs" role="tablist">
                <button
                  v-for="t in tables"
                  :key="t.key"
                  role="tab"
                  :aria-selected="t.key === tableKey"
                  class="mv-table-tab"
                  :class="{ active: t.key === tableKey }"
                  @click="tableKey = t.key"
                >
                  {{ tb(t.title) }} <span class="mv-count">{{ t.rows.length }}</span>
                </button>
              </div>
              <el-input v-model="filter" size="small" :placeholder="$t('monitor:filter')" clearable style="width: 200px">
                <template #prefix><el-icon><ei-search /></el-icon></template>
              </el-input>
            </div>
            <div v-if="table" class="mv-table-wrap">
              <table class="mv-table">
                <thead>
                  <tr><th v-for="c in table.columns" :key="c">{{ tb(c) }}</th></tr>
                </thead>
                <tbody>
                  <tr v-for="(r, i) in rows" :key="i">
                    <td v-for="(c, j) in r" :key="j" :title="c.length > 60 ? c : undefined">{{ c }}</td>
                  </tr>
                  <tr v-if="!rows.length">
                    <td :colspan="table.columns.length" class="nm-muted">{{ filter ? $t('monitor:noMatch') : $t('monitor:noRows') }}</td>
                  </tr>
                </tbody>
              </table>
            </div>
          </section>

          <section v-if="snap.notes.length" class="mv-notes">
            <div v-for="(n, i) in snap.notes" :key="i" class="mv-note">
              <el-icon><ei-info-filled /></el-icon><span>{{ tb(n) }}</span>
            </div>
          </section>
        </template>
      </template>
    </div>
  </div>
</template>

<style scoped>
.mv { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--nm-bg); }
.mv-head {
  display: flex; align-items: center; gap: 10px; padding: 8px 14px;
  border-bottom: 1px solid var(--nm-border-soft); flex-shrink: 0;
}
.mv-head .el-button { margin: 0; }
.mv-head .el-button span { margin-left: 4px; }
.mv-title { display: flex; flex-direction: column; line-height: 1.2; min-width: 0; }
.mv-title strong { color: var(--nm-text-strong); font-weight: 600; }
.mv-sub { font-size: 11px; color: var(--nm-text-dim); }
.mv-spacer { flex: 1; }
.mv-ago { font-size: 11.5px; color: var(--nm-text-dim); display: inline-flex; align-items: center; gap: 4px; }

.mv-body { flex: 1; overflow: auto; padding: 14px 16px 24px; }
.mv-alert { margin-bottom: 12px; }
.mv-empty {
  display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 6px;
  padding: 60px 20px; color: var(--nm-text-dim); text-align: center;
}

.mv-info {
  display: grid; grid-template-columns: repeat(auto-fill, minmax(200px, 1fr)); gap: 6px 18px;
  padding: 10px 12px; margin-bottom: 16px; border: 1px solid var(--nm-border-soft); border-radius: 4px;
  background: var(--nm-bg-elev);
}
.mv-info-item { display: flex; flex-direction: column; min-width: 0; }
.mv-info-k { font-size: 11px; color: var(--nm-text-dim); }
.mv-info-v { color: var(--nm-text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

.mv-groups { display: flex; flex-wrap: wrap; gap: 14px 22px; margin-bottom: 16px; }
.mv-group { min-width: 0; }
.mv-group h3 { margin: 0 0 6px; }
.mv-tiles { display: flex; flex-wrap: wrap; gap: 8px; }
.mv-tile {
  width: 220px; box-sizing: border-box;
  display: flex; flex-direction: column; gap: 4px; padding: 10px 12px 8px;
  border: 1px solid var(--nm-border-soft); border-radius: 4px; background: var(--nm-bg-elev); min-width: 0;
}
.mv-tile.warn { border-color: color-mix(in srgb, var(--nm-warning) 55%, transparent); }
.mv-tile.crit { border-color: color-mix(in srgb, var(--nm-danger) 60%, transparent); }
.mv-tile-label { font-size: 11.5px; color: var(--nm-text-dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.mv-tile-value {
  font-size: 20px; font-weight: 600; color: var(--nm-text-strong); font-variant-numeric: tabular-nums;
  display: flex; align-items: baseline; gap: 8px;
}
.mv-level { font-size: 10.5px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.04em; }
.warn .mv-level { color: var(--nm-warning); }
.crit .mv-level { color: var(--nm-danger); }
.mv-gauge { height: 4px; border-radius: 2px; background: var(--ide-input); overflow: hidden; }
.mv-gauge-fill { height: 100%; border-radius: 2px; background: var(--nm-accent); transition: width 0.4s ease; }
.warn .mv-gauge-fill { background: var(--nm-warning); }
.crit .mv-gauge-fill { background: var(--nm-danger); }
.mv-tile-max { font-size: 11px; color: var(--nm-text-muted); }

.mv-tables { margin-top: 8px; border: 1px solid var(--nm-border-soft); border-radius: 4px; }
.mv-table-bar {
  display: flex; align-items: center; gap: 10px; padding: 0 8px 0 0;
  border-bottom: 1px solid var(--nm-border-soft); background: var(--nm-bg-elev);
}
.mv-table-tabs { display: flex; flex: 1; overflow-x: auto; }
.mv-table-tab {
  padding: 8px 12px; border: 0; border-bottom: 2px solid transparent; background: none;
  color: var(--nm-text-dim); cursor: pointer; font: inherit; white-space: nowrap;
}
.mv-table-tab:hover { color: var(--nm-text); }
.mv-table-tab.active { color: var(--nm-text-strong); border-bottom-color: var(--nm-accent); }
.mv-count { font-size: 10.5px; color: var(--nm-text-muted); margin-left: 3px; }
.mv-table-wrap { max-height: 420px; overflow: auto; }
.mv-table { width: 100%; border-collapse: collapse; font-size: 12px; }
.mv-table th {
  position: sticky; top: 0; z-index: 1; text-align: left; font-weight: 600; padding: 5px 10px;
  background: var(--nm-bg-elev); color: var(--nm-text-dim); border-bottom: 1px solid var(--nm-border);
  white-space: nowrap;
}
.mv-table td {
  padding: 4px 10px; border-bottom: 1px solid var(--nm-border-soft); color: var(--nm-text);
  max-width: 420px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  font-variant-numeric: tabular-nums;
}
.mv-table tbody tr:hover td { background: var(--ide-hover); }

.mv-notes { margin-top: 14px; display: flex; flex-direction: column; gap: 4px; }
.mv-note { display: flex; gap: 6px; align-items: flex-start; font-size: 12px; color: var(--nm-text-dim); }
.mv-note .el-icon { margin-top: 2px; color: var(--nm-info); flex-shrink: 0; }
</style>
