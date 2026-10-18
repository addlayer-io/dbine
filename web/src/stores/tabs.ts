import { acceptHMRUpdate, defineStore } from 'pinia';
import { watch } from 'vue';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { ElMessage } from 'element-plus';
import { api } from '../api/client';
import type { ObjectRef, ProjectTarget, SavedQuery } from '../api/types';
import { emptySpec, type QuerySpec } from '../api/queryBuilder';
import { t as tr } from '../i18n';
import { readJson, writeJson } from './storage';
import { initWindowRole, windowRole } from '../composables/windowRole';
import type { ParamValue } from '../composables/queryParams';
import * as eg from './editorGroups';
import type { EditorGroup, SplitDir } from './editorGroups';

// Editor tabs. A query tab points at a saved query (its text lives in the
// state store, not here), so reopening a query focuses its tab instead of
// opening another. Object tabs show a table's data / structure / source.
// A single-click in the explorer opens a *preview* tab that the next
// single-click replaces (as in VS Code); editing or double-clicking pins it.
// Tabs stay together by connection (Chrome-style groups): a new tab opens
// in its connection's group, and a group can be collapsed.
// Split editor: the tabs are spread over editor groups (./editorGroups.ts);
// `activeId` / `active` are the focused group's, and new tabs open there.

export type ObjectView = 'data' | 'structure' | 'definition';

interface TabBase {
  id: string;
  connectionId: string;
  database: string;
  preview: boolean;
  /** Its editor group (split editor); absent in tabs saved before them: the first group. */
  group?: string;
}

export interface QueryTab extends TabBase {
  kind: 'query';
  queryId: string;
  /** "Seguir si hay un error"; absent: the engine's default. */
  continueOnError?: boolean;
  /** Manual transactions (autocommit off), on drivers that offer it. */
  manualTx?: boolean;
  /** Parameters (`:name`…) are not looked for before a run. */
  paramsOff?: boolean;
  /** The last values given to the parameters, by name. */
  params?: Record<string, ParamValue>;
}

/** Tabs that must ask before closing (a query tab with an open
 *  transaction): resolves true to go on. QueryView registers them. */
export const closeGuards = new Map<string, () => Promise<boolean>>();

/** Ask every guarded tab of `ids`, one after another; false if one says no. */
async function confirmClose(ids: string[]): Promise<boolean> {
  for (const id of ids) {
    const guard = closeGuards.get(id);
    if (guard && !(await guard())) return false;
  }
  return true;
}

export interface ObjectTab extends TabBase {
  kind: 'object';
  object: ObjectRef;
  view: ObjectView;
}

/** Designing a new table / collection / index… (the driver's designer). */
export interface DesignerTab extends TabBase {
  kind: 'designer';
  /** Schema preselected (the one the user right-clicked in). */
  schema: string | null;
  /** "Modificar tabla…": the existing table being edited. */
  table?: ObjectRef | null;
}

/** The ER diagram of a database. */
export interface DiagramTab extends TabBase {
  kind: 'diagram';
}

/** The server monitor dashboard of a connection. */
export interface MonitorTab extends TabBase {
  kind: 'monitor';
}

/** The profiler of a database: every statement run against it, live. */
export interface ProfilerTab extends TabBase {
  kind: 'profiler';
}

/** Migrating a database's structure to another engine: one tab per saved
 *  migration ("Migraciones" node). The entry is saved on the first change. */
export interface MigrationTab extends TabBase {
  kind: 'migration';
  /** Its saved migration (tabs from before saved migrations lack it: the view gives one). */
  migrationId?: string;
}

/** New / edit / duplicate connection form. `connectionId` is the edited
 *  connection ('' for a new one). */
export interface ConnectionFormTab extends TabBase {
  kind: 'connection';
  editId: string | null;
  duplicateOf: string | null;
  folderId: string | null;
}

/** "Comparar esquemas": this database (left) against another one. */
/** What a compare tab had picked (kept across restarts; not its result). */
export interface ComparePick { connectionId: string; database: string; schema?: string; table?: string }
export interface ComparePicks { left: ComparePick; right: ComparePick; key?: string[] }

export interface CompareTab extends TabBase {
  kind: 'compare';
  picks?: ComparePicks;
}

/** "Comparar datos": a table's rows (left) against another table's
 *  (docs/data-compare.md). */
export interface DataCompareTab extends TabBase {
  kind: 'dataCompare';
  object: ObjectRef | null;
  picks?: ComparePicks;
}

/** "Usuarios y permisos" (docs/users-and-permissions.md). */
export interface SecurityTab extends TabBase {
  kind: 'security';
}

/** Backups (docs/backups.md); `database` "" = the whole server. */
export interface BackupsTab extends TabBase {
  kind: 'backups';
}

