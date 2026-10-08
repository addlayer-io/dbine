// Development only (not part of the app build): renders UI pieces with
// sample data in a plain browser, to look at them without Tauri or a
// database. Open http://localhost:<vite port>/dev-preview.html?view=plan|chart|results|export|keys|tabs|dependencies
// |connection[&engine=<driver id>]|monitor|profiler[&mode=sampled]|compare|multidb[&running=1][&results=1]
// |docs (documents with nested fields: the JSON tree view)|docs-edit (the tree alone, editable)
// |projects[&sidebar=explorer] (the whole workbench over a fake backend: preview/projects-samples.ts)
import './lang';
import I18NextVue from 'i18next-vue';
import i18next from '../i18n';
import { loadBackendCatalog } from '../i18n/backend';
import { createApp, h, reactive, ref } from 'vue';
import { createPinia } from 'pinia';
import SettingsDialog from '../components/SettingsDialog.vue';
import { installSettingsMock } from './samples';
import { useUiStore } from '../stores/ui';
import { useSettingsStore } from '../stores/settings';
import { useSyncStore } from '../stores/sync';
import ElementPlus from 'element-plus';
import * as Icons from '@element-plus/icons-vue';
import 'element-plus/dist/index.css';
import 'element-plus/theme-chalk/dark/css-vars.css';
import '../styles/global.scss';
import PlanView from '../components/PlanView.vue';
import ChartView from '../components/ChartView.vue';
import JsonTreeView from '../components/JsonTreeView.vue';
import ResultsPane from '../components/ResultsPane.vue';
import CompareView from '../views/CompareView.vue';
import SupportReminder from '../components/SupportReminder.vue';
import { compareMock } from './compare-samples';
import { matchesLocal, type FilterState } from '../composables/gridFilter';
import ExportDialog from '../components/ExportDialog.vue';
import { samplePlans, sampleResult, parallelFkSchema } from './samples';
import DatabaseDiagram from '../components/DatabaseDiagram.vue';
import ConnectionView from '../views/ConnectionView.vue';
import { useTabsStore } from '../stores/tabs';
import MonitorView from '../views/MonitorView.vue';
import { useConnectionsStore } from '../stores/connections';
import { sampleDrivers, sampleProfilerPoll, sampleProfilerStart, sampleSnapshot } from './monitor-samples';
import ProfilerView from '../views/ProfilerView.vue';
import ExplorerSidebar from '../components/ExplorerSidebar.vue';
import { installKeysPreview } from './keys-samples';
import EditorTabs from '../components/EditorTabs.vue';
import { installTabsPreview } from './tabs-samples';
import { sampleDependents } from './dependencies-samples';
import DependenciesView from '../views/DependenciesView.vue';
import QueryView from '../views/QueryView.vue';
import { profilerAutostart } from '../stores/tabs';
import { bigSchema, sampleSchema } from './samples';
import ScriptGeneratorDialog from '../components/ScriptGeneratorDialog.vue';
import ImportDialog from '../components/ImportDialog.vue';
import RunScriptFileDialog from '../components/RunScriptFileDialog.vue';
import { installTauriMock, sampleImportTables, sampleKindLabels, sampleScriptObjects } from './samples';
import TableDesignerView, { type DesignerSection } from '../views/TableDesignerView.vue';
import {
  sampleDesignerMssql, sampleDesignerMongo, sampleExistingTables, sampleDesignerInitialMssql, sampleDesignerInitialMongo,
} from './samples';

import MultiDbRunDialog from '../components/MultiDbRunDialog.vue';
import App from '../App.vue';
import { installProjectsPreview } from './projects-samples';
import { multiDbOutcome, runSummary } from '../composables/multiDb';
import { sampleLive, sampleMultiDbResponse, sampleTenantDatabases } from './multidb-samples';

document.documentElement.classList.add('dark');
const params = new URLSearchParams(location.search);
const view = params.get('view') ?? 'plan';

