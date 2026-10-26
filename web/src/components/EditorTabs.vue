<script setup lang="ts">
import { computed, nextTick, ref } from 'vue';
import { useTranslation } from 'i18next-vue';
import { editorBridge } from '../stores/ai';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore, type Tab } from '../stores/tabs';
import { useProjectsStore } from '../stores/projects';
import { useUiStore } from '../stores/ui';
import { tabTitle } from '../composables/tabTitle';
import ContextMenu, { type MenuItem } from './ContextMenu.vue';
import EngineIcon from './EngineIcon.vue';
import { MAX_GROUPS } from '../stores/editorGroups';
import { isMac } from '../composables/shortcuts';

// Editor tab strip, like open files in VS Code. Preview tabs show their
// title in italics until pinned (double-click or edit). With tabs of more
// than one connection open, each connection's tabs are a group (as in
// Chrome): a chip with its name and color, which collapses the group.
// Split editor: one strip per editor group (`group`); its tabs can be
// dragged to reorder them, to another group's strip, or onto the editor
// area (EditorArea.vue) to split it.

const props = defineProps<{ group?: string }>();
const tabs = useTabsStore();
/** This strip's editor group (the first one when not given). */
const gid = computed(() => props.group ?? tabs.groups[0].id);
const mine = computed(() => tabs.tabsOf(gid.value));
const activeId = computed(() => tabs.activeOf(gid.value));
/** Whether this group has the focus (its active tab is the app's active tab). */
const focused = computed(() => tabs.focused === gid.value);
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
  if (t.kind === 'notebook') { if (t.projectId) projects.focusProject(t.projectId); return; }
  ui.revealInExplorer(t.kind === 'query'
    ? { connectionId: t.connectionId, database: t.database, queryId: t.queryId }
    : t.kind === 'object'
      ? { connectionId: t.connectionId, database: t.database, object: t.object }
      : { connectionId: t.connectionId, database: t.database });
}

