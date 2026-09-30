<script setup lang="ts">
import { computed, onMounted, reactive, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { historyApi, type HistoryEntry } from '../api/history';
import { newQuery } from '../composables/actions';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import { confirmNative } from '../native';
import { useConnectionsStore } from '../stores/connections';
import { useUiStore } from '../stores/ui';
import ContextMenu, { type MenuItem } from './ContextMenu.vue';
import EngineIcon from './EngineIcon.vue';

// The query history (the 🕘 in the activity bar; docs/historial.md): what was
// run from the editor, grouped by the server it ran on, newest first.

const { t } = useTranslation();
const conns = useConnectionsStore();
const ui = useUiStore();

const PAGE = 200;
const items = ref<HistoryEntry[]>([]);
const loading = ref(false);
const more = ref(false);
const search = ref('');
const selected = ref<number | null>(null);
/** Collapsed hosts. */
const collapsed = reactive(new Set<string>());

async function load(append = false) {
  loading.value = true;
  try {
    const before = append && items.value.length ? items.value[items.value.length - 1].id : null;
    const page = await historyApi.list(search.value.trim() || null, before, PAGE);
    items.value = append ? [...items.value, ...page] : page;
    more.value = page.length === PAGE;
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    loading.value = false;
  }
}
onMounted(() => load());
watch(() => ui.historySeq, () => load());
let timer: ReturnType<typeof setTimeout> | undefined;
watch(search, () => { clearTimeout(timer); timer = setTimeout(() => load(), 250); });

/** By host, the host with the newest run first. */
const groups = computed(() => {
  const by = new Map<string, HistoryEntry[]>();
  for (const e of items.value) {
    const key = e.host || e.connection_name;
    if (!by.has(key)) by.set(key, []);
    by.get(key)!.push(e);
  }
  return [...by.entries()].map(([host, entries]) => ({ host, entries, driver: entries[0].driver }));
});

function toggle(host: string) {
  if (collapsed.has(host)) collapsed.delete(host);
  else collapsed.add(host);
}

function time(e: HistoryEntry): string {
  const d = new Date(e.started_at);
  const today = new Date();
  const sameDay = d.toDateString() === today.toDateString();
  return sameDay
    ? d.toLocaleTimeString(locale(), { hour: '2-digit', minute: '2-digit', second: '2-digit' })
    : d.toLocaleString(locale(), { day: '2-digit', month: 'short', hour: '2-digit', minute: '2-digit' });
}

function duration(ms: number): string {
  return ms < 1000 ? `${ms} ms` : `${(ms / 1000).toLocaleString(locale(), { maximumFractionDigits: 1 })} s`;
}

function firstLine(sql: string): string {
  return sql.replace(/\s+/g, ' ').trim();
}

async function open(e: HistoryEntry) {
  if (!conns.byId(e.connection_id)) {
    ElMessage.warning(t('history:connectionGone', { name: e.connection_name }));
    return;
  }
  await newQuery(e.connection_id, e.database, e.sql);
}

async function copy(e: HistoryEntry) {
  await navigator.clipboard.writeText(e.sql);
  ElMessage.success(t('history:copied'));
}

async function remove(ids: number[]) {
  try {
    await historyApi.remove(ids);
    items.value = items.value.filter((x) => !ids.includes(x.id));
  } catch (err) {
    ElMessage.error(errorMessage(err));
  }
}

async function clearAll() {
  if (!(await confirmNative(t('history:clearConfirm'), { title: t('history:clearTitle'), okLabel: t('history:clear') }))) return;
  try {
    await historyApi.remove(null);
    items.value = [];
  } catch (err) {
    ElMessage.error(errorMessage(err));
  }
}

const menu = ref<{ x: number; y: number; items: MenuItem[] } | null>(null);
function openMenu(ev: MouseEvent, e: HistoryEntry) {
  selected.value = e.id;
  menu.value = {
    x: ev.clientX,
    y: ev.clientY,
    items: [
      { label: t('history:openNew'), action: () => open(e) },
      { label: t('history:copySql'), action: () => copy(e) },
      { label: t('common:delete'), divided: true, action: () => remove([e.id]) },
    ],
  };
}
function openHostMenu(ev: MouseEvent, host: string, entries: HistoryEntry[]) {
  menu.value = {
    x: ev.clientX,
    y: ev.clientY,
    items: [{ label: t('history:deleteHost', { host }), action: () => remove(entries.map((x) => x.id)) }],
  };
}

function onKey(ev: KeyboardEvent) {
  const e = items.value.find((x) => x.id === selected.value);
  if (!e) return;
  if (ev.key === 'Enter') { ev.preventDefault(); open(e); }
  if (ev.key === 'Delete' || ev.key === 'Backspace') { ev.preventDefault(); remove([e.id]); }
}
</script>

<template>
  <div class="hs">
    <div class="hs-header">
      <span class="hs-title">{{ $t('history:title') }}</span>
      <div style="flex: 1" />
      <button class="hs-icon" :title="$t('common:refresh')" @click="load()"><el-icon><ei-refresh /></el-icon></button>
      <button class="hs-icon" :title="$t('history:clear')" :disabled="!items.length" @click="clearAll"><el-icon><ei-delete /></el-icon></button>
    </div>
    <div class="hs-filter">
      <el-input v-model="search" size="small" clearable :placeholder="$t('history:search')">
        <template #prefix><el-icon><ei-search /></el-icon></template>
      </el-input>
    </div>
    <div class="hs-list" tabindex="0" @keydown="onKey">
      <div v-if="!loading && !items.length" class="hs-empty">
        <el-icon :size="22"><ei-clock /></el-icon>
        <span>{{ search ? $t('history:noMatch') : $t('history:empty') }}</span>
      </div>
      <div v-for="g in groups" :key="g.host" class="hs-group">
        <button class="hs-host" @click="toggle(g.host)" @contextmenu.prevent="openHostMenu($event, g.host, g.entries)">
          <el-icon class="hs-caret" :class="{ open: !collapsed.has(g.host) }"><ei-arrow-right /></el-icon>
          <EngineIcon :id="g.driver" :name="g.driver" :size="14" />
          <span class="hs-host-name" :title="g.host">{{ g.host }}</span>
          <span class="hs-count">{{ g.entries.length }}</span>
        </button>
        <template v-if="!collapsed.has(g.host)">
          <div
            v-for="e in g.entries"
            :key="e.id"
            class="hs-item"
            :class="{ sel: selected === e.id, err: e.error }"
            :title="e.sql"
            @click="selected = e.id"
            @dblclick="open(e)"
            @contextmenu.prevent="openMenu($event, e)"
          >
            <div class="hs-meta">
              <span>{{ time(e) }}</span>
              <span v-if="e.database" class="hs-db">{{ e.database }}</span>
              <span class="hs-sp" />
              <span v-if="e.error" class="hs-bad" :title="tb(e.error)"><el-icon><ei-circle-close-filled /></el-icon></span>
              <span v-else-if="e.rows !== null" class="hs-rows">{{ $t('history:rows', { count: e.rows }) }}</span>
              <span class="hs-dur">{{ duration(e.duration_ms) }}</span>
            </div>
            <div class="hs-sql">{{ firstLine(e.sql) }}</div>
          </div>
        </template>
      </div>
      <button v-if="more" class="hs-more" :disabled="loading" @click="load(true)">{{ $t('history:more') }}</button>
    </div>
    <ContextMenu v-if="menu" :x="menu.x" :y="menu.y" :items="menu.items" @close="menu = null" />
  </div>
</template>

<style scoped>
.hs { display: flex; flex-direction: column; height: 100%; min-height: 0; }
.hs-header { display: flex; align-items: center; gap: 2px; padding: 0 8px 0 16px; height: 35px; flex: none; }
.hs-title { font-size: 11px; font-weight: 600; letter-spacing: .06em; text-transform: uppercase; color: var(--nm-text-dim); }
.hs-icon { display: inline-flex; align-items: center; justify-content: center; width: 24px; height: 24px; border: 0; border-radius: 4px; background: none; color: var(--nm-text-dim); cursor: pointer; }
.hs-icon:hover:not(:disabled) { background: var(--ide-hover); color: var(--nm-text-strong); }
.hs-icon:disabled { opacity: .4; cursor: default; }
.hs-filter { padding: 0 10px 8px; flex: none; }
.hs-list { flex: 1; min-height: 0; overflow: auto; outline: none; padding-bottom: 12px; }
.hs-empty { display: flex; flex-direction: column; align-items: center; gap: 8px; padding: 40px 16px; color: var(--nm-text-dim); font-size: 12px; text-align: center; }
.hs-host { display: flex; align-items: center; gap: 6px; width: 100%; padding: 4px 10px; border: 0; background: none; color: var(--nm-text-strong); font: inherit; font-size: 12.5px; font-weight: 600; cursor: pointer; text-align: left; }
.hs-host:hover { background: var(--ide-hover); }
.hs-caret { transition: transform .12s; color: var(--nm-text-dim); }
.hs-caret.open { transform: rotate(90deg); }
.hs-host-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.hs-count { font-size: 11px; font-weight: 400; color: var(--nm-text-muted); }
.hs-item { padding: 5px 10px 5px 32px; cursor: pointer; border-left: 2px solid transparent; }
.hs-item:hover { background: var(--ide-hover); }
.hs-item.sel { background: var(--ide-selection); }
.hs-item.err { border-left-color: var(--nm-danger); }
.hs-meta { display: flex; align-items: center; gap: 6px; font-size: 11px; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; }
.hs-db { color: var(--nm-text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 40%; }
.hs-sp { flex: 1; }
.hs-bad { color: var(--nm-danger); display: inline-flex; }
.hs-sql { font-family: var(--nm-font-mono, ui-monospace, Menlo, monospace); font-size: 12px; color: var(--nm-text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; margin-top: 1px; }
.hs-more { display: block; margin: 8px auto 0; border: 0; background: none; color: var(--el-color-primary); font: inherit; font-size: 12px; cursor: pointer; }
</style>
