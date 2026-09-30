import { invoke } from '@tauri-apps/api/core';
import { tb } from '../i18n/backend';
import type {
  ColumnInfo, ConnectResult, ConnectionConfig, ConnectionFolder, DbObject, DriverInfo, KeyPage, KeyScan, MonitorSnapshot, ObjectRef, Permissions, ProfiledStatement, ProfilerStarted,
  QueryOutcome, SavedConnection, SavedQuery, TestResult,
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

  listDatabases: (connectionId: string) =>
    invoke<string[]>('list_databases', { args: { connection_id: connectionId, database: '' } }),
  listObjects: (connectionId: string, database: string) =>
    invoke<DbObject[]>('list_objects', { args: { connection_id: connectionId, database } }),
  /** What the explorer showed last time (the explorer cache), or null.
   *  `kind`: databases | objects | columns; `item`: the object, for columns. */
  getCached: <T>(connectionId: string, database: string, kind: 'databases' | 'objects' | 'columns', item = '') =>
    invoke<T | null>('get_cached', { args: { connection_id: connectionId, database, kind, item } }),
  /** What the login may do; `database` '': the server-level actions. */
  getPermissions: (connectionId: string, database: string) =>
    invoke<Permissions>('get_permissions', { args: { connection_id: connectionId, database } }),
  scanKeys: (connectionId: string, database: string, scan: KeyScan) =>
    invoke<KeyPage>('scan_keys', { args: { connection_id: connectionId, database, scan } }),
  getColumns: (connectionId: string, database: string, object: ObjectRef) =>
    invoke<ColumnInfo[]>('get_columns', { args: { connection_id: connectionId, database, object } }),
  getDefinition: (connectionId: string, database: string, object: ObjectRef) =>
    invoke<string>('get_definition', { args: { connection_id: connectionId, database, object } }),
  browseQuery: (connectionId: string, database: string, object: ObjectRef, limit: number) =>
    invoke<string>('browse_query', { args: { connection_id: connectionId, database, object, limit } }),

  executeQuery: (a: {
    sessionId: string; connectionId: string; database: string; sql: string;
    maxRows?: number; queryId?: string | null; plan?: 'none' | 'estimated' | 'actual';
    /** Keep it in the history (runs from the editor). */
    record?: boolean;
  }) =>
    invoke<QueryOutcome>('execute_query', {
      args: {
        session_id: a.sessionId, connection_id: a.connectionId, database: a.database,
        sql: a.sql, max_rows: a.maxRows ?? null, query_id: a.queryId ?? null, plan: a.plan ?? 'none',
        record: a.record ?? false,
      },
    }),
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
};
