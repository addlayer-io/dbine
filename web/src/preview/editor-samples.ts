import type { DbObject } from '../api/types';
import { dbKey, objKey, useConnectionsStore } from '../stores/connections';
import { useTabsStore } from '../stores/tabs';

// view=query&sample=editor: the query editor over a PostgreSQL-like database
// with a few tables, to try ⌘/Ctrl+click, the unknown-name warnings, the
// parameters dialog and the snippets without a server. The last SQL "run"
// is kept in `window.__lastSql`.

export const EDITOR_SAMPLE_SQL = `-- ⌘/Ctrl+clic sobre un nombre abre la tabla
select c.nombre, c.emial, p.total
from public.clientes c
join pedidos p on p.cliente_id = c.id
where p.fecha >= :desde and c.nombre = :nombre;

select * from pedidoz;
`;

const OBJECTS: DbObject[] = [
  { kind: 'table', schema: 'public', name: 'clientes', parent: null },
  { kind: 'table', schema: 'public', name: 'pedidos', parent: null },
  { kind: 'view', schema: 'public', name: 'v_ventas', parent: null },
  { kind: 'function', schema: 'public', name: 'total_cliente', parent: null },
];
const COLUMNS: Record<string, string[]> = {
  clientes: ['id', 'nombre', 'email', 'alta'],
  pedidos: ['id', 'cliente_id', 'fecha', 'total'],
};

export function installEditorSample() {
  const conns = useConnectionsStore();
  conns.drivers = conns.drivers.map((d) => ({
    ...d,
    object_kinds: [
      { id: 'table', label: 'Tablas', has_columns: true, browsable: true, has_definition: true },
      { id: 'view', label: 'Vistas', has_columns: true, browsable: true, has_definition: true },
      { id: 'function', label: 'Funciones', has_columns: false, browsable: false, has_definition: true },
    ],
  }));
  const db = 'tenant-ventas';
  conns.objects[dbKey('c1', db)] = { status: 'ready', items: OBJECTS, error: null };
  conns.schemas[dbKey('c1', db)] = [{ name: 'public', system: false }];
  for (const [table, cols] of Object.entries(COLUMNS)) {
    conns.columns[objKey('c1', db, 'public', table)] = {
      status: 'ready', error: null,
      items: cols.map((name) => ({ name, data_type: 'text', nullable: true, primary_key: name === 'id', auto_increment: false, default_value: null })),
    };
  }
  // The query tab lives in the store, as in the app (its per-tab options are kept there).
  const tabs = useTabsStore();
  tabs.tabs.push({ id: 'q', kind: 'query', connectionId: 'c1', database: db, queryId: 'q1', preview: false });
  // Opening an object makes a tab: listed for a test to read.
  const w = window as unknown as Record<string, unknown>;
  w.__openedTabs = () => tabs.tabs.filter((t) => t.kind === 'object').map((t) => (t.kind === 'object' ? `${t.object.name}:${t.view}` : ''));
}

installEditorSample.invoke = (cmd: string, args?: Record<string, unknown>): unknown => {
  const w = window as unknown as Record<string, unknown>;
  if (cmd === 'lint_script') return [];
  if (cmd === 'execute_query') {
    w.__lastSql = args?.sql;
    return { results: [], messages: ['Ejecutado en la vista previa'], error: null, elapsed_ms: 3, plans: [], log: [], errors: [] };
  }
  if (cmd === 'plugin:event|listen') return 1;
  return null;
};
