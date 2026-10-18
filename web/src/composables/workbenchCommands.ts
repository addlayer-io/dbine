import { onScopeDispose } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { ElMessage } from 'element-plus';
import { windowApi } from '../api/client';
import type { DriverInfo, Permissions, SavedConnection } from '../api/types';
import { t } from '../i18n';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { useProjectsStore } from '../stores/projects';
import { useTabsStore } from '../stores/tabs';
import { useUiStore, type SidebarView } from '../stores/ui';
import { createDatabase, newFromTemplate } from './actions';
import { defineCommand, registerCommand, type CommandGroup } from './commands';
import { openDbDocs } from './dbDocs';
import { activeTarget, openCommandPalette, openGoTo, resolveConnection, resolveDatabase } from './palette';
import { queryControls, type QueryControls } from './queryControls';
import { openQuickPick } from './quickPick';
import { requestQuit } from './quitGuard';
import { newMigration } from './savedMigrations';
import { checkForUpdateNow } from './updates';

// The command palette's actions, on the action registry (composables/
// commands.ts): what the menus, the explorer's context menus, the editor's
// toolbar and Configuración offer, besides the registry's own shortcuts
// (⌘N, ⌘B, ⌘↵…, whose handlers App.vue, the editors and the results
// register). Being in the registry, each one can get keys in Configuración ›
// Atajos de teclado; ⌘⇧P (the palette) and ⌘P ("Ir a…") are defaults.
//
// Database and connection actions follow the engine's capabilities as the
// explorer's menus do; they take the active tab's connection or database,
// or ask for it (composables/palette.ts). App.vue installs them
// (`useWorkbenchCommands`) with what lives in its layout.

/** What the commands run that lives in App.vue's layout. */
export interface WorkbenchLayout {
  showView: (v: SidebarView) => void;
  feedback: () => void;
}

interface Action {
  id: string;
  group: CommandGroup;
  run: () => void;
  when?: () => boolean;
  /** Default keys (CodeMirror style), when it has some. */
  key?: string;
}

type DbAccept = (d: DriverInfo, c: SavedConnection) => boolean;

/** The connection's engine is known and `accept` takes it. */
function accepting(accept: DbAccept) {
  const conns = useConnectionsStore();
  return (c: SavedConnection) => {
    const d = conns.driverOf(c.id);
    return !!d && accept(d, c);
  };
}

/** Available while the active tab's connection takes it, or (with no tab
 *  that has one) while some saved connection does. */
function whenAccepted(accept: DbAccept) {
  return () => {
    const conns = useConnectionsStore();
    const ok = accepting(accept);
    const a = activeTarget();
    if (a) {
      const c = conns.byId(a.connectionId);
      return !!c && ok(c);
    }
    return conns.list.some(ok);
  };
}

/** The login lacks the privilege: say so (the explorer's menu shows the item off). */
function deniedHere(connectionId: string, database: string, action: keyof Permissions): boolean {
  const missing = useConnectionsStore().denied(connectionId, database, action);
  if (missing) ElMessage.warning(t('common:noPermission', { missing }));
  return !!missing;
}

/** An action on a database (the explorer's database menu). */
function onDatabase(id: string, act: (cid: string, db: string) => void, accept: DbAccept = () => true): Action {
  return {
    id, group: 'database', when: whenAccepted(accept),
    run: () => { void resolveDatabase(accepting(accept)).then((x) => { if (x) act(x.connectionId, x.database); }); },
  };
}

/** An action on a connection (the explorer's connection menu). */
function onConnection(id: string, act: (cid: string) => void, accept: DbAccept = () => true): Action {
  return {
    id, group: 'connection', when: whenAccepted(accept),
    run: () => { void resolveConnection(accepting(accept)).then((cid) => { if (cid) act(cid); }); },
  };
}

/** The fixed default schema new objects go to (as the explorer's). */
function defaultSchema(d: DriverInfo): string | null {
  if (!d.has_schemas) return null;
  return d.dialect === 'mssql' ? 'dbo' : d.dialect === 'postgres' ? 'public' : null;
}

