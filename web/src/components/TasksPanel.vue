<script setup lang="ts">
import { computed } from 'vue';
import { useTranslation } from 'i18next-vue';
import { locale } from '../i18n';
import { formatElapsed, formatEta, formatEtaValue, useTasksStore, type Task } from '../stores/tasks';
import { useConnectionsStore } from '../stores/connections';
import { useQuitGuard } from '../composables/quitGuard';

// The "Tareas" panel: every long operation that ran in this session, the
// running ones first. Mounted once by the status bar (always present), which
// is also where the quit guard is installed.

const tasks = useTasksStore();
const conns = useConnectionsStore();
const { t } = useTranslation();
useQuitGuard();

// Elapsed times and ETAs tick with the store's shared clock (it runs only
// while some task does).
const elapsed = (x: Task) => formatElapsed(tasks.elapsedOf(x));
/** The row's "≈ 4 min restantes" / "calculando…"; nothing without a total. */
const etaRow = (x: Task) => formatEta(tasks.etaOf(x));

function where(x: Task) {
  const name = x.connectionId ? conns.byId(x.connectionId)?.name : undefined;
  return [name, x.database].filter(Boolean).join(' · ');
}

function unitLabel(u?: string) {
  if (!u) return '';
  const key = `tasks:unit.${u}`;
  const s = t(key);
  return s === key || s === `unit.${u}` ? u : s;
}