// view=script|import|run: dialogs that talk to the backend, over a mocked IPC.
// view=import&step=2[&target=new] jumps to the target step once the preview has loaded.
// &run=1 starts the job (script / run views): the mock keeps it running,
// except run_script_file, which ends with sample errors.
const importDialog = ref<{ step: number; targetMode: string } | null>(null);
const jobDialog = ref<{ run: () => void } | null>(null);
if (params.get('run')) setTimeout(() => jobDialog.value?.run(), 400);
function dialogView() {
  if (view === 'script') {
    return h(ScriptGeneratorDialog, {
      ref: jobDialog, connectionId: 'x', database: 'ventas', objects: sampleScriptObjects, kindLabels: sampleKindLabels,
      language: 'sql', dialect: 'mssql', supportsData: true,
    });
  }
  if (view === 'import') {
    const step = Number(params.get('step') ?? 1);
    setTimeout(() => {
      if (!importDialog.value) return;
      importDialog.value.step = step;
      if (params.get('target') === 'new') importDialog.value.targetMode = 'new';
    }, 400);
    return h(ImportDialog, {
      ref: importDialog, connectionId: 'x', database: 'ventas', tables: sampleImportTables,
      designerAvailable: true, defaultSchema: 'dbo', dialect: 'mssql', initialPath: '/Users/demo/Descargas/clientes.csv',
    });
  }
  return h(RunScriptFileDialog, { ref: jobDialog, connectionId: 'x', database: 'ventas', initialPath: '/Users/demo/backups/ventas_2026-09-25.sql', initialSize: 48_318_221 });
}
if (['script', 'import', 'run'].includes(view)) installTauriMock();

// view=designer[&engine=mongo][&new=1][&section=indexes|foreign_keys|options|script]
function designer() {
  const mongo = params.get('engine') === 'mongo';
  return h(TableDesignerView, {
    connectionId: 'x', database: mongo ? 'telemetria' : 'ventas',
    spec: mongo ? sampleDesignerMongo : sampleDesignerMssql,
    schemas: mongo ? [] : ['dbo', 'ventas', 'staging'],
    existingTables: mongo ? [] : sampleExistingTables,
    initial: params.get('new') ? undefined : mongo ? sampleDesignerInitialMongo : sampleDesignerInitialMssql,
    language: mongo ? 'json' : 'sql', dialect: mongo ? '' : 'mssql',
    initialSection: (params.get('section') as DesignerSection | null) ?? 'columns',
  });
}

// view=diagram | diagram-big (150 × 20) | diagram-parallel | diagram-nofk | diagram-empty; &select=<table> selects one.
// Exports download in the browser (the app saves them with its own dialog).
function diagram() {
  const schema = view === 'diagram-big' ? bigSchema()
    : view === 'diagram-parallel' ? parallelFkSchema()
    : view === 'diagram-empty' ? []
      : view === 'diagram-nofk' ? sampleSchema.map((t) => ({ ...t, foreign_keys: [] }))
        : sampleSchema;
  const download = (href: string, name: string) => Object.assign(document.createElement('a'), { href, download: name }).click();
  const diagramRef = ref<{ focusTable: (s: string | null, n: string) => void } | null>(null);
  const pick = params.get('select');
  if (pick) setTimeout(() => { const t = schema.find((x) => x.name === pick); if (t) diagramRef.value?.focusTable(t.schema, t.name); }, 300);
  return h(DatabaseDiagram, {
    ref: diagramRef, schema, title: 'ventas',
    onOpenTable: (t: { schema: string | null; name: string }) => console.log('open-table', t),
    onExportSvg: (svg: string) => download(URL.createObjectURL(new Blob([svg], { type: 'image/svg+xml' })), 'diagrama.svg'),
    onExportPng: (url: string) => download(url, 'diagrama.png'),
  });
}

