// view=split: the split editor — two query tabs side by side (&dir=column:
// one above the other; &groups=1: everything in one group, to try ⌘\ and
// dragging), each with its own tab strip, over a fake PostgreSQL. The status
// bar shows the focused group's tab. Running a query returns a sample result.

import { h } from 'vue';
import EditorArea from '../components/EditorArea.vue';
import QueryView from '../views/QueryView.vue';
import StatusBar from '../components/StatusBar.vue';
import { dbKey, useConnectionsStore } from '../stores/connections';
import type { Tab } from '../stores/tabs';
import { sampleDrivers } from './monitor-samples';
import { sampleResult } from './samples';

const DB = 'tenant-ventas';
const QUERIES: Record<string, { name: string; sql: string }> = {
  q1: { name: 'Ventas por ciudad', sql: 'select mes, ciudad, count(*) as pedidos, sum(total) as total\nfrom ventas.pedidos\ngroup by mes, ciudad\norder by mes;' },
  q2: { name: 'Clientes nuevos', sql: "select id, nombre, email, alta\nfrom ventas.clientes\nwhere alta >= now() - interval '30 days'\norder by alta desc;" },
  q3: { name: 'Pedidos pendientes', sql: "select * from ventas.pedidos where estado = 'pendiente';" },
};

/** Before the store is made: the saved tabs it restores. */
export function installSplitPreview(params: URLSearchParams) {
  const one = params.get('groups') === '1';
  const tab = (id: string, queryId: string, group: string) => ({ id, kind: 'query', connectionId: 'c1', database: DB, queryId, preview: false, group });
  const saved = {
    tabs: [tab('t1', 'q1', 'L'), tab('t3', 'q3', 'L'), tab('t2', 'q2', one ? 'L' : 'R')],
    activeId: one ? 't1' : 't2',
    groups: one ? [{ id: 'L', activeId: 't1', size: 1 }] : [{ id: 'L', activeId: 't1', size: 0.5 }, { id: 'R', activeId: 't2', size: 0.5 }],
    focused: one ? 'L' : 'R',
    split: params.get('dir') === 'column' ? 'column' : 'row',
  };
  try { localStorage.setItem('dbine.tabs', JSON.stringify(saved)); } catch { /* preview */ }
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => 0,
    invoke: async (cmd: string, a?: { args?: Record<string, unknown> }) => {
      const args = a?.args ?? {};
      if (cmd === 'get_query') {
        const q = QUERIES[args.id as string] ?? QUERIES.q1;
        return { id: args.id, name: q.name, sql: q.sql, connection_id: 'c1', database: DB, updated_at: '', last_run_at: null };
      }
      if (cmd === 'save_query') return args.query;
      if (cmd === 'lint_script') return [];
      if (cmd === 'execute_query') {
        return {
          results: [{ ...sampleResult, total_rows: sampleResult.rows.length, truncated: false, rows_affected: null }],
          messages: [], error: null, elapsed_ms: 14, plans: [], log: [], errors: [],
        };
      }
      if (cmd === 'plugin:event|listen') return 1;
      return null;
    },
  };
}

/** After the store is made: the connection and its queries. */
export function setupSplitConnections() {
  const conns = useConnectionsStore();
  conns.drivers = sampleDrivers;
  conns.list = [{
    id: 'c1', name: 'Producción · ventas', color: '#3794ff', folder_id: null, tags: ['prod'], save_password: true, updated_at: '',
    config: { driver: 'postgres', host: 'db-prod-01', port: 5432, database: 'ventas', username: 'app', password: null,
      encrypt: true, trust_server_certificate: false, read_only: false, options: {} },
  }];
  conns.live.c1 = { status: 'connected', serverVersion: 'PostgreSQL 16.4', databases: [DB], defaultDatabase: DB, error: null };
  conns.queries[dbKey('c1', DB)] = {
    status: 'ready', error: null,
    items: Object.entries(QUERIES).map(([id, q]) => ({ id, name: q.name, sql: q.sql, connection_id: 'c1', database: DB, updated_at: '', last_run_at: null })),
  };
}

export function splitPreview() {
  return h('div', { style: 'height: 100vh; display: flex; flex-direction: column; background: var(--ide-editor)' }, [
    h(EditorArea, { ready: true, style: 'flex: 1; min-height: 0' }, {
      default: ({ tab }: { tab: Tab }) => (tab.kind === 'query'
        ? h('div', { style: 'flex: 1; min-height: 0; display: flex; flex-direction: column' }, [h(QueryView, { tab, style: 'flex: 1; min-height: 0' })])
        : null),
      empty: () => h('div', { style: 'padding: 20px; color: var(--nm-text-muted)' }, 'Sin pestañas'),
    }),
    h(StatusBar, { panelOpen: false }),
  ]);
}
