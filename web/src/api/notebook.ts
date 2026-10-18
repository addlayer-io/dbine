import { invoke } from '@tauri-apps/api/core';
import type {
  ColumnPair, FileDecision, FileGrant, LocalSource, Notebook, NotebookRead, RunLocalResponse, WriteMode, WriteOutcome, WritePrepareResponse, WriteTarget,
  WriteTargetInfo,
} from './types';

// Notebooks (docs/notebooks.md): reading and writing Jupyter .ipynb files.
// Mirrors src-tauri/src/commands/notebook.rs (args in camelCase). Project
// files go through projectsApi.readFile / writeFile with the text from
// `serialize` / to `parse`; standalone files use `read` / `write`.

/** How much of each cell's result the file keeps (Configuración › Notebooks). */
export interface PreviewLimits {
  /** Rows per cell (1–1000). */
  rows: number;
  /** Bytes per cell (64 KiB–8 MiB). */
  bytes: number;
}

/** A write cell's request (src-tauri notebook::write::WriteArgs). */
export interface WriteArgs {
  tabId: string;
  cellId: string;
  /** The DataFrame: a cell above, by name. */
  source: string;
  sources: LocalSource[];
  rowCap: number;
  notebookPath: string | null;
  approval?: string | null;
  /** The AWS profile for s3:// reads (Settings › Notebooks). */
  awsProfile?: string | null;
  target: WriteTarget;
  mode: WriteMode;
  keys: string[];
  mapping: ColumnPair[] | null;
}

/** A chart of a cell, rendered for the export. */
export interface ExportImage {
  cellId: string;
  title: string;
  /** PNG, base64. */
  png: string;
}

export const notebookApi = {
  /** A notebook from .ipynb text (DBine, Azure Data Studio, Jupyter…). */
  parse: (text: string) => invoke<Notebook>('notebook_parse', { args: { text } }),
  /** The .ipynb text (nbformat 4.5), with the previews cut to the limits. */
  serialize: (notebook: Notebook, limits: PreviewLimits) =>
    invoke<string>('notebook_serialize', { args: { notebook, rowsPerCell: limits.rows, bytesPerCell: limits.bytes } }),
  /** A standalone file (outside projects). */
  read: (path: string) => invoke<NotebookRead>('notebook_read', { args: { path } }),
  write: (path: string, notebook: Notebook, limits: PreviewLimits) =>
    invoke<void>('notebook_write', { args: { path, notebook, rowsPerCell: limits.rows, bytesPerCell: limits.bytes } }),
  /** A self-contained HTML page or Markdown, with the previews the file
   *  would keep and the cells' chart `images` (embedded in HTML). With
   *  `path`, Markdown with images is written by the command (the .md and its
   *  `_files` folder next to it). */
  export: (notebook: Notebook, format: 'html' | 'markdown', limits: PreviewLimits, images: ExportImage[] = [], path?: string) =>
    invoke<string>('notebook_export', {
      args: { notebook, format, rowsPerCell: limits.rows, bytesPerCell: limits.bytes, images, path: path ?? null },
    }),
  /** A Python (Jupyter) notebook that reproduces this one: `targets` is each
   *  SQL cell's resolved connection (by cell id); DBine reads only their
   *  non-secret fields, and the code reads passwords from environment
   *  variables. `previews`: each cell's stored preview as its output. */
  exportPython: (notebook: Notebook, targets: Record<string, { connectionId: string; database: string }>, previews: boolean, limits: PreviewLimits) =>
    invoke<string>('notebook_export', {
      args: { notebook, format: 'python', rowsPerCell: limits.rows, bytesPerCell: limits.bytes, targets, previews },
    }),
  /** "DataFrame SQL": the cells above that `sql` needs (their names,
   *  in loading order, through the "results" cells it names). */
  localRefs: (sql: string, sources: LocalSource[]) => invoke<string[]>('notebook_local_refs', { args: { sql, sources } }),
  /** Run a "DataFrame SQL" cell on the tab's DuckDB session
   *  (`nbx:<tabId>`, closed with the tab; cancel_query stops it). */
  /** `notebookPath`: the notebook's file (relative paths in `read_csv('…')` are read from its folder). */
  /** `approval`: the token of the file approval just answered (the run may read those files). */
  runLocal: (a: { tabId: string; sql: string; maxRows: number; rowCap: number; sources: LocalSource[]; notebookPath: string | null; approval?: string | null; awsProfile?: string | null }) =>
    invoke<RunLocalResponse>('notebook_run_local', { args: a }),
  /** The answer to a run's `approval` request. */
  /** A write cell's target: the modes its engine writes and, for an existing table, its columns. */
  writeTarget: (tabId: string, target: WriteTarget) => invoke<WriteTargetInfo>('notebook_write_target', { args: { tabId, target } }),
  /** The DataFrame loaded, the target checked, a preview and the confirmation's token. Writes nothing. */
  writePrepare: (a: WriteArgs) => invoke<WritePrepareResponse>('notebook_write_prepare', { args: a }),
  /** The confirmed write: only with the plan's token and the row count it showed. */
  writeRun: (a: WriteArgs, token: string, rows: number) => invoke<WriteOutcome>('notebook_write_run', { args: { ...a, approval: null, token, rows } }),
  decideFiles: (token: string, decision: FileDecision) => invoke<void>('notebook_file_decision', { args: { token, decision } }),
  /** The files notebooks may read: one notebook's (its file), or every one. */
  fileGrants: (notebook: string | null = null) => invoke<FileGrant[]>('notebook_file_grants', { args: { notebook } }),
  removeFileGrant: (notebook: string, target: string) => invoke<boolean>('notebook_file_grant_remove', { args: { notebook, target } }),
  clearFileGrants: (notebook: string | null = null) => invoke<number>('notebook_file_grants_clear', { args: { notebook } }),
};
