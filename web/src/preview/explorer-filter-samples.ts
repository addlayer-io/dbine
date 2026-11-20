// view=explorerfilter: the explorer's filter over a fake backend (issue: the
// filter found nothing in folders that weren't open). A SQL Server-like
// database with 50,000 views and a few tables, none of its folders open; the
// explorer cache holds the tables (opened another day); the server answers
// name searches a little later; "En el código" finds a procedure and a view
// that read indicadores_directorio.medidas. A second connection is billed
// (BigQuery-like): its server search waits for Enter.
// `window.__filterCalls` lists the commands the explorer sent.

import type { DbObject, DriverInfo, SavedConnection } from '../api/types';
import { useConnectionsStore } from '../stores/connections';
import { sampleDrivers } from './monitor-samples';

type Args = Record<string, unknown>;
const calls: { cmd: string; args: Args }[] = [];
(window as unknown as Record<string, unknown>).__filterCalls = calls;

const KINDS = [
  { id: 'table', label: 'Tablas', has_columns: true, browsable: true, has_definition: true, generated_definition: true },
  { id: 'view', label: 'Vistas', has_columns: true, browsable: true, has_definition: true },
  { id: 'procedure', label: 'Procedimientos', has_columns: false, browsable: false, has_definition: true },
];
const base = sampleDrivers.find((d) => d.id === 'sqlserver') ?? sampleDrivers[0];
const MSSQL: DriverInfo = {
  ...base, id: 'sqlserver', name: 'SQL Server', has_schemas: true, object_kinds: KINDS, key_search: null, databases_label: 'Bases de datos',
  capabilities: { create_database: true, drop_database: true, foreign_keys: true, monitor: false },
};
const BQ: DriverInfo = {
  ...MSSQL, id: 'bigquery', name: 'BigQuery',
  capabilities: { ...MSSQL.capabilities, billed_metadata: true },
};
const conn = (id: string, name: string, driver: string): SavedConnection => ({
  id, name, color: null, folder_id: null, tags: [], save_password: true, updated_at: '2026-11-20T12:00:00Z',
  config: { driver, host: 'localhost', port: 1433, database: '', username: 'sa', password: null, encrypt: true, trust_server_certificate: false, read_only: false, options: {} },
});

const o = (kind: string, schema: string, name: string): DbObject => ({ kind, schema, name, parent: null });
const TABLES = [o('table', 'dbo', 'Clientes'), o('table', 'dbo', 'Pedidos'), o('table', 'dbo', 'clientes_hist'), o('table', 'dbo', 'Productos'), o('table', 'ventas', 'clientes'), o('table', 'ventas', 'facturas')];
const VIEWS = [
  ...Array.from({ length: 50_000 }, (_, i) => o('view', 'dbo', `v_${String(i).padStart(5, '0')}`)),
  o('view', 'dbo', 'v_clientes_activos'), o('view', 'dbo', 'v_medidas_resumen'),
];
const PROCS = [o('procedure', 'dbo', 'sp_cargar_medidas'), o('procedure', 'dbo', 'sp_clientes_baja')];
const ALL = [...TABLES, ...VIEWS, ...PROCS];
/** The objects' code, for "En el código". */
const CODE: Record<string, string[]> = {
  'procedure:sp_cargar_medidas': ['CREATE PROCEDURE dbo.sp_cargar_medidas AS', 'BEGIN', '  INSERT INTO indicadores_directorio.medidas (id, valor)', '  SELECT id, valor FROM dbo.Pedidos', 'END'],
  'view:v_medidas_resumen': ['CREATE VIEW dbo.v_medidas_resumen AS', 'SELECT m.id, sum(m.valor) AS total', 'FROM indicadores_directorio.medidas m', 'GROUP BY m.id'],
};
const BQ_ALL = [o('table', 'analytics', 'clientes_eventos'), o('table', 'analytics', 'sesiones')];

const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

function page(all: DbObject[], q: Args) {
  const p = typeof q.pattern === 'string' ? q.pattern.toLowerCase() : null;
  const name = typeof q.name === 'string' ? q.name.toLowerCase() : null;
  const notKinds = (q.not_kinds as string[] | undefined) ?? [];
  const fits = all.filter((x) => (q.schema == null || x.schema === q.schema)
    && (q.kind == null || x.kind === q.kind) && !notKinds.includes(x.kind)
    && (!p || x.name.toLowerCase().includes(p)) && (!name || x.name.toLowerCase() === name));
  const from = q.after ? Number(q.after) : 0;
  const limit = Number(q.limit) || 1000;
  const objects = fits.slice(from, from + limit);
  return { objects, next: from + limit < fits.length ? String(from + limit) : null };
}

