import { invoke } from '@tauri-apps/api/core';
import type { HistoryEntry } from './history';

// A tab's timeline in the history sidebar (docs/history.md): a saved
// query's versions, the runs of the query or project file, and a project
// file's commits.

/** A saved query's text at one moment (local to this machine). */
export interface QueryVersion {
  id: number;
  query_id: string;
  saved_at: string;
  /** Lines added and removed against the version before it. */
  added: number;
  removed: number;
  /** `null` in a list; `version()` brings it. */
  sql: string | null;
}

/** A commit that touched a project file. */
export interface FileCommit {
  hash: string;
  short: string;
  author: string;
  /** ISO 8601. */
  date: string;
  subject: string;
  /** The file's path in that commit (it may have been renamed since). */
  path: string;
}

export interface FileAtCommit {
  /** `null`: not in that commit, binary or too large. */
  text: string | null;
  binary: boolean;
  too_large: boolean;
}

export const timelineApi = {
  versions: (queryId: string) => invoke<QueryVersion[]>('query_versions', { args: { query_id: queryId } }),
  version: (id: number) => invoke<QueryVersion>('query_version', { args: { id } }),
  /** Keep the saved query's current text as a version; `null` when it already is. */
  checkpoint: (queryId: string) => invoke<QueryVersion | null>('query_version_checkpoint', { args: { query_id: queryId } }),
  runsOfQuery: (queryId: string, limit = 200) =>
    invoke<HistoryEntry[]>('history_of', { args: { query_id: queryId, project_id: null, file_path: null, limit } }),
  runsOfFile: (projectId: string, path: string, limit = 200) =>
    invoke<HistoryEntry[]>('history_of', { args: { query_id: null, project_id: projectId, file_path: path, limit } }),
  fileLog: (projectId: string, path: string, limit = 100) =>
    invoke<FileCommit[]>('project_file_log', { args: { id: projectId, path, limit } }),
  fileAt: (projectId: string, commit: string, path: string) =>
    invoke<FileAtCommit>('project_file_at', { args: { id: projectId, commit, path } }),
};
