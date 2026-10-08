<script setup lang="ts">
import { computed, nextTick, ref } from 'vue';
import { useTranslation } from 'i18next-vue';
import { tb } from '../i18n/backend';
import { editorBridge } from '../stores/ai';
import { dbKey, useConnectionsStore } from '../stores/connections';
import { baseName, useTabsStore, type Tab } from '../stores/tabs';
import { useProjectsStore } from '../stores/projects';
import { useUiStore } from '../stores/ui';
import ContextMenu, { type MenuItem } from './ContextMenu.vue';
import EngineIcon from './EngineIcon.vue';

// Editor tab strip, like open files in VS Code. Preview tabs show their
// title in italics until pinned (double-click or edit). With tabs of more
// than one connection open, each connection's tabs are a group (as in
// Chrome): a chip with its name and color, which collapses the group.

const tabs = useTabsStore();
const conns = useConnectionsStore();
const ui = useUiStore();
const projects = useProjectsStore();
// Reactive t: tab titles follow a language switch.
const { t: tr } = useTranslation();

/** Show the tab's query / object in the explorer (and pin the tab). */
function reveal(t: Tab) {
  tabs.pin(t.id);
  // A project's file: its project in Proyectos.
  if (t.kind === 'file' || t.kind === 'fileDiff') { projects.focusProject(t.projectId); return; }
  ui.revealInExplorer(t.kind === 'query'
    ? { connectionId: t.connectionId, database: t.database, queryId: t.queryId }
    : t.kind === 'object'
      ? { connectionId: t.connectionId, database: t.database, object: t.object }
      : { connectionId: t.connectionId, database: t.database });
}

function title(t: Tab): string {
  if (t.kind === 'object') return t.object.name;
  if (t.kind === 'file') return baseName(t.path);
  if (t.kind === 'fileDiff') return `${baseName(t.path)} ${tr('workbench:tabs.fileDiffSuffix')}`;
  if (t.kind === 'designer') return conns.driverOf(t.connectionId)?.designer?.label ? tb(conns.driverOf(t.connectionId)!.designer!.label) : tr('workbench:tabs.designer');
  if (t.kind === 'monitor') return `${tr('workbench:tabs.monitor')} · ${conns.byId(t.connectionId)?.name || ''}`;
  if (t.kind === 'profiler') return `Profiler · ${t.database || conns.byId(t.connectionId)?.name || ''}`;
  if (t.kind === 'connection') {
    if (t.editId) return `${tr('workbench:tabs.connection')} · ${conns.byId(t.editId)?.name ?? ''}`;
    return t.duplicateOf ? tr('workbench:tabs.duplicateConnection') : tr('workbench:tabs.newConnection');
  }
  if (t.kind === 'compare') return `${tr('workbench:tabs.compare')} · ${t.database || conns.byId(t.connectionId)?.name || ''}`;
  if (t.kind === 'indexes') return `${tr('explorer:indexes.tab')} · ${t.object.name}`;
  if (t.kind === 'search') return `${tr('search:tab')} · ${t.database}`;
  if (t.kind === 'dependencies') return `${tr('dependencies:tab')} · ${t.column ? `${t.object.name}.${t.column}` : t.object.name}`;
  if (t.kind === 'backups') return `${tr('backups:tab')} · ${t.database || conns.byId(t.connectionId)?.name || ''}`;
  if (t.kind === 'security') return `${tr('security:tab')} · ${t.database || conns.byId(t.connectionId)?.name || ''}`;
  if (t.kind === 'dataCompare') return `${tr('dataCompare:tab')} · ${t.object?.name || t.database || conns.byId(t.connectionId)?.name || ''}`;
  if (t.kind === 'migration') {
    const saved = t.migrationId ? conns.migrationById(t.migrationId) : undefined;
    return `${tr('workbench:tabs.migrate')} · ${saved?.name || t.database || conns.byId(t.connectionId)?.name || ''}`;
  }
  if (t.kind === 'diagram') return `${tr('workbench:tabs.diagram')} · ${t.database || conns.byId(t.connectionId)?.name || ''}`;
  if (t.kind !== 'query') return '';
  const q = conns.queries[dbKey(t.connectionId, t.database)]?.items.find((x) => x.id === t.queryId);
  return q?.name ?? tr('workbench:tabs.query');
}
function tooltip(t: Tab): string {
  const c = conns.byId(t.connectionId)?.name ?? '';
  if (t.kind === 'file' || t.kind === 'fileDiff') {
    const p = projects.byId(t.projectId)?.name ?? '';
    if (t.kind === 'fileDiff') return `${p} › ${t.path}`;
    return `${p} › ${t.path} · ${c ? `${c} › ${t.database}` : tr('workbench:tabs.noBase')}`;
  }
  const where = [c, t.database].filter(Boolean).join(' · ');
  return t.kind === 'object' ? `${t.object.schema ? t.object.schema + '.' : ''}${t.object.name} — ${where}` : `${title(t)} — ${where}`;
}

