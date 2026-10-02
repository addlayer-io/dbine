// Development only (not part of the app build): renders UI pieces with
// sample data in a plain browser, to look at them without Tauri or a
// database. Open http://localhost:<vite port>/dev-preview.html?view=plan|chart|results|export|keys|tabs|dependencies
// |connection[&engine=<driver id>]|monitor|profiler[&mode=sampled]
import './lang';
import I18NextVue from 'i18next-vue';
import i18next from '../i18n';
import { loadBackendCatalog } from '../i18n/backend';
import { createApp, h, ref } from 'vue';
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

const app = createApp({
  render: () =>
    h('div', { style: 'height: 100vh; display: flex; flex-direction: column;' }, [
      view === 'tabs' ? h('div', { style: 'width: 1100px; background: var(--ide-editor)' }, [h(EditorTabs)]) :
      view === 'keys' ? h('div', { style: 'width: 340px; height: 640px; background: var(--ide-sidebar)' }, [h(ExplorerSidebar)]) :
      view === 'connection' ? connectionTab() : view === 'monitor'
        ? h(MonitorView, { tab: { id: 'm', kind: 'monitor', connectionId: 'c1', database: '', preview: false }, active: true }) :
      view === 'support' ? h(SupportReminder) :
      view === 'dependencies' ? h(DependenciesView, { tab: { id: 'dep', kind: 'dependencies', connectionId: 'c1', database: 'ventas', object: { kind: 'table', schema: 'dbo', name: 'Clientes' }, column: 'Pepe', preview: false } }) :
      view === 'compare' ? h(CompareView, { tab: { id: 'cmp', kind: 'compare', connectionId: 'c1', database: 'ventas', preview: false } }) :
      view === 'profiler' ? h(ProfilerView, { tab: { id: 'p', kind: 'profiler', connectionId: 'c1', database: 'ventas', preview: false } }) :
      view === 'settings' ? h(SettingsDialog) : view.startsWith('diagram') ? diagram() : ['script', 'import', 'run'].includes(view) ? dialogView() : view === 'designer' ? designer() : view === 'chart'
        ? h(ChartView, { columns: sampleResult.columns, rows: sampleResult.rows })
        : view === 'filters' ? filtersView()
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
if (view === 'connection' || view === 'monitor' || view === 'profiler' || view === 'compare' || view === 'dependencies') {
  const conns = useConnectionsStore();
  conns.drivers = sampleDrivers;
  conns.list = [{
    id: 'c1', name: 'Producción · ventas', color: '#3794ff', folder_id: null, tags: ['prod'], save_password: true, updated_at: '',
    config: { driver: 'postgres', host: 'db-prod-01', port: 5432, database: 'ventas', username: 'app', password: null,
      encrypt: true, trust_server_certificate: false, read_only: false, options: {} },
  }];
  conns.live.c1 = { status: 'connected', serverVersion: 'PostgreSQL 16.4', databases: view === 'compare' ? ['ventas', 'ventas_qa'] : ['ventas'], defaultDatabase: 'ventas', error: null };
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => 0,
    invoke: async (cmd: string, a?: { args?: Record<string, unknown> }) => {
      if (cmd === 'monitor_snapshot') return sampleSnapshot();
      if (cmd === 'profiler_start') return sampleProfilerStart(params.get('mode') === 'sampled' ? 'sampled' : 'complete');
      if (cmd === 'profiler_poll') return sampleProfilerPoll();
      if (cmd === 'profiler_stop') return null;
      if (cmd === 'get_dependents') return sampleDependents;
      if (cmd === 'test_connection') return { ok: true, message: 'PostgreSQL 16.4 · 38 ms' };
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
for (const [name, comp] of Object.entries(Icons)) app.component(`Ei${name}`, comp as never);
app.mount('#app');
