import { invoke } from '@tauri-apps/api/core';
import i18next from 'i18next';
import { t } from '../i18n';

// "Documentar la base…" (docs/database-docs.md). Mirrors
// src-tauri/src/commands/dbdocs.rs and dbdocs::DocOptions.

export type DocFormat = 'html' | 'markdown';

export interface DocOptions {
  format: DocFormat;
  /** Empty: every schema. */
  schemas: string[];
  tables: boolean;
  views: boolean;
  routines: boolean;
  triggers: boolean;
  others: boolean;
  source: boolean;
  indexes: boolean;
  foreign_keys: boolean;
  dependencies: boolean;
  diagram: boolean;
  /** The document's texts (`dbDocs:doc.*`, flattened) in the user's language. */
  labels: Record<string, string>;
}

export interface DocsOutline {
  schemas: string[];
  /** Objects per kind. */
  kinds: Record<string, number>;
  foreign_keys: boolean;
  dependencies: boolean;
}

export interface Generated {
  path: string;
  tables: number;
  objects: number;
  bytes: number;
  notes: string[];
}

export interface DocsProgress {
  run_id: string;
  done: number;
  total: number;
  phase: string;
}

export const dbDocsApi = {
  outline: (connectionId: string, database: string) =>
    invoke<DocsOutline>('dbdocs_outline', { args: { connection_id: connectionId, database } }),
  generate: (connectionId: string, database: string, runId: string, path: string, options: DocOptions) =>
    invoke<Generated>('dbdocs_generate', { args: { connection_id: connectionId, database, run_id: runId, path, options } }),
  open: (path: string, reveal = false) => invoke<void>('dbdocs_open', { args: { path, reveal } }),
  cancel: (runId: string) => invoke<void>('cancel_query', { args: { session_id: `docs:${runId}` } }),
};

/** `dbDocs:doc.*` in the current language, flattened (`kinds.view`). The
 *  key list comes from the Spanish file, the source of every language. */
export function docLabels(): Record<string, string> {
  const out: Record<string, string> = {};
  const walk = (node: unknown, prefix: string) => {
    if (typeof node === 'string') out[prefix] = t(`dbDocs:doc.${prefix}`);
    else if (node && typeof node === 'object') for (const [k, v] of Object.entries(node)) walk(v, prefix ? `${prefix}.${k}` : k);
  };
  walk((i18next.getResourceBundle('es', 'dbDocs') as { doc?: unknown } | undefined)?.doc, '');
  return out;
}

export function defaultDocOptions(): DocOptions {
  return {
    format: 'html', schemas: [], tables: true, views: true, routines: true, triggers: true, others: true,
    source: true, indexes: true, foreign_keys: true, dependencies: true, diagram: true, labels: docLabels(),
  };
}
