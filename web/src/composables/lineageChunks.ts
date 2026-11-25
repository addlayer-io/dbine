// A big lineage read in chunks (`get_lineage_chunk`): each call is its own short read, and as soon as
// one answers the next is asked, until the graph says nothing is `next`. The cache holds the graph so
// far after every chunk, so an opening that was left half way continues from there. A chunk that fails
// is asked once more; if it fails again it is given up (it stays in `failed`, with a way to read just it
// again) and the loop goes on. Closing the panel (`cancelled`) ends the loop between two chunks.
import type { CachedLineage, LineageGraph, LineageStep } from '../api/types';

export interface ChunkRequest {
  /** `null`: start over. */
  step: LineageStep | null;
  /** Read again a chunk that is in `failed`. */
  retry?: boolean;
  /** Don't read it: leave it in `failed` and say the next one. */
  giveUp?: boolean;
}
export interface ChunkDeps {
  cached: () => Promise<CachedLineage>;
  chunk: (req: ChunkRequest) => Promise<LineageGraph>;
}
export interface ChunkEvents {
  /** The cached graph (maybe half read), to draw while the rest is read. */
  cached: (graph: LineageGraph, updatedAt: string | null) => void;
  /** After every chunk: the graph so far. */
  progress: (graph: LineageGraph) => void;
  /** A stage ended (or the first chunk answered): the moment to lay the diagram out again. */
  stage: (graph: LineageGraph) => void;
  /** Nothing is next: the graph is as complete as it will get (`failed` may still list chunks). */
  done: (graph: LineageGraph) => void;
  /** The read can't go on (the start failed, or a chunk couldn't even be given up); `kept`: something is on screen. */
  failed: (error: unknown, kept: boolean) => void;
}
export interface ChunkOptions {
  /** Read the cache first (not for the "Actualizar" button). */
  useCache?: boolean;
  /** Whether a complete cached graph of that age (RFC 3339) is taken as it is, without reading it again. */
  reuseComplete?: (updatedAt: string | null) => boolean;
  cancelled?: () => boolean;
}

export const progressOf = (g: LineageGraph | null): LineageStep | null => g?.next ?? null;
/** A graph that isn't all there: chunks still to read or given up. */
export const isPartial = (g: LineageGraph | null): boolean => !!g && (!!g.next || (g.failed?.length ?? 0) > 0);
/** Objects the given-up chunks hold. */
export const failedObjects = (g: LineageGraph | null): number => (g?.failed ?? []).reduce((n, s) => n + Math.max(s.len, 1), 0);

export async function runChunks(deps: ChunkDeps, on: ChunkEvents, opts: ChunkOptions = {}): Promise<void> {
  const cancelled = opts.cancelled ?? (() => false);
  let g: LineageGraph | null = null;
  if (opts.useCache ?? true) {
    const c = await deps.cached().catch((): CachedLineage => ({ graph: null, updated_at: null }));
    if (cancelled()) return;
    if (c.graph) {
      g = c.graph;
      on.cached(g, c.updated_at);
      if (!g.next && opts.reuseComplete?.(c.updated_at)) return void on.done(g);
    }
  }
  // Resume where the cached graph stopped, or start.
  let step: LineageStep | null = g?.next ?? null;
  for (;;) {
    if (cancelled()) return;
    let r: LineageGraph;
    try {
      r = await attempt(deps, step);
    } catch (e) {
      if (cancelled()) return;
      if (!step) return void on.failed(e, !!g);
      try {
        r = await deps.chunk({ step, giveUp: true });
      } catch (e2) {
        return void on.failed(e2, !!g);
      }
    }
    if (cancelled()) return;
    const boundary = !step || r.next?.stage !== step.stage;
    g = r;
    on.progress(g);
    if (boundary || !r.next) on.stage(g);
    if (!r.next) return void on.done(g);
    step = r.next;
  }
}

/** The chunk, and once more if it fails. */
async function attempt(deps: ChunkDeps, step: LineageStep | null): Promise<LineageGraph> {
  try {
    return await deps.chunk({ step });
  } catch {
    return await deps.chunk({ step });
  }
}

/** Reads again just one chunk that was given up (the "Reintentar" of the banner). */
export function retryChunk(deps: ChunkDeps, step: LineageStep): Promise<LineageGraph> {
  return deps.chunk({ step, retry: true });
}
