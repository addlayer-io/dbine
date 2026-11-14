import { invoke } from '@tauri-apps/api/core';
import type { FunctionInfo } from './types';

// The functions the query editor completes (issue #97), read once per
// connection and database (src-tauri/src/commands/functions.rs). An engine
// without a catalog answers an empty list.

export const functionsApi = {
  /** `database` '' : the connection's default. */
  catalog: (connectionId: string, database: string) =>
    invoke<FunctionInfo[]>('function_catalog', { args: { connection_id: connectionId, database } }),
  /** "DataFrame SQL" notebook cells: DuckDB's functions on the app's local
   *  DuckDB, read once per app run. Empty while DuckDB isn't downloaded
   *  (asking never downloads it): ask again later. */
  localCatalog: () => invoke<FunctionInfo[]>('local_function_catalog'),
};