/** "Índices · <tabla>": a table's indexes and how they're used. */
export interface IndexesTab extends TabBase {
  kind: 'indexes';
  object: ObjectRef;
  /** The index to show highlighted (clicked in the explorer). */
  focus?: string | null;
}

/** "Dependencias · <objeto>": what depends on an object or one of its columns. */
export interface DependenciesTab extends TabBase {
  kind: 'dependencies';
  object: ObjectRef;
  column: string | null;
}

/** "Salud · <base>": the database's health check. */
export interface HealthTab extends TabBase {
  kind: 'health';
}

/** "Optimizar · <base>": a query's rewrites, index suggestions and comparison. */
export interface OptimizerTab extends TabBase {
  kind: 'optimizer';
  /** The query, as it was when the tab opened. */
  sql: string;
  /** The editor it came from and where the query was in it ("Reemplazar la consulta en su editor"). */
  sourceTabId: string | null;
  from: number;
  to: number;
}

/** "Subconjunto · <tabla>": copy some rows of a table, with what they need, masked. */
export interface SubsetTab extends TabBase {
  kind: 'subset';
  object: ObjectRef;
}

/** A scheduled task's editor and history (docs/scheduled-tasks.md).
 *  Not tied to a connection (`connectionId` ''); `taskId` null: a new one. */
export interface ScheduledTaskTab extends TabBase {
  kind: 'scheduledTask';
  taskId: string | null;
}

/** "Buscar · <base>": object names and the text of views, routines, triggers… */
export interface SearchTab extends TabBase {
  kind: 'search';
}

/** A file of a linked project (docs/projects.md), in the query editor.
 *  `connectionId`/`database` hold the target in use, or '' when unbound. */
export interface FileTab extends TabBase {
  kind: 'file';
  projectId: string;
  /** Relative to the repo root, with '/'. */
  path: string;
  /** The tab picked its own database: it stops following the project's active base. */
  pinnedTarget?: boolean;
  continueOnError?: boolean;
  manualTx?: boolean;
  paramsOff?: boolean;
  params?: Record<string, ParamValue>;
}

/** A project file's changes against the last commit (side by side). */
export interface FileDiffTab extends TabBase {
  kind: 'fileDiff';
  projectId: string;
  path: string;
}

/** A notebook (docs/notebooks.md): SQL and text cells in one .ipynb file.
 *  One of: a standalone file (`path`), a project's file (`projectId` +
 *  `projectPath`) or a new one not saved yet (`untitled`, "Notebook N").
 *  `connectionId`/`database` follow the notebook's target (for the tab's
 *  group and the assistant); each cell runs on its own target. */
export interface NotebookTab extends TabBase {
  kind: 'notebook';
  /** A standalone file's absolute path. */
  path?: string;
  projectId?: string;
  /** Relative to the project's root, with '/'. */
  projectPath?: string;
  /** Not saved yet: its number ("Notebook 2"); its draft is kept in localStorage. */
  untitled?: number;
}

/** The backend sessions a notebook tab opened (one per connection and
 *  database its cells ran on), closed with the tab. NotebookView fills it. */
export const notebookSessions = new Map<string, Set<string>>();

/** Where an untitled notebook's draft is kept between restarts. */
export const notebookDraftKey = (tabId: string) => `dbine.notebook.draft.${tabId}`;

/** Notebook tabs closed for good: their view writes no draft and marks
 *  nothing unsaved any more (a run still in flight, the unmount). The view
 *  takes its id out when it goes. */
export const closedNotebooks = new Set<string>();

/** "Diseñar consulta" (docs/query-builder.md): the design is kept with the tab. */
export interface QueryBuilderTab extends TabBase {
  kind: 'queryBuilder';
  spec: QuerySpec;
}

export type Tab = QueryTab | FileTab | FileDiffTab | NotebookTab | ObjectTab | DesignerTab | DiagramTab | MonitorTab | ProfilerTab | MigrationTab | CompareTab | DataCompareTab | SecurityTab | BackupsTab | IndexesTab | DependenciesTab | SearchTab | HealthTab | ScheduledTaskTab | ConnectionFormTab | QueryBuilderTab | SubsetTab | OptimizerTab;

/** Editor tabs that can't move to another database right now (running, or
 *  with an open transaction). QueryView keeps it up to date. */
export const busyTabs = new Set<string>();

/** The file's name, without its folder. */
export const baseName = (path: string) => path.slice(path.lastIndexOf('/') + 1);

/** A Jupyter notebook (opened in a notebook tab). */
export const isNotebookPath = (path: string) => /\.ipynb$/i.test(path);

const KEY = 'dbine.tabs';

/** This window's label, known synchronously at load ("main" outside Tauri
 *  and in the dev-preview, whose fake backend has no window metadata). */
const LABEL = (() => {
  try { return getCurrentWindow().label; } catch { return 'main'; }
})();