// view=docs: MongoDB-like documents (nested fields arrive as JSON text,
// as the driver sends them), one with a 250-item array (chunked in the tree).
const sampleDocuments = {
  columns: [
    { name: '_id', type_name: 'objectId' }, { name: 'cliente', type_name: 'object' }, { name: 'items', type_name: 'array' },
    { name: 'total', type_name: 'double' }, { name: 'pagado', type_name: 'bool' }, { name: 'fecha', type_name: 'date' }, { name: 'nota', type_name: 'string|null' },
  ],
  rows: Array.from({ length: 40 }, (_, i) => [
    `65a1b2c3d4e5f6071829${(0x3a00 + i).toString(16)}`,
    JSON.stringify({ nombre: `Cliente ${i + 1}`, ciudad: ['Norte', 'Sur', 'Centro'][i % 3], contacto: { email: `cliente${i + 1}@ejemplo.com`, telefonos: ['555-0100', '555-0101'] } }),
    JSON.stringify(Array.from({ length: i === 3 ? 250 : (i % 4) + 1 }, (_, k) => ({ sku: `P-${100 + k}`, cantidad: (k % 5) + 1, precio: 9.5 + k }))),
    120.5 + i, i % 2 === 0, `2024-0${(i % 9) + 1}-1${i % 10} 10:3${i % 6}:00`, i % 5 === 0 ? null : `Entrega ${i % 3 === 0 ? 'urgente' : 'normal'}`,
  ]),
};

// view=docs-edit: the JSON tree alone and editable; each edit / delete it
// emits is applied to the sample and logged ("edit r c value").
const pvDocEdits = reactive<Record<number, Record<number, string | number | boolean | null>>>({});
const pvDocDeleted = ref(new Set<number>());
function docsEditView() {
  return h(JsonTreeView, {
    columns: sampleDocuments.columns, rows: sampleDocuments.rows, edits: pvDocEdits, deleted: pvDocDeleted.value,
    editable: true, deletable: true, insertable: true,
    onEdit: (r: number, c: number, v: string | number | boolean | null | undefined) => {
      if (v === undefined) delete pvDocEdits[r]?.[c];
      else (pvDocEdits[r] ??= {})[c] = v;
      console.log('edit', r, c, JSON.stringify(v));
    },
    onDelete: (rows: number[], mark: boolean) => {
      const next = new Set(pvDocDeleted.value);
      for (const r of rows) { if (mark) next.add(r); else next.delete(r); }
      pvDocDeleted.value = next;
      console.log('delete', JSON.stringify(rows), mark);
    },
  });
}

// view=filters: the column filter row, filtering the sample locally.
const pvFilters = ref<Record<string, FilterState>>({});
function filtersView() {
  const list = Object.values(pvFilters.value).flatMap((f) => f.filters);
  const names = sampleResult.columns.map((c) => c.name);
  const cols = [...sampleResult.columns, { name: 'activo', type_name: 'bit' }, { name: 'alta', type_name: 'date' }];
  const rows = sampleResult.rows.map((r, i) => [...r, i % 3 === 0 ? null : i % 2 === 0, `2024-0${(i % 9) + 1}-1${i % 10}`]);
  return h(ResultsPane, {
    running: false, title: 'Ventas', dialect: 'mssql', filterable: true, filters: pvFilters.value,
    outcome: { results: [{ columns: cols, rows: rows.filter((r) => matchesLocal(r, [...names, 'activo', 'alta'], list)), total_rows: rows.length, truncated: false, rows_affected: null }], messages: [], error: null, elapsed_ms: 5, plans: [] },
    onFilter: (c: string, st: FilterState | null) => {
      const next = { ...pvFilters.value };
      if (st) next[c] = st; else delete next[c];
      pvFilters.value = next;
      console.log('filter', JSON.stringify(list));
    },
  });
}

