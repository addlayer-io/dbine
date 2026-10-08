// view=timeline: the history sidebar's "Esta pestaña" (HistoryTimeline) next
// to a stand-in editor, over a fake backend: a saved query (versions + runs)
// and a project file (runs + commits). Switching the tab reloads the
// timeline; a version or commit opens its diff, with "Restaurar".

import { defineComponent, h, ref } from 'vue';
import type { HistoryEntry } from '../api/history';
import type { FileCommit, QueryVersion } from '../api/timeline';
import HistorySidebar from '../components/HistorySidebar.vue';
import { registerEditor } from '../stores/ai';
import { useConnectionsStore } from '../stores/connections';
import { useProjectsStore } from '../stores/projects';
import { useTabsStore } from '../stores/tabs';
import { useUiStore } from '../stores/ui';
import { sampleDrivers } from './monitor-samples';

const ago = (min: number) => new Date(Date.now() - min * 60_000).toISOString();

const VERSIONS: (QueryVersion & { sql: string })[] = [
  { id: 6, query_id: 'q1', saved_at: ago(4), added: 2, removed: 1, sql: "select c.id, c.nombre, sum(p.total) as total\nfrom clientes c\njoin pedidos p on p.cliente_id = c.id\nwhere p.fecha >= date_trunc('month', now())\n  and c.pais = 'AR'\ngroup by c.id, c.nombre\norder by total desc\nlimit 50;" },
  { id: 5, query_id: 'q1', saved_at: ago(38), added: 3, removed: 1, sql: "select c.id, c.nombre, sum(p.total) as total\nfrom clientes c\njoin pedidos p on p.cliente_id = c.id\nwhere p.fecha >= date_trunc('month', now())\ngroup by c.id, c.nombre\norder by total desc;" },
  { id: 4, query_id: 'q1', saved_at: ago(95), added: 1, removed: 1, sql: 'select c.id, c.nombre\nfrom clientes c\norder by c.nombre;' },
  { id: 3, query_id: 'q1', saved_at: ago(60 * 26), added: 2, removed: 0, sql: 'select id, nombre\nfrom clientes\norder by nombre;' },
  { id: 1, query_id: 'q1', saved_at: ago(60 * 24 * 5), added: 0, removed: 0, sql: 'select * from clientes;' },
];
const CURRENT_QUERY = `${VERSIONS[0].sql.replace('limit 50;', 'limit 100;')}`;

const run = (id: number, min: number, sql: string, o: Partial<HistoryEntry> = {}): HistoryEntry => ({
  id, connection_id: 'c1', connection_name: 'Producción · ventas', driver: 'postgres', host: 'db-prod-01', database: 'ventas',
  sql, started_at: ago(min), duration_ms: 182, rows: 50, error: null, query_id: 'q1', project_id: null, file_path: null, ...o,
});
const QUERY_RUNS = [
  run(21, 3, VERSIONS[0].sql, { duration_ms: 412 }),
  run(20, 30, VERSIONS[1].sql, { error: 'column "pais" does not exist', rows: null, duration_ms: 12 }),
  run(19, 90, VERSIONS[2].sql, { rows: 1204, duration_ms: 1840 }),
];

const FILE = 'consultas/pedidos_mes.sql';
const FILE_NOW = "select date_trunc('month', fecha) as mes, count(*), sum(total)\nfrom pedidos\nwhere fecha >= now() - interval '1 year'\ngroup by 1\norder by 1 desc;\n";
const COMMITS: (FileCommit & { text: string | null })[] = [
  { hash: 'a1b2c3d4e5f6a7b8c9d0a1b2c3d4e5f6a7b8c9d0', short: 'a1b2c3d', author: 'Ana Gómez', date: ago(60 * 3), subject: 'fix: pedidos del último año', path: FILE,
    text: "select date_trunc('month', fecha) as mes, count(*)\nfrom pedidos\nwhere fecha >= now() - interval '1 year'\ngroup by 1\norder by 1 desc;\n" },
  { hash: 'b2c3d4e5f6a7b8c9d0a1b2c3d4e5f6a7b8c9d0a1', short: 'b2c3d4e', author: 'Leo Ruiz', date: ago(60 * 24 * 2), subject: 'feat: pedidos por mes', path: FILE,
    text: "select date_trunc('month', fecha) as mes, count(*)\nfrom pedidos\ngroup by 1\norder by 1 desc;\n" },
  { hash: 'c3d4e5f6a7b8c9d0a1b2c3d4e5f6a7b8c9d0a1b2', short: 'c3d4e5f', author: 'Leo Ruiz', date: ago(60 * 24 * 9), subject: 'feat: primeras consultas', path: 'pedidos.sql',
    text: 'select count(*) from pedidos;\n' },
];
const FILE_RUNS = [
  run(30, 50, FILE_NOW, { query_id: null, project_id: 'p1', file_path: FILE, rows: 12, duration_ms: 95 }),
  run(29, 60 * 24 * 2 - 30, COMMITS[1].text!, { query_id: null, project_id: 'p1', file_path: FILE, rows: 24, duration_ms: 120 }),
];

/** What the stand-in editor of each tab holds. */
export const previewTexts = ref<Record<string, string>>({ tq: CURRENT_QUERY, tf: FILE_NOW });

