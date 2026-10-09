<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import type { HistoryEntry } from '../api/history';
import { timelineApi, type FileCommit, type QueryVersion } from '../api/timeline';
import { newQuery } from '../composables/actions';
import { currentText, restoreText, sourceKey, timelineSeq, timelineSource, type TimelineSource } from '../composables/timeline';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import { dbKey, useConnectionsStore } from '../stores/connections';
import { useProjectsStore } from '../stores/projects';
import { baseName, useTabsStore } from '../stores/tabs';
import { useUiStore } from '../stores/ui';
import ContextMenu, { type MenuItem } from './ContextMenu.vue';
import TimelineDiffDialog from './TimelineDiffDialog.vue';

// The history sidebar's "Esta pestaña" (docs/history.md): the active tab's
// timeline, newest first, as VS Code's Timeline. It follows the active tab:
// switching tabs reloads it. A saved query lists its versions (kept as it's
// saved) and its runs; a project file, its runs and its git commits.

const { t } = useTranslation();
const tabs = useTabsStore();
const conns = useConnectionsStore();
const projects = useProjectsStore();
const ui = useUiStore();

type Item =
  | { kind: 'version'; key: string; at: number; version: QueryVersion }
  | { kind: 'run'; key: string; at: number; run: HistoryEntry }
  | { kind: 'commit'; key: string; at: number; commit: FileCommit };

const source = computed<TimelineSource | null>(() => timelineSource(tabs.active));
const key = computed(() => sourceKey(source.value));
const items = ref<Item[]>([]);
const loading = ref(false);
const error = ref<string | null>(null);
const selected = ref<string | null>(null);
const show = ref({ version: true, run: true, commit: true });

let token = 0;
async function load() {
  const s = source.value;
  const mine = ++token;
  if (!s) { items.value = []; error.value = null; return; }
  loading.value = true;
  try {
    const out: Item[] = [];
    if (s.kind === 'query') {
      const [versions, runs] = await Promise.all([timelineApi.versions(s.queryId), timelineApi.runsOfQuery(s.queryId)]);
      out.push(...versions.map((v): Item => ({ kind: 'version', key: `v${v.id}`, at: Date.parse(v.saved_at), version: v })));
      out.push(...runs.map((r): Item => ({ kind: 'run', key: `r${r.id}`, at: Date.parse(r.started_at), run: r })));
    } else {
      const [runs, commits] = await Promise.all([
        timelineApi.runsOfFile(s.projectId, s.path),
        // No git, or not a repo: only the runs.
        timelineApi.fileLog(s.projectId, s.path).catch(() => [] as FileCommit[]),
      ]);
      out.push(...runs.map((r): Item => ({ kind: 'run', key: `r${r.id}`, at: Date.parse(r.started_at), run: r })));
      out.push(...commits.map((c): Item => ({ kind: 'commit', key: `c${c.hash}`, at: Date.parse(c.date), commit: c })));
    }
    out.sort((a, b) => b.at - a.at);
    if (mine !== token) return;
    items.value = out;
    error.value = null;
  } catch (e) {
    if (mine === token) error.value = errorMessage(e);
  } finally {
    if (mine === token) loading.value = false;
  }
}

// Another tab: reload at once. A save, a run or a commit: a moment later.
watch(key, () => { selected.value = null; items.value = []; void load(); }, { immediate: true });
let timer: ReturnType<typeof setTimeout> | undefined;
const soon = () => { clearTimeout(timer); timer = setTimeout(() => void load(), 300); };
watch(() => timelineSeq.value, soon);
watch(() => ui.historySeq, soon);
watch(() => (source.value?.kind === 'file' ? projects.status[source.value.projectId]?.data?.head : null), soon);
onBeforeUnmount(() => clearTimeout(timer));

const kinds = computed(() => (source.value?.kind === 'file' ? (['run', 'commit'] as const) : (['version', 'run'] as const)));
const visible = computed(() => items.value.filter((i) => show.value[i.kind]));

/** With a heading per day (Hoy, Ayer, the date). */
const rows = computed(() => {
  const out: ({ day: string } | Item)[] = [];
  let last = '';
  for (const i of visible.value) {
    const d = dayLabel(i.at);
    if (d !== last) { out.push({ day: d }); last = d; }
    out.push(i);
  }
  return out;
});

