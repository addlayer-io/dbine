<script setup lang="ts">
import { computed, nextTick, ref } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { projectsApi } from '../api/projects';
import type { ChangeMark, FsEntry } from '../api/types';
import { parentDir, useProjectsStore } from '../stores/projects';
import { useTabsStore } from '../stores/tabs';
import ContextMenu, { type MenuItem } from './ContextMenu.vue';

// A project's files, one folder level at a time (loaded when opened), with
// git's marks: M modified, A/U new, D deleted (shown in place, from the
// status), C in conflict. A folder with changes inside gets a dot; what
// .gitignore leaves out is dimmed. A click opens the file in a preview tab,
// a double click pins it.

const props = defineProps<{ projectId: string }>();
const projects = useProjectsStore();
const tabs = useTabsStore();
const { t } = useTranslation();

interface Row { entry: FsEntry; depth: number; ghost: boolean; open: boolean }

const exp = computed(() => projects.expandedOf(props.projectId));
const st = computed(() => projects.status[props.projectId]?.data ?? null);
const root = computed(() => projects.dirOf(props.projectId, ''));

/** Files deleted in the working tree (status `D`) under `dir`, not listed by the folder. */
function ghosts(dir: string, listed: FsEntry[]): FsEntry[] {
  const names = new Set(listed.map((e) => e.name));
  return (st.value?.changes ?? [])
    .filter((c) => c.mark === 'D' && parentDir(c.path) === dir)
    .map((c) => ({ name: c.path.slice(c.path.lastIndexOf('/') + 1), path: c.path, is_dir: false, symlink: false, size: 0, ignored: false }))
    .filter((e) => !names.has(e.name));
}

const rows = computed<Row[]>(() => {
  const out: Row[] = [];
  const walk = (dir: string, depth: number) => {
    const listed = projects.dirOf(props.projectId, dir)?.data ?? [];
    for (const e of listed) {
      const open = e.is_dir && exp.value.dirs.includes(e.path);
      out.push({ entry: e, depth, ghost: false, open });
      if (open) walk(e.path, depth + 1);
    }
    for (const g of ghosts(dir, listed)) out.push({ entry: g, depth, ghost: true, open: false });
  };
  walk('', 0);
  return out;
});

const markOf = (path: string): ChangeMark | null => projects.markOf(props.projectId, path);
const selected = ref<string | null>(null);

function onClick(r: Row) {
  selected.value = r.entry.path;
  if (r.entry.is_dir) { projects.toggleDir(props.projectId, r.entry.path); return; }
  if (r.ghost) { tabs.openFileDiff(props.projectId, r.entry.path); return; }
  tabs.openFile(props.projectId, r.entry.path, true, projects.activeTarget(props.projectId).target);
}
function onDblClick(r: Row) {
  if (r.entry.is_dir || r.ghost) return;
  tabs.openFile(props.projectId, r.entry.path, false, projects.activeTarget(props.projectId).target);
}

