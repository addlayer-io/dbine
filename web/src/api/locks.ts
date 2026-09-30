import { invoke } from '@tauri-apps/api/core';

// Blocking chains and ending sessions (docs/bloqueos.md).

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

export const locksApi = {
  blocking: (connectionId: string) => invoke<BlockedSession[]>('monitor_blocking', { args: { connection_id: connectionId } }),
  kill: (connectionId: string, id: string) => invoke<void>('monitor_kill_session', { args: { connection_id: connectionId, id } }),
};
