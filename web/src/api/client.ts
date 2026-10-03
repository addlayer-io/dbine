import { invoke } from '@tauri-apps/api/core';
import { tb } from '../i18n/backend';
import type {
  ColumnInfo, ConnectResult, ConnectionConfig, ConnectionFolder, DatabaseObjects, DbObject, DependencyReport, DependencyTarget, DriverInfo, IndexUsageReport, KeyPage, KeyScan, MonitorSnapshot, ObjectRef, Permissions, ProfiledStatement, ProfilerStarted,
  ExecuteResponse, RunMode, RunningTask, SavedConnection, SavedQuery, ScriptUnit, TaskSummary, TestResult, TxState, UpdateInfo, WindowRole,
} from './types';

// Every backend command, in one place. Commands take one `args` object whose
// fields are snake_case (serde's default on the Rust side).

export interface CommandErrorShape {
  kind: string;
  message: string;
}

/** An error's message in the app's language (the backend writes Spanish). */
export function errorMessage(e: unknown): string {
  if (e && typeof e === 'object' && 'message' in e) return tb(String((e as CommandErrorShape).message));
  return tb(String(e));
}

export function errorKind(e: unknown): string | null {
  return e && typeof e === 'object' && 'kind' in e ? String((e as CommandErrorShape).kind) : null;
}

