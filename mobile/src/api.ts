import { invoke } from '@tauri-apps/api/core';

// The app's Rust core (src-tauri/src/lib.rs) and, through it, DBine on the
// computer (crates/dbine-link/src/proto.rs).

export interface Status {
  paired: boolean;
  desktop: string | null;
  fingerprint: string | null;
}

export interface Hello {
  desktop: string;
  version: string;
  can_kill: boolean;
}

export interface ConnectionItem {
  id: string;
  name: string;
  engine: string;
  color: string | null;
  locks: boolean;
  processes: boolean;
  kill: boolean;
}

export interface LockRow {
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

export interface ProcessRow {
  id: string;
  status: string | null;
  user: string | null;
  host: string | null;
  database: string | null;
  elapsed_ms: number | null;
  blocked_by: string | null;
  sql: string | null;
}

type Call =
  | { call: 'hello' }
  | { call: 'connections' }
  | { call: 'databases'; connection_id: string }
  | { call: 'locks'; connection_id: string; database: string }
  | { call: 'processes'; connection_id: string; database: string }
  | { call: 'kill'; connection_id: string; database: string; session_id: string };

const ask = <T>(call: Call) => invoke<T>('call', { call });

export const api = {
  status: () => invoke<Status>('status'),
  pair: (code: string, name: string) => invoke<string>('pair', { code, name }),
  unpair: () => invoke<void>('unpair'),
  hello: () => ask<Hello>({ call: 'hello' }),
  connections: () => ask<ConnectionItem[]>({ call: 'connections' }),
  locks: (connection_id: string) => ask<LockRow[]>({ call: 'locks', connection_id, database: '' }),
  processes: (connection_id: string) => ask<ProcessRow[]>({ call: 'processes', connection_id, database: '' }),
  kill: (connection_id: string, session_id: string) => ask<boolean>({ call: 'kill', connection_id, database: '', session_id }),
};

export function errorText(e: unknown): string {
  return typeof e === 'string' ? e : e instanceof Error ? e.message : JSON.stringify(e);
}

/** "40 s", "7 min", "1 h 05 min". */
export function duration(ms: number | null | undefined): string {
  const s = Math.max(0, Math.floor((ms ?? 0) / 1000));
  if (s < 60) return `${s} s`;
  if (s < 3600) return `${Math.floor(s / 60)} min`;
  return `${Math.floor(s / 3600)} h ${String(Math.floor((s % 3600) / 60)).padStart(2, '0')} min`;
}
