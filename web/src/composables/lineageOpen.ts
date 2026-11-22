// How a lineage view opens when the explorer cache may hold its graph: the cached graph is
// drawn at once, the server is always asked in the same opening, and its answer replaces the
// cache's. A failed refresh keeps what is on screen. With no cache it is the plain load.
import type { CachedLineage } from '../api/types';
import type { LineageGraph } from '../api/types';

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

/** App setting: precompute the lineage after connecting (default on; Rust reads the same key). */
export const LINEAGE_PRECOMPUTE_SETTING = 'lineage_precompute';
