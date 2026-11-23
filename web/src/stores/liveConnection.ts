import type { LiveConnection } from './connections';

/** What a connection shows while it connects (again). A connection that was
 *  connected keeps its databases: the tree's databases are the children the
 *  connection's node holds, and without them the node falls back to its
 *  "Conectando…" placeholder, hiding the databases and tables already loaded
 *  (a re-check after a failed load, "Conectar", a reconnect). One that wasn't
 *  starts empty: the explorer cache primes it, and a failed one shows only
 *  its banner. */
export function connectingFrom(prev: LiveConnection | undefined): LiveConnection {
  const was = prev?.status === 'connected' ? prev : undefined;
  return {
    status: 'connecting',
    serverVersion: was?.serverVersion ?? '',
    databases: was ? [...was.databases] : [],
    defaultDatabase: prev?.defaultDatabase ?? '',
    error: null,
  };
}
