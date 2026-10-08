import { computed, onBeforeUnmount, ref, watch, type ComputedRef, type Ref } from 'vue';
import { ElMessage } from 'element-plus';
import { api, errorMessage, runFiles } from '../api/client';
import { timelineApi } from '../api/timeline';
import { projectsApi } from '../api/projects';
import type { DriverInfo, FileStat, Language, SavedQuery } from '../api/types';
import { t } from '../i18n';
import { useConnectionsStore } from '../stores/connections';
import { useProjectsStore } from '../stores/projects';
import { baseName, useTabsStore, type FileTab, type QueryTab } from '../stores/tabs';
import { useUiStore } from '../stores/ui';
import { timelineSeq } from './timeline';

// What the query editor (QueryView) edits: a saved query (its text in the
// local state, saved as you type) or a project's file (on disk, saved with
// ⌘S, with a check that nobody changed it meanwhile). The view runs,
// formats and shows results the same way for both.

export type SaveState = 'saved' | 'dirty' | 'saving' | 'error';

export interface TabDocument {
  text: Ref<string>;
  saveState: Ref<SaveState>;
  loadError: Ref<string | null>;
  title: ComputedRef<string>;
  /** For history and "mark run" (query tabs only). */
  queryId: string | undefined;
  autosave: boolean;
  load(): Promise<void>;
  /** True when it's saved (or there was nothing to save). */
  save(): Promise<boolean>;
  rename?(name: string): Promise<void>;
  /** The tab's database picker. */
  changeDatabase(db: string): Promise<void>;
  /** A statement switched the database (`USE`): the tab follows it. */
  followDatabase(db: string): Promise<void>;
  /** File tabs: changed on disk while edited here (`disk` null: deleted). */
  conflict: Ref<null | { disk: FileStat | null }>;
  /** File tabs: the file is gone from disk (or its project from DBine). */
  missing: Ref<null | 'file' | 'project'>;
  reloadFromDisk?(): Promise<void>;
  /** Save over what's on disk (drops the check). */
  keepMine?(): Promise<boolean>;
}

// -- a saved query --------------------------------------------------------------------

/** Open saved-query tabs (by tab id), for the timeline: save before a
 *  restore. */
export const queryDocs = new Map<string, { queryId: string; save(): Promise<boolean> }>();

export function useQueryDocument(tab: QueryTab): TabDocument & { query: Ref<SavedQuery | null> } {
  const conns = useConnectionsStore();
  const tabs = useTabsStore();
  const ui = useUiStore();
  const query = ref<SavedQuery | null>(null);
  const text = ref('');
  const loadError = ref<string | null>(null);
  const saveState = ref<SaveState>('saved');

  async function load() {
    loadError.value = null;
    try {
      const q = await api.getQuery(tab.queryId);
      query.value = q;
      text.value = q.sql;
      saveState.value = 'saved';
    } catch (e) {
      loadError.value = errorMessage(e);
    }
  }
  watch(() => tab.queryId, load, { immediate: true });
  // A restore from the cloud backup may have changed it (not while editing).
  watch(() => ui.syncSeq, () => { if (saveState.value === 'saved') load(); });

  // Autosave, 600 ms after the last change.
  let timer: ReturnType<typeof setTimeout> | null = null;
  watch(text, (v) => {
    if (!query.value || v === query.value.sql) return;
    saveState.value = 'dirty';
    tabs.pin(tab.id);
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => write(false), 600);
  });

  /** `checkpoint`: the text always becomes a version in the timeline (⌘S,
   *  a run, closing the tab); the autosave keeps one a minute at most. */
  async function write(checkpoint: boolean): Promise<boolean> {
    if (timer) { clearTimeout(timer); timer = null; }
    if (!query.value) return true;
    saveState.value = 'saving';
    try {
      query.value = await conns.saveQuery({ ...query.value, sql: text.value }, checkpoint);
      saveState.value = 'saved';
      timelineSeq.value++;
      return true;
    } catch (e) {
      saveState.value = 'error';
      ElMessage.error(t('query:saveFailed', { error: errorMessage(e) }));
      return false;
    }
  }
  const save = () => write(true);
  const handle = { queryId: tab.queryId, save };
  queryDocs.set(tab.id, handle);
  onBeforeUnmount(() => {
    if (queryDocs.get(tab.id) === handle) queryDocs.delete(tab.id);
    if (saveState.value === 'dirty') void save();
    // Closing the tab keeps its text in the timeline.
    else if (query.value) void timelineApi.checkpoint(tab.queryId).then((v) => { if (v) timelineSeq.value++; }).catch(() => {});
  });

  async function rename(name: string) {
    if (!query.value || !name.trim() || name === query.value.name) return;
    query.value = await conns.saveQuery({ ...query.value, name: name.trim(), sql: text.value });
  }

  async function changeDatabase(db: string) {
    if (!query.value) return;
    query.value = await conns.saveQuery({ ...query.value, database: db, sql: text.value });
    tabs.retarget(tab.id, tab.connectionId, db);
  }

  async function followDatabase(db: string) {
    if (query.value) query.value = await conns.saveQuery({ ...query.value, database: db, sql: text.value });
    tabs.retarget(tab.id, tab.connectionId, db);
  }

  return {
    query, text, saveState, loadError, queryId: tab.queryId, autosave: true,
    title: computed(() => query.value?.name ?? t('query:resultName')),
    load, save, rename, changeDatabase, followDatabase,
    conflict: ref(null), missing: ref(null),
  };
}

