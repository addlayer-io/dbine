<script setup lang="ts">
import { computed, onUnmounted, ref } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore, type SearchTab } from '../stores/tabs';

// "Buscar en la base" (docs/busqueda.md): object names and the text of
// views, routines, triggers… The scan runs on its own session in the
// backend; hits arrive as it goes ("code-search-progress") and the search
// can be cancelled. Clicking a hit opens the object's definition.

interface Hit { kind: string; schema: string | null; name: string; parent: string | null; line: number; text: string }
interface Result { hits: Hit[]; scanned: number; unreadable: string[]; truncated: boolean; cancelled: boolean; from_catalog: boolean }

const props = defineProps<{ tab: SearchTab }>();
const { t } = useTranslation();
const conns = useConnectionsStore();
const tabs = useTabsStore();

const text = ref('');
const names = ref(true);
const code = ref(true);
const caseSensitive = ref(false);
const wholeWord = ref(false);
const kinds = ref<string[]>([]);
const hits = ref<Hit[]>([]);
const result = ref<Result | null>(null);
const error = ref<string | null>(null);
const running = ref(false);
const progress = ref<{ done: number; total: number } | null>(null);
let searchId = '';
let unlisten: UnlistenFn | null = null;

const driver = computed(() => conns.driverOf(props.tab.connectionId));
const kindOptions = computed(() => driver.value?.object_kinds ?? []);
const kindLabel = (id: string) => tb(kindOptions.value.find((k) => k.id === id)?.label ?? id);

/** Hits by object, objects in the order they were found. */
const groups = computed(() => {
  const out = new Map<string, { key: string; kind: string; schema: string | null; name: string; parent: string | null; inName: boolean; lines: Hit[] }>();
  for (const h of hits.value) {
    const key = `${h.kind}|${h.schema ?? ''}|${h.parent ?? ''}|${h.name}`;
    let g = out.get(key);
    if (!g) out.set(key, (g = { key, kind: h.kind, schema: h.schema, name: h.name, parent: h.parent, inName: false, lines: [] }));
    if (h.line === 0) g.inName = true;
    else g.lines.push(h);
  }
  return [...out.values()];
});

async function search() {
  const q = text.value.trim();
  if (!q || running.value) return;
  if (!(await conns.ensureConnected(props.tab.connectionId))) return;
  searchId = `${props.tab.id}-${Date.now()}`;
  hits.value = [];
  result.value = null;
  error.value = null;
  progress.value = null;
  running.value = true;
  unlisten?.();
  const id = searchId;
  unlisten = await listen<{ search_id: string; done: number; total: number; hits: Hit[] }>('code-search-progress', (e) => {
    if (e.payload.search_id !== id) return;
    if (e.payload.hits.length) hits.value = hits.value.concat(e.payload.hits);
    if (e.payload.total) progress.value = { done: e.payload.done, total: e.payload.total };
  });
  try {
    const r = await invoke<Result>('search_database', {
      args: {
        connection_id: props.tab.connectionId,
        database: props.tab.database,
        search_id: id,
        query: { text: q, case_sensitive: caseSensitive.value, whole_word: wholeWord.value, kinds: kinds.value, max_hits: 0 },
        names: names.value,
        code: code.value,
      },
    });
    // The final list is the authority (events can arrive after the reply).
    hits.value = r.hits;
    result.value = r;
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    running.value = false;
    unlisten?.();
    unlisten = null;
  }
}

function cancel() {
  if (running.value) invoke('cancel_query', { args: { session_id: `search:${searchId}` } }).catch(() => {});
}
onUnmounted(() => {
  cancel();
  unlisten?.();
});

function open(g: { kind: string; schema: string | null; name: string }) {
  const k = kindOptions.value.find((x) => x.id === g.kind);
  tabs.openObject(props.tab.connectionId, props.tab.database, { kind: g.kind, schema: g.schema, name: g.name }, k?.has_definition ? 'definition' : k?.browsable ? 'data' : 'structure', false);
}

/** The line with the searched text marked (escaped first). */
function marked(line: string): string {
  const esc = (s: string) => s.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]!);
  const q = text.value.trim();
  if (!q) return esc(line);
  const re = new RegExp(q.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), caseSensitive.value ? 'g' : 'gi');
  let out = '';
  let last = 0;
  for (const m of line.matchAll(re)) {
    out += esc(line.slice(last, m.index)) + `<mark>${esc(m[0])}</mark>`;
    last = m.index! + m[0].length;
  }
  return out + esc(line.slice(last));
}
</script>

