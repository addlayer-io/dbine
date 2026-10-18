import { onScopeDispose, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { api, errorMessage } from '../api/client';
import { OPENABLE_EXTENSIONS, openFilesApi, type OpenedFile } from '../api/openFiles';
import type { SavedConnection } from '../api/types';
import { t } from '../i18n';
import { useConnectionsStore } from '../stores/connections';
import { useOutputStore } from '../stores/output';
import { useTabsStore } from '../stores/tabs';
import { useUiStore } from '../stores/ui';

// Data files opened from the OS (src-tauri/src/open_files.rs): Finder /
// Explorer, a drop on the window, File › Open…. Each one is a temporary
// connection (the explorer's "Archivo" badge) until "Guardar conexión"; a
// data file (CSV, Parquet, JSON, Excel) lands in a new query with its first
// rows already shown.
//
// The backend queues the paths it gets and tells a window (`open-files`);
// the window takes the queue. At launch the queue is taken once the
// connections have loaded.

/** Query ids to run as soon as their tab has its text (QueryView). */
export const runWhenOpened = new Set<string>();

/** A file is being dragged over the window ("Soltá para abrir"). */
export const fileDragOver = ref(false);

const output = () => useOutputStore();

/** The database a new query on the connection uses (as the explorer's "Nueva query"). */
function databaseOf(connectionId: string): string {
  const live = useConnectionsStore().live[connectionId];
  return live?.defaultDatabase || live?.databases[0] || '';
}

async function openOne(path: string) {
  const conns = useConnectionsStore();
  const tabs = useTabsStore();
  const ui = useUiStore();
  let opened: OpenedFile;
  try {
    opened = await openFilesApi.open(path);
  } catch (e) {
    ElMessage.error(t('core:openFiles.failed', { name: path.split(/[\\/]/).pop() ?? path, error: errorMessage(e) }));
    return;
  }
  const id = opened.connection_id;
  if (!conns.byId(id)) await conns.load();
  // Opened before: its tab, if one is open, else its node.
  if (opened.existing) {
    const tab = tabs.tabs.find((x) => x.connectionId === id);
    if (tab) { tabs.activate(tab.id); return; }
  }
  if (!(await conns.connect(id))) return;
  const database = databaseOf(id);
  ui.revealInExplorer({ connectionId: id, database });
  output().add('info', t('core:openFiles.opened', { name: opened.name }), { where: opened.name });
  if (opened.existing || !opened.sql) return;
  try {
    await conns.loadQueries(id, database);
    const q = await conns.saveQuery({
      id: '', connection_id: id, database, sql: opened.sql,
      name: conns.nextQueryName(id, database), updated_at: '', last_run_at: null,
    });
    runWhenOpened.add(q.id);
    tabs.openQuery(q);
  } catch (e) {
    ElMessage.error(t('core:actions.createQueryFailed', { error: errorMessage(e) }));
  }
}

let opening: Promise<void> = Promise.resolve();

/** Open these files, one after another (each connects first). */
export function openPaths(paths: string[]): Promise<void> {
  opening = opening.then(async () => { for (const p of paths) await openOne(p); });
  return opening;
}

/** Take what the backend has queued and open it. */
async function takeQueued() {
  try {
    const paths = await openFilesApi.take();
    if (paths.length) await openPaths(paths);
  } catch { /* outside Tauri */ }
}

/** File › Open…: the native picker, several files at once. */
export async function pickAndOpen() {
  let picked: string | string[] | null;
  try {
    picked = await openDialog({
      multiple: true,
      directory: false,
      title: t('core:openFiles.dialogTitle'),
      filters: [{ name: t('core:openFiles.filterName'), extensions: OPENABLE_EXTENSIONS }],
    });
  } catch {
    return;
  }
  if (!picked) return;
  await openPaths(Array.isArray(picked) ? picked : [picked]);
}

/** "Guardar conexión": the opened file's connection stays after this run. */
export async function saveOpenedFile(c: SavedConnection) {
  const conns = useConnectionsStore();
  try {
    const { temporary: _, ...conn } = c;
    const saved = await api.saveConnection(conn);
    // The same settings: the connection stays live, its tree as it is.
    const i = conns.list.findIndex((x) => x.id === saved.id);
    if (i >= 0) conns.list.splice(i, 1);
    conns.list.push(saved);
    ElMessage.success({ message: t('core:openFiles.saved', { name: saved.name }), duration: 2000 });
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

/** "Cerrar archivo": its connection and its tabs go (nothing to delete on disk). */
export async function closeOpenedFile(c: SavedConnection) {
  const conns = useConnectionsStore();
  await conns.remove(c.id);
  useTabsStore().forgetConnections(new Set([c.id]));
}

/** A drag from another app carrying files (not one of the page's own drags). */
const hasFiles = (e: DragEvent) => !!e.dataTransfer && Array.from(e.dataTransfer.types).includes('Files');

interface WebView2 { postMessageWithAdditionalObjects?: (message: string, objects: ArrayLike<unknown>) => void }

/** The window's side: the queue, the `open-files` event and dropping files.
 *  Call once per window (FileDropOverlay). */
export function useOpenFiles() {
  const conns = useConnectionsStore();
  if (!('__TAURI_INTERNALS__' in window)) return;

  // At launch: once the connections are there (a restored tab may be one).
  const stop = watch(() => conns.loaded, (loaded) => {
    if (!loaded) return;
    queueMicrotask(() => stop());
    void takeQueued();
  }, { immediate: true });

  const unlisteners: (() => void)[] = [];
  let disposed = false;
  try {
    getCurrentWebviewWindow()
      .listen('open-files', () => { if (conns.loaded) void takeQueued(); })
      .then((f) => { if (disposed) f(); else unlisteners.push(f); })
      .catch(() => {});
  } catch { /* no window metadata */ }

  // The drop. The page never gets a dropped file's path: on macOS and Linux
  // the backend reads it natively next to the webview; on Windows the page
  // posts the File objects to WebView2, which gives the backend their paths.
  // Here: the overlay, and keeping the editor (and the explorer's own drag
  // and drop) from taking the drop. Capture phase, before anyone else.
  let depth = 0;
  const onEnter = (e: DragEvent) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    depth++;
    fileDragOver.value = true;
  };
  const onOver = (e: DragEvent) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    e.stopPropagation();
    if (e.dataTransfer) e.dataTransfer.dropEffect = 'copy';
    fileDragOver.value = true;
  };
  const onLeave = (e: DragEvent) => {
    if (!hasFiles(e)) return;
    depth = Math.max(0, depth - 1);
    if (depth === 0) fileDragOver.value = false;
  };
  const onDrop = (e: DragEvent) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    e.stopPropagation();
    depth = 0;
    fileDragOver.value = false;
    const files = e.dataTransfer?.files;
    const webview = (window as unknown as { chrome?: { webview?: WebView2 } }).chrome?.webview;
    if (files?.length && webview?.postMessageWithAdditionalObjects) {
      webview.postMessageWithAdditionalObjects('dbine-open-files', Array.from(files));
    }
  };
  window.addEventListener('dragenter', onEnter, true);
  window.addEventListener('dragover', onOver, true);
  window.addEventListener('dragleave', onLeave, true);
  window.addEventListener('drop', onDrop, true);

  onScopeDispose(() => {
    disposed = true;
    stop();
    unlisteners.forEach((f) => f());
    window.removeEventListener('dragenter', onEnter, true);
    window.removeEventListener('dragover', onOver, true);
    window.removeEventListener('dragleave', onLeave, true);
    window.removeEventListener('drop', onDrop, true);
  });
}
