import { effectScope, watch } from 'vue';
import { listen } from '@tauri-apps/api/event';
import { projectsApi, unsavedFilesApi } from '../api/projects';
import type { FileStat, ProjectFilesChanged } from '../api/types';
import { useProjectsStore } from '../stores/projects';
import { baseName, useTabsStore } from '../stores/tabs';
import { useUiStore } from '../stores/ui';
import { fileDocs } from './tabDocument';

// Changes on disk to the files open in tabs, whoever made them: this or
// another window (the backend's `project-files-changed`, sent to every
// window), another editor, or git from a terminal (a check of the open
// files every few seconds while the window has the focus, and on focus).
// A tab with nothing unsaved reloads; one with unsaved changes shows
// "Recargar · Sobrescribir"; a deleted file says so.
//
// Also here: this window's unsaved file tabs go to the backend (quitting
// asks about every window's), and "files-save-all" saves them.

const POLL_MS = 3000;
const REPORT_DEBOUNCE_MS = 100;
const inTauri = () => '__TAURI_INTERNALS__' in window;

/** Compare the open file tabs of each project with the disk. */
async function checkOpenFiles(onlyProject?: string, onlyPaths?: Set<string>) {
  const byProject = new Map<string, { paths: string[]; known: FileStat[] }>();
  for (const d of fileDocs.values()) {
    const id = d.projectId();
    if (onlyProject && id !== onlyProject) continue;
    if (onlyPaths && !onlyPaths.has(d.path())) continue;
    const g = byProject.get(id) ?? { paths: [], known: [] };
    if (!g.paths.includes(d.path())) {
      g.paths.push(d.path());
      const k = d.known();
      if (k) g.known.push(k);
    }
    byProject.set(id, g);
  }
  await Promise.all([...byProject].map(async ([id, g]) => {
    let stats: FileStat[];
    try {
      stats = await projectsApi.statFiles(id, g.paths.slice(0, 200), g.known);
    } catch {
      return;
    }
    for (const s of stats) for (const d of fileDocs.values()) if (d.projectId() === id && d.path() === s.path) d.external(s);
  }));
}

function onFilesChanged(e: ProjectFilesChanged) {
  const projects = useProjectsStore();
  const tabs = useTabsStore();
  // Renamed (in any window): the tabs follow the file.
  if (e.reason === 'rename' && e.paths.length === 2) tabs.renamePath(e.project_id, e.paths[0], e.paths[1]);
  projects.applyFilesChanged(e);
  const touched = new Set(e.paths);
  const open = [...fileDocs.values()].some((d) => d.projectId() === e.project_id && (touched.has(d.path()) || e.reason === 'pull' || e.reason === 'operation' || e.reason === 'checkout'));
  // After a pull or a merge, any open file may have changed.
  if (open) void checkOpenFiles(e.project_id, e.reason === 'pull' || e.reason === 'operation' || e.reason === 'checkout' ? undefined : touched);
}

let started = false;

/** Mounted once, from App.vue. */
export function useProjectWatch() {
  if (started) return;
  started = true;
  listen<ProjectFilesChanged>('project-files-changed', (e) => onFilesChanged(e.payload)).catch(() => { /* outside Tauri */ });

  setInterval(() => { if (document.hasFocus() && fileDocs.size) void checkOpenFiles(); }, POLL_MS);
  window.addEventListener('focus', () => {
    if (fileDocs.size) void checkOpenFiles();
    const projects = useProjectsStore();
    if (projects.loaded) void projects.refreshAllStatus();
  });

  if (!inTauri()) return;
  // This window's unsaved file tabs, for the quit guard of any window.
  const ui = useUiStore();
  const tabs = useTabsStore();
  const projects = useProjectsStore();
  let timer: ReturnType<typeof setTimeout> | null = null;
  const report = () => {
    timer = null;
    const files = tabs.tabs
      .filter((x) => x.kind === 'file' && ui.unsaved[x.id] && ui.unsaved[x.id] !== 'saving')
      .map((x) => ({ id: x.id, title: x.kind === 'file' ? `${projects.byId(x.projectId)?.name ?? ''} › ${baseName(x.path)}` : '' }));
    unsavedFilesApi.report(files).catch(() => {});
  };
  effectScope(true).run(() => {
    watch(
      () => tabs.tabs.filter((x) => x.kind === 'file' && ui.unsaved[x.id]).map((x) => `${x.id}\u0000${ui.unsaved[x.id]}`).join('\u0001'),
      () => {
        if (timer) clearTimeout(timer);
        timer = setTimeout(report, REPORT_DEBOUNCE_MS);
      },
      { immediate: true },
    );
  });
  listen('files-save-all', () => { void saveAllFiles(); }).catch(() => {});
}

/** Save this window's unsaved file tabs; true when all went. */
export async function saveAllFiles(): Promise<boolean> {
  const dirty = [...fileDocs.values()].filter((d) => d.dirty());
  const ok = await Promise.all(dirty.map((d) => d.save()));
  return ok.every(Boolean);
}

/** This window's unsaved file tabs' titles. */
export function unsavedHere(): string[] {
  const projects = useProjectsStore();
  return [...fileDocs.values()].filter((d) => d.dirty()).map((d) => `${projects.byId(d.projectId())?.name ?? ''} › ${baseName(d.path())}`);
}