export function installExplorerFilterPreview() {
  try {
    // The connections and their databases open; no folder open.
    localStorage.setItem('dbine.openFolders', JSON.stringify(['c:c1', 'd:c1:ventas', 'c:c2', 'd:c2:analytics']));
    localStorage.setItem('dbine.explorerFilterCode', 'false');
  } catch { /* preview */ }
  const conns = useConnectionsStore();
  conns.drivers = [MSSQL, BQ];
  conns.list = [conn('c1', 'SQL Server · ventas', 'sqlserver'), conn('c2', 'BigQuery · analytics', 'bigquery')];
  conns.loaded = true;
  conns.live.c1 = { status: 'connected', serverVersion: 'SQL Server 2022', databases: ['ventas'], defaultDatabase: 'ventas', error: null };
  conns.live.c2 = { status: 'connected', serverVersion: 'BigQuery', databases: ['analytics'], defaultDatabase: 'analytics', error: null };
  let cb = 0;
  (window as unknown as Record<string, unknown>).__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} };
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => ++cb,
    unregisterCallback: () => {},
    invoke: async (cmd: string, payload?: { args?: Args }) => {
      const a = payload?.args ?? {};
      if (!cmd.startsWith('plugin:')) calls.push({ cmd, args: JSON.parse(JSON.stringify(a)) });
      const bq = a.connection_id === 'c2';
      switch (cmd) {
        case 'plugin:event|listen': return ++cb;
        case 'plugin:event|unlisten': return null;
        case 'list_drivers': return [MSSQL, BQ];
        case 'list_connections': return [conn('c1', 'SQL Server · ventas', 'sqlserver'), conn('c2', 'BigQuery · analytics', 'bigquery')];
        case 'connect': return { server_version: 'SQL Server 2022', databases: bq ? ['analytics'] : ['ventas'], default_database: bq ? 'analytics' : 'ventas' };
        case 'list_databases': return bq ? ['analytics'] : ['ventas'];
        case 'list_schemas': return bq ? [{ name: 'analytics', system: false }] : [{ name: 'dbo', system: false }, { name: 'ventas', system: false }];
        case 'list_object_page': {
          await wait(400);
          return page(bq ? BQ_ALL : ALL, (a.query as Args) ?? {});
        }
        case 'get_cached': {
          const q = a.query as Args | undefined;
          // The cache kept the tables (opened another day), not the views.
          if (a.kind === 'page' && q?.pattern && !bq) return page(TABLES, { pattern: q.pattern, limit: q.limit }).objects;
          return null;
        }
        case 'search_database': {
          await wait(600);
          const text = String((a.query as Args).text).toLowerCase();
          const hits = Object.entries(CODE).flatMap(([key, lines]) => {
            const [kind, name] = key.split(':');
            const at = lines.findIndex((l) => l.toLowerCase().includes(text));
            return at < 0 ? [] : [{ kind, schema: 'dbo', name, parent: null, line: at + 1, text: lines[at].trim() }];
          });
          return { hits, scanned: Object.keys(CODE).length, unreadable: [], truncated: false, cancelled: false, from_catalog: true };
        }
        case 'cancel_query': return null;
        case 'get_permissions': return {};
        case 'list_folders': case 'list_saved_migrations': case 'list_queries': case 'list_projects': case 'list_contexts': case 'list_library': case 'list_history': return [];
        case 'list_settings': case 'get_settings': return { 'telemetry.noticeSeen': true, 'telemetry.consent': false, 'import.suggested': true, 'support.next': '2099-01-01T00:00:00Z' };
        case 'set_setting': case 'window_role': case 'results_keep_tabs': case 'sealed_prune': return null;
        default: throw { kind: 'preview', message: `"${cmd}" no está disponible en la vista previa` };
      }
    },
  };
  // `__filterAdd(kind, schema, name)`: an object created outside DBine;
  // `__filterConns`: the store, to re-read in the background.
  const w = window as unknown as Record<string, unknown>;
  w.__filterAdd = (kind: string, schema: string, name: string) => { ALL.push(o(kind, schema, name)); };
  w.__filterConns = conns;
  // The open databases' schema level (no connect happens here to load it).
  setTimeout(() => {
    void conns.loadLevel('c1', 'ventas');
    void conns.loadLevel('c2', 'analytics');
  }, 300);
}
