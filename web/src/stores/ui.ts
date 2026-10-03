import { acceptHMRUpdate, defineStore } from 'pinia';
import { useTabsStore } from './tabs';

// App-wide UI state that isn't layout: which dialog is open, on what.

export type Movable = { kind: 'connection' | 'folder'; id: string };

/** Ask the explorer to show something: the tab's query / object, or its
 *  database with its context menu open. */
export interface RevealRequest {
  seq: number;
  connectionId: string;
  database: string;
  queryId?: string;
  object?: { kind: string; schema: string | null; name: string };
  /** Open the node's context menu once shown. */
  menu?: boolean;
}
let revealSeq = 0;

/** The left sidebar's views (the activity bar's buttons). */
export type SidebarView = 'explorer' | 'projects' | 'library' | 'history';

export const useUiStore = defineStore('ui', {
  state: () => ({
    /** Folder dialog: `null` closed, `''` new, else the id being edited. */
    editingFolder: null as string | null,
    newFolderParent: null as string | null,
    moving: null as Movable | null,
    reveal: null as RevealRequest | null,
    /** Database-level dialog open (script, export, import, run a file). */
    /** What the left sidebar shows. */
    sidebarView: 'explorer' as SidebarView,
    /** Bumped after each run from the editor: the history view reloads. */
    historySeq: 0,
    /** Query and file tabs with changes not saved yet (tab id → state): the tab
     *  strip shows a dot. */
    unsaved: {} as Record<string, 'dirty' | 'saving' | 'error'>,
    /** The AI assistant's sidebar. */
    aiOpen: false,
    /** Configuración open, on this section. */
    settingsSection: null as 'general' | 'sync' | 'drivers' | 'mcp' | null,
    /** Bumped when a cloud restore replaced the local state: views reload. */
    syncSeq: 0,
    dbDialog: null as { kind: 'script' | 'export' | 'import' | 'run'; connectionId: string; database: string } | null,
  }),
  actions: {
    openSettings(section: 'general' | 'sync' | 'drivers' | 'mcp' = 'general') { this.settingsSection = section; },
    closeSettings() { this.settingsSection = null; },
    // The connection form opens as an editor tab.
    newConnection(folderId: string | null = null) { useTabsStore().openConnectionForm({ folderId }); },
    duplicateConnection(id: string) { useTabsStore().openConnectionForm({ duplicateOf: id }); },
    editConnection(id: string) { useTabsStore().openConnectionForm({ editId: id }); },

    newFolder(parentId: string | null) {
      this.newFolderParent = parentId;
      this.editingFolder = '';
    },
    editFolder(id: string) { this.editingFolder = id; },
    closeFolderDialog() { this.editingFolder = null; },

    moveItem(item: Movable) { this.moving = item; },
    revealInExplorer(r: Omit<RevealRequest, 'seq'>) { this.reveal = { ...r, seq: ++revealSeq }; },
    closeMove() { this.moving = null; },
    openDbDialog(kind: 'script' | 'export' | 'import' | 'run', connectionId: string, database: string) {
      this.dbDialog = { kind, connectionId, database };
    },
    closeDbDialog() { this.dbDialog = null; },
  },
});

// Development: swap the store's code in place when it changes (no reload).
if (import.meta.hot) import.meta.hot.accept(acceptHMRUpdate(useUiStore, import.meta.hot));