// Chrome's group colors, for connections without a color of their own.
const GROUP_COLORS = ['#8ab4f8', '#f28b82', '#fdd663', '#81c995', '#ff8bcb', '#c58af9', '#78d9ec', '#fcad70', '#9aa0a6'];
function groupColor(connectionId: string): string {
  const own = conns.colorOf(connectionId);
  if (own) return own;
  const h = [...connectionId].reduce((n, c) => (n * 31 + c.charCodeAt(0)) >>> 0, 7);
  return GROUP_COLORS[h % GROUP_COLORS.length];
}

/** Groups show once tabs of two connections are open (the new-connection
 *  form belongs to none). */
const grouping = computed(() => new Set(tabs.tabs.map((t) => t.connectionId).filter(Boolean)).size > 1);

type Row =
  | { type: 'group'; connectionId: string; name: string; driver: string; color: string; count: number; collapsed: boolean }
  | { type: 'tab'; t: Tab; title: string; tooltip: string; color: string | null; icon: string };

const rows = computed<Row[]>(() => {
  const titles = tabs.tabs.map(title);
  const out: Row[] = [];
  let group: string | null = null;
  tabs.tabs.forEach((t, i) => {
    const inGroup = grouping.value && !!t.connectionId;
    if (inGroup && t.connectionId !== group) {
      const c = conns.byId(t.connectionId);
      const color = groupColor(t.connectionId);
      out.push({
        type: 'group', connectionId: t.connectionId, name: c?.name ?? tr('workbench:tabs.connection'), driver: c?.config.driver ?? '',
        color, count: tabs.tabs.filter((x) => x.connectionId === t.connectionId).length,
        collapsed: tabs.collapsed.includes(t.connectionId),
      });
    }
    group = inGroup ? t.connectionId : null;
    if (inGroup && tabs.collapsed.includes(t.connectionId)) return;
    out.push({
      type: 'tab', t,
      // Same title twice (a table in two databases): say where each one is.
      title: titles.filter((x) => x === titles[i]).length > 1
        ? `${titles[i]} · ${t.database || conns.byId(t.connectionId)?.name || ''}`
        : titles[i],
      tooltip: tooltip(t),
      color: inGroup ? groupColor(t.connectionId) : conns.colorOf(t.connectionId),
      icon: { query: 'document', file: 'document', fileDiff: 'files', object: 'grid', designer: 'edit-pen', diagram: 'share', monitor: 'odometer', profiler: 'view', migration: 'switch', compare: 'files', dataCompare: 'data-analysis', security: 'user', backups: 'box', indexes: 'collection', dependencies: 'link', search: 'search', connection: 'connection' }[t.kind],
    });
  });
  return out;
});

// -- rename in place: a click on the tab that's already active (a query)
// turns its title into an input. The first click only switches tabs; a
// double click still reveals it in the explorer.
const renaming = ref<{ id: string; text: string } | null>(null);
const renameInput = ref<HTMLInputElement[] | null>(null);
let renameTimer: ReturnType<typeof setTimeout> | undefined;

function onTabClick(t: Tab) {
  if (t.id !== tabs.activeId) {
    tabs.activate(t.id);
    return;
  }
  if (t.kind !== 'query' || renaming.value) return;
  clearTimeout(renameTimer);
  renameTimer = setTimeout(() => startRename(t), 350);
}
function onTabDblClick(t: Tab) {
  clearTimeout(renameTimer);
  if (renaming.value) return;
  reveal(t);
}
function startRename(t: Tab) {
  renaming.value = { id: t.id, text: title(t) };
  nextTick(() => {
    const el = renameInput.value?.[0];
    el?.focus();
    el?.select();
  });
}
async function commitRename() {
  const r = renaming.value;
  if (!r) return;
  renaming.value = null;
  const name = r.text.trim();
  const t = tabs.tabs.find((x) => x.id === r.id);
  if (!t || !name || name === title(t)) return;
  await editorBridge(t.id)?.rename?.(name);
}