function dayLabel(ms: number): string {
  const d = new Date(ms);
  const today = new Date();
  const yesterday = new Date(today.getTime() - 86_400_000);
  if (d.toDateString() === today.toDateString()) return t('history:tl.today');
  if (d.toDateString() === yesterday.toDateString()) return t('history:tl.yesterday');
  return d.toLocaleDateString(locale(), { weekday: 'short', day: 'numeric', month: 'short', year: d.getFullYear() === today.getFullYear() ? undefined : 'numeric' });
}
const time = (ms: number) => new Date(ms).toLocaleTimeString(locale(), { hour: '2-digit', minute: '2-digit' });
const duration = (ms: number) => (ms < 1000 ? `${ms} ms` : `${(ms / 1000).toLocaleString(locale(), { maximumFractionDigits: 1 })} s`);
const oneLine = (s: string) => s.replace(/\s+/g, ' ').trim();

const title = computed(() => {
  const s = source.value;
  if (!s) return '';
  if (s.kind === 'file') return baseName(s.path);
  return conns.queries[dbKey(s.connectionId, s.database)]?.items.find((q) => q.id === s.queryId)?.name ?? t('query:resultName');
});
const subtitle = computed(() => {
  const s = source.value;
  if (!s) return '';
  if (s.kind === 'file') return `${projects.byId(s.projectId)?.name ?? ''} › ${s.path}`;
  return [conns.byId(s.connectionId)?.name, s.database].filter(Boolean).join(' · ');
});

// -- the diff against the text now ----------------------------------------------------

const diff = ref<{ title: string; beforeLabel: string; before: string | null; after: string; note: string | null; canRestore: boolean; restoring: boolean } | null>(null);