<template>
  <div class="sv">
    <header class="sv-head">
      <el-input v-model="text" :placeholder="$t('search:placeholder')" clearable class="sv-input" @keydown.enter="search">
        <template #prefix><el-icon><ei-search /></el-icon></template>
      </el-input>
      <el-button v-if="!running" type="primary" :disabled="!text.trim() || (!names && !code)" @click="search">{{ $t('search:search') }}</el-button>
      <el-button v-else @click="cancel">{{ $t('common:cancel') }}</el-button>
    </header>
    <div class="sv-opts">
      <el-checkbox v-model="names" size="small">{{ $t('search:names') }}</el-checkbox>
      <el-checkbox v-model="code" size="small">{{ $t('search:code') }}</el-checkbox>
      <el-checkbox v-model="caseSensitive" size="small">{{ $t('search:caseSensitive') }}</el-checkbox>
      <el-checkbox v-model="wholeWord" size="small">{{ $t('search:wholeWord') }}</el-checkbox>
      <el-select v-model="kinds" multiple collapse-tags clearable size="small" :placeholder="$t('search:allKinds')" style="width: 220px">
        <el-option v-for="k in kindOptions" :key="k.id" :label="tb(k.label)" :value="k.id" />
      </el-select>
    </div>

    <div class="sv-status">
      <template v-if="running">
        <el-icon class="is-loading"><ei-loading /></el-icon>
        <span v-if="progress">{{ $t('search:progress', { done: progress.done, total: progress.total }) }}</span>
        <span v-else>{{ $t('search:starting') }}</span>
        <span class="sv-found">· {{ $t('search:found', { count: groups.length }) }}</span>
      </template>
      <template v-else-if="result">
        <span>{{ $t('search:found', { count: groups.length }) }}</span>
        <span v-if="result.scanned" class="nm-muted">· {{ $t('search:scanned', { count: result.scanned }) }}</span>
        <span v-if="result.cancelled" class="sv-warn">· {{ $t('search:cancelled') }}</span>
        <span v-if="result.truncated" class="sv-warn">· {{ $t('search:truncated') }}</span>
        <span v-if="result.unreadable.length" class="sv-warn" :title="result.unreadable.join(', ')">· {{ $t('search:unreadable', { count: result.unreadable.length }) }}</span>
      </template>
    </div>
    <el-alert v-if="error" type="error" :title="error" :closable="false" show-icon class="sv-alert" />

    <div class="sv-results">
      <div v-for="g in groups" :key="g.key" class="sv-group">
        <button class="sv-obj" @click="open(g)">
          <span class="sv-kind">{{ kindLabel(g.kind) }}</span>
          <span class="sv-name"><span v-if="g.schema" class="nm-muted">{{ g.schema }}.</span>{{ g.name }}</span>
          <span v-if="g.parent" class="nm-muted">({{ g.parent }})</span>
          <span v-if="g.inName" class="sv-badge">{{ $t('search:inName') }}</span>
          <span v-if="g.lines.length" class="sv-count">{{ $t('search:lines', { count: g.lines.length }) }}</span>
        </button>
        <button v-for="h in g.lines.slice(0, 50)" :key="h.line" class="sv-line" @click="open(g)">
          <span class="sv-ln">{{ h.line }}</span>
          <!-- eslint-disable-next-line vue/no-v-html -->
          <code v-html="marked(h.text)" />
        </button>
      </div>
      <p v-if="result && !groups.length && !error" class="nm-muted sv-empty">{{ $t('search:none') }}</p>
    </div>
  </div>
</template>

<style scoped>
.sv { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--nm-bg); padding: 12px 16px; gap: 8px; }
.sv-head { display: flex; gap: 8px; align-items: center; }
.sv-head .el-button { margin: 0; }
.sv-input { flex: 1; max-width: 640px; }
.sv-opts { display: flex; flex-wrap: wrap; align-items: center; gap: 4px 14px; }
.sv-opts .el-checkbox { margin-right: 0; }
.sv-status { display: flex; align-items: center; gap: 6px; font-size: 12px; color: var(--nm-text-dim); min-height: 18px; }
.sv-found { color: var(--nm-text); }
.sv-warn { color: var(--nm-warning); }
.sv-alert { width: auto; }
.sv-results { flex: 1; overflow: auto; border-top: 1px solid var(--nm-border-soft); padding-top: 6px; }
.sv-group { margin-bottom: 6px; }
.sv-obj, .sv-line {
  display: flex; align-items: center; gap: 8px; width: 100%; border: 0; background: none; cursor: pointer;
  font: inherit; color: var(--nm-text); text-align: left; padding: 3px 6px; border-radius: 3px;
}
.sv-obj:hover, .sv-line:hover { background: var(--ide-hover); }
.sv-obj { font-size: 12.5px; }
.sv-kind { font-size: 10.5px; color: var(--nm-text-dim); text-transform: uppercase; letter-spacing: 0.03em; min-width: 72px; }
.sv-name { color: var(--nm-text-strong); }
.sv-badge { font-size: 10.5px; padding: 0 6px; border-radius: 8px; background: color-mix(in srgb, var(--nm-accent) 22%, transparent); }
.sv-count { font-size: 11px; color: var(--nm-text-dim); }
.sv-line { padding-left: 86px; font-size: 12px; }
.sv-ln { min-width: 36px; text-align: right; color: var(--nm-text-muted); font-variant-numeric: tabular-nums; }
.sv-line code { font-family: var(--nm-mono); white-space: pre; overflow: hidden; text-overflow: ellipsis; color: var(--nm-text); }
.sv-line code :deep(mark) { background: color-mix(in srgb, var(--nm-warning) 40%, transparent); color: inherit; border-radius: 2px; }
.sv-empty { font-size: 12px; padding: 8px 6px; }
</style>