const menu = ref<{ x: number; y: number; items: MenuItem[] } | null>(null);
function onContext(e: MouseEvent, t: Tab) {
  e.preventDefault();
  menu.value = {
    x: e.clientX, y: e.clientY,
    items: [
      ...(t.kind === 'query' ? [{ label: tr('common:rename'), action: () => { tabs.activate(t.id); startRename(t); } }] : []),
      t.kind === 'file' || t.kind === 'fileDiff'
        ? { label: tr('workbench:tabs.revealInProjects'), shortcut: tr('workbench:tabs.doubleClick'), action: () => reveal(t) }
        : { label: tr('workbench:tabs.revealInExplorer'), shortcut: tr('workbench:tabs.doubleClick'), action: () => reveal(t) },
      ...(t.connectionId ? [{ label: tr('workbench:tabs.goToDatabase'), action: () => ui.revealInExplorer({ connectionId: t.connectionId, database: t.database, menu: true }) }] : []),
      { label: tr('common:close'), action: () => tabs.close(t.id), divided: true },
      { label: tr('workbench:tabs.closeOthers'), action: () => tabs.closeOthers(t.id) },
      ...(grouping.value && t.connectionId
        ? [{ label: tr('workbench:tabs.closeConnection'), action: () => tabs.closeGroup(t.connectionId) }]
        : []),
      { label: tr('workbench:tabs.closeAll'), action: () => tabs.closeAll() },
      ...(t.preview ? [{ label: tr('workbench:tabs.pin'), action: () => tabs.pin(t.id), divided: true }] : []),
    ],
  };
}
function onGroupContext(e: MouseEvent, g: Extract<Row, { type: 'group' }>) {
  e.preventDefault();
  menu.value = {
    x: e.clientX, y: e.clientY,
    items: [
      { label: g.collapsed ? tr('workbench:tabs.expandGroup') : tr('workbench:tabs.collapseGroup'), action: () => tabs.toggleGroup(g.connectionId) },
      { label: tr('workbench:tabs.revealConnection'), action: () => ui.revealInExplorer({
        connectionId: g.connectionId, database: tabs.tabs.find((t) => t.connectionId === g.connectionId)?.database ?? '',
      }) },
      { label: tr('workbench:tabs.closeGroup', { n: g.count }), divided: true, action: () => tabs.closeGroup(g.connectionId) },
      { label: tr('workbench:tabs.closeOtherGroups'), action: () => tabs.closeOtherGroups(g.connectionId) },
    ],
  };
}
</script>

<template>
  <div class="et" role="tablist">
    <template v-for="r in rows" :key="r.type === 'tab' ? r.t.id : `g:${r.connectionId}`">
      <button
        v-if="r.type === 'group'"
        class="et-group"
        :class="{ collapsed: r.collapsed }"
        :style="{ '--g': r.color }"
        :title="$t(r.collapsed ? 'workbench:tabs.groupTitleExpand' : 'workbench:tabs.groupTitleCollapse', { name: r.name, count: r.count })"
        :aria-expanded="!r.collapsed"
        @click="tabs.toggleGroup(r.connectionId)"
        @contextmenu="onGroupContext($event, r)"
      >
        <EngineIcon v-if="r.driver" :id="r.driver" :name="r.name" :size="13" />
        <span class="et-group-name">{{ r.name }}</span>
        <span v-if="r.collapsed" class="et-group-count">· {{ r.count }}</span>
      </button>
      <div
        v-else
        class="et-tab"
        :class="{ active: r.t.id === tabs.activeId, preview: r.t.preview, grouped: grouping && !!r.t.connectionId }"
        role="tab"
        :aria-selected="r.t.id === tabs.activeId"
        :title="r.tooltip"
        :style="r.color ? { '--tab-color': r.color } : undefined"
        @click="onTabClick(r.t)"
        @dblclick="onTabDblClick(r.t)"
        @mousedown.middle.prevent="tabs.close(r.t.id)"
        @contextmenu="onContext($event, r.t)"
      >
        <el-icon class="et-icon" :class="r.t.kind"><component :is="`ei-${r.icon}`" /></el-icon>
        <input
          v-if="renaming?.id === r.t.id"
          ref="renameInput"
          v-model="renaming.text"
          class="et-rename"
          spellcheck="false"
          @click.stop
          @dblclick.stop
          @keydown.enter.prevent="commitRename"
          @keydown.esc.prevent="renaming = null"
          @blur="commitRename"
        />
        <span v-else class="et-name">{{ r.title }}</span>
        <span
          v-if="ui.unsaved[r.t.id]"
          class="et-dirty"
          :class="ui.unsaved[r.t.id]"
          :title="ui.unsaved[r.t.id] === 'error' ? $t('workbench:tabs.saveFailed') : $t('workbench:tabs.unsaved')"
        />
        <button class="et-close ide-icon-btn" :title="$t('common:close')" @click.stop="tabs.close(r.t.id)">
          <el-icon :size="13"><ei-close /></el-icon>
        </button>
      </div>
    </template>
    <div class="et-fill" />
    <ContextMenu v-if="menu" :x="menu.x" :y="menu.y" :items="menu.items" @close="menu = null" />
  </div>
