<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { api, errorMessage } from '../api/client';
import { locale, t } from '../i18n';
import { tb } from '../i18n/backend';
import type { QueryOutcome, SavedQuery } from '../api/types';
import CodeEditor from '../components/CodeEditor.vue';
import ResultsPane from '../components/ResultsPane.vue';
import { dbKey, objKey, useConnectionsStore } from '../stores/connections';
import { useOutputStore } from '../stores/output';
import { useSettingsStore } from '../stores/settings';
import { useTabsStore, type QueryTab } from '../stores/tabs';
import { useUiStore } from '../stores/ui';
import { registerEditor } from '../stores/ai';
import { useLibraryStore } from '../stores/library';
import { readJson, writeJson } from '../stores/storage';
import { formatCode, formatUnavailable } from '../composables/formatCode';

// A saved query open in the editor. Its text is saved as you type (to the
// state store, under its database in the explorer); running uses the tab's
// own session, so SETs, temp tables and transactions persist between runs.

const props = defineProps<{ tab: QueryTab }>();

const conns = useConnectionsStore();
const tabs = useTabsStore();
const ui = useUiStore();
const output = useOutputStore();

const query = ref<SavedQuery | null>(null);
const loadError = ref<string | null>(null);
const text = ref('');
const saveState = ref<'saved' | 'saving' | 'dirty' | 'error'>('saved');
const outcome = ref<QueryOutcome | null>(null);
/** The script behind `outcome` (exports run it again for every row). */
const lastScript = ref('');
const running = ref(false);
const settings = useSettingsStore();
const maxRows = ref(settings.get('query.maxRows', 5000));
watch(maxRows, (v) => { if (v !== settings.get('query.maxRows', 5000)) settings.set('query.maxRows', v); });
// Changed in Configuración or by a sync.
watch(() => settings.values['query.maxRows'], (v) => { if (typeof v === 'number') maxRows.value = v; });

const conn = computed(() => conns.byId(props.tab.connectionId));
const driver = computed(() => conns.driverOf(props.tab.connectionId));
/** The server's databases once connected; before that, just the tab's
 *  (opening the select connects and loads the rest). */
const databases = computed(() => {
  const live = conns.live[props.tab.connectionId]?.databases ?? [];
  return live.length ? live : props.tab.database ? [props.tab.database] : [];
});
const loadingDatabases = ref(false);
async function onDatabaseMenu(visible: boolean) {
  if (!visible || conns.live[props.tab.connectionId]?.databases?.length) return;
  loadingDatabases.value = true;
  try { await conns.ensureConnected(props.tab.connectionId); } finally { loadingDatabases.value = false; }
}

async function load() {
  loadError.value = null;
  try {
    const q = await api.getQuery(props.tab.queryId);
    query.value = q;
    text.value = q.sql;
    saveState.value = 'saved';
  } catch (e) {
    loadError.value = errorMessage(e);
  }
}
watch(() => props.tab.queryId, load, { immediate: true });
// A restore from the cloud backup may have changed it (not while editing).
watch(() => ui.syncSeq, () => { if (saveState.value === 'saved') load(); });

// -- autosave ------------------------------------------------------------------
let timer: ReturnType<typeof setTimeout> | null = null;
watch(text, (v) => {
  if (!query.value || v === query.value.sql) return;
  saveState.value = 'dirty';
  tabs.pin(props.tab.id);
  if (timer) clearTimeout(timer);
  timer = setTimeout(save, 600);
});

async function save() {
  if (timer) { clearTimeout(timer); timer = null; }
  if (!query.value) return;
  saveState.value = 'saving';
  try {
    query.value = await conns.saveQuery({ ...query.value, sql: text.value });
    saveState.value = 'saved';
  } catch (e) {
    saveState.value = 'error';
    ElMessage.error(t('query:saveFailed', { error: errorMessage(e) }));
  }
}
onBeforeUnmount(() => { if (saveState.value === 'dirty') save(); });
// The tab strip shows a dot while there are changes not saved.
watch(saveState, (v) => {
  if (v === 'saved') delete ui.unsaved[props.tab.id];
  else ui.unsaved[props.tab.id] = v;
});
onBeforeUnmount(() => { delete ui.unsaved[props.tab.id]; });