export const api = {
  listDrivers: () => invoke<DriverInfo[]>('list_drivers'),
  listConnections: () => invoke<SavedConnection[]>('list_connections'),
  /** Secret fields filled in the config go to the keychain; empty ones keep
   *  the stored value. */
  saveConnection: (connection: SavedConnection) =>
    invoke<SavedConnection>('save_connection', { args: { connection } }),
  deleteConnection: (id: string) => invoke<void>('delete_connection', { args: { id } }),
  /** Trust an SSH tunnel server's key for a saved connection (docs/tuneles-ssh.md). */
  trustSshHost: (connectionId: string, fingerprint: string) =>
    invoke<void>('trust_ssh_host', { args: { connection_id: connectionId, fingerprint } }),
  testConnection: (config: ConnectionConfig, connectionId: string | null) =>
    invoke<TestResult>('test_connection', { args: { config, connection_id: connectionId } }),
  connect: (connectionId: string, password: string | null = null) =>
    invoke<ConnectResult>('connect', { args: { connection_id: connectionId, password } }),
  disconnect: (connectionId: string) => invoke<void>('disconnect', { args: { connection_id: connectionId } }),

  listFolders: () => invoke<ConnectionFolder[]>('list_folders'),
  saveFolder: (folder: ConnectionFolder) => invoke<ConnectionFolder>('save_folder', { args: { folder } }),
  deleteFolder: (id: string) => invoke<void>('delete_folder', { args: { id } }),
  moveConnection: (connectionId: string, folderId: string | null) =>
    invoke<void>('move_connection', { args: { connection_id: connectionId, folder_id: folderId } }),
  /** One explorer level (`parentId` null = top level): its connections or folders, in this order. */
  reorderExplorer: (parentId: string | null, kind: 'connection' | 'folder', ids: string[]) =>
    invoke<void>('reorder_explorer', { args: { parent_id: parentId, kind, ids } }),

  listDatabases: (connectionId: string) =>
    invoke<string[]>('list_databases', { args: { connection_id: connectionId, database: '' } }),
  listObjects: (connectionId: string, database: string) =>
    invoke<DbObject[]>('list_objects', { args: { connection_id: connectionId, database } }),
  /** The explorer's load of a database: its objects and, when the driver lists them, all its schemas. */
  listDatabaseObjects: (connectionId: string, database: string) =>
    invoke<DatabaseObjects>('list_database_objects', { args: { connection_id: connectionId, database } }),
  /** What the explorer showed last time (the explorer cache), or null.
   *  `kind`: databases | objects | schemas | columns; `item`: the object, for columns. */
  getCached: <T>(connectionId: string, database: string, kind: 'databases' | 'objects' | 'schemas' | 'columns', item = '') =>
    invoke<T | null>('get_cached', { args: { connection_id: connectionId, database, kind, item } }),
  /** What the login may do; `database` '': the server-level actions. */
  getPermissions: (connectionId: string, database: string) =>
    invoke<Permissions>('get_permissions', { args: { connection_id: connectionId, database } }),
  scanKeys: (connectionId: string, database: string, scan: KeyScan) =>
    invoke<KeyPage>('scan_keys', { args: { connection_id: connectionId, database, scan } }),
  getColumns: (connectionId: string, database: string, object: ObjectRef) =>
    invoke<ColumnInfo[]>('get_columns', { args: { connection_id: connectionId, database, object } }),
  /** A table's indexes and their usage; null: the driver doesn't report it. */
  getIndexUsage: (connectionId: string, database: string, object: ObjectRef) =>
    invoke<IndexUsageReport | null>('get_index_usage', { args: { connection_id: connectionId, database, object } }),
  /** What depends on a table, column, view or routine ("Ver dependencias…"). */
  getDependents: (connectionId: string, database: string, target: DependencyTarget) =>
    invoke<DependencyReport>('get_dependents', { args: { connection_id: connectionId, database, target } }),
  getDefinition: (connectionId: string, database: string, object: ObjectRef) =>
    invoke<string>('get_definition', { args: { connection_id: connectionId, database, object } }),
  browseQuery: (connectionId: string, database: string, object: ObjectRef, limit: number) =>
    invoke<string>('browse_query', { args: { connection_id: connectionId, database, object, limit } }),

  executeQuery: (a: {
    sessionId: string; connectionId: string; database: string; sql: string;
    maxRows?: number; queryId?: string | null; plan?: 'none' | 'estimated' | 'actual';
    /** Keep it in the history (runs from the editor). */
    record?: boolean;
    /** 'auto' from the editor: statement by statement when the driver
     *  allows, progress events, UPDATE/DELETE check, transaction state. */
    mode?: RunMode;
    /** Go on after a failed statement; omitted: the engine's default. */
    continueOnError?: boolean | null;
    /** The user confirmed the `needs_confirmation` statements. */
    confirmedUnsafe?: boolean;
    /** The tab's transactions: false = manual. Omitted: unchanged. */
    autocommit?: boolean | null;
  }) =>
    invoke<ExecuteResponse>('execute_query', {
      args: {
        session_id: a.sessionId, connection_id: a.connectionId, database: a.database,
        sql: a.sql, max_rows: a.maxRows ?? null, query_id: a.queryId ?? null, plan: a.plan ?? 'none',
        record: a.record ?? false, mode: a.mode ?? 'whole', continue_on_error: a.continueOnError ?? null,
        confirmed_unsafe: a.confirmedUnsafe ?? false, autocommit: a.autocommit ?? null,
      },
    }),
  /** The script cut as the engine's tool would. `statements`: statement by
   *  statement even inside T-SQL batches (run the statement at the cursor). */
  splitScript: (a: { connectionId?: string; driver?: string; sql: string; statements?: boolean }) =>
    invoke<ScriptUnit[]>('split_script', {
      args: { connection_id: a.connectionId ?? null, driver: a.driver ?? null, sql: a.sql, statements: a.statements ?? false },
    }),
  setTabAutocommit: (sessionId: string, connectionId: string, database: string, autocommit: boolean) =>
    invoke<TxState | null>('set_tab_autocommit', { args: { session_id: sessionId, connection_id: connectionId, database, autocommit } }),
  commitTab: (sessionId: string) => invoke<TxState | null>('commit_tab', { args: { session_id: sessionId } }),
  rollbackTab: (sessionId: string) => invoke<TxState | null>('rollback_tab', { args: { session_id: sessionId } }),
  tabTransactionState: (sessionId: string) => invoke<TxState | null>('tab_transaction_state', { args: { session_id: sessionId } }),
  cancelQuery: (sessionId: string) => invoke<void>('cancel_query', { args: { session_id: sessionId } }),
  closeSession: (sessionId: string) => invoke<void>('close_session', { args: { session_id: sessionId } }),

  listQueries: (connectionId: string, database: string) =>
    invoke<SavedQuery[]>('list_queries', { args: { connection_id: connectionId, database } }),
  getQuery: (id: string) => invoke<SavedQuery>('get_query', { args: { id } }),
  saveQuery: (query: SavedQuery) => invoke<SavedQuery>('save_query', { args: { query } }),
  deleteQuery: (id: string) => invoke<void>('delete_query', { args: { id } }),

  getLogDir: () => invoke<string | null>('get_log_dir'),

  monitorSnapshot: (connectionId: string) =>
    invoke<MonitorSnapshot>('monitor_snapshot', { args: { connection_id: connectionId } }),

  profilerStart: (profilerId: string, connectionId: string, database: string) =>
    invoke<ProfilerStarted>('profiler_start', { args: { profiler_id: profilerId, connection_id: connectionId, database } }),
  profilerPoll: (profilerId: string) => invoke<ProfiledStatement[]>('profiler_poll', { args: { profiler_id: profilerId } }),
  profilerStop: (profilerId: string) => invoke<void>('profiler_stop', { args: { profiler_id: profilerId } }),
  /** Look for a newer release (the updater manifest, else GitHub's latest
   *  release). The calling window becomes the update's owner. */
  checkForUpdate: (manual = false) => invoke<UpdateInfo>('check_for_update', { args: { manual } }),
  /** Download and verify the update found; progress comes as `update-progress`. */
  updateDownload: () => invoke<void>('update_download', { args: {} }),
  /** Stop a running download (the download answers `cancelled`). */
  updateCancel: () => invoke<void>('update_cancel'),
  /** Install the downloaded update and relaunch. Only through the quit guard
   *  (`requestUpdateRestart` in quitGuard.ts). */
  updateInstallAndRestart: () => invoke<void>('update_install_and_restart'),
  /** Open a release page of DBine in the browser (other URLs are refused). */
  openReleasePage: (url: string) => invoke<void>('open_release_page', { args: { url } }),
};

// Windows: several windows share one backend (src-tauri/src/windows.rs).
export const windowApi = {
  /** Opens a new, empty window; returns its label. */
  newWindow: () => invoke<string>('window_new'),
  /** Who the calling window is. */
  role: () => invoke<WindowRole>('window_role'),
  /** Closes the calling window (not the app), saving its geometry first. */
  close: () => invoke<void>('window_close'),
  /** This window's running tasks: the whole list, each time it changes. */
  reportTasks: (tasks: TaskSummary[]) => invoke<void>('tasks_report', { args: { tasks } }),
  /** Every window's running tasks (quitting asks once about all of them). */
  runningTasksAll: () => invoke<RunningTask[]>('tasks_running_all'),
  /** Asks every window to cancel its tasks ("tasks-cancel-all"). */
  cancelAllBroadcast: () => invoke<void>('tasks_cancel_all_broadcast'),
  /** True only for the first window to ask in this run: it does the
   *  once-per-run work (update check, app-start telemetry). */
  claimStartup: () => invoke<boolean>('app_claim_startup'),
};
