import { shallowReactive, toRaw } from 'vue';
import { functionsApi } from '../api/functions';
import type { FunctionInfo } from '../api/types';
import { useConnectionsStore } from '../stores/connections';

// The function catalog of each connection and database, for the editors'
// completion (composables/functionCompletion.ts). An editor asks for it on
// its first focus or completion (`needFunctions`), and only once the
// connection is open: asking never connects. It's kept for the connection
// as it is now: when the connection closes, reconnects or is edited, its
// `live` entry in the store is replaced or removed and the catalog is read
// again (the backend dropped its copy too). Another database is another
// catalog (the user's functions differ). A read that failed is tried again
// after RETRY_MS; one the server took too long for comes back empty from the
// backend, and stays so until the connection reconnects.

export interface CatalogEntry {
  /** The store's `live` entry it was read for. */
  live: object | undefined;
  list: FunctionInfo[] | null;
  loading: boolean;
  /** When the last read failed (ms): not asked again for a while. */
  failedAt: number;
}

/** A failed read is tried again after this long. */
export const RETRY_MS = 30_000;

/** The catalog an entry holds for the connection as it is now (`live`). */
export function catalogOf(e: CatalogEntry | undefined, live: object | undefined): FunctionInfo[] | null {
  return e?.list && e.live === live ? e.list : null;
}

/** Whether to read it: nothing read yet for the connection as it is now,
 *  or its last read failed at least RETRY_MS ago. */
export function shouldLoad(e: CatalogEntry | undefined, live: object | undefined, now: number): boolean {
  return !(e && e.live === live && (e.list || e.loading || now - e.failedAt < RETRY_MS));
}

/** What a read's answer (`list`, null when it failed) leaves under its key,
 *  where `current` is now; null when the connection moved on meanwhile (its
 *  next read is its own). */
export function settled(current: CatalogEntry | undefined, live: object | undefined, list: FunctionInfo[] | null, now: number): CatalogEntry | null {
  return current?.live === live ? { live, list, loading: false, failedAt: list ? 0 : now } : null;
}

const entries = shallowReactive(new Map<string, CatalogEntry>());
const keyOf = (connectionId: string, database: string) => `${connectionId}\u0000${database}`;

/** The live entry of a connection, as the entries compare it. */
function liveOf(connectionId: string): object | undefined {
  const live = useConnectionsStore().live[connectionId];
  return live ? toRaw(live) : undefined;
}

/** The catalog of the connection and database, if read for the connection
 *  as it is now; null otherwise. Reactive: hosts pass it to CodeEditor's
 *  `functions`. */
export function functionCatalog(connectionId: string | null | undefined, database: string): FunctionInfo[] | null {
  if (!connectionId) return null;
  return catalogOf(entries.get(keyOf(connectionId, database)), liveOf(connectionId));
}

/** Read it (CodeEditor's `needFunctions`): once per connection as it is now,
 *  and only when the connection is open. An engine without a catalog reads
 *  an empty one. */
export function loadFunctionCatalog(connectionId: string | null | undefined, database: string): void {
  if (!connectionId || !useConnectionsStore().isConnected(connectionId)) return;
  const live = liveOf(connectionId);
  const key = keyOf(connectionId, database);
  if (!shouldLoad(entries.get(key), live, Date.now())) return;
  entries.set(key, { live, list: null, loading: true, failedAt: 0 });
  const settle = (list: FunctionInfo[] | null) => {
    const next = settled(entries.get(key), live, list, Date.now());
    if (next) entries.set(key, next);
  };
  functionsApi.catalog(connectionId, database).then(settle, () => settle(null));
}