async function compare(i: Item) {
  const s = source.value;
  if (!s) return;
  selected.value = i.key;
  try {
    const after = await currentText(s);
    let before: string | null = null;
    let note: string | null = null;
    let label = '';
    if (i.kind === 'version') {
      before = (await timelineApi.version(i.version.id)).sql ?? '';
      label = t('history:tl.versionOf', { time: `${dayLabel(i.at)} ${time(i.at)}` });
    } else if (i.kind === 'commit' && s.kind === 'file') {
      const f = await timelineApi.fileAt(s.projectId, i.commit.hash, i.commit.path);
      before = f.text;
      note = f.too_large ? t('history:tl.tooLarge') : f.binary ? t('history:tl.binary') : f.text === null ? t('history:tl.notInCommit') : null;
      label = t('history:tl.commitOf', { hash: i.commit.short });
    } else if (i.kind === 'run') {
      before = i.run.sql;
      label = t('history:tl.runOf', { time: `${dayLabel(i.at)} ${time(i.at)}` });
    }
    diff.value = { title: t('history:tl.diffTitle', { what: label }), beforeLabel: label, before, after, note, canRestore: i.kind !== 'run', restoring: false };
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

async function restore() {
  const s = source.value;
  const d = diff.value;
  if (!s || !d || d.before === null) return;
  d.restoring = true;
  try {
    if (!(await restoreText(s, d.before))) { ElMessage.warning(t('history:tl.noEditor')); return; }
    ElMessage.success(s.kind === 'file' ? t('history:tl.restoredFile') : t('history:tl.restored'));
    diff.value = null;
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    if (diff.value) diff.value.restoring = false;
  }
}

async function openRun(r: HistoryEntry) {
  if (!conns.byId(r.connection_id)) {
    ElMessage.warning(t('history:connectionGone', { name: r.connection_name }));
    return;
  }
  await newQuery(r.connection_id, r.database, r.sql);
}

function activate(i: Item) {
  selected.value = i.key;
  if (i.kind === 'run') return;
  void compare(i);
}

const menu = ref<{ x: number; y: number; items: MenuItem[] } | null>(null);
function openMenu(ev: MouseEvent, i: Item) {
  selected.value = i.key;
  const copy = (text: string) => async () => { await navigator.clipboard.writeText(text); ElMessage.success(t('history:copied')); };
  const list: MenuItem[] = [{ label: t('history:tl.compare'), action: () => compare(i) }];
  if (i.kind === 'run') {
    list.unshift({ label: t('history:openNew'), action: () => openRun(i.run) });
    list.push({ label: t('history:copySql'), action: copy(i.run.sql) });
  }
  if (i.kind === 'commit') list.push({ label: t('history:tl.copyHash'), action: copy(i.commit.hash) });
  menu.value = { x: ev.clientX, y: ev.clientY, items: list };
}

function onKey(ev: KeyboardEvent) {
  const i = visible.value.findIndex((x) => x.key === selected.value);
  if (ev.key === 'ArrowDown' || ev.key === 'ArrowUp') {
    ev.preventDefault();
    const next = visible.value[Math.max(0, Math.min(visible.value.length - 1, i + (ev.key === 'ArrowDown' ? 1 : -1)))];
    if (next) selected.value = next.key;
    return;
  }
  const it = visible.value[i];
  if (!it) return;
  if (ev.key === 'Enter') { ev.preventDefault(); if (it.kind === 'run') void openRun(it.run); else void compare(it); }
}

defineExpose({ load });
</script>

<template>
  <div class="tl">
    <div v-if="!source" class="tl-empty">
      <el-icon :size="22"><ei-clock /></el-icon>
      <span>{{ $t('history:tl.noSource') }}</span>
    </div>
    <template v-else>
      <div class="tl-head" :title="subtitle">
        <el-icon class="tl-head-icon"><ei-document /></el-icon>
        <div class="tl-head-text">
          <div class="tl-name">{{ title }}</div>
          <div class="tl-sub">{{ subtitle }}</div>
        </div>
      </div>
      <div class="tl-filters">
        <button
          v-for="k in kinds"
          :key="k"
          class="tl-chip"
          :class="[k, { on: show[k] }]"
          :aria-pressed="show[k]"
          @click="show[k] = !show[k]"
        >
          {{ $t(k === 'version' ? 'history:tl.filterVersions' : k === 'run' ? 'history:tl.filterRuns' : 'history:tl.filterCommits') }}
        </button>
        <span class="tl-sp" />
        <button class="tl-icon" :title="$t('common:refresh')" @click="load()"><el-icon :class="{ spin: loading }"><ei-refresh /></el-icon></button>
      </div>
      <div class="tl-list" tabindex="0" @keydown="onKey">
        <el-alert v-if="error" type="error" :title="error" :closable="false" class="tl-error" />
        <div v-else-if="!loading && !visible.length" class="tl-empty">
          <span>{{ source.kind === 'file' ? $t('history:tl.emptyFile') : $t('history:tl.empty') }}</span>
        </div>
        <template v-for="r in rows" :key="'day' in r ? `d:${r.day}` : r.key">
          <div v-if="'day' in r" class="tl-day">{{ r.day }}</div>
          <div
            v-else
            class="tl-item"
            :class="[r.kind, { sel: selected === r.key, err: r.kind === 'run' && r.run.error }]"
            :data-kind="r.kind"
            @click="activate(r)"
            @dblclick="r.kind === 'run' && openRun(r.run)"
            @contextmenu.prevent="openMenu($event, r)"
          >
            <span class="tl-dot" :class="r.kind" />
            <div class="tl-main">
              <div class="tl-row">
                <template v-if="r.kind === 'version'">
                  <span class="tl-label">{{ $t('history:tl.saved') }}</span>
                  <span v-if="r.version.added || r.version.removed" class="tl-stats">
                    <span class="add">+{{ r.version.added }}</span> <span class="del">−{{ r.version.removed }}</span>
                  </span>
                </template>
                <template v-else-if="r.kind === 'run'">
                  <span class="tl-label">{{ r.run.error ? $t('history:tl.runError') : $t('history:tl.run') }}</span>
                  <span v-if="r.run.error" class="tl-bad" :title="tb(r.run.error)"><el-icon><ei-circle-close-filled /></el-icon></span>
                  <span class="tl-meta">{{ duration(r.run.duration_ms) }}</span>
                  <span v-if="!r.run.error && r.run.rows !== null" class="tl-meta">{{ $t('history:rows', { count: r.run.rows }) }}</span>
                </template>
                <template v-else>
                  <span class="tl-label" :title="r.commit.subject">{{ r.commit.subject }}</span>
                </template>
                <span class="tl-sp" />
                <span class="tl-time">{{ time(r.at) }}</span>
              </div>
              <div v-if="r.kind === 'run'" class="tl-sql" :title="r.run.sql">{{ oneLine(r.run.sql) }}</div>
              <div v-else-if="r.kind === 'commit'" class="tl-sub2">
                <span class="tl-hash">{{ r.commit.short }}</span> · {{ r.commit.author }}
                <template v-if="source.kind === 'file' && r.commit.path !== source.path"> · {{ $t('history:tl.renamedFrom', { path: r.commit.path }) }}</template>
              </div>
            </div>
          </div>
        </template>
      </div>
    </template>
    <ContextMenu v-if="menu" :x="menu.x" :y="menu.y" :items="menu.items" @close="menu = null" />
    <TimelineDiffDialog
      v-if="diff"
      :title="diff.title"
      :before-label="diff.beforeLabel"
      :before="diff.before"
      :after="diff.after"
      :note="diff.note"
      :can-restore="diff.canRestore"
      :restoring="diff.restoring"
      @close="diff = null"
      @restore="restore"
    />
  </div>
</template>

<style scoped>
.tl { display: flex; flex-direction: column; flex: 1; min-height: 0; }
.tl-head { display: flex; align-items: center; gap: 8px; padding: 2px 12px 6px 16px; flex: none; }
.tl-head-icon { color: var(--nm-text-dim); flex: none; }
.tl-head-text { min-width: 0; }
.tl-name { font-size: 12.5px; font-weight: 600; color: var(--nm-text-strong); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.tl-sub { font-size: 11px; color: var(--nm-text-dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.tl-filters { display: flex; align-items: center; gap: 4px; padding: 0 8px 6px 14px; flex: none; }
.tl-chip { border: 1px solid var(--nm-border-soft); background: none; color: var(--nm-text-dim); border-radius: 10px; padding: 1px 8px; font: inherit; font-size: 11px; cursor: pointer; }
.tl-chip.on { color: var(--nm-text-strong); background: var(--ide-hover); border-color: transparent; }
.tl-icon { display: inline-flex; align-items: center; justify-content: center; width: 22px; height: 22px; border: 0; border-radius: 4px; background: none; color: var(--nm-text-dim); cursor: pointer; }
.tl-icon:hover { background: var(--ide-hover); color: var(--nm-text-strong); }
.spin { animation: tl-spin 1s linear infinite; }
@keyframes tl-spin { to { transform: rotate(360deg); } }
.tl-list { flex: 1; min-height: 0; overflow: auto; outline: none; padding-bottom: 12px; }
.tl-error { margin: 8px 12px; width: auto; }
.tl-empty { display: flex; flex-direction: column; align-items: center; gap: 8px; padding: 40px 16px; color: var(--nm-text-dim); font-size: 12px; text-align: center; }
.tl-day { padding: 8px 16px 2px; font-size: 11px; font-weight: 600; color: var(--nm-text-muted); text-transform: uppercase; letter-spacing: .04em; }
.tl-item { position: relative; display: flex; gap: 8px; padding: 4px 10px 4px 16px; cursor: pointer; }
.tl-item:hover { background: var(--ide-hover); }
.tl-item.sel { background: var(--ide-selection); }
/* The timeline's line, through the dots. */
.tl-item::before { content: ''; position: absolute; left: 20px; top: 0; bottom: 0; width: 1px; background: var(--nm-border-soft); }
.tl-dot { position: relative; flex: none; width: 9px; height: 9px; margin-top: 4px; border-radius: 50%; border: 2px solid var(--nm-text-dim); background: var(--ide-sidebar); }
.tl-dot.version { border-color: #3794ff; }
.tl-dot.run { border-color: #73c991; }
.tl-item.err .tl-dot { border-color: var(--nm-danger); }
.tl-dot.commit { border-color: #e5c07b; border-radius: 2px; }
.tl-main { flex: 1; min-width: 0; }
.tl-row { display: flex; align-items: center; gap: 6px; font-size: 12px; min-width: 0; }
.tl-label { color: var(--nm-text-strong); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.tl-stats { font-family: var(--nm-mono); font-size: 11px; }
.tl-stats .add { color: #73c991; }
.tl-stats .del { color: var(--nm-danger); }
.tl-meta { font-size: 11px; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; white-space: nowrap; }
.tl-bad { color: var(--nm-danger); display: inline-flex; }
.tl-sp { flex: 1; }
.tl-time { font-size: 11px; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; flex: none; }
.tl-sql { font-family: var(--nm-mono, ui-monospace, Menlo, monospace); font-size: 11.5px; color: var(--nm-text-dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; margin-top: 1px; }
.tl-sub2 { font-size: 11px; color: var(--nm-text-dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; margin-top: 1px; }
.tl-hash { font-family: var(--nm-mono); }
</style>