</template>

<style scoped>
.et { display: flex; height: 35px; flex-shrink: 0; background: var(--ide-tabs); overflow-x: auto; overflow-y: hidden; }
.et::-webkit-scrollbar { height: 3px; }
.et-tab {
  position: relative; display: flex; align-items: center; gap: 6px; min-width: 0; max-width: 220px;
  padding: 0 6px 0 10px; background: var(--ide-tab); border-right: 1px solid var(--ide-tabs);
  color: #969696; cursor: pointer; user-select: none; flex-shrink: 0; font-size: 13px;
}
.et-tab.active { background: var(--ide-tab-active); color: #ffffff; }
.et-tab.active::before {
  content: ''; position: absolute; left: 0; right: 0; top: 0; height: 1px;
  background: var(--tab-color, var(--ide-focus));
}
.et-tab::after {
  content: ''; position: absolute; left: 0; right: 0; bottom: 0; height: 2px;
  background: var(--tab-color, transparent); opacity: 0.6;
}
.et-tab.preview .et-name { font-style: italic; }
.et-icon { font-size: 14px; flex-shrink: 0; }
.et-icon.query { color: #75beff; }
.et-icon.object { color: #4ec9b0; }
.et-icon.file { color: #e5c07b; }
.et-icon.fileDiff { color: #e5c07b; }
.et-name { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.et-dirty { width: 7px; height: 7px; border-radius: 50%; flex: none; background: var(--nm-text-strong); opacity: 0.85; }
.et-dirty.error { background: var(--nm-danger); opacity: 1; }
.et-rename {
  width: 150px; height: 20px; padding: 0 4px; box-sizing: border-box; border: 1px solid var(--ide-focus, var(--nm-accent)); border-radius: 2px;
  background: var(--ide-input, #1e1e1e); color: var(--nm-text-strong); font: inherit; outline: none;
}
.et-close { opacity: 0; color: inherit; }
.et-tab:hover .et-close, .et-tab.active .et-close { opacity: 1; }
.et-fill { flex: 1; }
/* Tab groups (by connection), as in Chrome but flat: a header the height
   of a tab with the connection's name in its color, and one colored line
   under the header and all its tabs. */
.et-group {
  position: relative; display: flex; align-items: center; gap: 6px; flex-shrink: 0; max-width: 190px;
  padding: 0 10px 0 12px; border: 0; border-left: 1px solid rgba(255, 255, 255, 0.07); cursor: pointer;
  background: transparent; font: 600 12px/1 inherit; white-space: nowrap;
  color: color-mix(in srgb, var(--g) 75%, #ffffff);
}
.et-group:first-child { border-left: 0; }
.et-group::after {
  content: ''; position: absolute; left: 0; right: 0; bottom: 0; height: 2px; background: var(--g);
}
.et-group:hover { background: var(--ide-hover); }
.et-group:focus-visible { outline: 1px solid var(--ide-focus); outline-offset: -1px; }
.et-group-name { overflow: hidden; text-overflow: ellipsis; }
.et-group-count { font-weight: 500; color: var(--nm-text-muted); }
.et-tab.grouped::after { opacity: 1; }
</style>
