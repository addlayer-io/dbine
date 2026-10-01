<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import elEn from 'element-plus/es/locale/lang/en.mjs';
import elEs from 'element-plus/es/locale/lang/es.mjs';
import elPt from 'element-plus/es/locale/lang/pt-br.mjs';
import elFr from 'element-plus/es/locale/lang/fr.mjs';
import elIt from 'element-plus/es/locale/lang/it.mjs';
import { SETTING_KEY, isLang, language, setLanguage, t } from './i18n';
import { ElMessage } from 'element-plus';
import FolderDialog from './components/FolderDialog.vue';
import MoveDialog from './components/MoveDialog.vue';
import DatabaseDialogs from './components/DatabaseDialogs.vue';
import SettingsDialog from './components/SettingsDialog.vue';
import McpApprovals from './components/McpApprovals.vue';
import AiSidebar from './components/AiSidebar.vue';
import EditorTabs from './components/EditorTabs.vue';
import ExplorerSidebar from './components/ExplorerSidebar.vue';
import LibrarySidebar from './components/LibrarySidebar.vue';
import HistorySidebar from './components/HistorySidebar.vue';
import LibraryDialogs from './components/LibraryDialogs.vue';
import OutputPanel from './components/OutputPanel.vue';
import StatusBar from './components/StatusBar.vue';
import { errorMessage } from './api/client';
import { newQuery } from './composables/actions';
import { ownsShortcut, useAppMenu } from './composables/appMenu';
import { useConnectionsStore } from './stores/connections';
import { useSettingsStore } from './stores/settings';
import { readJson, writeJson } from './stores/storage';
import { useSyncStore } from './stores/sync';
import { useTabsStore } from './stores/tabs';
import { useUiStore } from './stores/ui';
import ObjectView from './views/ObjectView.vue';
import QueryView from './views/QueryView.vue';
import DesignerTabView from './views/DesignerTabView.vue';
import DiagramTabView from './views/DiagramTabView.vue';
import MigrationView from './views/MigrationView.vue';
import CompareView from './views/CompareView.vue';
import DataCompareView from './views/DataCompareView.vue';
import SecurityView from './views/SecurityView.vue';
import BackupsView from './views/BackupsView.vue';
import IndexUsageView from './views/IndexUsageView.vue';
import ConnectionView from './views/ConnectionView.vue';
import MonitorView from './views/MonitorView.vue';
import ProfilerView from './views/ProfilerView.vue';
import WelcomeView from './views/WelcomeView.vue';

// VS Code-like workbench: activity bar · explorer ·
// editor tabs + editor · bottom panel (output) · status bar. Every open tab
// stays mounted (hidden when inactive) so its editor, results and scroll
// survive switching tabs. Sizes are remembered per viewer.

const conns = useConnectionsStore();
const tabs = useTabsStore();
const ui = useUiStore();
// Revealing something in the explorer needs the explorer visible.
watch(() => ui.reveal?.seq, () => { sidebarOpen.value = true; ui.sidebarView = 'explorer'; });

const sync = useSyncStore();

onMounted(async () => {
  try {
    await Promise.all([conns.load(), useSettingsStore().load()]);
    sync.init();
  } catch (e) {
    ElMessage.error(t('workbench:app.loadFailed', { error: errorMessage(e) }));
    return;
  }
  // Tabs of connections deleted since last run.
  const ids = new Set(conns.list.map((c) => c.id));
  tabs.closeWhere((t) => t.kind !== 'connection' && !ids.has(t.connectionId));
  // Titles of reopened query tabs come from their database's query list.
  for (const t of tabs.tabs) if (t.kind === 'query') conns.loadQueries(t.connectionId, t.database);
});

