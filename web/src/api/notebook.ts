import { invoke } from '@tauri-apps/api/core';
import type { LocalSource, Notebook, NotebookRead, RunLocalResponse } from './types';

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
  /** "Resultados del notebook": the cells above that `sql` needs (their names,
   *  in loading order, through the "results" cells it names). */
  localRefs: (sql: string, sources: LocalSource[]) => invoke<string[]>('notebook_local_refs', { args: { sql, sources } }),
  /** Run a "Resultados del notebook" cell on the tab's DuckDB session
   *  (`nbx:<tabId>`, closed with the tab; cancel_query stops it). */
  runLocal: (a: { tabId: string; sql: string; maxRows: number; rowCap: number; sources: LocalSource[] }) =>
    invoke<RunLocalResponse>('notebook_run_local', { args: a }),
};
