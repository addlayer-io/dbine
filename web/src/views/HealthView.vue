<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { tb } from '../i18n/backend';
import { newQuery } from '../composables/actions';
import { useConnectionsStore } from '../stores/connections';
import type { HealthTab } from '../stores/tabs';

// "Chequeo de salud" of a database (docs/chequeo-de-salud.md): findings by
// severity and category, each with what it means, the objects involved and
// a fix script that only opens in a query (never runs by itself).

type Severity = 'ok' | 'info' | 'warning' | 'critical';
interface Check { id: string; category: string; title: string; severity: Severity; detail: string; objects: string[]; fix: string | null }
interface Report { checks: Check[]; checked_at: string; skipped: string[] }

const props = defineProps<{ tab: HealthTab }>();
const { t } = useTranslation();
const conns = useConnectionsStore();

const report = ref<Report | null>(null);
const error = ref<string | null>(null);
const running = ref(false);
const showOk = ref(false);
const open = ref<Set<string>>(new Set());
let runId = '';

const SEVERITIES: Severity[] = ['critical', 'warning', 'info', 'ok'];
const counts = computed(() => Object.fromEntries(SEVERITIES.map((s) => [s, report.value?.checks.filter((c) => c.severity === s).length ?? 0])));
const groups = computed(() => {
  const out = new Map<string, Check[]>();
  for (const c of report.value?.checks ?? []) {
    if (c.severity === 'ok' && !showOk.value) continue;
    (out.get(c.category) ?? out.set(c.category, []).get(c.category)!).push(c);
  }
  return [...out.entries()];
});

async function run() {
  if (running.value) return;
  if (!(await conns.ensureConnected(props.tab.connectionId))) return;
  running.value = true;
  error.value = null;
  runId = `${props.tab.id}-${Date.now()}`;
  try {
    report.value = await invoke<Report>('database_health', { args: { connection_id: props.tab.connectionId, database: props.tab.database, run_id: runId } });
    open.value = new Set(report.value.checks.filter((c) => c.severity === 'critical').map((c) => c.id));
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    running.value = false;
  }
}
onMounted(run);
onUnmounted(() => {
  if (running.value) invoke('cancel_query', { args: { session_id: `health:${runId}` } }).catch(() => {});
});

function toggle(id: string) {
  const s = new Set(open.value);
  if (s.has(id)) s.delete(id);
  else s.add(id);
  open.value = s;
}

function openFix(c: Check) {
  if (c.fix) newQuery(props.tab.connectionId, props.tab.database, c.fix, t('health:fixName', { title: tb(c.title).slice(0, 40) }));
}
</script>

<template>
  <div class="hv">
    <header class="hv-head">
      <strong>{{ $t('health:title', { name: tab.database }) }}</strong>
      <span v-if="report" class="nm-muted hv-when">{{ $t('health:checkedAt', { at: report.checked_at }) }}</span>
      <div class="hv-spacer" />
      <el-checkbox v-model="showOk" size="small">{{ $t('health:showOk') }}</el-checkbox>
      <el-button size="small" :loading="running" @click="run">{{ $t('health:run') }}</el-button>
    </header>

    <div v-if="report" class="hv-summary">
      <div v-for="s in SEVERITIES" :key="s" class="hv-count" :class="s">
        <span class="hv-n">{{ counts[s] }}</span>
        <span>{{ $t(`health:severity.${s}`) }}</span>
      </div>
    </div>

    <el-alert v-if="error" type="error" :title="error" :closable="false" show-icon class="hv-alert" />
    <div v-if="running && !report" class="hv-empty"><el-icon class="is-loading" :size="22"><ei-loading /></el-icon><span>{{ $t('health:running') }}</span></div>

    <div class="hv-body">
      <section v-for="[cat, list] in groups" :key="cat" class="hv-group">
        <h3 class="nm-section-title">{{ tb(cat) }}</h3>
        <div v-for="c in list" :key="c.id" class="hv-item" :class="c.severity">
          <button class="hv-row" @click="toggle(c.id)">
            <span class="hv-dot" />
            <span class="hv-title">{{ tb(c.title) }}</span>
            <span v-if="c.objects.length" class="hv-badge">{{ c.objects.length }}</span>
            <el-icon class="hv-chev" :class="{ open: open.has(c.id) }"><ei-arrow-right /></el-icon>
          </button>
          <div v-if="open.has(c.id)" class="hv-detail">
            <p v-if="c.detail">{{ tb(c.detail) }}</p>
            <ul v-if="c.objects.length" class="hv-objects">
              <li v-for="o in c.objects.slice(0, 200)" :key="o">{{ o }}</li>
              <li v-if="c.objects.length > 200" class="nm-muted">{{ $t('health:more', { count: c.objects.length - 200 }) }}</li>
            </ul>
            <div v-if="c.fix" class="hv-fix">
              <pre>{{ c.fix }}</pre>
              <el-button size="small" @click="openFix(c)">{{ $t('health:openFix') }}</el-button>
            </div>
          </div>
        </div>
      </section>
      <p v-if="report && !groups.length" class="nm-muted hv-allok">{{ $t('health:allOk') }}</p>
      <div v-if="report?.skipped.length" class="hv-skipped">
        <div class="nm-muted">{{ $t('health:skipped') }}</div>
        <div v-for="s in report.skipped" :key="s" class="nm-muted">· {{ tb(s) }}</div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.hv { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--nm-bg); }