const sidebarWidth = ref(readJson('dbine.sidebarWidth', 300));
const sidebarOpen = ref(readJson('dbine.sidebarOpen', true));
const panelHeight = ref(readJson('dbine.panelHeight', 180));
const panelOpen = ref(readJson('dbine.panelOpen', false));
watch(sidebarOpen, (v) => writeJson('dbine.sidebarOpen', v));
// The left sidebar shows the Explorer or the script Library (⭐).
ui.sidebarView = readJson<'explorer' | 'library' | 'history'>('dbine.sidebarView', 'explorer');
watch(() => ui.sidebarView, (v) => writeJson('dbine.sidebarView', v));
function toggleView(v: 'explorer' | 'library' | 'history') {
  if (sidebarOpen.value && ui.sidebarView === v) sidebarOpen.value = false;
  else { ui.sidebarView = v; sidebarOpen.value = true; }
}
watch(panelOpen, (v) => writeJson('dbine.panelOpen', v));
// AI assistant (right sidebar, ⌘I): open state and width per viewer.
ui.aiOpen = readJson('dbine.aiOpen', false);
const aiWidth = ref(readJson('dbine.aiWidth', 380));
watch(() => ui.aiOpen, (v) => writeJson('dbine.aiOpen', v));
function dragAi(e: PointerEvent) {
  const start = e.clientX;
  const from = aiWidth.value;
  const move = (ev: PointerEvent) => { aiWidth.value = Math.min(760, Math.max(280, from - (ev.clientX - start))); };
  const up = () => {
    window.removeEventListener('pointermove', move);
    window.removeEventListener('pointerup', up);
    writeJson('dbine.aiWidth', aiWidth.value);
  };
  window.addEventListener('pointermove', move);
  window.addEventListener('pointerup', up);
}

function drag(e: PointerEvent, axis: 'x' | 'y') {
  const start = axis === 'x' ? e.clientX : e.clientY;
  const from = axis === 'x' ? sidebarWidth.value : panelHeight.value;
  const move = (ev: PointerEvent) => {
    if (axis === 'x') sidebarWidth.value = Math.min(640, Math.max(200, from + ev.clientX - start));
    else panelHeight.value = Math.min(window.innerHeight - 200, Math.max(80, from - (ev.clientY - start)));
  };
  const up = () => {
    window.removeEventListener('pointermove', move);
    window.removeEventListener('pointerup', up);
    writeJson(axis === 'x' ? 'dbine.sidebarWidth' : 'dbine.panelHeight', axis === 'x' ? sidebarWidth.value : panelHeight.value);
  };
  window.addEventListener('pointermove', move);
  window.addEventListener('pointerup', up);
}

/** New query where the active tab is. */
function newQueryHere() {
  const tab = tabs.active;
  if (!tab) { ElMessage.info(t('workbench:app.pickDatabase')); return; }
  newQuery(tab.connectionId, tab.database);
}

// ⌘B explorer, ⌘J panel, ⌘W close tab, ⌘N new query (as in VS Code).
function onKey(e: KeyboardEvent) {
  if (!(e.metaKey || e.ctrlKey) || ownsShortcut(e)) return;
  const k = e.key.toLowerCase();
  if (k === 'b') { e.preventDefault(); sidebarOpen.value = !sidebarOpen.value; }
  if (k === 'j') { e.preventDefault(); panelOpen.value = !panelOpen.value; }
  if (k === 'w' && tabs.activeId) { e.preventDefault(); tabs.close(tabs.activeId); }
  if (k === 'n') { e.preventDefault(); newQueryHere(); }
  if (k === ',') { e.preventDefault(); ui.openSettings(); }
  if (k === 'i' && !e.shiftKey) { e.preventDefault(); ui.aiOpen = !ui.aiOpen; }
}
onMounted(() => window.addEventListener('keydown', onKey));
onBeforeUnmount(() => window.removeEventListener('keydown', onKey));
// macOS menu bar; it runs the shortcuts above while it's there.
useAppMenu({ newQuery: newQueryHere, toggleExplorer: () => { sidebarOpen.value = !sidebarOpen.value; }, toggleOutput: () => { panelOpen.value = !panelOpen.value; } });

