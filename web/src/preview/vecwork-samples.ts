// view=vecwork[&tab=data|indexes|datacompare][&usage=vector][&vtype=<type>]: the workbench (App.vue)
// over a fake pgvector backend with a `documentos` table (a vector column
// and an HNSW index), to check vectors where the app lists indexes (the
// explorer's "Índices" folder, the "Índices" tab, "Eliminar índice…") and
// in the table's data (a vector column's filters and sort), and in a data
// compare of it with `documentos_copia` (vectors that differ past their
// first numbers, or by less than the preview's digits), without Tauri
// or a database. &vtype: the vector column's type as the compare gets it
// (default `vector(1536)`; `FLOAT[1536]` as DuckDB). &usage=vector: the backend's index usage carries each
// index's vector (as `IndexUsage.vector` would), so the table's schema
// isn't read. `window.__vecCalls`: the commands the backend got, with
// their args (filtered_browse_query's filters and sort, database_schema).

import type { ColumnInfo, DriverInfo, IndexUsage, IndexUsageReport, SavedConnection } from '../api/types';
import type { TableSchema } from '../api/schema-types';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore } from '../stores/tabs';
import { sampleDrivers } from './monitor-samples';
import { sampleVectorResult } from './samples';

type Args = Record<string, unknown>;
const calls: { cmd: string; args: Args }[] = [];
(window as unknown as Record<string, unknown>).__vecCalls = calls;

const DRIVER: DriverInfo = {
  ...sampleDrivers.find((d) => d.id === 'postgres')!,
  has_schemas: false,
  object_kinds: [{ id: 'table', label: 'Tablas', has_columns: true, browsable: true, has_definition: true }],
  supports_index_usage: true,
  capabilities: { create_database: true, drop_database: true, foreign_keys: true, monitor: true, vectors: true },
};
const CONNECTION: SavedConnection = {
  id: 'c1', name: 'pgvector · docs', color: null, folder_id: null, tags: [], save_password: true, updated_at: '2026-11-19T12:00:00Z',
  config: { driver: 'postgres', host: 'localhost', port: 5432, database: 'docs', username: 'app', password: null, encrypt: true, trust_server_certificate: false, read_only: false, options: {} },
};
const TABLE = { kind: 'table', schema: 'public', name: 'documentos', parent: null };
const COPY = { kind: 'table', schema: 'public', name: 'documentos_copia', parent: null };

const COLUMNS: ColumnInfo[] = [
  { name: 'id', data_type: 'int8', nullable: false, primary_key: true, auto_increment: true, default_value: null },
  { name: 'titulo', data_type: 'text', nullable: true, primary_key: false, auto_increment: false, default_value: null },
  { name: 'embedding', data_type: 'vector(1536)', nullable: true, primary_key: false, auto_increment: false, default_value: null },
];
const HNSW = { algorithm: 'hnsw', metric: 'cosine', params: { m: '24', ef_construction: '128' } };
const SCHEMA: TableSchema[] = [{
  kind: 'table', schema: 'public', name: 'documentos',
  columns: COLUMNS.map((c) => ({ name: c.name, data_type: c.data_type, nullable: c.nullable, default_value: null, auto_increment: c.auto_increment, comment: null, options: {} })),
  primary_key: { name: 'documentos_pkey', columns: ['id'] }, foreign_keys: [],
  indexes: [
    { name: 'documentos_pkey', columns: ['id'], unique: true, kind: 'btree', filter: null },
    { name: 'documentos_embedding_idx', columns: ['embedding'], unique: false, kind: 'hnsw', filter: null, vector: HNSW },
  ],
  comment: null, options: {},
}];
function usage(name: string, kind: string, keys: string[], o: Partial<IndexUsage> = {}): IndexUsage {
  return {
    name, kind, unique: false, primary_key: false, key_columns: keys, included_columns: [], filter: null, size_kb: 12_288,
    seeks: 1_200, scans: 40, lookups: 0, updates: 300, last_read: null, last_write: null,
    reads: 1_240, read_share: 0.5, unused: false, writes_per_read: 0.24, seek_ratio: 0.97, seek_health: 'good', ...o,
  };
}
function report(withVector: boolean): IndexUsageReport {
  return {
    since: null, stats_available: true, note: null, foreign_keys: [], seek_scan_split: true, writes_counted: true,
    indexes: [
      usage('documentos_pkey', 'btree', ['id'], { unique: true, primary_key: true }),
      { ...usage('documentos_embedding_idx', 'hnsw', ['embedding']), ...(withVector ? { vector: HNSW } : {}) } as IndexUsage,
    ],
  };
}