// The AI assistant reads this tab's editor and adds code to it (it never
// runs anything).
const unregisterAi = registerEditor(props.tab.id, {
  text: () => text.value,
  selection: () => editor.value?.selectionText() ?? '',
  lastError: () => outcome.value?.error ?? null,
  append: (code) => { if (editor.value) editor.value.appendText(code); else text.value = `${text.value.replace(/\s+$/, '')}\n\n${code}\n`; },
  replace: (code) => { if (editor.value) editor.value.replaceAll(code); else text.value = code; },
  rename: (name) => rename(name),
});
onBeforeUnmount(unregisterAi);

async function rename(name: string) {
  if (!query.value || !name.trim() || name === query.value.name) return;
  query.value = await conns.saveQuery({ ...query.value, name: name.trim(), sql: text.value });
}

async function changeDatabase(db: string) {
  if (!query.value) return;
  query.value = await conns.saveQuery({ ...query.value, database: db, sql: text.value });
  tabs.retarget(props.tab.id, props.tab.connectionId, db);
  conns.loadObjects(props.tab.connectionId, db);
}

// -- completion schema ------------------------------------------------------------
const schema = computed(() => {
  const objs = conns.objects[dbKey(props.tab.connectionId, props.tab.database)]?.items ?? [];
  const out: Record<string, string[]> = {};
  for (const o of objs) {
    const kind = driver.value?.object_kinds.find((k) => k.id === o.kind);
    if (!kind?.has_columns) continue;
    const cols = conns.columns[objKey(props.tab.connectionId, props.tab.database, o.schema, o.name)]?.items.map((c) => c.name) ?? [];
    out[o.name] = cols;
    if (o.schema) out[`${o.schema}.${o.name}`] = cols;
  }
  return out;
});

// Completion reads objects and columns from the store, which only the explorer
// filled. The tab loads its database's objects itself once the connection is
// open (it never connects just for this).
watch(() => [conns.live[props.tab.connectionId]?.status, props.tab.database] as const, ([status, db]) => {
  if (status === 'connected') conns.loadObjects(props.tab.connectionId, db);
}, { immediate: true });