// Element Plus's own texts (date pickers, "no data"…) in the app's language;
// the synced preference wins over this machine's once settings load.
const settingsStore = useSettingsStore();
const elLocale = computed(() => ({ en: elEn, es: elEs, pt: elPt, fr: elFr, it: elIt })[language.value]);
watch(
  () => settingsStore.get<string | null>(SETTING_KEY, null),
  (lang) => { if (isLang(lang)) setLanguage(lang); },
);
</script>

<template>
  <el-config-provider :locale="elLocale">
  <div class="ide">
    <div class="ide-main" :style="{ gridTemplateColumns: `48px ${sidebarOpen ? sidebarWidth + 'px' : '0px'} 1fr ${ui.aiOpen ? aiWidth + 'px' : ''}` }">
      <nav class="ide-activity" :aria-label="$t('workbench:app.activityBar')">
        <div class="ide-activity-logo" title="DBine">
          <img src="/dbine.png" alt="DBine" width="26" height="26" />
        </div>
        <button class="ide-activity-item" :class="{ active: sidebarOpen && ui.sidebarView === 'explorer' }" :title="$t('workbench:app.explorer')" @click="toggleView('explorer')">
          <el-icon :size="22"><ei-coin /></el-icon>
        </button>
        <button class="ide-activity-item" :class="{ active: sidebarOpen && ui.sidebarView === 'library' }" :title="$t('workbench:app.library')" @click="toggleView('library')">
          <el-icon :size="21"><ei-star /></el-icon>
        </button>
        <button class="ide-activity-item" :class="{ active: sidebarOpen && ui.sidebarView === 'history' }" :title="$t('history:title')" @click="toggleView('history')">
          <el-icon :size="21"><ei-clock /></el-icon>
        </button>
        <button class="ide-activity-item" :title="$t('workbench:app.newQuery')" @click="newQueryHere">
          <el-icon :size="22"><ei-document-add /></el-icon>
        </button>
        <div style="flex: 1;" />
        <button class="ide-activity-item" :class="{ active: panelOpen }" :title="$t('workbench:app.output')" @click="panelOpen = !panelOpen">
          <el-icon :size="20"><ei-tickets /></el-icon>
        </button>
        <button class="ide-activity-item" :class="{ active: ui.aiOpen }" :title="$t('workbench:app.ai')" @click="ui.aiOpen = !ui.aiOpen">
          <el-icon :size="21"><ei-magic-stick /></el-icon>
        </button>
        <button class="ide-activity-item" :class="{ active: ui.settingsSection }" :title="$t('workbench:app.settings')" @click="ui.openSettings()">
          <el-icon :size="21"><ei-setting /></el-icon>
        </button>
      </nav>

      <div class="ide-sidebar" :class="{ hidden: !sidebarOpen }">
        <ExplorerSidebar v-show="ui.sidebarView === 'explorer'" />
        <LibrarySidebar v-if="ui.sidebarView === 'library'" />
        <HistorySidebar v-if="ui.sidebarView === 'history'" />
        <div class="ide-sash-x" @pointerdown.prevent="drag($event, 'x')" />
      </div>

      <div class="ide-editor-col">
        <EditorTabs v-if="tabs.tabs.length" />
        <main class="ide-editor">
          <!-- Restored tabs need their connection loaded before they run anything. -->
          <template v-for="t in conns.loaded ? tabs.tabs : []" :key="t.id">
            <div v-show="t.id === tabs.activeId" class="ide-tab-view">
              <QueryView v-if="t.kind === 'query'" :tab="t" />
              <ObjectView v-else-if="t.kind === 'object'" :tab="t" />
              <DesignerTabView v-else-if="t.kind === 'designer'" :tab="t" />
              <DiagramTabView v-else-if="t.kind === 'diagram'" :tab="t" />
              <MonitorView v-else-if="t.kind === 'monitor'" :tab="t" :active="t.id === tabs.activeId" />
              <ProfilerView v-else-if="t.kind === 'profiler'" :tab="t" />
              <MigrationView v-else-if="t.kind === 'migration'" :tab="t" />
              <CompareView v-else-if="t.kind === 'compare'" :tab="t" />
              <DataCompareView v-else-if="t.kind === 'dataCompare'" :tab="t" />
              <SecurityView v-else-if="t.kind === 'security'" :tab="t" />
              <BackupsView v-else-if="t.kind === 'backups'" :tab="t" />
              <IndexUsageView v-else-if="t.kind === 'indexes'" :tab="t" />
              <ConnectionView v-else-if="t.kind === 'connection'" :tab="t" />
            </div>
          </template>
          <WelcomeView v-if="!tabs.active" />
        </main>
        <template v-if="panelOpen">
          <div class="ide-sash-y" @pointerdown.prevent="drag($event, 'y')" />
          <OutputPanel :style="{ height: panelHeight + 'px' }" @close="panelOpen = false" />
        </template>
      </div>

      <div v-if="ui.aiOpen" class="ide-ai">
        <div class="ide-sash-ai" @pointerdown.prevent="dragAi" />
        <AiSidebar @close="ui.aiOpen = false" />
      </div>
    </div>

    <StatusBar :panel-open="panelOpen" @toggle-panel="panelOpen = !panelOpen" />
    <FolderDialog />
    <MoveDialog />
    <DatabaseDialogs />
    <LibraryDialogs />
    <SettingsDialog />
    <McpApprovals />
  </div>
  </el-config-provider>