// view=multidb: the merged results of a run on several databases, with the
// dialog on top (&results=1: without it; &running=1: the dialog mid-run).
function multiDbView() {
  try { localStorage.setItem('dbine.multiDb.x', JSON.stringify(['tenant-norte', 'tenant-sur', 'tenant-centro', 'tenant-acme'])); } catch { /* ignore */ }
  const { outcome, labels } = multiDbOutcome(sampleMultiDbResponse);
  return [
    h('div', { style: 'display: flex; align-items: center; gap: 6px; padding: 3px 10px; font-size: 12px; border-bottom: 1px solid var(--nm-border-soft)' },
      i18next.t('multiDb:results.bar', { summary: runSummary(sampleMultiDbResponse) })),
    h('div', { style: 'flex: 1; min-height: 0' }, [h(ResultsPane, { running: false, title: 'Clientes', dialect: 'mssql', outcome, labels, hideStatus: true })]),
    params.get('results') ? null : h(MultiDbRunDialog, {
      connectionId: 'x', currentDatabase: 'tenant-norte', databases: sampleTenantDatabases,
      live: params.get('running') ? sampleLive() : null,
      onRun: (dbs: string[]) => console.log('run', dbs),
    }),
  ];
}
if (view === 'multidb') installTauriMock();

const app = createApp({
  render: () =>
    view === 'projects' ? h(App) : h('div', { style: 'height: 100vh; display: flex; flex-direction: column;' }, [
      view === 'tabs' ? h('div', { style: 'width: 1100px; background: var(--ide-editor)' }, [h(EditorTabs)]) :
      view === 'keys' ? h('div', { style: 'width: 340px; height: 640px; background: var(--ide-sidebar)' }, [h(ExplorerSidebar)]) :
      view === 'connection' ? connectionTab() : view === 'monitor'
        ? h(MonitorView, { tab: { id: 'm', kind: 'monitor', connectionId: 'c1', database: '', preview: false }, active: true }) :
      // view=query&w=<px>: the query editor's toolbar at a given width.
      view === 'query' ? h('div', { style: `width: ${params.get('w') ?? 1300}px; height: 260px; display: flex; flex-direction: column; background: var(--ide-editor)` }, [
        h(QueryView, { tab: { id: 'q', kind: 'query', connectionId: 'c1', database: 'tenant-ventas', queryId: 'q1', preview: false, continueOnError: true } }),
      ]) :
      view === 'support' ? h(SupportReminder) :
      view === 'multidb' ? multiDbView() :
      view === 'dependencies' ? h(DependenciesView, { tab: { id: 'dep', kind: 'dependencies', connectionId: 'c1', database: 'ventas', object: { kind: 'table', schema: 'dbo', name: 'Clientes' }, column: 'Pepe', preview: false } }) :
      view === 'compare' ? h(CompareView, { tab: { id: 'cmp', kind: 'compare', connectionId: 'c1', database: 'ventas', preview: false } }) :
      view === 'profiler' ? h(ProfilerView, { tab: { id: 'p', kind: 'profiler', connectionId: 'c1', database: 'ventas', preview: false } }) :
      view === 'settings' ? h(SettingsDialog) : view.startsWith('diagram') ? diagram() : ['script', 'import', 'run'].includes(view) ? dialogView() : view === 'designer' ? designer() : view === 'chart'
        ? h(ChartView, { columns: sampleResult.columns, rows: sampleResult.rows })
        : view === 'filters' ? filtersView()
        : view === 'docs-edit' ? docsEditView()
        : view === 'docs'
          ? h(ResultsPane, {
            running: false, title: 'pedidos',
            outcome: { results: [{ ...sampleDocuments, total_rows: sampleDocuments.rows.length, truncated: false, rows_affected: null }], messages: [], error: null, elapsed_ms: 12, plans: [] },
          })
        : view === 'results'
          ? h(ResultsPane, {
            running: false, title: 'Ventas', dialect: 'mssql',
            source: { connectionId: 'x', database: 'ventas', sql: 'select 1' },
            outcome: { results: [{ ...sampleResult, total_rows: 12000, truncated: true, rows_affected: null }], messages: [], error: null, elapsed_ms: 38, plans: [] },
          })
          : view === 'export'
            ? h(ExportDialog, {
              columns: sampleResult.columns, rows: sampleResult.rows, totalRows: 12000, truncated: true,
              source: { connectionId: 'x', database: 'ventas', sql: 'select 1', resultIndex: 0 },
              title: 'Ventas', dialect: 'mssql', initialFormat: 'sql',
            })
            : h(PlanView, { plans: samplePlans }),
    ]),
});
app.use(createPinia());
app.use(I18NextVue, { i18next });
loadBackendCatalog();
app.use(ElementPlus, { size: 'small' });
if (view === 'support') {
  // A reminder that's due: `status` = none | once (…?status=once).
  const st = useSettingsStore();
  st.values = { 'support.first_seen': '2026-01-01T00:00:00Z', 'support.next': '2026-01-02T00:00:00Z', 'support.status': params.get('status') ?? 'none' };
  st.loaded = true;
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = { transformCallback: () => 0, invoke: async () => null };
}
if (view === 'settings') {
  installSettingsMock(params.get('state') ?? 'none');
  useSettingsStore().load();
  useSyncStore().refresh();
  useUiStore().openSettings((params.get('section') as 'general' | 'sync') ?? 'sync');
}
if (view === 'connection' || view === 'monitor' || view === 'profiler' || view === 'compare' || view === 'dependencies' || view === 'query') {
  const conns = useConnectionsStore();
  // Compare reads index usage and checks dependents before a drop (both faked in compare-samples).
  conns.drivers = view === 'compare' ? sampleDrivers.map((d) => ({ ...d, supports_index_usage: true, supports_dependencies: true }))
    : view === 'query' ? sampleDrivers.map((d) => ({ ...d, supports_manual_transactions: true }))
      : sampleDrivers;
  conns.list = [{
    id: 'c1', name: 'Producción · ventas', color: '#3794ff', folder_id: null, tags: ['prod'], save_password: true, updated_at: '',
    config: { driver: 'postgres', host: 'db-prod-01', port: 5432, database: 'ventas', username: 'app', password: null,
      encrypt: true, trust_server_certificate: false, read_only: false, options: {} },
  }];
  conns.live.c1 = { status: 'connected', serverVersion: 'PostgreSQL 16.4', databases: view === 'compare' ? ['ventas', 'ventas_qa'] : view === 'query' ? ['tenant-ventas', 'tenant-compras'] : ['ventas'], defaultDatabase: 'ventas', error: null };
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => 0,
    invoke: async (cmd: string, a?: { args?: Record<string, unknown> }) => {
      if (cmd === 'monitor_snapshot') return sampleSnapshot();
      if (cmd === 'profiler_start') return sampleProfilerStart(params.get('mode') === 'sampled' ? 'sampled' : 'complete');
      if (cmd === 'profiler_poll') return sampleProfilerPoll();
      if (cmd === 'profiler_stop') return null;
      if (cmd === 'get_dependents' && view !== 'compare') return sampleDependents;
      if (cmd === 'test_connection') return { ok: true, message: 'PostgreSQL 16.4 · 38 ms' };
      if (view === 'query') {
        if (cmd === 'get_query') return { id: 'q1', name: 'Query', sql: 'select top 10 * from ventas.clientes', connection_id: 'c1', database: null, folder: null, updated_at: '' };
        return null;
      }
      const cmp = compareMock(cmd, a);
      if (cmp !== undefined) return cmp;
      throw { kind: 'preview', message: `"${cmd}" no está disponible en la vista previa` };
    },
  };
  if (view === 'profiler') profilerAutostart.add('p');
  if (view === 'connection') {
    const ui = useUiStore();
    const engine = params.get('engine');
    ui.newConnection(null);
    if (engine) setTimeout(() => document.querySelector<HTMLButtonElement>(`[data-engine="${engine}"]`)?.click(), 200);
  }
}
function connectionTab() {
  const t = useTabsStore().tabs.find((x) => x.kind === 'connection');
  return t && t.kind === 'connection' ? h(ConnectionView, { tab: t }) : null;
}
if (view === 'keys') installKeysPreview();
if (view === 'tabs') installTabsPreview();
if (view === 'projects') installProjectsPreview();
for (const [name, comp] of Object.entries(Icons)) app.component(`Ei${name}`, comp as never);
app.mount('#app');