// Columns are loaded per table, only for the tables the text names.
let columnsTimer: ReturnType<typeof setTimeout> | null = null;
watch([text, () => conns.objects[dbKey(props.tab.connectionId, props.tab.database)]?.items], () => {
  if (columnsTimer) clearTimeout(columnsTimer);
  columnsTimer = setTimeout(loadReferencedColumns, 400);
});
onBeforeUnmount(() => { if (columnsTimer) clearTimeout(columnsTimer); });
function loadReferencedColumns() {
  const { connectionId, database } = props.tab;
  const objs = conns.objects[dbKey(connectionId, database)]?.items ?? [];
  if (!objs.length) return;
  const words = new Set(text.value.toLowerCase().match(/[\w$#]+/g) ?? []);
  let started = 0;
  for (const o of objs) {
    if (started >= 20) break;
    if (!words.has(o.name.toLowerCase()) || conns.columns[objKey(connectionId, database, o.schema, o.name)]) continue;
    if (!driver.value?.object_kinds.find((k) => k.id === o.kind)?.has_columns) continue;
    conns.loadColumns(connectionId, database, o);
    started++;
  }
}

/** ⭐ Save the selection (or the whole query) as a Library script. */
function saveToLibrary() {
  const sel = editor.value?.selectionText() ?? '';
  const lib = useLibraryStore();
  lib.newScript(sel.trim() ? sel : text.value, driver.value ? [driver.value.id] : [], sel.trim() ? '' : query.value?.name ?? '');
}

/** UPDATE code from edited result cells: at the end of the query, selected. */
function appendScript(code: string) {
  if (editor.value) editor.value.appendText(code);
  else text.value = `${text.value.replace(/\s+$/, '')}\n\n${code}\n`;
  ElMessage.success(t('query:scriptAppended'));
}

// -- run ----------------------------------------------------------------------------
const editor = ref<InstanceType<typeof CodeEditor> | null>(null);

// -- Formatear (⇧⌥F): the selection, or everything --------------------------------------
const formatting = ref(false);
const formatOff = computed(() => formatUnavailable(driver.value?.language));
async function formatQuery() {
  if (formatOff.value) {
    ElMessage.info({ message: formatOff.value, duration: 3000 });
    return;
  }
  formatting.value = true;
  try {
    await editor.value?.format((t) => formatCode(t, driver.value?.language, driver.value?.dialect ?? ''));
  } catch (e) {
    ElMessage.warning({ message: t('query:formatFailed', { error: errorMessage(e) }), duration: 4500 });
  } finally {
    formatting.value = false;
  }
}

type PlanMode = 'none' | 'estimated' | 'actual';

async function run(sqlText?: string, plan: PlanMode = 'none') {
  const script = (sqlText ?? editor.value?.runnableText() ?? text.value).trim();
  if (!script || running.value) return;
  if (!(await conns.ensureConnected(props.tab.connectionId))) return;
  if (saveState.value === 'dirty') save();
  running.value = true;
  lastScript.value = script;
  const where = `${conn.value?.name ?? ''} · ${props.tab.database || t('query:defaultDatabase')}`;
  try {
    const o = await api.executeQuery({
      sessionId: props.tab.id, connectionId: props.tab.connectionId, database: props.tab.database,
      sql: script, maxRows: maxRows.value, queryId: props.tab.queryId, plan, record: true,
    }).finally(() => { ui.historySeq++; });
    outcome.value = o;
    const firstLine = script.split('\n').find((l) => l.trim())?.trim().slice(0, 120) ?? '';
    if (o.error) output.add('error', `${firstLine}\n${tb(o.error)}`, { where, elapsedMs: o.elapsed_ms });
    else output.add('info', firstLine, { where, elapsedMs: o.elapsed_ms });
  } catch (e) {
    outcome.value = { results: [], messages: [], error: errorMessage(e), elapsed_ms: 0, plans: [] };
    output.add('error', errorMessage(e), { where });
  } finally {
    running.value = false;
  }
}

function cancel() {
  api.cancelQuery(props.tab.id).catch(() => {});
}

// -- editor / results split -----------------------------------------------------------
const split = ref(readJson('dbine.querySplit', 0.45));
const col = ref<HTMLDivElement | null>(null);
function drag(e: PointerEvent) {
  const box = col.value!.getBoundingClientRect();
  const move = (ev: PointerEvent) => {
    split.value = Math.min(0.85, Math.max(0.12, (ev.clientY - box.top) / box.height));
  };
  const up = () => {
    window.removeEventListener('pointermove', move);
    window.removeEventListener('pointerup', up);
    writeJson('dbine.querySplit', split.value);
  };
  window.addEventListener('pointermove', move);
  window.addEventListener('pointerup', up);
  e.preventDefault();
}
</script>

<template>
  <div v-if="loadError" class="nm-content">
    <el-alert type="error" :title="loadError" :closable="false" />
  </div>
  <div v-else class="qv">
    <div class="nm-toolbar">
      <template v-if="!running">
        <el-button type="primary" :title="$t('query:runTitle')" @click="run()">
          <el-icon><ei-video-play /></el-icon>&nbsp;{{ $t('common:run') }}
        </el-button>
        <el-button-group v-if="driver?.supports_explain">
          <el-tooltip :content="$t('query:estimatedPlanTip')" placement="bottom" :show-after="300">
            <el-button :aria-label="$t('query:estimatedPlan')" @click="run(undefined, 'estimated')"><el-icon><ei-share /></el-icon></el-button>
          </el-tooltip>
          <el-tooltip :content="$t('query:actualPlanTip')" placement="bottom" :show-after="300">
            <el-button :aria-label="$t('query:runWithPlan')" @click="run(undefined, 'actual')"><el-icon><ei-data-analysis /></el-icon></el-button>
          </el-tooltip>
        </el-button-group>
      </template>
      <el-button v-else type="danger" @click="cancel">
        <el-icon><ei-video-pause /></el-icon>&nbsp;{{ $t('common:cancel') }}
      </el-button>
      <el-tooltip :content="formatOff ?? $t('query:formatTip')" placement="bottom" :show-after="400">
        <el-button :disabled="!!formatOff" :loading="formatting" :aria-label="$t('query:format')" @click="formatQuery">
          <el-icon v-if="!formatting"><ei-magic-stick /></el-icon>&nbsp;{{ $t('query:format') }}
        </el-button>
      </el-tooltip>
      <el-select
        v-if="driver?.databases_label !== ''"
        :model-value="tab.database"
        filterable
        size="small"
        style="width: 234px"
        :loading="loadingDatabases"
        :placeholder="driver?.databases_label ? tb(driver.databases_label) : $t('query:database')"
        :title="tab.database"
        @visible-change="onDatabaseMenu"
        @update:model-value="changeDatabase"
      >
        <el-option v-for="d in databases" :key="d" :label="d" :value="d" />
      </el-select>
      <el-tooltip :content="$t('query:saveToLibraryTip')" placement="bottom" :show-after="300">
        <el-button link :aria-label="$t('query:saveToLibrary')" @click="saveToLibrary"><el-icon :size="15"><ei-star /></el-icon></el-button>
      </el-tooltip>
      <div class="nm-spacer" />
      <el-popover v-if="driver?.query_help" placement="bottom-end" :width="520" trigger="click">
        <template #reference>
          <el-button link :title="$t('query:syntaxTitle')"><el-icon><ei-question-filled /></el-icon>&nbsp;{{ $t('query:syntax') }}</el-button>
        </template>
        <pre class="qv-help nm-selectable">{{ tb(driver.query_help) }}</pre>
      </el-popover>
      <span class="nm-muted">{{ $t('query:maxRows') }}</span>
      <el-select v-model="maxRows" size="small" style="width: 96px">
        <el-option v-for="n in [100, 1000, 5000, 20000, 100000]" :key="n" :label="n.toLocaleString(locale())" :value="n" />
      </el-select>
    </div>
    <div ref="col" class="qv-body">
      <div class="qv-editor" :style="{ height: split * 100 + '%' }">
        <CodeEditor
          ref="editor"
          v-model="text"
          :language="driver?.language"
          :dialect="driver?.dialect"
          :schema="schema"
          :placeholder="$t('query:editorPlaceholder')"
          @run="run"
          @plan="(t: string, actual: boolean) => run(t, actual ? 'actual' : 'estimated')"
          @save="save"
          @format="formatQuery"
        />
      </div>
      <div class="qv-sash" @pointerdown="drag" />
      <div class="qv-results">
        <ResultsPane
          :outcome="outcome"
          :running="running"
          :source="lastScript ? { connectionId: tab.connectionId, database: tab.database, sql: lastScript } : null"
          :title="query?.name ?? $t('query:resultName')"
          :dialect="driver?.dialect ?? ''"
          :edit-source="lastScript ? { connectionId: tab.connectionId, database: tab.database, language: driver?.language ?? 'sql', script: lastScript } : null"
          @script="appendScript"
        />
      </div>
    </div>
  </div>
</template>

<style scoped>
.qv { display: flex; flex-direction: column; height: 100%; min-height: 0; }
.qv-help { margin: 0; max-height: 60vh; overflow: auto; white-space: pre-wrap; font-family: var(--nm-mono); font-size: 12px; line-height: 1.5; }
.qv-body { flex: 1; min-height: 0; display: flex; flex-direction: column; }
.qv-editor { min-height: 60px; overflow: hidden; }
.qv-sash { height: 5px; flex-shrink: 0; cursor: row-resize; border-top: 1px solid var(--nm-border-soft); }
.qv-sash:hover { background: var(--ide-focus); }
.qv-results { flex: 1; min-height: 0; }
</style>
