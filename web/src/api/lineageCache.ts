import { invoke } from '@tauri-apps/api/core';
import type { LineageGraph, LineageScope } from './types';

// Lineage cache commands (the Rust side: explorer cache, docs/architecture/background-cache).
// To be merged into client.ts / types.ts.

export interface CachedLineage {
  graph: LineageGraph | null;
  /** ISO date the graph was saved. */
  updated_at: string | null;
}

export const lineageCacheApi = {
  /** The saved graph of a scope, instantly (no server call). */
  lineageGraphCached: (connectionId: string, database: string, scope: LineageScope = {}) =>
    invoke<CachedLineage>('lineage_graph_cached', { args: { connection_id: connectionId, database, scope } }),
  /** Forget the saved lineage of one connection, or of all (null). */
  lineageCacheClear: (connectionId: string | null = null) =>
    invoke<null>('lineage_cache_clear', { args: { connection_id: connectionId } }),
};

/** App setting: precompute the lineage after connecting (default on; Rust reads the same key). */
export const LINEAGE_PRECOMPUTE_SETTING = 'lineage_precompute';