/** "940 KB", "1.2 GB": byte counters read as sizes, not raw numbers. */
function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v.toLocaleString(locale(), { maximumFractionDigits: v < 10 ? 1 : 0 })} ${units[i]}`;
}

function progressText(x: Task) {
  const p = x.progress;
  const parts: string[] = [];
  const bytes = p.unit === 'bytes';
  const fmt = (n: number) => (bytes ? formatBytes(n) : n.toLocaleString(locale()));
  const unit = bytes ? '' : unitLabel(p.unit);
  if (p.done != null && p.total != null) parts.push(t('tasks:progress.of', { done: fmt(p.done), total: fmt(p.total), unit }).trim());
  else if (p.done != null) parts.push(t('tasks:progress.count', { done: fmt(p.done), unit }).trim());
  if (p.phase) parts.push(p.phase);
  return parts.join(' · ');
}

const percent = (x: Task) =>
  x.progress.total && x.progress.done != null ? Math.min(100, Math.round((x.progress.done / x.progress.total) * 100)) : null;

const sorted = computed(() => [...tasks.tasks].sort((a, b) => Number(b.state === 'running') - Number(a.state === 'running') || b.startedAt - a.startedAt));
const hasFinished = computed(() => tasks.tasks.some((x) => x.state !== 'running'));

const detail = computed(() => (tasks.detailId ? tasks.byId(tasks.detailId) ?? null : null));
const detailResult = computed(() => {
  const r = detail.value?.result;
  if (r == null) return '';
  if (typeof r === 'string') return r;
  try { return JSON.stringify(r, null, 2); } catch { return String(r); }
});
const timeOf = (ms: number) => new Date(ms).toLocaleTimeString(locale());

/** The detail's "Restante estimado": the value, "calculando…" or "sin estimación". */
const detailEta = computed(() => {
  const x = detail.value;
  if (!x || x.state !== 'running') return null;
  const e = tasks.etaOf(x);
  if (e.status === 'none') return { text: t('tasks:eta.none'), title: '' };
  if (e.status === 'calculating') return { text: t('tasks:eta.calculating'), title: '' };
  const value = formatEtaValue(e.remainingMs);
  return {
    text: e.scope === 'phase' ? t('tasks:eta.phase', { eta: value }) : value,
    title: e.confidence === 'low' ? t('tasks:eta.low') : '',
  };
});
</script>

<template>
  <el-drawer
    v-model="tasks.panelOpen" :title="$t('tasks:panel.title')" direction="rtl" size="440px"
    :modal="false" append-to-body class="tasks-drawer"
  >
    <div v-if="!tasks.tasks.length" class="tp-empty">{{ $t('tasks:panel.empty') }}</div>
    <ul v-else class="tp-list">
      <li v-for="x in sorted" :key="x.id" class="tp-item" :class="`is-${x.state}`">
        <div class="tp-head">
          <el-icon v-if="x.state === 'running'" class="is-loading tp-icon"><ei-refresh /></el-icon>
          <el-icon v-else-if="x.state === 'done'" class="tp-icon tp-ok"><ei-circle-check /></el-icon>
          <el-icon v-else-if="x.state === 'error'" class="tp-icon tp-err"><ei-circle-close /></el-icon>
          <el-icon v-else class="tp-icon tp-warn"><ei-remove /></el-icon>
          <span class="tp-title" :title="x.title">{{ x.title }}</span>
          <span class="tp-time">{{ elapsed(x) }}<template v-if="x.state === 'running' && etaRow(x)"> · {{ etaRow(x) }}</template></span>
        </div>
        <div v-if="where(x)" class="tp-where">{{ where(x) }}</div>
        <div class="tp-meta">
          <span class="tp-state">{{ x.cancelling ? $t('tasks:panel.cancelling') : $t(`tasks:state.${x.state}`) }}</span>
          <span v-if="x.state === 'running' && progressText(x)">· {{ progressText(x) }}</span>
          <span v-else-if="x.summary">· {{ x.summary }}</span>
        </div>
        <el-progress
          v-if="x.state === 'running' && percent(x) != null" :percentage="percent(x)!" :show-text="false" :stroke-width="3"
          class="tp-bar"
        />
        <div v-if="x.state === 'error' && x.error" class="tp-error" :title="x.error">{{ x.error }}</div>
        <div class="tp-actions">
          <el-button v-if="x.state === 'running' && x.canCancel" size="small" :disabled="x.cancelling" @click="tasks.cancel(x.id)">
            {{ $t('tasks:panel.cancel') }}
          </el-button>
          <el-button size="small" @click="tasks.reopen(x.id)">{{ $t('tasks:panel.detail') }}</el-button>
          <el-button v-if="x.state !== 'running'" size="small" text @click="tasks.remove(x.id)">{{ $t('tasks:panel.remove') }}</el-button>
        </div>
      </li>
    </ul>
    <template v-if="hasFinished" #footer>
      <el-button size="small" @click="tasks.clearFinished()">{{ $t('tasks:panel.clearFinished') }}</el-button>
    </template>
  </el-drawer>

  <el-dialog
    :model-value="!!detail" :title="detail?.title ?? $t('tasks:detail.title')" width="640px" append-to-body
    @update:model-value="(v: boolean) => { if (!v) tasks.detailId = null; }"
  >
    <template v-if="detail">
      <div class="td-row">
        <span>{{ where(detail) }}</span>
        <span>{{ $t('tasks:detail.started') }}: {{ timeOf(detail.startedAt) }}</span>
        <span v-if="detail.endedAt">{{ $t('tasks:detail.ended') }}: {{ timeOf(detail.endedAt) }}</span>
        <span>{{ detail.endedAt ? $t('tasks:detail.duration') : $t('tasks:detail.elapsed') }}: {{ elapsed(detail) }}</span>
        <span v-if="detailEta" :title="detailEta.title">{{ $t('tasks:detail.remaining') }}: {{ detailEta.text }}</span>
        <span>{{ detail.cancelling ? $t('tasks:panel.cancelling') : $t(`tasks:state.${detail.state}`) }}</span>
      </div>
      <div v-if="detail.state === 'running' && progressText(detail)" class="td-row">{{ progressText(detail) }}</div>
      <template v-if="detail.error">
        <h4>{{ $t('tasks:detail.error') }}</h4>
        <pre class="td-pre td-err">{{ detail.error }}</pre>
      </template>
      <template v-if="detailResult || detail.summary">
        <h4>{{ $t('tasks:detail.result') }}</h4>
        <div v-if="detail.summary">{{ detail.summary }}</div>
        <pre v-if="detailResult" class="td-pre">{{ detailResult }}</pre>
      </template>
      <h4>{{ $t('tasks:detail.messages') }}</h4>
      <div v-if="!detail.messages.length" class="tp-empty">{{ $t('tasks:detail.noMessages') }}</div>
      <div v-else class="td-log">
        <div v-for="(m, i) in detail.messages" :key="i" :class="`td-${m.level}`">
          <span class="td-at">{{ timeOf(m.at) }}</span> {{ m.text }}
        </div>
      </div>
    </template>
    <template #footer>
      <el-button v-if="detail?.state === 'running' && detail.canCancel" :disabled="detail.cancelling" @click="tasks.cancel(detail.id)">
        {{ $t('tasks:panel.cancel') }}
      </el-button>
      <el-button @click="tasks.detailId = null">{{ $t('common:close') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.tp-empty { color: var(--nm-text-dim); padding: 8px 0; }
.tp-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 8px; }
.tp-item { border: 1px solid var(--nm-border); border-radius: var(--nm-radius); padding: 8px 10px; background: var(--nm-bg-elev); }
.tp-head { display: flex; align-items: center; gap: 6px; }
.tp-icon { flex: none; }
.tp-ok { color: var(--nm-success); }
.tp-err { color: var(--nm-danger); }
.tp-warn { color: var(--nm-warning); }
.tp-title { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--nm-text-strong); }
.tp-time { flex: none; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; }
.tp-where, .tp-meta { color: var(--nm-text-dim); font-size: 12px; margin-top: 2px; }
.tp-meta { display: flex; gap: 4px; flex-wrap: wrap; }
.tp-bar { margin-top: 6px; }
.tp-error { color: var(--nm-danger); font-size: 12px; margin-top: 4px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.tp-actions { display: flex; gap: 4px; margin-top: 6px; }
.tp-actions .el-button + .el-button { margin-left: 0; }
.td-row { display: flex; flex-wrap: wrap; gap: 4px 16px; color: var(--nm-text-dim); margin-bottom: 6px; }
.td-pre { font-family: var(--nm-mono); font-size: 12px; white-space: pre-wrap; word-break: break-word; max-height: 200px; overflow: auto; background: var(--nm-bg); border: 1px solid var(--nm-border-soft); padding: 6px; margin: 0; user-select: text; }
.td-err { color: var(--nm-danger); }
.td-log { font-family: var(--nm-mono); font-size: 12px; max-height: 260px; overflow: auto; background: var(--nm-bg); border: 1px solid var(--nm-border-soft); padding: 6px; user-select: text; }
.td-at { color: var(--nm-text-muted); }
.td-warn { color: var(--nm-warning); }
.td-error { color: var(--nm-danger); }
h4 { margin: 12px 0 6px; font-weight: 600; color: var(--nm-text-strong); }
</style>