// -- a project's file -----------------------------------------------------------------

/** What other parts need from an open file tab (the watcher, the quit
 *  guard, "Guardar todo" before a pull). Keyed by tab id. */
export interface FileDocHandle {
  tabId: string;
  projectId(): string;
  path(): string;
  dirty(): boolean;
  /** The file as last read or written here. */
  known(): FileStat | null;
  save(): Promise<boolean>;
  /** The file changed on disk (`stat`), or is gone (`stat.exists` false). */
  external(stat: FileStat): void;
  projectGone(gone: boolean): void;
}
export const fileDocs = new Map<string, FileDocHandle>();

export function useFileDocument(tab: FileTab): TabDocument {
  const tabs = useTabsStore();
  const projects = useProjectsStore();
  const text = ref('');
  const loadError = ref<string | null>(null);
  const saveState = ref<SaveState>('saved');
  const conflict = ref<null | { disk: FileStat | null }>(null);
  const missing = ref<null | 'file' | 'project'>(null);
  /** The text as on disk, and how it's written back. */
  let savedText = '';
  let meta: { eol: 'lf' | 'crlf'; bom: boolean } = { eol: 'lf', bom: false };
  let stat: FileStat | null = null;
  let loading = false;

  async function load() {
    loadError.value = null;
    loading = true;
    try {
      const f = await projectsApi.readFile(tab.projectId, tab.path);
      savedText = f.text;
      text.value = f.text;
      meta = { eol: f.eol, bom: f.bom };
      stat = { path: f.path, exists: true, mtime_ms: f.mtime_ms, size: f.size, hash: f.hash };
      saveState.value = 'saved';
      conflict.value = null;
      missing.value = null;
    } catch (e) {
      loadError.value = errorMessage(e);
    } finally {
      // The text watcher runs after this tick: it must see the new baseline.
      queueMicrotask(() => { loading = false; });
    }
  }
  watch(() => `${tab.projectId}\u0000${tab.path}`, () => { void load(); }, { immediate: true });

  watch(text, (v) => {
    if (loading || saveState.value === 'saving') return;
    if (v === savedText) { if (saveState.value === 'dirty') saveState.value = 'saved'; return; }
    saveState.value = 'dirty';
    tabs.pin(tab.id);
  });

  async function write(check: boolean): Promise<boolean> {
    if (missing.value === 'project') return false;
    const sending = text.value;
    saveState.value = 'saving';
    try {
      const out = await projectsApi.writeFile({
        id: tab.projectId, path: tab.path, text: sending, eol: meta.eol, bom: meta.bom,
        expectedHash: check && stat?.exists ? stat.hash : null,
      });
      if (out.conflict || !out.written) {
        // Deleted meanwhile: "Guardar de nuevo" writes it without the check.
        if (!out.stat.exists) missing.value = 'file';
        else conflict.value = { disk: out.stat };
        saveState.value = 'dirty';
        return false;
      }
      stat = out.stat;
      savedText = sending;
      conflict.value = null;
      missing.value = null;
      saveState.value = text.value === sending ? 'saved' : 'dirty';
      projects.refreshStatusSoon(tab.projectId);
      return true;
    } catch (e) {
      saveState.value = 'error';
      ElMessage.error(t('projects:file.saveFailed', { name: baseName(tab.path), error: errorMessage(e) }));
      return false;
    }
  }

  /** One save at a time (⌘S, "Guardar todo" from a pull or a quit). */
  let pending: Promise<boolean> | null = null;
  const save = (): Promise<boolean> => {
    if (pending) return pending;
    if (saveState.value === 'saved' && !missing.value) return Promise.resolve(true);
    pending = write(true).finally(() => { pending = null; });
    return pending;
  };

  async function changeDatabase(db: string) {
    tabs.retarget(tab.id, tab.connectionId, db);
    tab.pinnedTarget = true;
    tabs.persist();
  }

  const handle: FileDocHandle = {
    tabId: tab.id,
    projectId: () => tab.projectId,
    path: () => tab.path,
    dirty: () => saveState.value === 'dirty' || saveState.value === 'error',
    known: () => stat,
    save,
    external(disk) {
      if (saveState.value === 'saving' || loading) return;
      if (!disk.exists) {
        if (stat?.exists !== false) missing.value = 'file';
        return;
      }
      if (stat && disk.hash === stat.hash) { if (missing.value === 'file') missing.value = null; return; }
      if (saveState.value === 'saved') void load();
      else conflict.value = { disk };
    },
    projectGone(gone) {
      if (gone) missing.value = 'project';
      else if (missing.value === 'project') missing.value = null;
    },
  };
  fileDocs.set(tab.id, handle);
  // The editor's runs from this tab go to the file's timeline.
  watch(() => `${tab.projectId}\u0000${tab.path}`, () => runFiles.set(tab.id, { projectId: tab.projectId, path: tab.path }), { immediate: true });
  onBeforeUnmount(() => {
    if (fileDocs.get(tab.id) === handle) fileDocs.delete(tab.id);
    runFiles.delete(tab.id);
  });

  return {
    text, saveState, loadError, conflict, missing, queryId: undefined, autosave: false,
    title: computed(() => baseName(tab.path)),
    load, save, changeDatabase,
    followDatabase: changeDatabase,
    reloadFromDisk: load,
    keepMine: () => write(false),
  };
}

