import { invoke } from '@tauri-apps/api/core';
import type { Cell, ResultColumn } from '../api/types';
import { t } from '../i18n';
import { rt } from './i18nLabels';

// Result exports (crates/dbine-core/src/export.rs does the writing).

export type ExportFormat = 'json' | 'json_lines' | 'sql' | 'csv' | 'csv_semicolon' | 'csv_excel' | 'tsv' | 'xlsx' | 'xml';

/** `label` is a getter in the current language. */
export const EXPORT_FORMATS: { id: ExportFormat; readonly label: string; ext: string; filter: string }[] = [
  { id: 'json', label: 'JSON', ext: 'json', filter: 'JSON' },
  { id: 'json_lines', label: 'JSON Lines / NDJSON', ext: 'jsonl', filter: 'JSON Lines' },
  { id: 'sql', label: 'SQL (INSERT)', ext: 'sql', filter: 'SQL' },
  { id: 'csv', label: 'CSV', ext: 'csv', filter: 'CSV' },
  { id: 'csv_semicolon', get label() { return rt('core:export.csvSemicolon'); }, ext: 'csv', filter: 'CSV' },
  { id: 'csv_excel', get label() { return rt('core:export.csvExcel'); }, ext: 'csv', filter: 'CSV' },
  { id: 'tsv', get label() { return rt('core:export.tsv'); }, ext: 'tsv', filter: 'TSV' },
  { id: 'xlsx', label: 'MS Excel (.xlsx)', ext: 'xlsx', filter: 'Excel' },
  { id: 'xml', label: 'XML', ext: 'xml', filter: 'XML' },
];

export interface ExportOptions {
  format: ExportFormat;
  header: boolean;
  null_text: string;
  delimiter: string;
  quote_all: boolean;
  /** CSV/TSV: a `'` before text that a spreadsheet would run as a formula. */
  formula_safe: boolean;
  crlf: boolean;
  bom: boolean;
  pretty: boolean;
  table: string;
  rows_per_insert: number;
  quote: 'double' | 'bracket' | 'backtick';
  /** SQL: the target reads strings the standard way; backslashes are written as they are. Only used when
   * the backend has no exact form for the source (no connection, or an engine other than PostgreSQL,
   * SQL Server, Oracle, SQLite or DuckDB) and the source doesn't read backslash escapes. */
  standard_strings: boolean;
  sheet: string;
  xml_root: string;
  xml_row: string;
}

export function defaultOptions(format: ExportFormat, table = t('core:export.defaultTable'), dialect = ''): ExportOptions {
  return {
    format, header: true, null_text: '', delimiter: '', quote_all: false, formula_safe: true, crlf: false, bom: false,
    pretty: true, table, rows_per_insert: 100, quote: quoteFor(dialect), standard_strings: false, sheet: t('core:export.defaultSheet'),
    xml_root: 'rows', xml_row: 'row',
  };
}

/** Identifier quoting of the driver's SQL dialect, for SQL exports. */
export function quoteFor(dialect: string): ExportOptions['quote'] {
  if (['mssql', 'sybase', 'access'].includes(dialect)) return 'bracket';
  if (['mysql', 'bigquery', 'hive', 'clickhouse', 'sparksql', 'databricks'].includes(dialect)) return 'backtick';
  return 'double';
}

/** A file name from a title: "Ventas por mes" → "Ventas por mes.csv". */
export function fileName(title: string, format: ExportFormat) {
  const base = (title || t('core:export.defaultFile')).replace(/[\\/:*?"<>|]+/g, '_').slice(0, 80);
  return `${base}.${EXPORT_FORMATS.find((f) => f.id === format)!.ext}`;
}

export interface ExportResult { rows: number; elapsed_ms: number }

export const exportApi = {
  /** `connectionId`: where the rows came from; SQL literals follow its engine's escaping. */
  rows: (path: string, options: ExportOptions, columns: ResultColumn[], rows: Cell[][], connectionId?: string | null) =>
    invoke<ExportResult>('export_rows_to_file', { args: { path, options, columns, rows, connection_id: connectionId ?? null } }),
  query: (a: {
    exportId: string; connectionId: string; database: string; sql: string; resultIndex: number;
    path: string; options: ExportOptions;
  }) =>
    invoke<ExportResult>('export_query_to_file', {
      args: {
        export_id: a.exportId, connection_id: a.connectionId, database: a.database, sql: a.sql,
        result_index: a.resultIndex, path: a.path, options: a.options,
      },
    }),
  cancel: (exportId: string) => invoke<void>('cancel_query', { args: { session_id: `export:${exportId}` } }),
};
