import { invoke } from '@tauri-apps/api/core';

// The query history (docs/historial.md): what was run from the editor, on
// this machine only.

export interface HistoryEntry {
  id: number;
  connection_id: string;
  /** The connection's name when it ran. */
  connection_name: string;
  driver: string;
  /** The server it ran on (a file's name for embedded engines). */
  host: string;
  database: string;
  sql: string;
  started_at: string;
  duration_ms: number;
  rows: number | null;
  error: string | null;
  /** The saved query it ran from (its tab's timeline). */
  query_id: string | null;
  /** The project file it ran from. */
  project_id: string | null;
  file_path: string | null;
}

export const historyApi = {
  list: (search: string | null, before: number | null = null, limit = 200) =>
    invoke<HistoryEntry[]>('history_list', { args: { search, before, limit } }),
  /** `null`: the whole history. */
  remove: (ids: number[] | null) => invoke<void>('history_delete', { args: { ids } }),
};
