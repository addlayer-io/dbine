import { invoke } from '@tauri-apps/api/core';
import type { Notebook, NotebookRead } from './types';

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
  /** A self-contained HTML page or Markdown, with the stored previews. */
  export: (notebook: Notebook, format: 'html' | 'markdown') =>
    invoke<string>('notebook_export', { args: { notebook, format } }),
};
