import { invoke } from '@tauri-apps/api/core';

// Data files opened from the OS: Finder / Explorer ("Open with DBine"), a
// drop on the window, File › Open… (src-tauri/src/open_files.rs).

export interface OpenedFile {
  connection_id: string;
  kind: 'sqlite' | 'duckdb' | 'data';
  /** The file's name. */
  name: string;
  /** It already had a connection (temporary or saved): focus it. */
  existing: boolean;
  /** Data files: `SELECT * FROM "<view>" LIMIT 1000`. */
  sql: string | null;
}

/** The extensions File › Open… offers (what the backend opens). */
export const OPENABLE_EXTENSIONS = [
  'sqlite', 'sqlite3', 'db', 'db3', 'duckdb', 'ddb', 'csv', 'tsv', 'parquet', 'json', 'jsonl', 'ndjson', 'xlsx', 'gz', 'zst',
];

export const openFilesApi = {
  /** Paths the OS handed over that no window has opened yet (drains them). */
  take: () => invoke<string[]>('open_files_take'),
  /** The file's connection: the one it has, or a new temporary one. */
  open: (path: string) => invoke<OpenedFile>('open_file', { args: { path } }),
};
