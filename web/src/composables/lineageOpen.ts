// How a lineage view opens when the explorer cache may hold its graph: the cached graph is
// drawn at once, the server is always asked in the same opening, and its answer replaces the
// cache's. A failed refresh keeps what is on screen. With no cache it is the plain load.
import type { CachedLineage } from '../api/types';
import type { LineageGraph, LineageScope } from '../api/types';
import type { ChunkRequest } from './lineageChunks';

export interface OpenDeps {
  /** The cache's answer (instant); a failure counts as "nothing cached". */
  cached: () => Promise<CachedLineage>;
  /** The server read. */
  fetch: () => Promise<LineageGraph>;
}
export interface OpenEvents {
  /** The cached graph, to draw now while the server answers. */
  cached: (graph: LineageGraph, updatedAt: string | null) => void;
  /** The server's graph (replaces whatever is shown). */
  fresh: (graph: LineageGraph) => void;
  /** The server read failed; `kept` says whether a graph is still on screen. */
  failed: (error: unknown, kept: boolean) => void;
}

/** `useCache` false (the "Actualizar" button) skips the cache; `shown` says a graph is already drawn. */
export async function openLineage(deps: OpenDeps, on: OpenEvents, opts: { useCache?: boolean; shown?: boolean } = {}): Promise<void> {
  let answered = false;
  const server = deps.fetch().then(
    (graph) => { answered = true; return { graph } as const; },
    (error: unknown) => { answered = true; return { error } as const; },
  );
  let kept = opts.shown ?? false;
  if (opts.useCache ?? true) {
    const c = await deps.cached().catch((): CachedLineage => ({ graph: null, updated_at: null }));
    // A server answer that came first is never replaced by an older cached one.
    if (c.graph && !answered) {
      on.cached(c.graph, c.updated_at);
      kept = true;
    }
  }
  const r = await server;
  if ('graph' in r) on.fresh(r.graph);
  else on.failed(r.error, kept);
}

/** The api client's calls a database's lineage panel reads through. */
export interface LineageCalls {
  lineageGraphCached: (connectionId: string, database: string, scope: LineageScope) => Promise<CachedLineage>;
  getLineageGraph: (connectionId: string, database: string, scope: LineageScope) => Promise<LineageGraph>;
  getLineageChunk: (connectionId: string, database: string, scope: LineageScope, req: ChunkRequest) => Promise<LineageGraph>;
}

/** The scope a database's lineage panel asks for: the one it was opened with, plus the server's jobs
 *  (the backend leaves them out where the engine has none). The precompute after connecting reads
 *  `panelScope({})` (Rust `panel_scope`). */
export const panelScope = (scope: LineageScope): LineageScope => ({ ...scope, jobs: true });

/** The panel's calls, all with one scope. The cache keeps a graph under the scope it was read with:
 *  a lookup with another one never finds it, and every opening read the whole lineage again. */
export function panelCalls(api: LineageCalls, connectionId: string, database: string, scope: LineageScope) {
  const s = panelScope(scope);
  return {
    cached: () => api.lineageGraphCached(connectionId, database, s),
    fetch: () => api.getLineageGraph(connectionId, database, s),
    chunk: (req: ChunkRequest) => api.getLineageChunk(connectionId, database, s, req),
  };
}

/** App setting: precompute the lineage after connecting (default on; Rust reads the same key). */
export const LINEAGE_PRECOMPUTE_SETTING = 'lineage_precompute';