export function installTimelinePreview() {
  let cb = 0;
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => ++cb,
    unregisterCallback: () => {},
    invoke: async (cmd: string, a?: { args?: Record<string, unknown> }) => {
      const args = a?.args ?? {};
      await new Promise((r) => setTimeout(r, 60));
      switch (cmd) {
        case 'plugin:event|listen': return ++cb;
        case 'plugin:event|unlisten': return null;
        case 'query_versions': return args.query_id === 'q1' ? VERSIONS.map((v) => ({ ...v, sql: null })) : [];
        case 'query_version': return VERSIONS.find((v) => v.id === args.id);
        case 'query_version_checkpoint': {
          const text = previewTexts.value.tq;
          if (VERSIONS[0].sql === text) return null;
          VERSIONS.unshift({ id: VERSIONS[0].id + 1, query_id: 'q1', saved_at: new Date().toISOString(), added: 1, removed: 1, sql: text });
          return { ...VERSIONS[0], sql: null };
        }
        case 'history_of': return args.query_id === 'q1' ? QUERY_RUNS : args.file_path === FILE ? FILE_RUNS : [];
        case 'project_file_log': return args.path === FILE ? COMMITS.map(({ text: _t, ...c }) => c) : [];
        case 'project_file_at': {
          const c = COMMITS.find((x) => x.hash === args.commit);
          return { text: c?.text ?? null, binary: false, too_large: false };
        }
        case 'get_query': return { id: 'q1', connection_id: 'c1', database: 'ventas', name: 'Top clientes del mes', sql: previewTexts.value.tq, updated_at: ago(4), last_run_at: ago(3) };
        default: throw { kind: 'preview', message: `"${cmd}" no está disponible en la vista previa` };
      }
    },
  };

  const conns = useConnectionsStore();
  conns.drivers = sampleDrivers;
  conns.list = [{
    id: 'c1', name: 'Producción · ventas', color: '#3794ff', folder_id: null, tags: [], save_password: true, updated_at: '',
    config: { driver: 'postgres', host: 'db-prod-01', port: 5432, database: 'ventas', username: 'app', password: null, encrypt: true, trust_server_certificate: false, read_only: false, options: {} },
  }];
  conns.queries['c1\u0000ventas'] = { status: 'ready', items: [{ id: 'q1', connection_id: 'c1', database: 'ventas', name: 'Top clientes del mes', sql: CURRENT_QUERY, updated_at: ago(4), last_run_at: ago(3) }], error: null };
  const projects = useProjectsStore();
  (projects as unknown as { list: unknown[] }).list = [{
    id: 'p1', name: 'scripts-ventas', path: '/Users/demo/scripts-ventas', binding: { direct: null, environments: {}, active_environment: null }, sort_order: 0,
    created_at: '', updated_at: '', exists: true, is_repo: true, manifest: null, manifest_error: null, manifest_warnings: [],
  }];
  const tabs = useTabsStore();
  tabs.tabs = [
    { id: 'tq', kind: 'query', queryId: 'q1', connectionId: 'c1', database: 'ventas', preview: false },
    { id: 'tf', kind: 'file', projectId: 'p1', path: FILE, connectionId: 'c1', database: 'ventas', preview: false },
    { id: 'to', kind: 'object', object: { kind: 'table', schema: 'public', name: 'clientes' }, view: 'data', connectionId: 'c1', database: 'ventas', preview: false },
  ];
  tabs.activeId = new URLSearchParams(location.search).get('tab') ?? 'tq';
  for (const id of ['tq', 'tf']) {
    registerEditor(id, {
      text: () => previewTexts.value[id],
      selection: () => '',
      lastError: () => null,
      append: () => {},
      replace: (code) => { previewTexts.value = { ...previewTexts.value, [id]: code }; },
    });
  }
  const ui = useUiStore();
  ui.sidebarView = 'history';
}

/** The sidebar and a stand-in editor with its tab strip. */
export const TimelinePreview = defineComponent({
  setup() {
    const tabs = useTabsStore();
    const label: Record<string, string> = { tq: 'Top clientes del mes', tf: 'pedidos_mes.sql', to: 'clientes (datos)' };
    return () => h('div', { style: 'display: flex; height: 100vh; background: var(--ide-editor)' }, [
      h('div', { style: 'width: 340px; flex: none; display: flex; flex-direction: column; background: var(--ide-sidebar); border-right: 1px solid var(--nm-border-soft)' }, [h(HistorySidebar)]),
      h('div', { style: 'flex: 1; display: flex; flex-direction: column; min-width: 0' }, [
        h('div', { style: 'display: flex; gap: 1px; background: var(--ide-sidebar)' }, tabs.tabs.map((t) => h('button', {
          'data-tab': t.id,
          style: `padding: 8px 14px; border: 0; font: inherit; font-size: 12.5px; cursor: pointer; color: var(--nm-text-strong); background: ${t.id === tabs.activeId ? 'var(--ide-editor)' : 'transparent'}`,
          onClick: () => { tabs.activeId = t.id; },
        }, label[t.id]))),
        h('pre', { class: 'preview-editor', style: 'flex: 1; margin: 0; padding: 14px 18px; font-family: var(--nm-mono); font-size: 12.5px; color: var(--nm-text); white-space: pre-wrap' },
          previewTexts.value[tabs.activeId ?? ''] ?? '(sin editor)'),
      ]),
    ]);
  },
});