/** Whether this window owns the saved tabs and AI conversation. Several
 *  windows share localStorage: only the primary one ("main", or the window
 *  promoted when it closes) restores and saves them; the rest start empty.
 *  Until the backend has answered, only "main" counts as primary. */
export function ownsSavedState(): boolean {
  return windowRole.value.primary && windowRole.value.label === LABEL;
}

let followingPromotion = false;
/** A window promoted to primary saves its tabs at once, so they're the
 *  ones the next start restores; one with no tabs keeps the closed
 *  window's saved session instead of wiping it (its next change saves). */
function followPromotion() {
  if (followingPromotion) return;
  followingPromotion = true;
  void initWindowRole();
  watch(ownsSavedState, (owns) => {
    const tabs = useTabsStore();
    if (owns && tabs.tabs.length) tabs.persist();
  });
}

/** Profiler tabs to start on first show: only those just opened from the
 *  menu. A tab restored with the app waits for "Iniciar" (starting may
 *  change server settings). */
export const profilerAutostart = new Set<string>();

function newId() {
  return crypto.randomUUID();
}

/** Tabs side by side by connection, groups in the order they first appear. */
function grouped(tabs: Tab[]): Tab[] {
  const order: string[] = [];
  const by = new Map<string, Tab[]>();
  for (const t of tabs) {
    const g = by.get(t.connectionId);
    if (g) g.push(t);
    else { order.push(t.connectionId); by.set(t.connectionId, [t]); }
  }
  return order.flatMap((c) => by.get(c)!);
}

