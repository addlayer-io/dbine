<script setup lang="ts">
import { computed, nextTick, onUnmounted, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import type { ProfiledStatement, ProfilerStarted } from '../api/types';
import CodeEditor from '../components/CodeEditor.vue';
import ContextMenu, { type MenuItem } from '../components/ContextMenu.vue';
import EngineIcon from '../components/EngineIcon.vue';
import { newQuery } from '../composables/actions';
import { matchesLocal, parseFilter, type ColumnFilter, type ColumnKind } from '../composables/gridFilter';
import { readJson, writeJson } from '../stores/storage';
import { useConnectionsStore } from '../stores/connections';
import { profilerAutostart, type ProfilerTab } from '../stores/tabs';

// The profiler: every statement run against the database, live (SQL Server
// Profiler style). The driver's session keeps what arrives; this tab polls
// it every second while running. Stopping puts back any server setting the
// driver switched on to capture.

const props = defineProps<{ tab: ProfilerTab }>();
const conns = useConnectionsStore();
const { t } = useTranslation();
/** A number with the current language's separators. */
const num = (n: number) => n.toLocaleString(locale());

/** Rows kept in the tab; older ones are dropped. */
const MAX_ROWS = 5000;
const EVERY_MS = 1000;

const conn = computed(() => conns.byId(props.tab.connectionId));
const driver = computed(() => conns.driverOf(props.tab.connectionId));
/** Why the login can't watch other clients' statements ('' when it can). */
const noPermission = computed(() => {
  const missing = conns.denied(props.tab.connectionId, props.tab.database, 'profiler');
  return missing ? t('common:noPermission', { missing }) : '';
});
const unsupported = computed(() => !!driver.value && !driver.value.supports_profiler);

const started = ref<ProfilerStarted | null>(null);
const running = ref(false);
const starting = ref(false);
const error = ref<string | null>(null);
const rows = ref<(ProfiledStatement & { n: number })[]>([]);
const dropped = ref(0);
const selected = ref<number | null>(null);
/** Statements the filters left out since the capture began. */
const skipped = ref(0);
const follow = ref(true);
const list = ref<HTMLElement | null>(null);
let seq = 0;
let timer: ReturnType<typeof setTimeout> | undefined;

async function start() {
  if (starting.value || running.value) return;
  starting.value = true;
  error.value = null;
  try {
    if (!(await conns.ensureConnected(props.tab.connectionId))) {
      error.value = t('profiler:couldNotConnect');
      return;
    }
    await conns.loadPermissions(props.tab.connectionId, props.tab.database);
    if (noPermission.value) return;
    started.value = await api.profilerStart(props.tab.id, props.tab.connectionId, props.tab.database);
    running.value = true;
    schedule();
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    starting.value = false;
  }
}

async function stop() {
  clearTimeout(timer);
  if (!running.value) return;
  running.value = false;
  try {
    await api.profilerStop(props.tab.id);
  } catch (e) {
    error.value = errorMessage(e);
  }
}

/** A sampling driver spends most of a second looking within each poll:
 *  ask again right away. */
function schedule() {
  clearTimeout(timer);
  if (running.value) timer = setTimeout(poll, started.value?.mode === 'sampled' ? 50 : EVERY_MS);
}

async function poll() {
  try {
    const all = await api.profilerPoll(props.tab.id);
    for (const r of all) remember(r);
    // Only what passes the filters is kept.
    const got = all.filter(passes);
    skipped.value += all.length - got.length;
    if (got.length) {
      rows.value.push(...got.map((r) => ({ ...r, n: ++seq })));
      const over = rows.value.length - MAX_ROWS;
      if (over > 0) {
        rows.value.splice(0, over);
        dropped.value += over;
      }
      if (follow.value && !sort.value) nextTick(() => list.value?.scrollTo({ top: list.value.scrollHeight }));
    }
    error.value = null;
    schedule();
  } catch (e) {
    // The session died (connection lost): stop; the user can start again.
    error.value = errorMessage(e);
    running.value = false;
  }
}

function clear() {
  rows.value = [];
  dropped.value = 0;
  skipped.value = 0;
  selected.value = null;
}

/** Follow new rows only while the list is scrolled to the bottom. */
function onScroll() {
  const el = list.value;
  if (el) follow.value = el.scrollHeight - el.scrollTop - el.clientHeight < 30;
}

if (profilerAutostart.delete(props.tab.id)) start();
onUnmounted(() => {
  clearTimeout(timer);
  if (running.value) api.profilerStop(props.tab.id).catch(() => {});
});

// -- view ---------------------------------------------------------------------------------

// -- filters (the row under the headers, as in the results grid) ----------------------

type Key = 'duration_ms' | 'text' | 'database' | 'user' | 'client' | 'application' | 'rows' | Metric;
/** Figures only some engines report: their columns show once a statement has one. */
type Metric = 'cpu_ms' | 'reads' | 'writes';
const METRICS: Metric[] = ['cpu_ms', 'reads', 'writes'];
/** `hint` is a translation key (the filter's placeholder). */
const COLUMNS: { key: Key; kind: ColumnKind; hint: string }[] = [
  { key: 'duration_ms', kind: 'number', hint: 'profiler:hint.duration' },
  { key: 'text', kind: 'text', hint: 'profiler:hint.text' },
  { key: 'database', kind: 'text', hint: 'profiler:hint.database' },
  { key: 'user', kind: 'text', hint: '' },
  { key: 'client', kind: 'text', hint: '' },
  { key: 'application', kind: 'text', hint: '' },
  { key: 'rows', kind: 'number', hint: '> 1000 · = 0' },
  { key: 'cpu_ms', kind: 'number', hint: 'profiler:hint.duration' },
  { key: 'reads', kind: 'number', hint: '> 1000 · = 0' },
  { key: 'writes', kind: 'number', hint: '> 1000 · = 0' },
];
const KEYS = COLUMNS.map((c) => c.key);
const filterText = ref<Record<Key, string>>({ duration_ms: '', text: '', database: '', user: '', client: '', application: '', rows: '', cpu_ms: '', reads: '', writes: '' });

/** User, client and application are picked from the values seen so far
 *  (also those of statements the filters left out), several at a time. */
type Pick = 'user' | 'client' | 'application';
const PICKS: Pick[] = ['user', 'client', 'application'];
const isPick = (k: Key): k is Pick => (PICKS as string[]).includes(k);
const picked = ref<Record<Pick, string[]>>({ user: [], client: [], application: [] });
const seen = ref<Record<Pick, string[]>>({ user: [], client: [], application: [] });
function remember(r: ProfiledStatement) {
  for (const k of PICKS) {
    const v = r[k];
    if (v && !seen.value[k].includes(v)) {
      seen.value[k].push(v);
      seen.value[k].sort((a, b) => a.localeCompare(b));
    }
  }
}

/** Durations are in milliseconds; `1s` / `2,5 s` are accepted too. */
function normalize(key: Key, text: string) {
  if (key !== 'duration_ms' && key !== 'cpu_ms') return text;
  return text
    .replace(/(\d+(?:[.,]\d+)?)\s*s\b/gi, (_, n: string) => String(parseFloat(n.replace(',', '.')) * 1000))
    .replace(/(\d+(?:[.,]\d+)?)\s*ms\b/gi, (_, n: string) => n.replace(',', '.'));
}
const filters = computed(() =>
  COLUMNS.flatMap((c): ColumnFilter[] => {
    if (isPick(c.key)) {
      const values = picked.value[c.key];
      return values.length ? [{ column: c.key, op: 'in', values }] : [];
    }
    return parseFilter(c.key, c.kind, normalize(c.key, filterText.value[c.key]));
  }),
);
const filtering = computed(() => filters.value.length > 0 || !!sameAs.value);
function passes(r: ProfiledStatement) {
  if (sameAs.value && fingerprint(r.text) !== sameAs.value.print) return false;
  return !filters.value.length || matchesLocal(KEYS.map((k) => r[k] ?? null), KEYS, filters.value);
}
function clearFilters() {
  for (const k of KEYS) filterText.value[k] = '';
  for (const k of PICKS) picked.value[k] = [];
  sameAs.value = null;
}

// -- "the same statement": its text with the values taken out --------------------------

/** A statement's shape: comments, literals and numbers out, spacing and case
 *  normalized, `IN (…)` lists collapsed. Runs of the same query with other
 *  values share it. */
const prints = new Map<string, string>();
function fingerprint(text: string) {
  let p = prints.get(text);
  if (p === undefined) {
    p = text
      .replace(/--[^\n]*/g, ' ')
      .replace(/\/\*[\s\S]*?\*\//g, ' ')
      .replace(/N?'(?:[^']|'')*'/g, '?')
      .replace(/\b0x[0-9a-f]+\b/gi, '?')
      .replace(/(?<![\w@$.])-?\d+(?:\.\d+)?(?:e[+-]?\d+)?\b/gi, '?')
      .replace(/\s+/g, ' ')
      .replace(/\(\s*\?(?:\s*,\s*\?)*\s*\)/g, '(?)')
      .trim()
      .toLowerCase();
    if (prints.size > 20000) prints.clear();
    prints.set(text, p);
  }
  return p;
}
const sameAs = ref<{ print: string; label: string } | null>(null);
function filterSame(r: ProfiledStatement) {
  sameAs.value = { print: fingerprint(r.text), label: oneLine(r.text).slice(0, 80) };
}

interface Spread { avg: number; min: number; max: number; p95: number }
function spread(values: (number | null)[]): Spread | null {
  const d = values.filter((v): v is number => v !== null).sort((a, b) => a - b);
  if (!d.length) return null;
  return { avg: d.reduce((a, b) => a + b, 0) / d.length, min: d[0], max: d[d.length - 1], p95: d[Math.min(d.length - 1, Math.floor(d.length * 0.95))] };
}
/** Duration, CPU, reads and writes of what's shown when looking at one statement. */
const stats = computed(() => {
  if (!sameAs.value) return null;
  const list = shown.value;
  return {
    n: list.length,
    figures: (['duration_ms', ...METRICS] as const)
      .map((key) => ({ key, s: spread(list.map((r) => r[key] ?? null)) }))
      .filter((f): f is { key: 'duration_ms' | Metric; s: Spread } => f.s !== null),
  };
});
/** A figure as its column shows it. */
function figure(key: 'duration_ms' | Metric, v: number | null | undefined) {
  if (v === null || v === undefined) return '';
  return key === 'duration_ms' || key === 'cpu_ms' ? duration(v) : num(Math.round(v));
}

// -- right click on a row ------------------------------------------------------------

const menu = ref<{ x: number; y: number; items: MenuItem[] } | null>(null);
function openMenu(e: MouseEvent, r: ProfiledStatement & { n: number }) {
  selected.value = r.n;
  const pick = (k: Pick): MenuItem[] => {
    const v = r[k];
    return v ? [{ label: t(`profiler:menu.filter.${k}`, { value: v.length > 40 ? `${v.slice(0, 40)}…` : v }), action: () => (picked.value[k] = [v]) }] : [];
  };
  menu.value = {
    x: e.clientX,
    y: e.clientY,
    items: [
      { label: t('profiler:menu.filterSame'), action: () => filterSame(r) },
      ...pick('user'),
      ...pick('client'),
      ...pick('application'),
      ...(manyDbs.value && r.database ? [{ label: t('profiler:menu.filter.database', { value: r.database }), action: () => (filterText.value.database = `=${r.database}`) }] : []),
      { label: t('profiler:menu.copy'), divided: true, action: () => copy(r.text) },
      { label: t('profiler:openInQuery'), action: () => openInQuery(r) },
    ],
  };
}

// -- sorting: a header click sorts up, then down, then back to arrival order ------------

type SortKey = 'time' | Key;
const sort = ref<{ key: SortKey; desc: boolean } | null>(null);
function sortBy(key: SortKey) {
  const s = sort.value;
  sort.value = s?.key !== key ? { key, desc: false } : !s.desc ? { key, desc: true } : null;
}
function arrow(key: SortKey) {
  return sort.value?.key === key ? (sort.value.desc ? '↓' : '↑') : '';
}
/** Empty values go last either way. */
function compare(a: ProfiledStatement, b: ProfiledStatement, key: SortKey, desc: boolean) {
  const x = a[key];
  const y = b[key];
  if (x === null || x === '') return y === null || y === '' ? 0 : 1;
  if (y === null || y === '') return -1;
  const c = typeof x === 'number' && typeof y === 'number' ? x - y : String(x).localeCompare(String(y));
  return desc ? -c : c;
}

// -- visible columns (chosen from the header's menu; remembered) ------------------------

/** Column titles, in the current language. */
const COLUMN_KEY: Record<SortKey, string> = {
  time: 'time', duration_ms: 'duration', text: 'query', database: 'database',
  user: 'user', client: 'client', application: 'application', rows: 'rows',
  cpu_ms: 'cpu', reads: 'reads', writes: 'writes',
};
/** Engines report CPU, reads and writes or not: their columns show once one came. */
const reported = computed(() => {
  const has: Record<Metric, boolean> = { cpu_ms: false, reads: false, writes: false };
  for (const r of rows.value) for (const k of METRICS) if (r[k] != null) has[k] = true;
  return has;
});
/** What reads / writes count on this engine ("páginas", "filas"…), for the header's tooltip. */
function unitHint(key: SortKey) {
  const unit = key === 'reads' ? started.value?.reads_unit : key === 'writes' ? started.value?.writes_unit : key === 'cpu_ms' ? 'ms' : null;
  return unit ? t('profiler:unitHint', { unit: tb(unit) }) : '';
}
const ALL_COLUMNS = computed(() =>
  (Object.keys(COLUMN_KEY) as SortKey[])
    .filter((key) => !(METRICS as string[]).includes(key) || reported.value[key as Metric])
    .map((key) => ({ key, label: t(`profiler:column.${COLUMN_KEY[key]}`) })),
);
const hidden = ref<string[]>(readJson('dbine.profilerHiddenColumns', []));
watch(hidden, (h) => writeJson('dbine.profilerHiddenColumns', h), { deep: true });
/** Base only shows when statements come from several databases. */
function vis(key: SortKey) {
  if ((METRICS as string[]).includes(key) && !reported.value[key as Metric]) return false;
  return !hidden.value.includes(key) && (key !== 'database' || manyDbs.value);
}
const visibleCount = computed(() => ALL_COLUMNS.value.filter((c) => vis(c.key)).length);
function columnsMenu(e: MouseEvent) {
  menu.value = {
    x: e.clientX,
    y: e.clientY,
    items: [
      { label: t('profiler:columns'), header: true },
      ...ALL_COLUMNS.value.map((c) => ({
        label: c.label,
        checked: !hidden.value.includes(c.key),
        // The statement itself always shows.
        disabled: c.key === 'text',
        action: () => {
          hidden.value = hidden.value.includes(c.key) ? hidden.value.filter((k) => k !== c.key) : [...hidden.value, c.key];
        },
      })),
      { label: t('profiler:showAll'), divided: true, disabled: !hidden.value.length, action: () => (hidden.value = []) },
    ],
  };
}

const shown = computed(() => {
  const list = filtering.value ? rows.value.filter(passes) : rows.value;
  const s = sort.value;
  return s ? [...list].sort((a, b) => compare(a, b, s.key, s.desc) || a.n - b.n) : list;
});
const current = computed(() => rows.value.find((r) => r.n === selected.value) ?? null);
/** A database column only when statements come from several. */
const manyDbs = computed(() => new Set(rows.value.map((r) => r.database ?? '')).size > 1);

/** Local time of day (the driver sends UTC). */
function clock(t: string) {
  const d = new Date(`${t.replace(' ', 'T')}Z`);
  if (Number.isNaN(d.getTime())) return t;
  const ms = String(d.getMilliseconds()).padStart(3, '0');
  return `${d.toLocaleTimeString(locale(), { hour12: false })}.${ms}`;
}
function duration(ms: number | null) {
  if (ms === null) return '';
  if (ms < 1) return '<1 ms';
  if (ms < 1000) return `${Math.round(ms)} ms`;
  return `${(ms / 1000).toLocaleString(locale(), { maximumFractionDigits: ms < 10000 ? 2 : 1 })} s`;
}
/** How far a duration is above the usual latency (a colour hint on the time only). */
function slowness(ms: number | null) {
  if (ms === null) return '';
  const r = Math.round(ms);
  if (r <= 50) return '';
  if (r <= 200) return 'slow-1';
  if (r <= 500) return 'slow-2';
  return 'slow-3';
}
const oneLine = (s: string) => s.replace(/\s+/g, ' ').trim();

function copy(text: string) {
  navigator.clipboard.writeText(text).then(() => ElMessage.success({ message: t('common:copied'), duration: 1200 }));
}
function openInQuery(r: ProfiledStatement) {
  newQuery(props.tab.connectionId, r.database ?? props.tab.database, r.text);
}

// -- detail pane: its height is dragged from the sash above it -----------------------------

const body = ref<HTMLElement | null>(null);
const detailHeight = ref(readJson('dbine.profilerDetail', 220));
function drag(e: PointerEvent) {
  const box = body.value!.getBoundingClientRect();
  const move = (ev: PointerEvent) => {
    detailHeight.value = Math.round(Math.min(box.height - 80, Math.max(90, box.bottom - ev.clientY)));
  };
  const up = () => {
    window.removeEventListener('pointermove', move);
    window.removeEventListener('pointerup', up);
    writeJson('dbine.profilerDetail', detailHeight.value);
  };
  window.addEventListener('pointermove', move);
  window.addEventListener('pointerup', up);
  e.preventDefault();
}

const status = computed(() => {
  if (starting.value) return t('profiler:status.starting');
  if (running.value) return t('profiler:status.capturing');
  return started.value ? t('profiler:status.stopped') : t('profiler:status.notStarted');
});
</script>

<template>
  <div class="pv">
    <header class="pv-head">
      <EngineIcon v-if="driver" :id="driver.id" :name="driver.name" :size="22" />
      <div class="pv-title">
        <strong>{{ tab.database || conn?.name || $t('profiler:connection') }}</strong>
        <span class="pv-sub">{{ conn?.name }} · {{ driver?.name }} · Profiler</span>
      </div>
      <span v-if="started" class="pv-mode" :class="started.mode" :title="$t('profiler:source', { source: tb(started.source) })">
        {{ started.mode === 'complete' ? $t('profiler:mode.complete') : $t('profiler:mode.sampled') }}
      </span>
      <div class="pv-spacer" />
      <span class="pv-status" :class="{ on: running }">
        <el-icon v-if="starting" class="is-loading"><ei-loading /></el-icon>
        <span v-else class="pv-dot" />
        {{ status }}
      </span>
      <el-button size="small" text :title="$t('profiler:columnsHint')" @click="columnsMenu">
        <el-icon><ei-grid /></el-icon><span>{{ $t('profiler:columns') }}</span>
      </el-button>
      <el-button v-if="filtering" size="small" text @click="clearFilters">
        <el-icon><ei-filter /></el-icon><span>{{ $t('profiler:clearFilters') }}</span>
      </el-button>
      <el-button v-if="running" size="small" @click="stop">
        <el-icon><ei-video-pause /></el-icon><span>{{ $t('common:stop') }}</span>
      </el-button>
      <span v-else :title="noPermission">
        <el-button size="small" type="primary" :disabled="unsupported || starting || !!noPermission" @click="start">
          <el-icon><ei-video-play /></el-icon><span>{{ started ? $t('profiler:resume') : $t('common:start') }}</span>
        </el-button>
      </span>
      <el-button size="small" :disabled="!rows.length" @click="clear">
        <el-icon><ei-delete /></el-icon><span>{{ $t('common:clear') }}</span>
      </el-button>
    </header>

    <div v-if="unsupported" class="pv-empty">
      <el-icon :size="28"><ei-view /></el-icon>
      <p>{{ $t('profiler:unsupported', { engine: driver?.name ?? $t('profiler:thisEngine') }) }}</p>
    </div>

    <template v-else>
      <div v-if="error || started || noPermission" class="pv-notes">
        <el-alert v-if="error" type="error" :title="error" :closable="false" show-icon />
        <el-alert v-else-if="noPermission && !started" type="warning" :title="noPermission" :closable="false" show-icon />
        <template v-if="started">
          <div v-if="started.mode === 'sampled' || started.note" class="pv-note">
            <el-icon><ei-info-filled /></el-icon>
            <span>
              <template v-if="started.mode === 'sampled'">
                {{ $t('profiler:sampledNote') }}
              </template>
              <template v-if="started.note"> {{ tb(started.note) }}</template>
            </span>
          </div>
          <div v-if="started.changes.length" class="pv-note warn">
            <el-icon><ei-info-filled /></el-icon>
            <span>{{ running ? $t('profiler:enabled', { source: tb(started.source) }) : $t('profiler:disabled', { source: tb(started.source) }) }}</span>
            <el-tooltip placement="bottom-start" :show-after="150" popper-class="pv-changes-tip">
              <template #content>
                <div v-for="(c, i) in started.changes" :key="i">{{ tb(c) }}</div>
              </template>
              <span class="pv-note-more">{{ $t('common:details') }}</span>
            </el-tooltip>
          </div>
        </template>
      </div>

      <div v-if="stats" class="pv-stats">
        <span class="pv-stats-title">{{ $t('profiler:stats.title') }}</span>
        <span>
          <i18next :translation="$t('profiler:stats.runs', { count: stats.n })">
            <template #n><b>{{ num(stats.n) }}</b></template>
          </i18next>
        </span>
        <span v-for="f in stats.figures" :key="f.key" class="pv-stats-group">
          <span class="pv-stats-name">{{ $t(`profiler:column.${COLUMN_KEY[f.key]}`) }}</span>
          <span>{{ $t('profiler:stats.avg') }} <b>{{ figure(f.key, f.s.avg) }}</b></span>
          <span>{{ $t('profiler:stats.min') }} <b>{{ figure(f.key, f.s.min) }}</b></span>
          <span>p95 <b>{{ figure(f.key, f.s.p95) }}</b></span>
          <span>{{ $t('profiler:stats.max') }} <b>{{ figure(f.key, f.s.max) }}</b></span>
        </span>
      </div>

      <div ref="body" class="pv-body">
        <div ref="list" class="pv-list" @scroll="onScroll">
          <table class="pv-table">
            <thead>
              <tr @contextmenu.prevent="columnsMenu">
                <th v-if="vis('time')" class="c-time pv-sortable" @click="sortBy('time')">{{ $t('profiler:column.time') }}<span class="pv-arrow">{{ arrow('time') }}</span></th>
                <th v-if="vis('duration_ms')" class="c-dur pv-sortable" @click="sortBy('duration_ms')">{{ $t('profiler:column.duration') }}<span class="pv-arrow">{{ arrow('duration_ms') }}</span></th>
                <th class="c-text pv-sortable" @click="sortBy('text')">{{ $t('profiler:column.query') }}<span class="pv-arrow">{{ arrow('text') }}</span></th>
                <th v-if="vis('database')" class="c-db pv-sortable" @click="sortBy('database')">{{ $t('profiler:column.database') }}<span class="pv-arrow">{{ arrow('database') }}</span></th>
                <th v-if="vis('user')" class="c-user pv-sortable" @click="sortBy('user')">{{ $t('profiler:column.user') }}<span class="pv-arrow">{{ arrow('user') }}</span></th>
                <th v-if="vis('client')" class="c-client pv-sortable" @click="sortBy('client')">{{ $t('profiler:column.client') }}<span class="pv-arrow">{{ arrow('client') }}</span></th>
                <th v-if="vis('application')" class="c-app pv-sortable" @click="sortBy('application')">{{ $t('profiler:column.application') }}<span class="pv-arrow">{{ arrow('application') }}</span></th>
                <th v-if="vis('rows')" class="c-rows pv-sortable" @click="sortBy('rows')">{{ $t('profiler:column.rows') }}<span class="pv-arrow">{{ arrow('rows') }}</span></th>
                <template v-for="m in METRICS" :key="m">
                  <th v-if="vis(m)" class="c-metric pv-sortable" :title="unitHint(m)" @click="sortBy(m)">{{ $t(`profiler:column.${COLUMN_KEY[m]}`) }}<span class="pv-arrow">{{ arrow(m) }}</span></th>
                </template>
              </tr>
              <tr class="pv-filters" :title="$t('profiler:filtersHint')">
                <th v-if="vis('time')" class="c-time"><el-icon class="pv-filter-icon"><ei-filter /></el-icon></th>
                <th v-for="c in COLUMNS.filter((c) => vis(c.key))" :key="c.key" :class="`c-${c.key}`">
                  <span v-if="c.key === 'text' && sameAs" class="pv-same" :title="sameAs.label">
                    <span>{{ $t('profiler:sameAs', { label: sameAs.label }) }}</span>
                    <button class="ide-icon-btn" :title="$t('profiler:removeFilter')" @click="sameAs = null"><el-icon :size="12"><ei-close /></el-icon></button>
                  </span>
                  <el-select
                    v-else-if="isPick(c.key)"
                    v-model="picked[c.key]"
                    class="pv-pick"
                    :class="{ on: picked[c.key].length > 0 }"
                    size="small"
                    multiple
                    collapse-tags
                    collapse-tags-tooltip
                    filterable
                    clearable
                    :placeholder="$t('common:all')"
                    :no-data-text="running ? $t('profiler:noneYet') : $t('profiler:noValues')"
                  >
                    <el-option v-for="v in seen[c.key]" :key="v" :label="v" :value="v" />
                  </el-select>
                  <input v-else v-model="filterText[c.key]" class="pv-filter" :class="{ on: !!filterText[c.key].trim() }" :placeholder="c.hint ? $t(c.hint) : ''" spellcheck="false" />
                </th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="r in shown"
                :key="r.n"
                :class="{ sel: r.n === selected, err: !!r.error }"
                @click="selected = r.n"
                @dblclick="openInQuery(r)"
                @contextmenu.prevent="openMenu($event, r)"
              >
                <td v-if="vis('time')" class="c-time">{{ clock(r.time) }}</td>
                <td v-if="vis('duration_ms')" class="c-dur" :class="slowness(r.duration_ms)">{{ duration(r.duration_ms) }}</td>
                <td class="c-text">
                  <el-icon v-if="r.error" class="pv-err-icon"><ei-circle-close-filled /></el-icon>{{ oneLine(r.text) }}
                </td>
                <td v-if="vis('database')" class="c-db">{{ r.database }}</td>
                <td v-if="vis('user')" class="c-user">{{ r.user }}</td>
                <td v-if="vis('client')" class="c-client">{{ r.client }}</td>
                <td v-if="vis('application')" class="c-app">{{ r.application }}</td>
                <td v-if="vis('rows')" class="c-rows">{{ r.rows ?? '' }}</td>
                <template v-for="m in METRICS" :key="m">
                  <td v-if="vis(m)" class="c-metric">{{ figure(m, r[m]) }}</td>
                </template>
              </tr>
              <tr v-if="!shown.length">
                <td :colspan="visibleCount" class="pv-none">
                  {{ filtering && rows.length ? $t('profiler:empty.noMatch') : running ? (filtering ? $t('profiler:empty.waitingFiltered') : $t('profiler:empty.waiting')) : $t('profiler:empty.none') }}
                </td>
              </tr>
            </tbody>
          </table>
        </div>

        <div v-if="current" class="pv-sash" :title="$t('profiler:dragHint')" @pointerdown="drag" />
        <section v-if="current" class="pv-detail" :style="{ height: `${detailHeight}px` }">
          <div class="pv-detail-bar">
            <span class="pv-detail-meta">
              {{ clock(current.time) }}
              <template v-if="current.duration_ms !== null"> · {{ duration(current.duration_ms) }}</template>
              <template v-if="current.rows !== null"> · {{ $t('profiler:detail.rows', { count: current.rows, n: num(current.rows) }) }}</template>
              <template v-for="m in METRICS" :key="m">
                <template v-if="current[m] != null"> · {{ $t(`profiler:column.${COLUMN_KEY[m]}`) }} {{ figure(m, current[m]) }}</template>
              </template>
              <template v-if="current.detail"> · {{ tb(current.detail) }}</template>
            </span>
            <div class="pv-spacer" />
            <el-button size="small" text @click="copy(current.text)"><el-icon><ei-document-copy /></el-icon><span>{{ $t('common:copy') }}</span></el-button>
            <el-button size="small" text @click="openInQuery(current)"><el-icon><ei-document /></el-icon><span>{{ $t('profiler:openInQuery') }}</span></el-button>
            <button class="ide-icon-btn" :title="$t('common:close')" @click="selected = null"><el-icon :size="13"><ei-close /></el-icon></button>
          </div>
          <div v-if="current.error" class="pv-detail-error">{{ tb(current.error) }}</div>
          <div class="pv-sql">
            <CodeEditor :model-value="current.text" read-only :language="driver?.language" :dialect="driver?.dialect" />
          </div>
        </section>
      </div>

      <ContextMenu v-if="menu" :x="menu.x" :y="menu.y" :items="menu.items" @close="menu = null" />

      <footer class="pv-foot">
        {{ $t('profiler:foot.queries', { count: rows.length, n: num(rows.length) }) }}
        <template v-if="filtering"> · {{ $t('profiler:foot.passing', { n: num(shown.length) }) }}</template>
        <template v-if="skipped"> · {{ $t('profiler:foot.skipped', { n: num(skipped) }) }}</template>
        <template v-if="dropped"> · {{ $t('profiler:foot.dropped', { n: num(dropped), max: num(MAX_ROWS) }) }}</template>
        <span v-if="sort" class="pv-follow" @click="sort = null">{{ $t('profiler:foot.arrivalOrder') }}</span>
        <span v-else-if="!follow && running" class="pv-follow" @click="follow = true; list?.scrollTo({ top: list.scrollHeight })">{{ $t('profiler:foot.toEnd') }}</span>
      </footer>
    </template>
  </div>
</template>

<style scoped>
.pv { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--nm-bg); }
.pv-head {
  display: flex; align-items: center; gap: 10px; padding: 8px 14px;
  border-bottom: 1px solid var(--nm-border-soft); flex-shrink: 0;
}
.pv-head .el-button { margin: 0; }
.pv-head .el-button span, .pv-detail-bar .el-button span { margin-left: 4px; }
.pv-title { display: flex; flex-direction: column; line-height: 1.2; min-width: 0; }
.pv-title strong { color: var(--nm-text-strong); font-weight: 600; }
.pv-sub { font-size: 11px; color: var(--nm-text-dim); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.pv-spacer { flex: 1; }
.pv-mode {
  font-size: 11px; padding: 1px 7px; border-radius: 9px; white-space: nowrap;
  border: 1px solid var(--nm-border-soft); color: var(--nm-text-dim);
}
.pv-mode.complete { color: var(--nm-success, #89d185); border-color: color-mix(in srgb, var(--nm-success, #89d185) 45%, transparent); }
.pv-mode.sampled { color: var(--nm-warning); border-color: color-mix(in srgb, var(--nm-warning) 45%, transparent); }
.pv-status { font-size: 11.5px; color: var(--nm-text-dim); display: inline-flex; align-items: center; gap: 6px; white-space: nowrap; }
.pv-dot { width: 8px; height: 8px; border-radius: 50%; background: var(--nm-text-dim); }
.pv-status.on .pv-dot { background: var(--nm-danger); animation: ide-pulse 1.4s ease-in-out infinite; }

.pv-empty {
  display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 6px;
  padding: 60px 20px; color: var(--nm-text-dim); text-align: center;
}
.pv-notes { display: flex; flex-direction: column; gap: 6px; padding: 8px 14px; flex-shrink: 0; }
.pv-note { display: flex; gap: 6px; align-items: flex-start; font-size: 12px; color: var(--nm-text-dim); }
.pv-note .el-icon { margin-top: 2px; flex-shrink: 0; }
.pv-note.warn { color: var(--nm-warning); }
.pv-sortable { cursor: pointer; user-select: none; }
.pv-sortable:hover { color: var(--nm-text); }
.pv-arrow { margin-left: 4px; color: var(--nm-text); }
.pv-note-more { text-decoration: underline dotted; cursor: help; white-space: nowrap; }

.pv-body { flex: 1; display: flex; flex-direction: column; min-height: 0; }
.pv-list { flex: 1; overflow: auto; min-height: 80px; }
.pv-table { width: 100%; border-collapse: collapse; table-layout: fixed; font-size: 12px; }
.pv-table th {
  position: sticky; top: 0; z-index: 1; text-align: left; font-weight: 500; color: var(--nm-text-dim);
  background: var(--nm-bg-elev); padding: 5px 8px; border-bottom: 1px solid var(--nm-border-soft);
}
.pv-table td {
  padding: 3px 8px; border-bottom: 1px solid color-mix(in srgb, var(--nm-border-soft) 50%, transparent);
  white-space: nowrap; overflow: hidden; text-overflow: ellipsis; color: var(--nm-text);
}
.pv-table tbody tr { cursor: default; }
.pv-table tbody tr:hover td { background: var(--nm-bg-hover, color-mix(in srgb, var(--nm-text) 6%, transparent)); }
.pv-table tbody tr.sel td { background: color-mix(in srgb, var(--nm-accent, #0e639c) 30%, transparent); }
.pv-table tbody tr.err .c-text { color: var(--nm-danger); }
.c-time { width: 100px; font-variant-numeric: tabular-nums; }
.c-dur, .c-rows, .c-metric { width: 74px; text-align: right !important; font-variant-numeric: tabular-nums; }
.pv-table td.c-dur.slow-1 { color: var(--nm-warning); }
.pv-table td.c-dur.slow-2 { color: #e8833a; }
.pv-table td.c-dur.slow-3 { color: var(--nm-danger); font-weight: 600; }
.c-text { font-family: var(--nm-mono); }
.c-db, .c-user { width: 120px; }
.c-client { width: 150px; }
.c-app { width: 150px; }
.pv-same {
  display: flex; align-items: center; gap: 4px; height: 22px; padding: 0 2px 0 8px; font-size: 11.5px; font-weight: 400;
  border-radius: 3px; color: var(--nm-text); background: color-mix(in srgb, var(--nm-accent, #0e639c) 28%, transparent);
  border: 1px solid color-mix(in srgb, var(--nm-accent, #0e639c) 70%, transparent);
}
.pv-same > span { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-family: var(--nm-mono); }
.pv-stats {
  display: flex; flex-wrap: wrap; align-items: baseline; gap: 4px 16px; padding: 6px 14px; flex-shrink: 0; font-size: 12px;
  color: var(--nm-text-dim); border-top: 1px solid var(--nm-border-soft);
  background: color-mix(in srgb, var(--nm-accent, #0e639c) 10%, transparent);
}
.pv-stats b { color: var(--nm-text-strong); font-weight: 600; font-variant-numeric: tabular-nums; }
.pv-stats-title { color: var(--nm-text); font-weight: 600; }
.pv-stats-group { display: inline-flex; gap: 8px; padding-left: 10px; border-left: 1px solid var(--nm-border); }
.pv-stats-name { color: var(--nm-text); }
.pv-err-icon { vertical-align: -2px; margin-right: 4px; }
.pv-none { color: var(--nm-text-dim); text-align: center; padding: 24px !important; }

.pv-sash { height: 5px; flex-shrink: 0; cursor: row-resize; border-top: 1px solid var(--nm-border-soft); }
.pv-sash:hover { background: color-mix(in srgb, var(--nm-accent, #0e639c) 45%, transparent); }
.pv-detail { flex-shrink: 0; display: flex; flex-direction: column; min-height: 0; background: var(--nm-bg-elev); }
.pv-detail-bar { display: flex; align-items: center; gap: 4px; padding: 4px 8px 4px 14px; }
.pv-detail-meta { font-size: 11.5px; color: var(--nm-text-dim); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.pv-detail-error { padding: 0 14px 6px; color: var(--nm-danger); font-size: 12px; }
.pv-sql { flex: 1; min-height: 0; border-top: 1px solid var(--nm-border-soft); }
.pv-filters th { top: 26px; padding: 2px 4px; }
.pv-filter-icon { color: var(--nm-text-dim); vertical-align: -2px; margin-left: 4px; }
.pv-filter {
  width: 100%; box-sizing: border-box; height: 22px; padding: 0 6px; font: inherit; font-size: 11.5px;
  color: var(--nm-text); background: var(--nm-bg); border: 1px solid var(--nm-border-soft); border-radius: 3px; outline: none;
}
.pv-filter::placeholder { color: color-mix(in srgb, var(--nm-text-dim) 70%, transparent); }
.pv-filter:focus { border-color: var(--nm-accent, #0e639c); }
.pv-pick { width: 100%; }
.pv-pick :deep(.el-select__wrapper) { min-height: 22px; padding: 0 6px; background: var(--nm-bg); box-shadow: 0 0 0 1px var(--nm-border-soft) inset; }
.pv-pick.on :deep(.el-select__wrapper) { box-shadow: 0 0 0 1px color-mix(in srgb, var(--nm-accent, #0e639c) 70%, transparent) inset; }
.pv-filter.on { border-color: color-mix(in srgb, var(--nm-accent, #0e639c) 70%, transparent); }
.pv-foot {
  flex-shrink: 0; display: flex; gap: 4px; align-items: center; padding: 4px 14px;
  font-size: 11.5px; color: var(--nm-text-dim); border-top: 1px solid var(--nm-border-soft);
}
.pv-follow { margin-left: auto; color: var(--nm-link, #3794ff); cursor: pointer; }
</style>

<style>
/* The server-change detail (a tooltip, outside the component). */
.pv-changes-tip { max-width: 520px; line-height: 1.5; overflow-wrap: anywhere; }
.pv-changes-tip div + div { margin-top: 4px; }
</style>
