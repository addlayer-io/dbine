<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { ANY_SQL, libraryApi, type LibraryScript } from '../api/library';
import { confirmNative } from '../native';
import { useConnectionsStore } from '../stores/connections';
import { useLibraryStore } from '../stores/library';
import { useTabsStore } from '../stores/tabs';
import ContextMenu, { type MenuItem } from './ContextMenu.vue';
import LibraryGitDialog from './LibraryGitDialog.vue';

// The script Library view (the ⭐ in the activity bar): reusable scripts per
// engine, by folder. Opening one copies it into a query of the active
// database (asking its {{parameters}}); the Library keeps the original.

const lib = useLibraryStore();
const { t } = useTranslation();
/** The empty state's `{{parameters}}` sample, in the current language. */
const paramsSample = computed(() => `{{${t('library:sidebar.paramsWord')}}}`);
const conns = useConnectionsStore();
const tabs = useTabsStore();
onMounted(() => lib.load());

const query = ref('');
/** Only the scripts for the active tab's engine. */
/** The script clicked (a double click or Enter opens it). */
const selected = ref<string | null>(null);
function onListKey(e: KeyboardEvent) {
  if (e.key !== 'Enter' || !selected.value) return;
  const s = lib.items.find((x) => x.id === selected.value);
  if (s) { e.preventDefault(); open(s); }
}
/** "Otros motores" (scripts for other engines than the active tab's) open. */
const othersOpen = ref(false);
const driver = computed(() => (tabs.active ? conns.driverOf(tabs.active.connectionId) : null));

function matches(s: LibraryScript) {
  const q = query.value.trim().toLowerCase();
  return !q || [s.name, s.folder, s.description, s.text].some((t) => t.toLowerCase().includes(q));
}
const fitting = computed(() => lib.items.filter((s) => matches(s) && (!driver.value || lib.fits(s, driver.value))));
const others = computed(() => (driver.value ? lib.items.filter((s) => matches(s) && !lib.fits(s, driver.value)) : []));

/** Scripts grouped by folder (root first). */
function groups(list: LibraryScript[]) {
  const map = new Map<string, LibraryScript[]>();
  for (const s of list) map.set(s.folder, [...(map.get(s.folder) ?? []), s]);
  return [...map.entries()].sort(([a], [b]) => (a === '' ? -1 : b === '' ? 1 : a.localeCompare(b)));
}
const closed = ref<Set<string>>(new Set());
function toggle(key: string) {
  const s = new Set(closed.value);
  if (s.has(key)) s.delete(key);
  else s.add(key);
  closed.value = s;
}
/** Hidden because a parent folder is closed. */
function hiddenBy(section: string, folder: string) {
  const parts = folder.split('/');
  for (let i = 1; i < parts.length; i++) if (closed.value.has(section + parts.slice(0, i).join('/'))) return true;
  return false;
}
/** Every folder level to show (a script in "a/b" shows "a" too). */
function folderRows(section: string, list: LibraryScript[]) {
  // The main section shows every folder (empty ones too: something can be
  // dropped in them); "Otros motores" only the ones it has scripts in.
  const all = new Set<string>(section === 'fit' ? lib.createdFolders : []);
  for (const s of list) {
    const parts = s.folder.split('/').filter(Boolean);
    for (let i = 1; i <= parts.length; i++) all.add(parts.slice(0, i).join('/'));
  }
  const byFolder = new Map(groups(list));
  return [...all].sort((a, b) => a.localeCompare(b)).map((f) => ({
    folder: f,
    depth: f.split('/').length - 1,
    label: f.split('/').pop()!,
    scripts: byFolder.get(f) ?? [],
    count: list.filter((s) => s.folder === f || s.folder.startsWith(f + '/')).length,
    hidden: hiddenBy(section, f),
  }));
}

function engineLabel(e: string) {
  if (e === ANY_SQL) return 'SQL';
  return conns.drivers.find((d) => d.id === e)?.name ?? e;
}

// -- actions ------------------------------------------------------------------------------
function open(s: LibraryScript, mode: 'new' | 'append' = 'new') {
  lib.opening = { script: s, mode };
}
function edit(s: LibraryScript) {
  lib.editing = { ...s, engines: [...s.engines] };
}
async function remove(s: LibraryScript) {
  if (!(await confirmNative(t('library:sidebar.deleteScriptConfirm', { name: s.name }), { title: t('library:sidebar.deleteScriptTitle'), okLabel: t('common:delete') }))) return;
  try { await lib.remove(s.id); } catch (e) { ElMessage.error(errorMessage(e)); }
}
function newScript() {
  lib.newScript('', driver.value ? [driver.value.id] : []);
}