function actions(layout: WorkbenchLayout): Action[] {
  const conns = useConnectionsStore();
  const tabs = useTabsStore();
  const ui = useUiStore();
  const projects = useProjectsStore();
  const connected = (c: SavedConnection) => conns.isConnected(c.id);
  const hasDatabases = (d: DriverInfo) => !!d.databases_label;

  /** The active query or file tab's controls, when it's one. */
  const editorTab = (): QueryControls | null => {
    const tab = tabs.active;
    return tab && (tab.kind === 'query' || tab.kind === 'file') ? queryControls.get(tab.id) ?? null : null;
  };
  /** An action of the active query editor (beyond the registry's own ⌘↵ & co.). */
  const onEditor = (id: string, run: (e: QueryControls) => void, when: (e: QueryControls) => boolean): Action => ({
    id, group: 'editor',
    run: () => { const e = editorTab(); if (e) run(e); },
    when: () => { const e = editorTab(); return !!e && when(e); },
  });
  const idle = (e: QueryControls) => !e.running && !e.txBusy;
  const view = (id: string, v: SidebarView): Action => ({ id, group: 'view', run: () => layout.showView(v) });
  const settings = (id: string, section: Parameters<typeof ui.openSettings>[0]): Action => ({ id, group: 'settings', run: () => ui.openSettings(section) });

  return [
    // -- General (beyond ⌘N, ⌘W, ⌘, …)
    { id: 'file.newConnection', group: 'general', run: () => ui.newConnection() },
    { id: 'file.newFolder', group: 'general', run: () => ui.newFolder(null) },
    { id: 'file.newScheduledTask', group: 'general', run: () => tabs.openScheduledTask(null) },
    { id: 'file.newNotebook', group: 'general', run: () => tabs.newNotebook() },
    { id: 'file.closeOtherTabs', group: 'general', run: () => { if (tabs.activeId) tabs.closeOthers(tabs.activeId); }, when: () => tabs.tabs.length > 1 && !!tabs.activeId },
    { id: 'file.closeAllTabs', group: 'general', run: () => tabs.closeAll(), when: () => tabs.tabs.length > 0 },

    // -- Vista
    { id: 'palette.open', group: 'view', key: 'Mod-Shift-p', run: () => { void openCommandPalette(); } },
    { id: 'goto.open', group: 'view', key: 'Mod-p', run: () => { void openGoTo(); } },
    view('view.showExplorer', 'explorer'),
    view('view.showProjects', 'projects'),
    view('view.showLibrary', 'library'),
    view('view.showHistory', 'history'),
    view('view.showScheduled', 'scheduled'),
    {
      id: 'view.revealActive', group: 'view',
      when: () => !!activeTarget() || tabs.active?.kind === 'file' || tabs.active?.kind === 'fileDiff' || (tabs.active?.kind === 'notebook' && !!tabs.active.projectId),
      run: () => {
        const tab = tabs.active;
        if (!tab) return;
        if (tab.kind === 'file' || tab.kind === 'fileDiff') { projects.focusProject(tab.projectId); return; }
        if (tab.kind === 'notebook') { if (tab.projectId) projects.focusProject(tab.projectId); return; }
        ui.revealInExplorer(tab.kind === 'query' ? { connectionId: tab.connectionId, database: tab.database, queryId: tab.queryId }
          : tab.kind === 'object' ? { connectionId: tab.connectionId, database: tab.database, object: tab.object }
            : { connectionId: tab.connectionId, database: tab.database });
      },
    },

    // -- Editor (the query editor's toolbar and menus)
    onEditor('editor.runSelection', (e) => e.editor.runSelection(), (e) => e.editor.runnable && !e.running && e.editor.hasSelection()),
    onEditor('editor.cancel', (e) => e.editor.cancel(), (e) => e.running),
    onEditor('editor.optimize', (e) => e.editor.optimize(), (e) => e.editor.runnable),
    onEditor('editor.runMultiDb', (e) => e.editor.runMultiDb(), (e) => e.editor.runnable && e.editor.multiDb),
    onEditor('editor.saveToLibrary', (e) => e.editor.saveToLibrary(), () => true),
    onEditor('editor.showProblems', (e) => e.editor.showProblems(), (e) => e.editor.lint),
    onEditor('editor.commit', (e) => e.editor.endTransaction(true), (e) => (e.txOpen || e.manualTx) && idle(e)),
    onEditor('editor.rollback', (e) => e.editor.endTransaction(false), (e) => (e.txOpen || e.manualTx) && idle(e)),
    onEditor('editor.toggleManualTx', (e) => { void e.setManualTx(!e.manualTx); }, (e) => e.offersManualTx && idle(e)),
    onEditor('editor.toggleContinueOnError', (e) => e.editor.toggleContinueOnError(), (e) => e.editor.perStatement),

    // -- Base de datos (the explorer's database menu)
    onDatabase('database.reveal', (cid, db) => ui.revealInExplorer({ connectionId: cid, database: db })),
    onDatabase('database.search', (cid, db) => tabs.openSearch(cid, db), hasDatabases),
    onDatabase('database.healthCheck', (cid, db) => tabs.openHealth(cid, db), hasDatabases),
    onDatabase('database.designer', (cid, db) => tabs.openDesigner(cid, db, null), (d) => !!d.designer),
    onDatabase('database.newFromTemplate', (cid, db) => {
      const d = conns.driverOf(cid);
      if (!d) return;
      void openQuickPick({
        placeholder: t('palette:pick.template'),
        items: () => d.create_templates.map((tpl, i) => ({ id: String(i), label: tb(tpl.label) })),
      }).then((picked) => {
        const tpl = picked ? d.create_templates[Number(picked.id)] : undefined;
        if (tpl) void newFromTemplate(cid, db, tpl, defaultSchema(d));
      });
    }, (d) => d.create_templates.length > 0),
    onDatabase('database.diagram', (cid, db) => tabs.openDiagram(cid, db)),
    onDatabase('database.queryBuilder', (cid, db) => tabs.openQueryBuilder(cid, db), (d) => d.language === 'sql' || d.language === 'cql'),
    onDatabase('database.document', (cid, db) => openDbDocs(cid, db)),
    onDatabase('database.profiler', (cid, db) => { if (!deniedHere(cid, db, 'profiler')) tabs.openProfiler(cid, db); }, (d) => d.supports_profiler),
    onDatabase('database.scriptGenerator', (cid, db) => ui.openDbDialog('script', cid, db)),
    onDatabase('database.migrate', (cid, db) => newMigration(cid, db)),
    onDatabase('database.compareSchemas', (cid, db) => tabs.openCompare(cid, db)),
    onDatabase('database.compareData', (cid, db) => tabs.openDataCompare(cid, db, null)),
    onDatabase('database.usersAndPermissions', (cid, db) => tabs.openSecurity(cid, db), (d) => !!d.security),
    onDatabase('database.backups', (cid, db) => tabs.openBackups(cid, db)),
    onDatabase('database.importData', (cid, db) => ui.openDbDialog('import', cid, db)),
    onDatabase('database.runScriptFile', (cid, db) => ui.openDbDialog('run', cid, db)),
    onDatabase('database.linkProject', (cid, db) => { projects.dialog = { kind: 'link', mode: 'link', binding: null, target: { connection_id: cid, database: db } }; }),

    // -- Conexión (the explorer's connection menu)
    onConnection('connection.connect', (cid) => { void conns.connect(cid); }, (_d, c) => !connected(c)),
    onConnection('connection.disconnect', (cid) => { void conns.disconnect(cid); }, (_d, c) => connected(c)),
    onConnection('connection.monitor', (cid) => tabs.openMonitor(cid), (d) => !!d.capabilities.monitor),
    onConnection('connection.serverBackups', (cid) => tabs.openBackups(cid, ''), (d, c) => connected(c) && !!d.backup?.server_wide && hasDatabases(d)),
    onConnection('connection.newDatabase', (cid) => { if (!deniedHere(cid, '', 'create_database')) createDatabase(cid); },
      (d, c) => connected(c) && !!d.capabilities.create_database && !c.config.read_only),
    onConnection('connection.edit', (cid) => ui.editConnection(cid)),
    onConnection('connection.duplicate', (cid) => ui.duplicateConnection(cid)),

    // -- Preferencias (Configuración's sections; ⌘, opens General)
    settings('settings.sync', 'sync'),
    settings('settings.drivers', 'drivers'),
    settings('settings.mcp', 'mcp'),
    settings('settings.devices', 'devices'),
    settings('settings.mail', 'mail'),
    settings('settings.lint', 'lint'),
    settings('settings.snippets', 'snippets'),
    settings('settings.shortcuts', 'shortcuts'),

    // -- Ayuda
    { id: 'help.checkUpdates', group: 'help', run: () => { void checkForUpdateNow(); } },
    { id: 'help.support', group: 'help', run: () => { invoke('open_support_page').catch(() => {}); } },
    { id: 'help.feedback', group: 'help', run: layout.feedback },
  ];
}

/** Define and register the palette's actions, plus handlers for the
 *  registry's shortcuts nothing else runs from the palette (a new window,
 *  quit). Call from App.vue's setup. */
export function useWorkbenchCommands(layout: WorkbenchLayout) {
  const releases: (() => void)[] = [];
  for (const a of actions(layout)) {
    defineCommand({ id: a.id, label: `palette:cmd.${a.id}`, group: a.group, scope: 'global', key: a.key ?? null });
    releases.push(registerCommand(a.id, a.run, a.when));
  }
  // Their keys are the menu's or the window's (appMenu.ts); these run them from the palette.
  releases.push(registerCommand('file.newWindow', () => { windowApi.newWindow().catch(() => {}); }));
  releases.push(registerCommand('app.quit', () => { void requestQuit(); }));
  onScopeDispose(() => releases.forEach((f) => f()));
}
