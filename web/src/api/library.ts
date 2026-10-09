import { invoke } from '@tauri-apps/api/core';

// The script Library (src-tauri/src/commands/library.rs, docs/library.md).

export interface LibraryScript {
  id: string;
  name: string;
  /** "Mantenimiento/Índices"; '' = root. */
  folder: string;
  description: string;
  /** Driver ids; `*sql` = any SQL engine. */
  engines: string[];
  text: string;
  updated_at: string;
}

export const ANY_SQL = '*sql';

export const libraryApi = {
  list: () => invoke<LibraryScript[]>('list_library'),
  save: (script: LibraryScript) => invoke<LibraryScript>('save_library_script', { args: { script } }),
  remove: (id: string) => invoke<void>('delete_library_script', { args: { id } }),
  importFiles: (paths: string[], engines: string[], folder: string) =>
    invoke<{ imported: number; skipped: string[] }>('import_library_files', { args: { paths, engines, folder } }),
  exportTo: (dir: string, ids: string[] = []) => invoke<number>('export_library', { args: { dir, ids } }),
};

// -- the Library in a git repo (src-tauri/src/commands/library_git.rs) ------------------

export interface GitChange {
  state: 'added' | 'modified' | 'deleted' | 'renamed' | 'conflict';
  path: string;
}

export interface LibraryGitStatus {
  /** `git --version`; null when git isn't installed. */
  git: string | null;
  remote: string | null;
  branch: string | null;
  dir: string | null;
  changes: GitChange[];
  ahead: number;
  behind: number;
  last_commit: string | null;
  fetch_error: string | null;
}

export interface GitApplied {
  added: number;
  updated: number;
  deleted: number;
}

export interface GitPullOut {
  applied: GitApplied;
  /** Files changed on both sides; empty when it went through. */
  conflicts: string[];
}

export const libraryGitApi = {
  status: (fetch = false) => invoke<LibraryGitStatus>('library_git_status', { args: { fetch } }),
  link: (remote: string, branch: string) =>
    invoke<{ applied: GitApplied; pushed: boolean }>('library_git_link', { args: { remote, branch } }),
  unlink: () => invoke<void>('library_git_unlink'),
  commit: (message: string) => invoke<void>('library_git_commit', { args: { message } }),
  pull: () => invoke<GitPullOut>('library_git_pull'),
  push: () => invoke<void>('library_git_push'),
  sync: (message: string) => invoke<GitPullOut>('library_git_sync', { args: { message } }),
  resolve: (keep: 'remote' | 'local') => invoke<GitApplied>('library_git_resolve', { args: { keep } }),
};
