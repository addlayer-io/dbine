import { acceptHMRUpdate, defineStore } from 'pinia';
import { watch } from 'vue';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { ElMessage } from 'element-plus';
import { api } from '../api/client';
import type { ObjectRef, ProjectTarget, SavedQuery } from '../api/types';
import { t as tr } from '../i18n';
import { readJson, writeJson } from './storage';
import { initWindowRole, windowRole } from '../composables/windowRole';

// Editor tabs. A query tab points at a saved query (its text lives in the
// state store, not here), so reopening a query focuses its tab instead of
// opening another. Object tabs show a table's data / structure / source.
// A single-click in the explorer opens a *preview* tab that the next
// single-click replaces (as in VS Code); editing or double-clicking pins it.
// Tabs stay together by connection (Chrome-style groups): a new tab opens
// in its connection's group, and a group can be collapsed.

export type ObjectView = 'data' | 'structure' | 'definition';

interface TabBase {
  id: string;
  connectionId: string;
  database: string;
  preview: boolean;
}

export interface QueryTab extends TabBase {
  kind: 'query';
  queryId: string;
  /** "Seguir si hay un error"; absent: the engine's default. */
  continueOnError?: boolean;
  /** Manual transactions (autocommit off), on drivers that offer it. */
  manualTx?: boolean;
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
 *  (docs/comparacion-de-datos.md). */
export interface DataCompareTab extends TabBase {
  kind: 'dataCompare';
  object: ObjectRef | null;
  picks?: ComparePicks;
}

/** "Usuarios y permisos" (docs/usuarios-y-permisos.md). */
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

/** "Buscar · <base>": object names and the text of views, routines, triggers… */
export interface SearchTab extends TabBase {
  kind: 'search';
}

/** A file of a linked project (docs/proyectos.md), in the query editor.
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
}

/** A project file's changes against the last commit (side by side). */
export interface FileDiffTab extends TabBase {
  kind: 'fileDiff';
  projectId: string;
  path: string;
}

export type Tab = QueryTab | FileTab | FileDiffTab | ObjectTab | DesignerTab | DiagramTab | MonitorTab | ProfilerTab | MigrationTab | CompareTab | DataCompareTab | SecurityTab | BackupsTab | IndexesTab | DependenciesTab | SearchTab | ConnectionFormTab;

/** Editor tabs that can't move to another database right now (running, or
 *  with an open transaction). QueryView keeps it up to date. */
export const busyTabs = new Set<string>();

/** The file's name, without its folder. */
export const baseName = (path: string) => path.slice(path.lastIndexOf('/') + 1);

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
    const empty = { tabs: [] as Tab[], activeId: null as string | null };
    const saved = LABEL === 'main'
      ? readJson<{ tabs: Tab[]; activeId: string | null; collapsed?: string[] }>(KEY, empty)
      : { ...empty, collapsed: [] as string[] };
    return {
      tabs: grouped(saved.tabs),
      activeId: saved.activeId,
      /** Connections whose tab group is collapsed. */
      // Groups that still have tabs, never the active tab's.
      collapsed: (saved.collapsed ?? []).filter((c) => saved.tabs.some((t) => t.connectionId === c) &&
        saved.tabs.find((t) => t.id === saved.activeId)?.connectionId !== c),
    };
  },
  getters: {
    active: (s) => s.tabs.find((t) => t.id === s.activeId) ?? null,
  },
  actions: {
    persist() {
      if (!ownsSavedState()) return;
      writeJson(KEY, { tabs: this.tabs, activeId: this.activeId, collapsed: this.collapsed });
    },

    activate(id: string) {
      this.activeId = id;
      // A tab shown is never hidden in a collapsed group.
      const c = this.tabs.find((t) => t.id === id)?.connectionId;
      if (c !== undefined && this.collapsed.includes(c)) this.collapsed = this.collapsed.filter((x) => x !== c);
      this.persist();
    },

    /** Where a new tab goes: next to the active tab of its connection, else
     *  at the end of its connection's group, else at the end. */
    slotFor(connectionId: string): number {
      const at = this.tabs.findIndex((t) => t.id === this.activeId);
      if (at >= 0 && this.tabs[at].connectionId === connectionId) return at + 1;
      let last = -1;
      this.tabs.forEach((t, i) => { if (t.connectionId === connectionId) last = i; });
      return last >= 0 ? last + 1 : this.tabs.length;
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
      if (this.active?.connectionId === connectionId) {
        const at = this.tabs.findIndex((t) => t.id === this.activeId);
        const outside = (t: Tab) => t.connectionId !== connectionId && !this.collapsed.includes(t.connectionId);
        const after = this.tabs.slice(at + 1).find(outside);
        const before = this.tabs.slice(0, at).reverse().find(outside);
        this.activeId = (after ?? before)?.id ?? null;
      }
      this.collapsed = [...this.collapsed, connectionId];
      this.persist();
    },

    closeGroup(connectionId: string) {
      this.closeAsking((t) => t.connectionId === connectionId);
    },

    closeOtherGroups(connectionId: string) {
      this.closeAsking((t) => t.connectionId !== connectionId);
    },

    /** Close the tabs that match, after the guarded ones agree. */
    closeAsking(pred: (t: Tab) => boolean) {
      const ids = this.tabs.filter(pred).map((t) => t.id);
      confirmClose(ids).then((ok) => { if (ok) this.closeWhere((t) => ids.includes(t.id)); });
    },

    /** A query or file tab's run options ("Seguir si hay un error", transactions). */
    setQueryOptions(id: string, patch: Pick<QueryTab, 'continueOnError' | 'manualTx'>) {
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

    /** A project's file, on the project's active base (a preview tab by default). */
    openFile(projectId: string, path: string, preview = true, target?: ProjectTarget | null) {
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
        if ((t.kind !== 'file' && t.kind !== 'fileDiff') || t.projectId !== projectId) continue;
        if (t.path === from) t.path = to;
        else if (t.path.startsWith(`${from}/`)) t.path = to + t.path.slice(from.length);
      }
      this.persist();
    },

    /** The project was unlinked: its tabs close (asking about unsaved ones). */
    closeProject(projectId: string) {
      this.closeAsking((t) => (t.kind === 'file' || t.kind === 'fileDiff') && t.projectId === projectId);
    },

    /** Connections that no longer exist: their tabs close, except project
     *  files, which stay open without a base (their text is the user's). */
    forgetConnections(gone: Set<string>) {
      for (const t of this.tabs) {
        if (t.kind === 'file' && gone.has(t.connectionId)) { t.connectionId = ''; t.database = ''; }
      }
      this.closeWhere((t) => t.kind !== 'file' && t.kind !== 'fileDiff' && gone.has(t.connectionId));
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

    /** Add a tab: in place of the preview tab when it's one, else after the
     *  active tab. */
    place(tab: Tab) {
      const previewIdx = tab.preview ? this.tabs.findIndex((t) => t.preview) : -1;
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

    openDesigner(connectionId: string, database: string, schema: string | null) {
      this.place({ id: newId(), kind: 'designer', connectionId, database, schema, preview: false });
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

    /** A database's "Buscar" tab, one per database. */
    openSearch(connectionId: string, database: string) {
      const open = this.tabs.find((t) => t.kind === 'search' && t.connectionId === connectionId && t.database === database);
      if (open) return this.activate(open.id);
      this.place({ id: newId(), kind: 'search', connectionId, database, preview: false });
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
      this.tabs.splice(moved ? this.slotFor(connectionId) : i, 0, t);
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
      this.release(gone);
      this.tabs.splice(i, 1);
      if (this.activeId === id) {
        // Next the neighbor of the same connection, else any shown tab.
        const near = [this.tabs[i], this.tabs[i - 1]].filter((t): t is Tab => !!t);
        const shown = (t: Tab) => !this.collapsed.includes(t.connectionId);
        this.activeId = (near.find((t) => t.connectionId === gone.connectionId) ?? near.find(shown) ?? this.tabs.find(shown) ?? near[0])?.id ?? null;
      }
      if (!this.tabs.some((t) => t.connectionId === gone.connectionId)) this.collapsed = this.collapsed.filter((c) => c !== gone.connectionId);
      this.persist();
    },

    closeOthers(id: string, force = false) {
      const others = this.tabs.filter((x) => x.id !== id).map((t) => t.id);
      if (!force && others.some((x) => closeGuards.has(x))) {
        confirmClose(others).then((ok) => { if (ok) this.closeOthers(id, true); });
        return;
      }
      for (const t of this.tabs.filter((x) => x.id !== id)) this.release(t);
      this.tabs = this.tabs.filter((x) => x.id === id);
      this.collapsed = [];
      this.activate(id);
    },

    closeAll(force = false) {
      const ids = this.tabs.map((t) => t.id);
      if (!force && ids.some((x) => closeGuards.has(x))) {
        confirmClose(ids).then((ok) => { if (ok) this.closeAll(true); });
        return;
      }
      for (const t of this.tabs) this.release(t);
      this.tabs = [];
      this.activeId = null;
      this.collapsed = [];
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

    /** The tab's backend session (its connection) is no longer needed. */
    release(t: Tab) {
      api.closeSession(t.id).catch(() => {});
    },
  },
});

// Development: swap the store's code in place when it changes (no reload).
if (import.meta.hot) import.meta.hot.accept(acceptHMRUpdate(useTabsStore, import.meta.hot));