// -- languages and file extensions ----------------------------------------------------

/** Extensions opened as scripts (mirrors the backend's SCRIPT_EXTS). */
export const SCRIPT_EXTS = ['sql', 'cql', 'js', 'json', 'txt', 'redis', 'cypher', 'flux', 'ksql', 'n1ql', 'psql'];

export const extOf = (path: string) => {
  const name = baseName(path);
  const i = name.lastIndexOf('.');
  return i > 0 ? name.slice(i + 1).toLowerCase() : '';
};

export const isScriptFile = (path: string) => SCRIPT_EXTS.includes(extOf(path));

/** The editor's language for a file when no engine says it. */
export function languageForExt(ext: string): Language | undefined {
  switch (ext) {
    case 'sql': case 'psql': case 'ksql': case 'n1ql': return 'sql';
    case 'cql': return 'cql';
    case 'js': case 'json': return 'json';
    case 'redis': return 'redis';
    case 'cypher': return 'cypher';
    case 'flux': return 'flux';
    default: return undefined;
  }
}

/** Whether a script file's extension fits the engine it would run on
 *  (the backend's engines_for_ext, by query language). `.txt` fits any. */
export function extFitsDriver(ext: string, d: DriverInfo | undefined): boolean {
  if (!d || ext === 'txt' || !SCRIPT_EXTS.includes(ext)) return true;
  switch (ext) {
    case 'js': case 'json': return d.language === 'json';
    case 'cql': return d.language === 'cql';
    case 'redis': return d.language === 'redis';
    case 'cypher': return d.language === 'cypher';
    case 'flux': return d.language === 'flux';
    default: return d.language === 'sql';
  }
}