.hv-head { display: flex; align-items: center; gap: 10px; padding: 10px 16px; border-bottom: 1px solid var(--nm-border-soft); }
.hv-head .el-button { margin: 0; }
.hv-head strong { color: var(--nm-text-strong); }
.hv-when { font-size: 11.5px; }
.hv-spacer { flex: 1; }
.hv-summary { display: flex; gap: 10px; padding: 12px 16px 4px; }
.hv-count { display: flex; align-items: baseline; gap: 6px; padding: 6px 12px; border: 1px solid var(--nm-border-soft); border-radius: 4px; font-size: 12px; color: var(--nm-text-dim); }
.hv-n { font-size: 18px; font-weight: 600; color: var(--nm-text-strong); }
.hv-count.critical .hv-n { color: var(--nm-danger); }
.hv-count.warning .hv-n { color: var(--nm-warning); }
.hv-count.info .hv-n { color: var(--nm-info); }
.hv-count.ok .hv-n { color: var(--nm-success); }
.hv-alert { margin: 8px 16px; width: auto; }
.hv-empty { display: flex; align-items: center; gap: 8px; justify-content: center; padding: 40px; color: var(--nm-text-dim); }
.hv-body { flex: 1; overflow: auto; padding: 8px 16px 24px; }
.hv-group { margin-bottom: 14px; }
.hv-group h3 { margin: 6px 0; }
.hv-item { border: 1px solid var(--nm-border-soft); border-radius: 4px; margin-bottom: 6px; border-left-width: 3px; }
.hv-item.critical { border-left-color: var(--nm-danger); }
.hv-item.warning { border-left-color: var(--nm-warning); }
.hv-item.info { border-left-color: var(--nm-info); }
.hv-item.ok { border-left-color: var(--nm-success); }
.hv-row { display: flex; align-items: center; gap: 8px; width: 100%; padding: 7px 10px; border: 0; background: none; cursor: pointer; font: inherit; color: var(--nm-text); text-align: left; }
.hv-row:hover { background: var(--ide-hover); }
.hv-title { flex: 1; }
.hv-badge { font-size: 10.5px; padding: 0 6px; border-radius: 8px; background: var(--ide-hover); color: var(--nm-text-dim); }
.hv-chev { transition: transform 0.15s; color: var(--nm-text-dim); }
.hv-chev.open { transform: rotate(90deg); }
.hv-detail { padding: 0 12px 10px 12px; font-size: 12.5px; color: var(--nm-text); }
.hv-detail p { margin: 0 0 6px; color: var(--nm-text-dim); line-height: 1.45; }
.hv-objects { margin: 0 0 8px; padding-left: 18px; max-height: 180px; overflow: auto; font-family: var(--nm-mono); font-size: 11.5px; }
.hv-fix pre {
  margin: 0 0 6px; padding: 8px 10px; max-height: 160px; overflow: auto; white-space: pre-wrap;
  font-family: var(--nm-mono); font-size: 12px; background: var(--nm-bg-elev); border: 1px solid var(--nm-border-soft); border-radius: 3px;
}
.hv-allok { font-size: 13px; padding: 12px 0; }
.hv-skipped { margin-top: 14px; font-size: 11.5px; }
</style>