/** The tab's title, in the current language (reactive through `tr`). */
const title = (x: Tab) => tabTitle(x, tr);
function tooltip(t: Tab): string {
  const c = conns.byId(t.connectionId)?.name ?? '';
  if (t.kind === 'file' || t.kind === 'fileDiff') {
    const p = projects.byId(t.projectId)?.name ?? '';
    if (t.kind === 'fileDiff') return `${p} › ${t.path}`;
    return `${p} › ${t.path} · ${c ? `${c} › ${t.database}` : tr('workbench:tabs.noBase')}`;
  }
  if (t.kind === 'notebook') {
    const where = t.projectId ? `${projects.byId(t.projectId)?.name ?? ''} › ${t.projectPath}` : t.path ?? title(t);
    return c ? `${where} · ${c}${t.database ? ` › ${t.database}` : ''}` : where;
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
const grouping = computed(() => new Set(mine.value.map((t) => t.connectionId).filter(Boolean)).size > 1);

type Row =
  | { type: 'group'; connectionId: string; name: string; driver: string; color: string; count: number; collapsed: boolean }
  | { type: 'tab'; t: Tab; title: string; tooltip: string; color: string | null; icon: string };

const rows = computed<Row[]>(() => {
  const list = mine.value;
  const titles = list.map(title);
  const out: Row[] = [];
  let group: string | null = null;
  list.forEach((t, i) => {
    const inGroup = grouping.value && !!t.connectionId;
    if (inGroup && t.connectionId !== group) {
      const c = conns.byId(t.connectionId);
      const color = groupColor(t.connectionId);
      out.push({
        type: 'group', connectionId: t.connectionId, name: c?.name ?? tr('workbench:tabs.connection'), driver: c?.config.driver ?? '',
        color, count: list.filter((x) => x.connectionId === t.connectionId).length,
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
      icon: { query: 'document', file: 'document', notebook: 'notebook', fileDiff: 'files', object: 'grid', designer: 'edit-pen', diagram: 'share', monitor: 'odometer', profiler: 'view', migration: 'switch', compare: 'files', dataCompare: 'data-analysis', security: 'user', backups: 'box', indexes: 'collection', dependencies: 'link', search: 'search', health: 'first-aid-kit', scheduledTask: 'alarm-clock', connection: 'connection', queryBuilder: 'set-up', subset: 'scissor', optimizer: 'trend-charts' }[t.kind],
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
  if (t.id !== activeId.value || !focused.value) {
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
      t.kind === 'file' || t.kind === 'fileDiff' || (t.kind === 'notebook' && t.projectId)
        ? { label: tr('workbench:tabs.revealInProjects'), shortcut: tr('workbench:tabs.doubleClick'), action: () => reveal(t) }
        : { label: tr('workbench:tabs.revealInExplorer'), shortcut: tr('workbench:tabs.doubleClick'), action: () => reveal(t) },
      ...(t.connectionId ? [{ label: tr('workbench:tabs.goToDatabase'), action: () => ui.revealInExplorer({ connectionId: t.connectionId, database: t.database, menu: true }) }] : []),
      { label: tr('split:right'), shortcut: splitKey, disabled: !canSplit.value, action: () => tabs.splitEditor('row', t.id), divided: true },
      { label: tr('split:down'), disabled: !canSplit.value, action: () => tabs.splitEditor('column', t.id) },
      ...otherGroups(t),
      { label: tr('common:close'), action: () => tabs.close(t.id), divided: true },
      { label: tr('workbench:tabs.closeOthers'), action: () => tabs.closeOthers(t.id) },
      ...(grouping.value && t.connectionId
        ? [{ label: tr('workbench:tabs.closeConnection'), action: () => tabs.closeGroup(t.connectionId, gid.value) }]
        : []),
      { label: tr('workbench:tabs.closeAll'), action: () => tabs.closeAll(false, gid.value) },
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
        connectionId: g.connectionId, database: mine.value.find((t) => t.connectionId === g.connectionId)?.database ?? '',
      }) },
      { label: tr('workbench:tabs.closeGroup', { n: g.count }), divided: true, action: () => tabs.closeGroup(g.connectionId, gid.value) },
      { label: tr('workbench:tabs.closeOtherGroups'), action: () => tabs.closeOtherGroups(g.connectionId, gid.value) },
    ],
  };
}

// -- split editor
/** ⌘\ on macOS, Ctrl+\ elsewhere. */
const splitKey = isMac ? '⌘\\' : 'Ctrl+\\';
const canSplit = computed(() => tabs.groups.length < MAX_GROUPS);
/** "Mover al grupo de la izquierda / derecha / arriba / abajo". */
function otherGroups(t: Tab): MenuItem[] {
  const i = tabs.groups.findIndex((g) => g.id === gid.value);
  const row = tabs.split === 'row';
  return [
    ...(i > 0 ? [{ label: tr(row ? 'split:moveLeft' : 'split:moveUp'), action: () => tabs.moveTab(t.id, tabs.groups[i - 1].id) }] : []),
    ...(i < tabs.groups.length - 1 ? [{ label: tr(row ? 'split:moveRight' : 'split:moveDown'), action: () => tabs.moveTab(t.id, tabs.groups[i + 1].id) }] : []),
  ];
}
/** The button at the end of the strip: split right (⌥: down, as in VS Code). */
function splitFromButton(e: MouseEvent) {
  tabs.splitEditor(e.altKey ? 'column' : 'row', activeId.value);
}

// -- drag: a tab dragged onto a strip lands before the tab under the
// pointer (or after it, on its right half), or at the strip's end.
const DRAG_TYPE = 'application/x-dbine-tab';
const dropAt = ref<{ id: string | null; after: boolean } | null>(null);
function onDragStart(e: DragEvent, t: Tab) {
  if (renaming.value?.id === t.id || !e.dataTransfer) return;
  e.dataTransfer.setData(DRAG_TYPE, t.id);
  e.dataTransfer.effectAllowed = 'move';
  // After the browser took the drag image: the drop zones show over the editors.
  setTimeout(() => { tabs.dragTab = t.id; });
}
function onDragEnd() {
  tabs.dragTab = null;
  dropAt.value = null;
}
const isTabDrag = (e: DragEvent) => !!e.dataTransfer?.types.includes(DRAG_TYPE);
function onDragOver(e: DragEvent) {
  if (!isTabDrag(e)) return;
  e.preventDefault();
  if (e.dataTransfer) e.dataTransfer.dropEffect = 'move';
  const el = (e.target as Element).closest<HTMLElement>('[data-tab]');
  if (!el) { dropAt.value = { id: null, after: false }; return; }
  const r = el.getBoundingClientRect();
  dropAt.value = { id: el.dataset.tab!, after: e.clientX > r.left + r.width / 2 };
}
function onDragLeave(e: DragEvent) {
  if (!(e.currentTarget as Element).contains(e.relatedTarget as Node | null)) dropAt.value = null;
}
function onDrop(e: DragEvent) {
  const id = e.dataTransfer?.getData(DRAG_TYPE);
  const at = dropAt.value;
  dropAt.value = null;
  tabs.dragTab = null;
  if (!id || !at) return;
  e.preventDefault();
  let before: string | null = at.id;
  if (at.id && at.after) {
    const list = mine.value;
    before = list[list.findIndex((x) => x.id === at.id) + 1]?.id ?? null;
  }
  if (before === id) return;
  tabs.moveTab(id, gid.value, before);
}
</script>

<template>
  <div class="et" :class="{ unfocused: !focused, dropping: dropAt && !dropAt.id }" role="tablist" @dragover="onDragOver" @dragleave="onDragLeave" @drop="onDrop">
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
        :class="{
          active: r.t.id === activeId, preview: r.t.preview, grouped: grouping && !!r.t.connectionId,
          'drop-before': dropAt?.id === r.t.id && !dropAt.after, 'drop-after': dropAt?.id === r.t.id && dropAt.after,
        }"
        role="tab"
        :aria-selected="r.t.id === activeId"
        :data-tab="r.t.id"
        :draggable="renaming?.id !== r.t.id"
        @dragstart="onDragStart($event, r.t)"
        @dragend="onDragEnd"
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
        <button :aria-label="$t('common:close')" class="et-close ide-icon-btn" :title="$t('common:close')" @click.stop="tabs.close(r.t.id)">
          <el-icon :size="13"><ei-close /></el-icon>
        </button>
      </div>
    </template>
    <div class="et-fill" />
    <div class="et-actions">
      <button
        v-if="canSplit && mine.length"
        class="ide-icon-btn et-split"
        :title="$t('split:button', { key: splitKey, alt: isMac ? '⌥' : 'Alt' })"
        :aria-label="$t('split:right')"
        @click="splitFromButton"
      >
        <svg viewBox="0 0 16 16" width="15" height="15" aria-hidden="true"><rect x="1.5" y="2.5" width="13" height="11" rx="1" fill="none" stroke="currentColor" /><path d="M8 2.5v11" stroke="currentColor" /></svg>
      </button>
      <button
        v-if="tabs.groups.length > 1"
        class="ide-icon-btn"
        :title="$t('split:closeGroup')"
        :aria-label="$t('split:closeGroup')"
        @click="tabs.closeEditorGroup(gid)"
      >
        <el-icon :size="13"><ei-close /></el-icon>
      </button>
    </div>
    <ContextMenu v-if="menu" :x="menu.x" :y="menu.y" :items="menu.items" @close="menu = null" />
  </div>
</template>

<style scoped>
.et { display: flex; height: 35px; flex-shrink: 0; background: var(--ide-tabs); overflow-x: auto; overflow-y: hidden; }
.et::-webkit-scrollbar { height: 3px; }
.et-tab {
  position: relative; display: flex; align-items: center; gap: 6px; min-width: 0; max-width: 220px;
  padding: 0 6px 0 10px; background: var(--ide-tab); border-right: 1px solid var(--ide-tabs);
  color: var(--ide-tab-fg); cursor: pointer; user-select: none; flex-shrink: 0; font-size: 13px;
}
.et-tab.active { background: var(--ide-tab-active); color: var(--ide-tab-fg-active); }
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
.et-icon.query { color: var(--nm-accent-2); }
.et-icon.object { color: var(--nm-syn-type); }
.et-icon.file { color: var(--nm-yellow); }
.et-icon.fileDiff { color: var(--nm-yellow); }
.et-name { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.et-dirty { width: 7px; height: 7px; border-radius: 50%; flex: none; background: var(--nm-text-strong); opacity: 0.85; }
.et-dirty.error { background: var(--nm-danger); opacity: 1; }
.et-rename {
  width: 150px; height: 20px; padding: 0 4px; box-sizing: border-box; border: 1px solid var(--ide-focus, var(--nm-accent)); border-radius: 2px;
  background: var(--ide-input); color: var(--nm-text-strong); font: inherit; outline: none;
}
.et-close { opacity: 0; color: inherit; }
.et-tab:hover .et-close, .et-tab.active .et-close { opacity: 1; }
.et-fill { flex: 1; }
/* Tab groups (by connection), as in Chrome but flat: a header the height
   of a tab with the connection's name in its color, and one colored line
   under the header and all its tabs. */
.et-group {
  position: relative; display: flex; align-items: center; gap: 6px; flex-shrink: 0; max-width: 190px;
  padding: 0 10px 0 12px; border: 0; border-left: 1px solid var(--nm-overlay-code); cursor: pointer;
  background: transparent; font: 600 12px/1 inherit; white-space: nowrap;
  color: color-mix(in srgb, var(--g) 75%, var(--nm-text-max));
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
/* Split editor: the strip of a group without the focus dims its active tab
   (as VS Code); drop markers while a tab is dragged. */
.et.unfocused .et-tab.active { color: var(--nm-text-dim); }
.et.unfocused .et-tab.active::before { opacity: 0.35; }
.et-tab.drop-before { box-shadow: inset 2px 0 0 var(--ide-focus); }
.et-tab.drop-after { box-shadow: inset -2px 0 0 var(--ide-focus); }
.et.dropping .et-fill { box-shadow: inset 2px 0 0 var(--ide-focus); background: color-mix(in srgb, var(--ide-focus) 12%, transparent); }
.et-actions { position: sticky; right: 0; display: flex; align-items: center; gap: 2px; padding: 0 6px; flex-shrink: 0; background: var(--ide-tabs); }
.et-split { color: var(--nm-text-muted); }
</style>