async function importFiles(folders: boolean) {
  let picked: string | string[] | null = null;
  try {
    picked = await openDialog({
      multiple: !folders,
      directory: folders,
      title: folders ? t('library:sidebar.importFolderTitle') : t('library:sidebar.importFilesTitle'),
      filters: folders ? undefined : [{ name: 'Scripts', extensions: ['sql', 'cql', 'js', 'json', 'txt', 'cypher', 'flux'] }],
    });
  } catch { return; }
  if (!picked) return;
  const paths = Array.isArray(picked) ? picked : [picked];
  const engines = driver.value ? [driver.value.id] : [ANY_SQL];
  try {
    const r = await libraryApi.importFiles(paths, engines, '');
    await lib.load(true);
    ElMessage.success(t('library:sidebar.imported', { count: r.imported, engines: engines.map(engineLabel).join(', ') }));
    if (r.skipped.length) ElMessage.warning({ message: t('library:sidebar.skipped', { list: r.skipped.join(' · ') }), duration: 6000 });
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}
async function exportAll() {
  let dir: string | string[] | null = null;
  try { dir = await openDialog({ directory: true, title: t('library:sidebar.exportAllTitle') }); } catch { return; }
  if (!dir || Array.isArray(dir)) return;
  try {
    const n = await libraryApi.exportTo(dir);
    ElMessage.success(t('library:sidebar.exported', { count: n, dir }));
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

const menu = ref<{ x: number; y: number; items: MenuItem[] } | null>(null);
function onContext(e: MouseEvent, s: LibraryScript) {
  e.preventDefault();
  menu.value = {
    x: e.clientX,
    y: e.clientY,
    items: [
      { label: t('library:menu.openNew'), action: () => open(s, 'new') },
      { label: t('library:menu.appendOpen'), disabled: tabs.active?.kind !== 'query' && tabs.active?.kind !== 'file', action: () => open(s, 'append') },
      { label: t('library:menu.edit'), divided: true, action: () => edit(s) },
      { label: t('library:menu.move'), action: () => { lib.moving = s; } },
      { label: t('common:duplicate'), action: () => lib.newScript(s.text, [...s.engines], t('library:menu.copyName', { name: s.name })) },
      { label: t('library:menu.exportSql'), action: () => exportOne(s) },
      { label: t('common:delete'), divided: true, danger: true, action: () => remove(s) },
    ],
  };
}
async function exportOne(s: LibraryScript) {
  let dir: string | string[] | null = null;
  try { dir = await openDialog({ directory: true, title: t('library:sidebar.exportOneTitle', { name: s.name }) }); } catch { return; }
  if (!dir || Array.isArray(dir)) return;
  try { await libraryApi.exportTo(dir, [s.id]); ElMessage.success(t('library:sidebar.exportedOne')); } catch (e) { ElMessage.error(errorMessage(e)); }
}
// -- folders -----------------------------------------------------------------------------------
async function askName(title: string, value = '') {
  try {
    const { value: v } = await ElMessageBox.prompt(t('library:folder.namePrompt'), title, {
      inputValue: value, confirmButtonText: t('common:accept'), cancelButtonText: t('common:cancel'),
      inputValidator: (x) => (!!x?.trim() && !x.includes('/')) || t('library:folder.nameInvalid'),
    });
    return v.trim();
  } catch { return null; }
}
async function newFolder(parent = '') {
  const name = await askName(parent ? t('library:folder.newIn', { parent }) : t('library:folder.new'));
  if (!name) return;
  const path = await lib.createFolder(parent ? `${parent}/${name}` : name);
  if (path) {
    const s = new Set(closed.value);
    for (const k of [...s]) if (path.startsWith(k.replace(/^fit/, '') + '/')) s.delete(k);
    closed.value = s;
  }
}
async function renameFolder(path: string) {
  const i = path.lastIndexOf('/');
  const name = await askName(t('library:folder.rename'), path.slice(i + 1));
  if (!name) return;
  try { await lib.moveFolder(path, (i >= 0 ? path.slice(0, i + 1) : '') + name); } catch (e) { ElMessage.error(errorMessage(e)); }
}
async function deleteFolder(path: string) {
  const n = lib.items.filter((s) => s.folder === path || s.folder.startsWith(path + '/')).length;
  const msg = n ? t('library:folder.deleteConfirmScripts', { path, count: n }) : t('library:folder.deleteConfirm', { path });
  if (!(await confirmNative(msg, { title: t('library:folder.delete'), okLabel: t('common:delete') }))) return;
  try { await lib.deleteFolder(path); } catch (e) { ElMessage.error(errorMessage(e)); }
}
function onFolderContext(e: MouseEvent, path: string) {
  e.preventDefault();
  menu.value = {
    x: e.clientX,
    y: e.clientY,
    items: [
      { label: t('library:menu.newScriptHere'), action: () => { lib.newScript('', driver.value ? [driver.value.id] : []); if (lib.editing) lib.editing.folder = path; } },
      { label: t('library:menu.newSubfolder'), action: () => newFolder(path) },
      { label: t('library:menu.rename'), divided: true, action: () => renameFolder(path) },
      { label: t('library:folder.delete'), danger: true, action: () => deleteFolder(path) },
    ],
  };
}

// -- drag & drop: scripts and folders into folders (or the root) -------------------
type Dragged = { type: 'script'; id: string } | { type: 'folder'; path: string };
const dragged = ref<Dragged | null>(null);
const dropTarget = ref<string | null>(null);
function onDragStart(e: DragEvent, d: Dragged) {
  dragged.value = d;
  e.dataTransfer?.setData('text/plain', d.type === 'script' ? d.id : d.path);
  if (e.dataTransfer) e.dataTransfer.effectAllowed = 'move';
}
function canDrop(folder: string) {
  const d = dragged.value;
  if (!d) return false;
  if (d.type === 'script') return lib.items.find((s) => s.id === d.id)?.folder !== folder;
  // A folder can't go into itself or its subfolders, nor where it already is.
  const parent = d.path.includes('/') ? d.path.slice(0, d.path.lastIndexOf('/')) : '';
  return folder !== d.path && !folder.startsWith(d.path + '/') && folder !== parent;
}
function onDragOver(e: DragEvent, folder: string) {
  if (!canDrop(folder)) return;
  e.preventDefault();
  e.stopPropagation();
  if (e.dataTransfer) e.dataTransfer.dropEffect = 'move';
  dropTarget.value = folder;
}
async function onDrop(e: DragEvent, folder: string) {
  e.preventDefault();
  e.stopPropagation();
  const d = dragged.value;
  const ok = canDrop(folder);
  dragged.value = null;
  dropTarget.value = null;
  if (!d || !ok) return;
  try {
    if (d.type === 'script') await lib.moveScript(d.id, folder);
    else await lib.moveFolder(d.path, (folder ? folder + '/' : '') + d.path.split('/').pop());
  } catch (err) {
    ElMessage.error(errorMessage(err));
  }
}
function onDragEnd() {
  dragged.value = null;
  dropTarget.value = null;
}

/** The Library's git window (backup in a repo, commit / pull / push). */
const gitOpen = ref(false);
const headMenu = ref<{ x: number; y: number; items: MenuItem[] } | null>(null);
function openHeadMenu(e: MouseEvent) {
  const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
  headMenu.value = {
    x: r.left,
    y: r.bottom + 2,
    items: [
      { label: t('library:menu.importFiles'), action: () => importFiles(false) },
      { label: t('library:menu.importFolder'), action: () => importFiles(true) },
      { label: t('library:menu.exportAll'), divided: true, disabled: !lib.items.length, action: exportAll },
    ],
  };
}
</script>

<template>
  <div class="lb">
    <div class="lb-header">
      <span class="lb-title">{{ $t('library:sidebar.title') }}</span>
      <div style="flex: 1" />
      <button class="lb-icon" :title="$t('library:sidebar.newScript')" @click="newScript"><el-icon><ei-plus /></el-icon></button>
      <button class="lb-icon" :title="$t('library:folder.new')" @click="newFolder()"><el-icon><ei-folder-add /></el-icon></button>
      <button class="lb-icon" :title="$t('library:sidebar.gitHint')" @click="gitOpen = true"><el-icon><ei-share /></el-icon></button>
      <button class="lb-icon" :title="$t('library:sidebar.importExport')" @click="openHeadMenu"><el-icon><ei-more-filled /></el-icon></button>
    </div>
    <div class="lb-filter">
      <el-input v-model="query" size="small" clearable :placeholder="$t('library:sidebar.search')">
        <template #prefix><el-icon><ei-search /></el-icon></template>
      </el-input>
    </div>

    <div
      class="lb-list"
      tabindex="0"
      @keydown="onListKey"
      :class="{ 'drop-root': dropTarget === '' }"
      @dragover="onDragOver($event, '')"
      @dragleave.self="dropTarget = null"
      @drop="onDrop($event, '')"
    >
      <div v-if="lib.loaded && !lib.items.length" class="lb-empty">
        <p>{{ $t('library:sidebar.emptyLead') }}</p>
        <p class="lb-muted">
          <i18next :translation="$t('library:sidebar.emptyHelp')">
            <template #params><code>{{ paramsSample }}</code></template>
          </i18next>
        </p>
        <el-button size="small" type="primary" @click="newScript">{{ $t('library:sidebar.newScript') }}</el-button>
        <el-button size="small" @click="importFiles(true)">{{ $t('library:sidebar.importSqlFolder') }}</el-button>
      </div>

      <div v-if="driver && lib.items.length && !fitting.length" class="lb-empty lb-muted">
        {{ $t('library:sidebar.noneFor', { engine: driver.name }) }}
      </div>
      <template v-for="section in [{ key: 'fit', list: fitting }, ...(others.length ? [{ key: 'other', list: others }] : [])]" :key="section.key">
        <div v-if="section.key === 'other'" class="lb-section" :title="$t('library:sidebar.othersHint', { engine: driver?.name ?? '' })" @click="othersOpen = !othersOpen">
          <el-icon class="lb-caret" :class="{ open: othersOpen }"><ei-arrow-right /></el-icon>
          {{ $t('library:sidebar.others', { n: others.length }) }}
        </div>
        <template v-if="section.key === 'fit' || othersOpen">
        <div
          v-for="s in section.list.filter((x) => !x.folder)"
          :key="s.id"
          class="lb-item"
          :class="{ dim: section.key === 'other', selected: selected === s.id }"
          :title="s.description || s.text.slice(0, 300)"
          draggable="true"
          @dragstart.stop="onDragStart($event, { type: 'script', id: s.id })"
          @dragend="onDragEnd"
          @click="selected = s.id"
          @dblclick="open(s)"
          @contextmenu="onContext($event, s)"
        >
          <el-icon class="lb-star"><ei-star-filled /></el-icon>
          <span class="lb-name">{{ s.name }}</span>
          <span class="lb-engines">{{ s.engines.map(engineLabel).join(' · ') || $t('library:sidebar.noEngine') }}</span>
          <button class="lb-act" :title="$t('common:edit')" @click.stop="edit(s)"><el-icon><ei-edit /></el-icon></button>
        </div>
        <template v-for="f in folderRows(section.key, section.list)" :key="section.key + f.folder">
          <div
            v-if="!f.hidden"
            class="lb-folder"
            :class="{ drop: dropTarget === f.folder && section.key === 'fit' }"
            :style="{ paddingLeft: 10 + f.depth * 14 + 'px' }"
            draggable="true"
            @dragstart.stop="onDragStart($event, { type: 'folder', path: f.folder })"
            @dragend="onDragEnd"
            @dragover="onDragOver($event, f.folder)"
            @drop="onDrop($event, f.folder)"
            @click="toggle(section.key + f.folder)"
            @contextmenu="onFolderContext($event, f.folder)"
          >
            <el-icon class="lb-caret" :class="{ open: !closed.has(section.key + f.folder) }"><ei-arrow-right /></el-icon>
            <el-icon><ei-folder /></el-icon>
            <span>{{ f.label }}</span>
            <span class="lb-count">{{ f.count }}</span>
          </div>
          <template v-if="!f.hidden && !closed.has(section.key + f.folder)">
            <div
              v-for="s in f.scripts"
              :key="s.id"
              class="lb-item"
              :style="{ paddingLeft: 32 + f.depth * 14 + 'px' }"
              :class="{ dim: section.key === 'other', selected: selected === s.id }"
              draggable="true"
              @dragstart.stop="onDragStart($event, { type: 'script', id: s.id })"
              @dragend="onDragEnd"
              :title="s.description || s.text.slice(0, 300)"
              @click="selected = s.id"
          @dblclick="open(s)"
              @contextmenu="onContext($event, s)"
            >
              <el-icon class="lb-star"><ei-star-filled /></el-icon>
              <span class="lb-name">{{ s.name }}</span>
              <span class="lb-engines">{{ s.engines.map(engineLabel).join(' · ') || $t('library:sidebar.noEngine') }}</span>
              <button class="lb-act" :title="$t('common:edit')" @click.stop="edit(s)"><el-icon><ei-edit /></el-icon></button>
            </div>
          </template>
        </template>
        </template>
      </template>
    </div>
    <ContextMenu v-if="menu" :x="menu.x" :y="menu.y" :items="menu.items" @close="menu = null" />
    <ContextMenu v-if="headMenu" :x="headMenu.x" :y="headMenu.y" :items="headMenu.items" @close="headMenu = null" />
    <LibraryGitDialog v-if="gitOpen" v-model="gitOpen" />
  </div>
</template>

<style scoped>
.lb { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--ide-sidebar); font-size: 13px; }
.lb-header { display: flex; align-items: center; gap: 2px; height: 35px; padding: 0 8px 0 20px; flex: none; }
.lb-title { font-size: 11px; font-weight: 600; letter-spacing: 0.05em; text-transform: uppercase; color: var(--nm-text-dim); }
.lb-icon { display: inline-flex; align-items: center; justify-content: center; width: 22px; height: 22px; border: none; border-radius: 4px; background: transparent; color: var(--nm-text-dim); cursor: pointer; }
.lb-icon:hover { background: var(--ide-hover); color: var(--nm-text-strong); }
.lb-filter { padding: 0 10px 6px; display: flex; flex-direction: column; gap: 4px; flex: none; }
.lb-list { flex: 1; min-height: 0; overflow: auto; padding-bottom: 12px; }
.lb-empty { padding: 10px 16px; line-height: 1.5; color: var(--nm-text); }
.lb-empty p { margin: 0 0 8px; }
.lb-empty code { font-family: var(--nm-mono); font-size: 12px; }
.lb-muted { color: var(--nm-text-dim); font-size: 12px; }
.lb-section { display: flex; align-items: center; gap: 5px; cursor: pointer; user-select: none; padding: 8px 10px 4px; font-size: 11px; text-transform: uppercase; letter-spacing: 0.04em; color: var(--nm-text-dim); border-top: 1px solid var(--nm-border); margin-top: 6px; }
.lb-folder { display: flex; align-items: center; gap: 5px; height: 24px; padding: 0 10px; cursor: pointer; color: var(--nm-text); user-select: none; }
.lb-folder:hover, .lb-item:hover { background: var(--ide-hover); }
/* WebKit (Tauri on macOS) only drags elements marked like this. */
.lb-item[draggable='true'], .lb-folder[draggable='true'] { -webkit-user-drag: element; }
.lb-folder.drop { background: var(--ide-selection); outline: 1px dashed var(--nm-accent); outline-offset: -1px; }
.lb-list.drop-root { background: color-mix(in srgb, var(--nm-accent) 6%, transparent); }
.lb-caret { transition: transform 0.1s; font-size: 11px; }
.lb-caret.open { transform: rotate(90deg); }
.lb-count { margin-left: auto; font-size: 11px; color: var(--nm-text-dim); }
.lb-item { display: flex; align-items: center; gap: 6px; height: 24px; padding: 0 8px 0 14px; cursor: pointer; min-width: 0; }
.lb-item.nested { padding-left: 32px; }
.lb-item.dim { opacity: 0.6; }
.lb-item.selected { background: var(--ide-selection); }
.lb-list { outline: none; }
.lb-star { color: #e5c07b; font-size: 12px; flex: none; }
.lb-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--nm-text-strong); }
.lb-engines { flex: none; max-width: 45%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 11px; color: var(--nm-text-dim); }
.lb-act { display: none; border: none; background: transparent; color: var(--nm-text-dim); cursor: pointer; padding: 2px; }
.lb-item:hover .lb-act { display: inline-flex; }
.lb-item:hover .lb-engines { display: none; }
.lb-act:hover { color: var(--nm-text-strong); }
</style>
