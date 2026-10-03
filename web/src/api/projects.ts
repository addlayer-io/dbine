import { invoke } from '@tauri-apps/api/core';
import type {
  FileContent, FileDiff, FileStat, FolderInspect, FsEntry, ProjectBinding, ProjectInfo, ProjectManifest, ProjectPullOut,
  ProjectStatus, ProjectSyncOut, RunningTask, TaskSummary, WriteOut,
} from './types';

// Proyectos: folders (git repos) of scripts linked on this machine
// (src-tauri/src/commands/projects.rs and projects_git.rs, docs/proyectos.md).
// Every file operation goes through the backend, which keeps paths inside
// the project's folder. Paths are relative to the repo root, with '/'.

export const projectsApi = {
  // -- registry and manifest ------------------------------------------------------
  list: () => invoke<ProjectInfo[]>('list_projects'),
  defaultDir: () => invoke<string>('project_default_dir'),
  inspectFolder: (path: string) => invoke<FolderInspect>('project_inspect_folder', { args: { path } }),
  link: (a: { path: string; name?: string | null; init?: boolean; binding?: ProjectBinding | null }) =>
    invoke<ProjectInfo>('project_link', { args: { path: a.path, name: a.name ?? null, init: a.init ?? false, binding: a.binding ?? null } }),
  clone: (a: { url: string; parentDir: string; name?: string | null; branch?: string | null; opId: string; binding?: ProjectBinding | null }) =>
    invoke<ProjectInfo>('project_clone', {
      args: { url: a.url, parent_dir: a.parentDir, name: a.name ?? null, branch: a.branch ?? null, op_id: a.opId, binding: a.binding ?? null },
    }),
  rename: (id: string, name: string) => invoke<ProjectInfo>('project_update', { args: { id, name } }),
  relocate: (id: string, path: string) => invoke<ProjectInfo>('project_relocate', { args: { id, path } }),
  unlink: (id: string) => invoke<void>('project_unlink', { args: { id } }),
  setBinding: (id: string, binding: ProjectBinding) => invoke<ProjectInfo>('project_set_binding', { args: { id, binding } }),
  reorder: (ids: string[]) => invoke<void>('project_reorder', { args: { ids } }),
  writeManifest: (id: string, manifest: ProjectManifest) => invoke<ProjectInfo>('project_write_manifest', { args: { id, manifest } }),
  reveal: (id: string, path: string | null = null) => invoke<void>('project_reveal', { args: { id, path } }),
  setRemote: (id: string, url: string) => invoke<void>('project_set_remote', { args: { id, url } }),
  setIdentity: (id: string, name: string, email: string, global: boolean) =>
    invoke<void>('project_set_identity', { args: { id, name, email, global } }),

  // -- files ------------------------------------------------------------------------
  listDir: (id: string, dir: string) => invoke<FsEntry[]>('project_list_dir', { args: { id, dir } }),
  readFile: (id: string, path: string) => invoke<FileContent>('project_read_file', { args: { id, path } }),
  /** `expectedHash`: the hash read last; a different file on disk comes back as `conflict` (nothing written). */
  writeFile: (a: { id: string; path: string; text: string; eol: 'lf' | 'crlf'; bom: boolean; expectedHash: string | null }) =>
    invoke<WriteOut>('project_write_file', { args: { id: a.id, path: a.path, text: a.text, eol: a.eol, bom: a.bom, expected_hash: a.expectedHash } }),
  statFiles: (id: string, paths: string[], known: FileStat[] = []) =>
    invoke<FileStat[]>('project_stat_files', { args: { id, paths, known } }),
  createFile: (id: string, path: string, text: string | null = null) => invoke<FileStat>('project_create_file', { args: { id, path, text } }),
  createDir: (id: string, path: string) => invoke<void>('project_create_dir', { args: { id, path } }),
  renamePath: (id: string, from: string, to: string) => invoke<void>('project_rename', { args: { id, from, to } }),
  deletePath: (id: string, path: string) => invoke<void>('project_delete', { args: { id, path } }),

  // -- git ----------------------------------------------------------------------------
  status: (id: string, fetch = false, opId: string | null = null) =>
    invoke<ProjectStatus>('project_status', { args: { id, fetch, op_id: opId } }),
  diff: (id: string, path: string) => invoke<FileDiff>('project_diff', { args: { id, path } }),
  /** Stages everything and commits; returns the short oid. */
  commit: (id: string, message: string) => invoke<string>('project_commit', { args: { id, message } }),
  pull: (id: string, opId: string) => invoke<ProjectPullOut>('project_pull', { args: { id, op_id: opId } }),
  /** True when it set the upstream. */
  push: (id: string, opId: string) => invoke<boolean>('project_push', { args: { id, op_id: opId } }),
  sync: (id: string, opId: string) => invoke<ProjectSyncOut>('project_sync', { args: { id, op_id: opId } }),
  conflict: (id: string, path: string, action: 'ours' | 'theirs' | 'resolved') =>
    invoke<void>('project_conflict', { args: { id, path, action } }),
  operation: (id: string, action: 'continue' | 'abort') => invoke<ProjectPullOut>('project_operation', { args: { id, action } }),
  discard: (id: string, paths: string[]) => invoke<void>('project_discard', { args: { id, paths } }),
  cancel: (opId: string) => invoke<boolean>('project_git_cancel', { args: { op_id: opId } }),
};

// Unsaved file tabs, shared across windows so quitting asks about all of
// them (src-tauri/src/windows.rs, like the running tasks).
export const unsavedFilesApi = {
  /** This window's unsaved file tabs: the whole list, each time it changes. */
  report: (files: TaskSummary[]) => invoke<void>('files_report', { args: { files } }),
  /** Every window's unsaved file tabs. */
  all: () => invoke<RunningTask[]>('files_unsaved_all'),
  /** Asks every window to save its file tabs ("files-save-all"). */
  saveAllBroadcast: () => invoke<void>('files_save_all_broadcast'),
};