// -- inline rename (F2 / Enter, or "Renombrar…") ---------------------------------------
const renaming = ref<{ path: string; text: string } | null>(null);
const renameInput = ref<HTMLInputElement[] | null>(null);
function startRename(path: string) {
  const name = path.slice(path.lastIndexOf('/') + 1);
  renaming.value = { path, text: name };
  nextTick(() => {
    const el = renameInput.value?.[0];
    el?.focus();
    const dot = name.lastIndexOf('.');
    el?.setSelectionRange(0, dot > 0 ? dot : name.length);
  });
}
async function commitRename() {
  const r = renaming.value;
  if (!r) return;
  renaming.value = null;
  const name = r.text.trim();
  const dir = parentDir(r.path);
  const to = dir ? `${dir}/${name}` : name;
  if (!name || to === r.path) return;
  try {
    await projectsApi.renamePath(props.projectId, r.path, to);
    tabs.renamePath(props.projectId, r.path, to);
    projects.invalidateDirs(props.projectId, [r.path, to]);
    projects.refreshStatusSoon(props.projectId);
    selected.value = to;
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}
function onKey(e: KeyboardEvent) {
  if (renaming.value || !selected.value) return;
  if (e.key === 'F2' || e.key === 'Enter') { e.preventDefault(); startRename(selected.value); }
}

// -- new, delete ---------------------------------------------------------------------------
async function askName(title: string, placeholder: string): Promise<string | null> {
  try {
    const { value } = await ElMessageBox.prompt(t('projects:files.nameLabel'), title, {
      inputPlaceholder: placeholder, confirmButtonText: t('projects:files.create'), cancelButtonText: t('common:cancel'),
      inputValidator: (v: string) => (!!v?.trim() && !/[\\:*?"<>|]/.test(v) && !v.split('/').some((p) => p === '..' || p === '.git')) || t('projects:files.badName'),
    });
    return value.trim();
  } catch {
    return null;
  }
}
async function newFile(dir: string) {
  const name = await askName(t('projects:files.newFile'), 'consulta.sql');
  if (!name) return;
  const path = dir ? `${dir}/${name}` : name;
  try {
    await projectsApi.createFile(props.projectId, path, '');
    if (dir) projects.toggleDir(props.projectId, dir, true);
    projects.invalidateDirs(props.projectId, [path]);
    projects.refreshStatusSoon(props.projectId);
    tabs.openFile(props.projectId, path, false, projects.activeTarget(props.projectId).target);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}
async function newFolder(dir: string) {
  const name = await askName(t('projects:files.newFolder'), 'migraciones');
  if (!name) return;
  const path = dir ? `${dir}/${name}` : name;
  try {
    await projectsApi.createDir(props.projectId, path);
    if (dir) projects.toggleDir(props.projectId, dir, true);
    projects.invalidateDirs(props.projectId, [path]);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}
async function remove(e: FsEntry) {
  try {
    await ElMessageBox.confirm(
      t(e.is_dir ? 'projects:files.deleteFolderAsk' : 'projects:files.deleteFileAsk', { name: e.path }),
      t('projects:files.deleteTitle'),
      { type: 'warning', confirmButtonText: t('common:delete'), cancelButtonText: t('common:cancel'), confirmButtonClass: 'el-button--danger' },
    );
  } catch { return; }
  try {
    await projectsApi.deletePath(props.projectId, e.path);
    projects.invalidateDirs(props.projectId, [e.path]);
    projects.refreshStatusSoon(props.projectId);
  } catch (err) {
    ElMessage.error(errorMessage(err));
  }
}
function copy(text: string) {
  navigator.clipboard?.writeText(text).then(() => ElMessage.success({ message: t('common:copied'), duration: 1200 }), () => {});
}

const menu = ref<{ x: number; y: number; items: MenuItem[] } | null>(null);
function onContext(ev: MouseEvent, r: Row | null) {
  ev.preventDefault();
  ev.stopPropagation();
  const e = r?.entry;
  const dir = !e ? '' : e.is_dir ? e.path : parentDir(e.path);
  if (e) selected.value = e.path;
  const changed = e && !e.is_dir && markOf(e.path);
  const items: MenuItem[] = [
    { label: t('projects:files.newFile'), action: () => newFile(dir) },
    { label: t('projects:files.newFolder'), action: () => newFolder(dir) },
  ];
  if (e && !r?.ghost) {
    items.push({ label: t('projects:files.rename'), shortcut: 'F2', divided: true, action: () => startRename(e.path) });
    items.push({ label: t('projects:files.delete'), danger: true, action: () => remove(e) });
  }
  if (changed) {
    items.push({ label: t('projects:changes.viewChanges'), divided: true, action: () => tabs.openFileDiff(props.projectId, e!.path) });
    if (changed !== 'C') items.push({ label: t('projects:changes.discardEllipsis'), action: () => projects.discard(props.projectId, [e!.path]) });
  }
  items.push({ label: t('projects:files.reveal'), divided: true, action: () => projectsApi.reveal(props.projectId, e?.path ?? null).catch((x) => ElMessage.error(errorMessage(x))) });
  if (e) items.push({ label: t('projects:files.copyPath'), action: () => copy(e.path) });
  items.push({ label: t('common:refresh'), divided: true, action: () => { for (const d of ['', ...exp.value.dirs]) void projects.loadDir(props.projectId, d, true); } });
  menu.value = { x: ev.clientX, y: ev.clientY, items };
}

/** The open file tab's file is highlighted. */
const activePath = computed(() => {
  const a = tabs.active;
  return a && (a.kind === 'file' || a.kind === 'fileDiff') && a.projectId === props.projectId ? a.path : null;
});
</script>

<template>
  <div class="ft" tabindex="0" @keydown="onKey" @contextmenu="onContext($event, null)">
    <div v-if="root?.status === 'loading' && !root.data" class="ft-note"><el-icon class="is-loading"><ei-loading /></el-icon> {{ $t('common:loading') }}</div>
    <div v-else-if="root?.status === 'error'" class="ft-note err">{{ root.error }}</div>
    <div v-else-if="root?.data && !rows.length" class="ft-note">{{ $t('projects:files.empty') }}</div>
    <div
      v-for="r in rows"
      :key="r.entry.path"
      class="ft-row"
      :class="{
        dir: r.entry.is_dir, ghost: r.ghost, ignored: r.entry.ignored,
        selected: selected === r.entry.path, active: activePath === r.entry.path,
        [`m-${markOf(r.entry.path) ?? ''}`]: !!markOf(r.entry.path),
      }"
      :style="{ paddingLeft: `${8 + r.depth * 12}px` }"
      :title="r.entry.symlink ? $t('projects:files.symlink', { path: r.entry.path }) : r.entry.path"
      @click="onClick(r)"
      @dblclick="onDblClick(r)"
      @contextmenu="onContext($event, r)"
    >
      <el-icon v-if="r.entry.is_dir" class="ft-caret" :class="{ open: r.open }"><ei-arrow-right /></el-icon>
      <span v-else class="ft-caret" />
      <el-icon v-if="r.entry.is_dir" class="ft-ic dir"><ei-folder-opened v-if="r.open" /><ei-folder v-else /></el-icon>
      <el-icon v-else-if="r.entry.symlink" class="ft-ic"><ei-link /></el-icon>
      <el-icon v-else class="ft-ic file"><ei-document /></el-icon>
      <input
        v-if="renaming?.path === r.entry.path"
        ref="renameInput"
        v-model="renaming.text"
        class="ft-rename"
        spellcheck="false"
        @click.stop
        @dblclick.stop
        @keydown.enter.prevent.stop="commitRename"
        @keydown.esc.prevent.stop="renaming = null"
        @blur="commitRename"
      />
      <span v-else class="ft-name">{{ r.entry.name }}</span>
      <span v-if="r.entry.is_dir && projects.dirHasChanges(projectId, r.entry.path)" class="ft-dot" :title="$t('projects:files.dirChanged')" />
      <span v-else-if="markOf(r.entry.path)" class="ft-mark" :title="$t(`projects:mark.${markOf(r.entry.path)}`)">{{ markOf(r.entry.path) === 'C' ? '!' : markOf(r.entry.path) }}</span>
    </div>
    <ContextMenu v-if="menu" :x="menu.x" :y="menu.y" :items="menu.items" @close="menu = null" />
  </div>
</template>

<style scoped>
.ft { outline: none; padding-bottom: 4px; }
.ft-note { padding: 4px 22px; font-size: 12px; color: var(--nm-text-dim); }
.ft-note.err { color: var(--nm-danger); }
.ft-row { display: flex; align-items: center; gap: 4px; height: 22px; padding-right: 10px; cursor: pointer; user-select: none; color: var(--nm-text); }
.ft-row:hover { background: var(--ide-hover); }
.ft-row.active { background: color-mix(in srgb, var(--ide-selection) 70%, transparent); }
.ft-row.selected { background: var(--ide-selection); }
.ft:focus .ft-row.selected { outline: 1px solid var(--ide-focus); outline-offset: -1px; }
.ft-caret { width: 12px; flex: none; font-size: 10px; color: var(--nm-text-dim); transition: transform 0.1s; }
.ft-caret.open { transform: rotate(90deg); }
.ft-ic { flex: none; font-size: 14px; }
.ft-ic.dir { color: #c5a46d; }
.ft-ic.file { color: var(--nm-text-dim); }
.ft-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.ft-row.ignored { opacity: 0.5; }
.ft-row.m-M .ft-name, .ft-row.m-M .ft-mark { color: #e2c08d; }
.ft-row.m-A .ft-name, .ft-row.m-A .ft-mark, .ft-row.m-U .ft-name, .ft-row.m-U .ft-mark { color: #73c991; }
.ft-row.m-R .ft-name, .ft-row.m-R .ft-mark { color: #75beff; }
.ft-row.m-D .ft-name, .ft-row.m-D .ft-mark, .ft-row.m-C .ft-name, .ft-row.m-C .ft-mark { color: var(--nm-danger); }
.ft-row.ghost .ft-name { text-decoration: line-through; }
.ft-mark { flex: none; width: 12px; text-align: center; font-family: var(--nm-mono); font-size: 11px; font-weight: 700; }
.ft-dot { flex: none; width: 6px; height: 6px; margin: 0 3px; border-radius: 50%; background: #e2c08d; }
.ft-rename {
  flex: 1; min-width: 0; height: 18px; padding: 0 4px; box-sizing: border-box; border: 1px solid var(--ide-focus, var(--nm-accent));
  border-radius: 2px; background: var(--ide-input, #1e1e1e); color: var(--nm-text-strong); font: inherit; outline: none;
}
</style>