</template>

<style scoped>
.ide { display: grid; grid-template-rows: 1fr 22px; height: 100vh; }
.ide-main { display: grid; min-height: 0; overflow: hidden; }
.ide-activity { display: flex; flex-direction: column; align-items: center; background: var(--ide-activity); padding-bottom: 6px; }
.ide-activity-logo { display: flex; align-items: center; justify-content: center; width: 48px; height: 48px; }
.ide-activity-item {
  position: relative; width: 48px; height: 48px; display: flex; align-items: center; justify-content: center;
  border: none; background: transparent; color: #858585; cursor: pointer;
}
.ide-activity-item:hover, .ide-activity-item.active { color: #ffffff; }
.ide-activity-item.active::before { content: ''; position: absolute; left: 0; top: 0; bottom: 0; width: 2px; background: #ffffff; }
.ide-sidebar { position: relative; display: flex; flex-direction: column; min-width: 0; min-height: 0; overflow: hidden; background: var(--ide-sidebar); }
.ide-sidebar > :first-child { flex: 1; }
.ide-sidebar.hidden { visibility: hidden; }
.ide-sash-x { position: absolute; top: 0; right: -2px; width: 4px; height: 100%; cursor: col-resize; z-index: 10; }
.ide-sash-x:hover, .ide-sash-y:hover { background: var(--ide-focus); }
.ide-ai { position: relative; min-width: 0; min-height: 0; }
.ide-sash-ai { position: absolute; left: -2px; top: 0; bottom: 0; width: 5px; cursor: col-resize; z-index: 5; }
.ide-sash-ai:hover { background: var(--ide-focus); }
.ide-editor-col { display: flex; flex-direction: column; min-width: 0; min-height: 0; background: var(--ide-editor); }
.ide-editor { flex: 1; min-height: 0; overflow: hidden; display: flex; flex-direction: column; }
.ide-tab-view { flex: 1; min-height: 0; display: flex; flex-direction: column; }
.ide-tab-view > * { flex: 1; min-height: 0; }
.ide-sash-y { height: 4px; margin-top: -2px; margin-bottom: -2px; cursor: row-resize; z-index: 10; position: relative; }
</style>