export const useTabsStore = defineStore('tabs', {
  state: () => {
    followPromotion();
    const saved = LABEL === 'main' ? readJson<eg.SavedLayout<Tab>>(KEY, {}) : {};
    // Older saves have no editor groups: their tabs make the one group.
    const layout = eg.restoreLayout({ ...saved, tabs: grouped(saved.tabs ?? []) }, newId());
    return {
      tabs: layout.tabs,
      /** The focused editor group's active tab. */
      activeId: layout.activeId,
      /** Connections whose tab group is collapsed (never a shown tab's). */
      collapsed: layout.collapsed,
      /** Editor groups (split editor), in the order they're laid out. */
      groups: layout.groups as EditorGroup[],
      /** The group new tabs open in; `activeId` is its active tab. */
      focused: layout.focused,
      /** Groups side by side ('row') or one above the other ('column'). */
      split: layout.split as SplitDir,
      /** The tab being dragged (not saved): drop targets show while it's set. */
      dragTab: null as string | null,
    };
  },
  getters: {
    active: (s) => s.tabs.find((t) => t.id === s.activeId) ?? null,
    /** A group's tabs, in order. */
    tabsOf: (s) => (gid: string) => eg.tabsOf(s, gid),
    /** A group's active tab id (the focused group's is `activeId`). */
    activeOf: (s) => (gid: string) => eg.activeOf(s, gid),
    /** The editor group a tab is in. */
    groupOf: (s) => (t: Tab) => eg.groupOf(s, t),
    /** The tab is the one its group shows. */
    isShown: (s) => (t: Tab) => eg.isShown(s, t),
  },
  actions: {
    persist() {
      if (!ownsSavedState()) return;
      writeJson(KEY, {
        tabs: this.tabs, activeId: this.activeId, collapsed: this.collapsed,
        groups: eg.savedGroups(this), focused: this.focused, split: this.split,
      });
    },

    /** Give an editor group the focus: new tabs go there, and it's the one
     *  the status bar, the assistant… speak about. */
    focusGroup(gid: string) {
      if (gid === this.focused) return;
      eg.focusGroup(this, gid);
      this.persist();
    },

    /** "Dividir a la derecha / abajo" (⌘\): tab `id` (the focused one by
     *  default) moves to a new group beside its own; alone in its group, a
     *  new empty group opens beside it for the next tab. */
    splitEditor(dir: SplitDir, id?: string | null) {
      if (eg.splitTab(this, dir, newId(), id === undefined ? this.activeId : id)) this.persist();
    },

    /** A tab dropped on group `gid`'s strip (before `beforeId`, or at its end). */
    moveTab(id: string, gid: string, beforeId: string | null = null) {
      eg.moveTab(this, id, gid, beforeId);
      this.persist();
    },

    /** A tab dropped on the right / bottom half of group `gid`: a new group there. */
    moveToNewGroup(id: string, gid: string, dir: SplitDir) {
      if (eg.splitInto(this, dir, gid, newId(), id)) this.persist();
    },

    /** Close an editor group: its tabs close (asking about guarded ones);
     *  an empty one just goes. */
    closeEditorGroup(gid: string) {
      if (!eg.tabsOf(this, gid).length) {
        eg.removeGroup(this, gid);
        return this.persist();
      }
      this.closeAsking((t) => eg.groupOf(this, t) === gid);
    },

    /** The divider before group `i` was dragged to `frac` of the two groups. */
    resizeGroups(i: number, frac: number, save = false) {
      eg.resizeGroups(this, i, frac);
      if (save) this.persist();
    },

    activate(id: string) {
      const tab = this.tabs.find((t) => t.id === id);
      if (tab) eg.focusGroup(this, eg.groupOf(this, tab));
      this.activeId = id;
      // A tab shown is never hidden in a collapsed group.
      const c = this.tabs.find((t) => t.id === id)?.connectionId;
      if (c !== undefined && this.collapsed.includes(c)) this.collapsed = this.collapsed.filter((x) => x !== c);
      this.persist();
    },

    /** Where a new tab goes in editor group `gid` (the focused one): next to
     *  the active tab of its connection, else at the end of its connection's
     *  group, else at the end. */
    slotFor(connectionId: string, gid?: string): number {
      return eg.slotFor(this, connectionId, gid ?? this.focused);
    },

    /** Collapse or expand a connection's group. Collapsing the group of the
     *  active tab moves to the nearest tab outside it (as Chrome does); with
     *  every other group collapsed, no tab is active and the welcome view
     *  shows (the tabs' views stay mounted: a profiler keeps running). */
    toggleGroup(connectionId: string) {
      if (this.collapsed.includes(connectionId)) {
        this.collapsed = this.collapsed.filter((c) => c !== connectionId);
        return this.persist();
      }
      // In every editor group that shows a tab of it.
      for (const g of this.groups) {
        const activeId = eg.activeOf(this, g.id);
        const at = this.tabs.findIndex((t) => t.id === activeId);
        if (at < 0 || this.tabs[at].connectionId !== connectionId) continue;
        const outside = (t: Tab) => eg.groupOf(this, t) === g.id && t.connectionId !== connectionId && !this.collapsed.includes(t.connectionId);
        const after = this.tabs.slice(at + 1).find(outside);
        const before = this.tabs.slice(0, at).reverse().find(outside);
        eg.setActive(this, g.id, (after ?? before)?.id ?? null);
      }
      this.collapsed = [...this.collapsed, connectionId];
      this.persist();
    },

    /** Close a connection's tabs (in editor group `gid` only, when given). */
    closeGroup(connectionId: string, gid?: string) {
      this.closeAsking((t) => t.connectionId === connectionId && (!gid || eg.groupOf(this, t) === gid));
    },

    closeOtherGroups(connectionId: string, gid?: string) {
      this.closeAsking((t) => t.connectionId !== connectionId && (!gid || eg.groupOf(this, t) === gid));
    },

    /** Close the tabs that match, after the guarded ones agree. */
    closeAsking(pred: (t: Tab) => boolean) {
      const ids = this.tabs.filter(pred).map((t) => t.id);
      confirmClose(ids).then((ok) => { if (ok) this.closeWhere((t) => ids.includes(t.id)); });
    },

    /** A query or file tab's run options ("Seguir si hay un error", transactions). */
    setQueryOptions(id: string, patch: Partial<Pick<QueryTab, 'continueOnError' | 'manualTx' | 'paramsOff' | 'params'>>) {
      const t = this.tabs.find((x) => x.id === id);
      if (t?.kind !== 'query' && t?.kind !== 'file') return;
      Object.assign(t, patch);
      this.persist();
    },

    openQuery(q: SavedQuery, preview = false) {
      const open = this.tabs.find((t) => t.kind === 'query' && t.queryId === q.id);
      if (open) {
        if (!preview) open.preview = false;
        return this.activate(open.id);
      }
      this.place({ id: newId(), kind: 'query', queryId: q.id, connectionId: q.connection_id, database: q.database, preview });
    },

    /** A project's file, on the project's active base (a preview tab by default).
     *  A notebook (.ipynb) opens in a notebook tab. */
    openFile(projectId: string, path: string, preview = true, target?: ProjectTarget | null) {
      if (isNotebookPath(path)) return this.openNotebook({ projectId, projectPath: path });
      const open = this.tabs.find((t) => t.kind === 'file' && t.projectId === projectId && t.path === path);
      if (open) {
        if (!preview) open.preview = false;
        return this.activate(open.id);
      }
      this.place({
        id: newId(), kind: 'file', projectId, path, preview,
        connectionId: target?.connection_id ?? '', database: target?.database ?? '',
      });
    },

    /** A notebook: a standalone file (`path`) or a project's (`projectId` +
     *  `projectPath`); focused when it's open already. */
    openNotebook(o: { path?: string; projectId?: string; projectPath?: string }) {
      const open = this.tabs.find((t) => t.kind === 'notebook' && (o.path
        ? t.path === o.path
        : !!o.projectId && t.projectId === o.projectId && t.projectPath === o.projectPath));
      if (open) return this.activate(open.id);
      this.place({ id: newId(), kind: 'notebook', connectionId: '', database: '', preview: false, ...o });
    },

    /** "Archivo › Nuevo notebook": an untitled one ("Notebook N"). */
    newNotebook() {
      const used = new Set(this.tabs.flatMap((t) => (t.kind === 'notebook' && t.untitled ? [t.untitled] : [])));
      let n = 1;
      while (used.has(n)) n++;
      this.place({ id: newId(), kind: 'notebook', connectionId: '', database: '', preview: false, untitled: n });
    },

    /** An untitled notebook was saved: it's that file from now on. */
    notebookSaved(id: string, path: string) {
      const t = this.tabs.find((x) => x.id === id);
      if (t?.kind !== 'notebook') return;
      t.path = path;
      delete t.untitled;
      try { localStorage.removeItem(notebookDraftKey(id)); } catch { /* storage blocked */ }
      this.persist();
    },

    /** The notebook's target changed: the tab joins that connection's group. */
    setNotebookTarget(id: string, connectionId: string, database: string) {
      const t = this.tabs.find((x) => x.id === id);
      if (t?.kind !== 'notebook' || (t.connectionId === connectionId && t.database === database)) return;
      this.retarget(id, connectionId, database);
    },

    /** A project file's diff tab (one per file). */
    openFileDiff(projectId: string, path: string) {
      const open = this.tabs.find((t) => t.kind === 'fileDiff' && t.projectId === projectId && t.path === path);
      if (open) return this.activate(open.id);
      this.place({ id: newId(), kind: 'fileDiff', projectId, path, connectionId: '', database: '', preview: true });
    },

    /** The project's active base changed: its file tabs follow it, except
     *  the ones that picked their own database, and the ones running or with
     *  an open transaction (they stay where they are, with a notice). */
    retargetProject(projectId: string, target: ProjectTarget | null) {
      const c = target?.connection_id ?? '';
      const d = target?.database ?? '';
      for (const t of this.tabs.filter((x) => x.kind === 'file' && x.projectId === projectId && !x.pinnedTarget)) {
        if (t.connectionId === c && t.database === d) continue;
        if (busyTabs.has(t.id)) {
          ElMessage.warning({ message: tr('projects:tabs.stayed', { name: baseName((t as FileTab).path) }), duration: 5000 });
          continue;
        }
        this.retarget(t.id, c, d);
      }
    },

    /** A file or folder of a project was renamed: its tabs follow. */
    renamePath(projectId: string, from: string, to: string) {
      for (const t of this.tabs) {
        if (t.kind === 'notebook' && t.projectId === projectId && t.projectPath) {
          if (t.projectPath === from) t.projectPath = to;
          else if (t.projectPath.startsWith(`${from}/`)) t.projectPath = to + t.projectPath.slice(from.length);
          continue;
        }
        if ((t.kind !== 'file' && t.kind !== 'fileDiff') || t.projectId !== projectId) continue;
        if (t.path === from) t.path = to;
        else if (t.path.startsWith(`${from}/`)) t.path = to + t.path.slice(from.length);
      }
      this.persist();
    },

    /** The project was unlinked: its tabs close (asking about unsaved ones). */
    closeProject(projectId: string) {
      this.closeAsking((t) => (t.kind === 'file' || t.kind === 'fileDiff' || t.kind === 'notebook') && t.projectId === projectId);
    },

    /** Connections that no longer exist: their tabs close, except project
     *  files, which stay open without a base (their text is the user's). */
    forgetConnections(gone: Set<string>) {
      for (const t of this.tabs) {
        if ((t.kind === 'file' || t.kind === 'notebook') && gone.has(t.connectionId)) { t.connectionId = ''; t.database = ''; }
      }
      this.closeWhere((t) => t.kind !== 'file' && t.kind !== 'fileDiff' && t.kind !== 'notebook' && gone.has(t.connectionId));
      this.persist();
    },

    openObject(connectionId: string, database: string, object: ObjectRef, view: ObjectView, preview = true) {
      const open = this.tabs.find(
        (t) => t.kind === 'object' && t.connectionId === connectionId && t.database === database &&
          t.object.kind === object.kind && t.object.schema === object.schema && t.object.name === object.name,
      ) as ObjectTab | undefined;
      if (open) {
        open.view = view;
        if (!preview) open.preview = false;
        return this.activate(open.id);
      }
      this.place({ id: newId(), kind: 'object', connectionId, database, object, view, preview });
    },

    /** Add a tab to the focused editor group: in place of its preview tab
     *  when it's one, else after the active tab. */
    place(tab: Tab) {
      tab.group = this.focused;
      const previewIdx = tab.preview ? this.tabs.findIndex((t) => t.preview && eg.groupOf(this, t) === this.focused) : -1;
      if (previewIdx >= 0 && this.tabs[previewIdx].connectionId === tab.connectionId) {
        this.release(this.tabs[previewIdx]);
        this.tabs.splice(previewIdx, 1, tab);
      } else {
        // A preview of another connection goes; the new tab joins its own group.
        if (previewIdx >= 0) {
          this.release(this.tabs[previewIdx]);
          this.tabs.splice(previewIdx, 1);
        }
        this.tabs.splice(this.slotFor(tab.connectionId), 0, tab);
      }
      this.activate(tab.id);
    },

    openDesigner(connectionId: string, database: string, schema: string | null, table: ObjectRef | null = null) {
      const open = table && this.tabs.find((t) => t.kind === 'designer' && t.connectionId === connectionId && t.database === database
        && t.table?.name === table.name && (t.table?.schema ?? null) === (table.schema ?? null));
      if (open) return this.activate(open.id);
      this.place({ id: newId(), kind: 'designer', connectionId, database, schema, table, preview: false });
    },

    openDiagram(connectionId: string, database: string) {
      const open = this.tabs.find((t) => t.kind === 'diagram' && t.connectionId === connectionId && t.database === database);
      if (open) return this.activate(open.id);
      this.place({ id: newId(), kind: 'diagram', connectionId, database, preview: false });
    },

    /** The connection form in a tab (one per edited connection; a new one
     *  reuses an open "new connection" tab). */
    openConnectionForm(o: { editId?: string | null; duplicateOf?: string | null; folderId?: string | null }) {
      const editId = o.editId ?? null;
      const open = this.tabs.find((t) => t.kind === 'connection' && t.editId === editId && !o.duplicateOf && !t.duplicateOf);
      if (open) return this.activate(open.id);
      this.place({
        id: newId(), kind: 'connection', connectionId: editId ?? '', database: '', preview: false,
        editId, duplicateOf: o.duplicateOf ?? null, folderId: o.folderId ?? null,
      });
    },

    openCompare(connectionId: string, database: string) {
      const open = this.tabs.find((t) => t.kind === 'compare' && t.connectionId === connectionId && t.database === database);
      if (open) return this.activate(open.id);
      this.place({ id: newId(), kind: 'compare', connectionId, database, preview: false });
    },

    openSecurity(connectionId: string, database: string) {
      const open = this.tabs.find((t) => t.kind === 'security' && t.connectionId === connectionId && t.database === database);
      if (open) return this.activate(open.id);
      this.place({ id: newId(), kind: 'security', connectionId, database, preview: false });
    },

    openBackups(connectionId: string, database: string) {
      const open = this.tabs.find((t) => t.kind === 'backups' && t.connectionId === connectionId && t.database === database);
      if (open) return this.activate(open.id);
      this.place({ id: newId(), kind: 'backups', connectionId, database, preview: false });
    },

    /** A table's "Índices" tab (one per table), focusing `focus` when given. */
    openIndexes(connectionId: string, database: string, object: ObjectRef, focus: string | null = null) {
      const open = this.tabs.find(
        (t) => t.kind === 'indexes' && t.connectionId === connectionId && t.database === database &&
          t.object.schema === object.schema && t.object.name === object.name,
      ) as IndexesTab | undefined;
      if (open) {
        open.focus = focus;
        return this.activate(open.id);
      }
      this.place({ id: newId(), kind: 'indexes', connectionId, database, object, focus, preview: false });
    },

    /** An object's (or column's) "Dependencias" tab, one per target. */
    openDependencies(connectionId: string, database: string, object: ObjectRef, column: string | null = null) {
      const open = this.tabs.find(
        (t) => t.kind === 'dependencies' && t.connectionId === connectionId && t.database === database &&
          t.object.kind === object.kind && t.object.schema === object.schema && t.object.name === object.name && t.column === column,
      );
      if (open) return this.activate(open.id);
      this.place({ id: newId(), kind: 'dependencies', connectionId, database, object, column, preview: false });
    },

    /** A database's "Chequeo de salud" tab, one per database. */
    openHealth(connectionId: string, database: string) {
      const open = this.tabs.find((t) => t.kind === 'health' && t.connectionId === connectionId && t.database === database);
      if (open) return this.activate(open.id);
      this.place({ id: newId(), kind: 'health', connectionId, database, preview: false });
    },

    /** "Optimizar consulta" of `sql` (one tab per query and editor). */
    openOptimizer(connectionId: string, database: string, sql: string, sourceTabId: string | null, from: number, to: number) {
      const open = this.tabs.find((t) => t.kind === 'optimizer' && t.connectionId === connectionId && t.database === database && t.sql === sql && t.sourceTabId === sourceTabId);
      if (open) return this.activate(open.id);
      this.place({ id: newId(), kind: 'optimizer', connectionId, database, sql, sourceTabId, from, to, preview: false });
    },

    /** A table's "Copiar un subconjunto" tab, one per table. */
    openSubset(connectionId: string, database: string, object: ObjectRef) {
      const open = this.tabs.find(
        (t) => t.kind === 'subset' && t.connectionId === connectionId && t.database === database && t.object.schema === object.schema && t.object.name === object.name,
      );
      if (open) return this.activate(open.id);
      this.place({ id: newId(), kind: 'subset', connectionId, database, object, preview: false });
    },

    /** A scheduled task's tab, one per task (`null`: a new one). */
    openScheduledTask(taskId: string | null) {
      const open = taskId && this.tabs.find((t) => t.kind === 'scheduledTask' && t.taskId === taskId);
      if (open) return this.activate(open.id);
      this.place({ id: newId(), kind: 'scheduledTask', connectionId: '', database: '', taskId, preview: false });
    },

    /** A database's "Buscar" tab, one per database. */
    openSearch(connectionId: string, database: string) {
      const open = this.tabs.find((t) => t.kind === 'search' && t.connectionId === connectionId && t.database === database);
      if (open) return this.activate(open.id);
      this.place({ id: newId(), kind: 'search', connectionId, database, preview: false });
    },

    /** A new "Diseñar consulta" tab, starting with `table` on the canvas when given. */
    openQueryBuilder(connectionId: string, database: string, table: ObjectRef | null = null) {
      const spec = emptySpec(database);
      if (table) spec.tables.push({ id: newId(), kind: table.kind, schema: table.schema, name: table.name, alias: '', x: 40, y: 40 });
      this.place({ id: newId(), kind: 'queryBuilder', connectionId, database, spec, preview: false });
    },

    /** A query builder tab's design, so reopening the app keeps it. */
    setBuilderSpec(id: string, spec: QuerySpec) {
      const t = this.tabs.find((x) => x.id === id);
      if (t?.kind !== 'queryBuilder') return;
      t.spec = spec;
      this.persist();
    },

    openDataCompare(connectionId: string, database: string, object: ObjectRef | null) {
      this.place({ id: newId(), kind: 'dataCompare', connectionId, database, object, preview: false });
    },

    /** A saved migration's tab (focused if open); without `migrationId`, a new draft. */
    openMigration(connectionId: string, database: string, migrationId?: string) {
      const open = migrationId && this.tabs.find((t) => t.kind === 'migration' && t.migrationId === migrationId);
      if (open) return this.activate(open.id);
      this.place({ id: newId(), kind: 'migration', connectionId, database, preview: false, migrationId: migrationId ?? newId() });
    },

    openMonitor(connectionId: string) {
      const open = this.tabs.find((t) => t.kind === 'monitor' && t.connectionId === connectionId);
      if (open) return this.activate(open.id);
      this.place({ id: newId(), kind: 'monitor', connectionId, database: '', preview: false });
    },

    openProfiler(connectionId: string, database: string) {
      const open = this.tabs.find((t) => t.kind === 'profiler' && t.connectionId === connectionId && t.database === database);
      if (open) return this.activate(open.id);
      const id = newId();
      profilerAutostart.add(id);
      this.place({ id, kind: 'profiler', connectionId, database, preview: false });
    },

    pin(id: string) {
      const t = this.tabs.find((x) => x.id === id);
      if (t?.preview) { t.preview = false; this.persist(); }
    },

    /** A compare tab's picks, so reopening the app keeps them. */
    setPicks(id: string, picks: ComparePicks) {
      const t = this.tabs.find((x) => x.id === id);
      if (t?.kind !== 'compare' && t?.kind !== 'dataCompare') return;
      if (JSON.stringify(t.picks) === JSON.stringify(picks)) return;
      t.picks = picks;
      this.persist();
    },

    setView(id: string, view: ObjectView) {
      const t = this.tabs.find((x) => x.id === id);
      if (t?.kind === 'object') { t.view = view; this.persist(); }
    },

    /** Move a query tab along with its query (renamed database…). */
    retarget(id: string, connectionId: string, database: string) {
      const i = this.tabs.findIndex((x) => x.id === id);
      if (i < 0) return;
      const [t] = this.tabs.splice(i, 1);
      const moved = t.connectionId !== connectionId;
      t.connectionId = connectionId;
      t.database = database;
      // Another connection: into that connection's group.
      this.tabs.splice(moved ? this.slotFor(connectionId, eg.groupOf(this, t)) : i, 0, t);
      this.persist();
    },

    /** Close a tab; one with an open transaction asks first (unless `force`). */
    close(id: string, force = false) {
      const guard = !force && closeGuards.get(id);
      if (guard) {
        guard().then((ok) => { if (ok) this.close(id, true); });
        return;
      }
      const i = this.tabs.findIndex((t) => t.id === id);
      if (i < 0) return;
      const gone = this.tabs[i];
      const gid = eg.groupOf(this, gone);
      this.release(gone, true);
      this.tabs.splice(i, 1);
      // Next its group's neighbor of the same connection, else any shown tab
      // of the group; the group closes with its last tab.
      eg.afterLeaving(this, gone, i, gid);
      if (!this.tabs.some((t) => t.connectionId === gone.connectionId)) this.collapsed = this.collapsed.filter((c) => c !== gone.connectionId);
      this.persist();
    },

    /** Close the other tabs of `id`'s editor group. */
    closeOthers(id: string, force = false) {
      const tab = this.tabs.find((x) => x.id === id);
      if (!tab) return;
      const gid = eg.groupOf(this, tab);
      const others = eg.tabsOf(this, gid).filter((x) => x.id !== id).map((t) => t.id);
      if (!force && others.some((x) => closeGuards.has(x))) {
        confirmClose(others).then((ok) => { if (ok) this.closeOthers(id, true); });
        return;
      }
      for (const t of this.tabs.filter((x) => others.includes(x.id))) this.release(t, true);
      this.tabs = this.tabs.filter((x) => !others.includes(x.id));
      this.collapsed = this.collapsed.filter((c) => this.tabs.some((t) => t.connectionId === c && t.id !== id));
      this.activate(id);
    },

    /** Close every tab (of editor group `gid` only, when given). */
    closeAll(force = false, gid?: string) {
      if (gid && this.groups.length > 1) return this.closeEditorGroup(gid);
      const ids = this.tabs.map((t) => t.id);
      if (!force && ids.some((x) => closeGuards.has(x))) {
        confirmClose(ids).then((ok) => { if (ok) this.closeAll(true); });
        return;
      }
      for (const t of this.tabs) this.release(t, true);
      this.tabs = [];
      this.activeId = null;
      this.collapsed = [];
      this.groups = [{ id: this.focused, activeId: null, size: 1 }];
      this.persist();
    },

    /** After "Renombrar…": the tabs of `from` (data, structure, definition,
     *  Índices, Dependencias…) follow it to `to`. Query tabs aren't edited. */
    renameObject(connectionId: string, database: string, from: ObjectRef, to: ObjectRef) {
      for (const t of this.tabs) {
        if (t.connectionId !== connectionId || t.database !== database || !('object' in t) || !t.object) continue;
        const o = t.object;
        if (o.name === from.name && (o.schema ?? null) === (from.schema ?? null) && (o.kind === from.kind || t.kind !== 'object')) t.object = { ...o, name: to.name, schema: to.schema };
      }
      this.persist();
    },

    /** After a schema rename: the tabs of its objects follow it. */
    renameSchema(connectionId: string, database: string, from: string, to: string) {
      for (const t of this.tabs) {
        if (t.connectionId === connectionId && t.database === database && 'object' in t && t.object?.schema === from) t.object = { ...t.object, schema: to };
      }
      this.persist();
    },

    /** After a column rename: its "Dependencias" tab follows it. */
    renameColumn(connectionId: string, database: string, table: ObjectRef, from: string, to: string) {
      for (const t of this.tabs) {
        if (t.kind === 'dependencies' && t.connectionId === connectionId && t.database === database && t.column === from
          && t.object.name === table.name && (t.object.schema ?? null) === (table.schema ?? null)) t.column = to;
      }
      this.persist();
    },

    /** Tabs of a query / connection that no longer exists. */
    closeWhere(pred: (t: Tab) => boolean) {
      for (const t of this.tabs.filter(pred)) this.close(t.id, true);
    },

    /** Before this window closes: free every tab's backend session, the
     *  same way closing each tab does. The tabs (and what's saved) stay as
     *  they are; the window goes away right after. */
    releaseAll() {
      for (const t of this.tabs) this.release(t);
    },

    /** The tab's backend session (its connection) is no longer needed.
     *  `closing`: the tab goes for good (not the window): an untitled
     *  notebook's draft goes with it. */
    release(t: Tab, closing = false) {
      if (t.kind === 'notebook') {
        for (const s of notebookSessions.get(t.id) ?? []) api.closeSession(s).catch(() => {});
        notebookSessions.delete(t.id);
        if (closing) {
          closedNotebooks.add(t.id);
          try { localStorage.removeItem(notebookDraftKey(t.id)); } catch { /* storage blocked */ }
        }
        return;
      }
      api.closeSession(t.id).catch(() => {});
    },
  },
});

// Development: swap the store's code in place when it changes (no reload).
if (import.meta.hot) import.meta.hot.accept(acceptHMRUpdate(useTabsStore, import.meta.hot));
