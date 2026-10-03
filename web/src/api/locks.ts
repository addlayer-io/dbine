import { invoke } from '@tauri-apps/api/core';

// Blocking chains, the process list and ending sessions or their
// statements (docs/bloqueos.md, docs/procesos.md).

/** A session in a blocking chain: waiting on `blocked_by`, or holding
 *  what others wait for (`blocked_by` null). */
export interface BlockedSession {
  id: string;
  blocked_by: string | null;
  user: string | null;
  client: string | null;
  database: string | null;
  wait: string | null;
  waited_ms: number | null;
  object: string | null;
  sql: string | null;
}

/** A server session or running request (the Monitor's "Procesos"). */
export interface ServerProcess {
  id: string;
  status: string | null;
  active: boolean;
  system: boolean;
  own: boolean;
  user: string | null;
  host: string | null;
  program: string | null;
  database: string | null;
  command: string | null;
  elapsed_ms: number | null;
  cpu_ms: number | null;
  reads: number | null;
  writes: number | null;
  wait: string | null;
  blocked_by: string | null;
  sql: string | null;
}

export const locksApi = {
  blocking: (connectionId: string) => invoke<BlockedSession[]>('monitor_blocking', { args: { connection_id: connectionId } }),
  kill: (connectionId: string, id: string) => invoke<void>('monitor_kill_session', { args: { connection_id: connectionId, id } }),
  processes: (connectionId: string) => invoke<ServerProcess[]>('monitor_processes', { args: { connection_id: connectionId } }),
  cancel: (connectionId: string, id: string) => invoke<void>('monitor_cancel_query', { args: { connection_id: connectionId, id } }),
};