export function installVectorWorkbench(params: URLSearchParams) {
  try {
    localStorage.setItem('dbine.sidebarView', JSON.stringify('explorer'));
    localStorage.setItem('dbine.sidebarOpen', 'true');
    localStorage.setItem('dbine.sidebarWidth', '330');
    localStorage.setItem('dbine.aiOpen', 'false');
    localStorage.setItem('dbine.tabs', JSON.stringify({ tabs: [], activeId: null, collapsed: [] }));
    localStorage.setItem('dbine.openFolders', JSON.stringify(['c:c1', 'd:c1:docs', 'f:c1:docs:table']));
  } catch { /* preview */ }
  const conns = useConnectionsStore();
  conns.live.c1 = { status: 'connected', serverVersion: 'PostgreSQL 17.2 (pgvector 0.8.0)', databases: ['docs'], defaultDatabase: 'docs', error: null };
  const withVector = params.get('usage') === 'vector';
  let cb = 0;
  (window as unknown as Record<string, unknown>).__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} };
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => ++cb,
    unregisterCallback: () => {},
    convertFileSrc: (p: string) => p,
    invoke: async (cmd: string, payload?: { args?: Args }) => {
      const a = payload?.args ?? {};
      if (!cmd.startsWith('plugin:')) calls.push({ cmd, args: JSON.parse(JSON.stringify(a)) });
      switch (cmd) {
        case 'plugin:event|listen': return ++cb;
        case 'plugin:event|unlisten': return null;
        case 'list_drivers': return [DRIVER];
        case 'list_connections': return [CONNECTION];
        case 'connect': return { server_version: 'PostgreSQL 17.2', databases: ['docs'], default_database: 'docs' };
        case 'list_databases': return ['docs'];
        case 'list_database_objects': return { objects: [TABLE, COPY], schemas: null };
        case 'list_objects': return [TABLE, COPY];
        case 'data_compare': return dataCompare(params.get('vtype') ?? 'vector(1536)');
        case 'get_columns': return COLUMNS;
        case 'get_index_usage': return report(withVector);
        case 'database_schema': return SCHEMA;
        case 'list_folders': case 'list_saved_migrations': case 'list_library': case 'list_history': case 'list_queries': case 'list_projects': return [];
        case 'list_settings': case 'get_settings': return { 'telemetry.noticeSeen': true, 'telemetry.consent': false, 'import.suggested': true, 'support.next': '2099-01-01T00:00:00Z' };
        case 'set_setting': case 'get_cached': case 'files_report': case 'files_save_all_broadcast': return null;
        case 'files_unsaved_all': return [];
        case 'get_definition': return 'CREATE TABLE documentos (…);';
        case 'filtered_browse_query': {
          const sort = (a.sort as { column: string; descending: boolean }[]) ?? [];
          const order = sort.length ? ` ORDER BY ${sort.map((k) => `"${k.column}"${k.descending ? ' DESC' : ''}`).join(', ')}` : '';
          return { query: `SELECT * FROM "public"."documentos"${order} LIMIT ${a.limit}`, server_side: true, reason: null, sort_server_side: true, sort_reason: null };
        }
        case 'execute_query': {
          const { columns, rows } = sampleVectorResult;
          const keep = [0, 1, 2];
          return {
            results: [{ columns: keep.map((i) => columns[i]), rows: rows.slice(0, 12).map((r) => keep.map((i) => r[i])), total_rows: 12, truncated: false, rows_affected: null }],
            messages: [], error: null, elapsed_ms: 9, plans: [], log: [], errors: [], transaction: null, database: null,
          };
        }
        default: throw { kind: 'preview', message: `"${cmd}" no está disponible en la vista previa` };
      }
    },
  };
  // The database's objects (el-tree-v2 doesn't fire `node-expand` for the
  // nodes it restores open, and no connect happens here to reload them).
  setTimeout(() => { void conns.loadObjects('c1', 'docs'); }, 300);
  // The table's data or its "Índices" tab, once the stores are up.
  const tab = params.get('tab');
  if (tab) {
    setTimeout(() => {
      const tabs = useTabsStore();
      const ref = { kind: 'table', schema: 'public', name: 'documentos' };
      if (tab === 'indexes') tabs.openIndexes('c1', 'docs', ref);
      else if (tab === 'datacompare') {
        tabs.openDataCompare('c1', 'docs', ref, {
          left: { connectionId: 'c1', database: 'docs', table: 'public\u0001documentos' },
          right: { connectionId: 'c1', database: 'docs', table: 'public\u0001documentos_copia' },
          mode: 'tables',
        });
      } else tabs.openObject('c1', 'docs', ref, 'data', false);
    }, 400);
  }
}

/** A compare of `documentos` with its copy: row 3's vectors differ at
 *  number 10 by 1e-9 (the same first numbers and digits), row 5's at the
 *  first number, row 8's only in its title; one row on each side only. */
function dataCompare(vtype: string) {
  const { rows } = sampleVectorResult;
  const pick = (r: number) => [rows[r][0], rows[r][1], rows[r][2]];
  const nudged = (r: number, at: number, by: number) => {
    const v = JSON.parse(String(rows[r][2])) as number[];
    v[at] += by;
    return JSON.stringify(v);
  };
  const changed = [
    { left: pick(2), right: [rows[2][0], rows[2][1], nudged(2, 9, 1e-9)], diff: [2] },
    { left: pick(4), right: [rows[4][0], rows[4][1], nudged(4, 0, 0.5)], diff: [2] },
    { left: pick(7), right: [rows[7][0], `${rows[7][1]} (rev. 2)`, rows[7][2]], diff: [1] },
  ];
  return {
    id: 'vec-compare', key: ['id'], columns: ['id', 'titulo', 'embedding'], types: ['int8', 'text', vtype],
    only_left_columns: [], only_right_columns: [],
    counts: { left: 40, right: 40, same: 36, only_left: 1, only_right: 1, changed: changed.length },
    only_left: [pick(10)], only_right: [pick(11)], changed,
    truncated_left: false, truncated_right: false, ordered: true, duplicate_keys: 0,
    diff_truncated: false, diff_limit: 10_000, listed: { changed: changed.length, only_left: 1, only_right: 1 },
  };
}
