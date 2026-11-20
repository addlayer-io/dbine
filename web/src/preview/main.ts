// Development only (not part of the app build): renders UI pieces with
// sample data in a plain browser, to look at them without Tauri or a
// database. Open http://localhost:<vite port>/dev-preview.html?view=plan|chart|results|export|keys|tabs|split|dependencies|fonts
// |connection[&engine=<driver id>][&testfail=1]|monitor|profiler[&mode=sampled]|compare|multidb[&running=1][&results=1]
// |datacompare (Comparar datos in query mode, 40 paired columns after "Leer columnas": the column table scrolls, "Comparar" stays in view)
// |docs (documents with nested fields: the JSON tree view)|docs-edit (the tree alone, editable)
// |query[&sample=editor|q] (⌘-click, unknown names, parameters, snippets, a table's columns on hover: preview/editor-samples.ts)
// |query&sample=named[&exec=1][&again=1][&rename=1][&show=<name>] (named results and the run lens: preview/named-samples.ts)
// |projects[&sidebar=explorer] (the whole workbench over a fake backend: preview/projects-samples.ts)
// |timeline[&tab=tq|tf|to] (the history sidebar's "Esta pestaña" next to a stand-in editor: preview/timeline-samples.ts)
// |rename[&prod=0][&name=<nuevo>][&fail=1] ("Renombrar…" over a fake impact and script: preview/rename-samples.ts)
// |map[&set=points|shapes|latlon|oracle][&theme=light] (the results' map view over the offline base map: preview/map-samples.ts)
// |chart[&kind=…]|nbcell[&set=charts|map|latlon] (charts of every kind; a notebook cell's charts and map: preview/chart-samples.ts)
// |nbxresults[&dialog=1] (cells on three engines and a "DataFrame SQL" cell joining them: preview/nbx-samples.ts)
// |schedule[&set=report|problems] (the "Ejecutar notebook" step and the mail with its HTML body: preview/schedule-samples.ts)
// |jobs ("Programadas" with its server jobs next to a job's tab: preview/jobs-samples.ts)
// |scheduler[&kind=job|operator|alert]|scheduler-tree|scheduler-log|scheduler-jobtab (the scheduler designer, the server's tree and a job's tab over a fake SQL Server Agent: preview/scheduler-samples.ts)
// |nbwrite[&state=form|confirm|result|error|full][&prod=1] (a write cell, its confirmation and its result: preview/nbwrite-samples.ts)
// |nbperm[&set=one|many|glob|unsaved|sidecars|remote|settings|notebook] (a notebook's file approval dialog and the grants lists: preview/nbperm-samples.ts)
// |nbparams[&open=panel|dialog|edit] (a notebook's parameters: the panel, the run's dialog, a stale result: preview/nbparams-samples.ts)
// |zoho-wizard[&step=1..4][&filled=1][&result=ok][&error=expired|mismatch] ("Conectar Zoho Vault", each step)
// |connection&source=zoho|zoho-manual|zoho-missing (the Zoho block: an account, the inline sign-in, an account not on this computer)
// |account-wizard&kind=<bitwarden|bitwarden_sm|vault|delinea|conjur|ccp|onepassword|aws|azure|gcp|keepass>[&step=n][&result=ok][&error=1][&detect=none|missing]
//   ("Conectar <gestor>", each step: preview/account-samples.ts)
// |connstring[&include=1][&reveal=1][&engine=sqlserver] ("Copiar cadena de conexión…": preview/connstring-samples.ts)
// |connection&source=vault-account|bitwarden-account|aws-account|keepass-account|onepassword-account|vault-missing (the account block of each manager)
// |results&set=vectors[&edit=1] (vector columns: each cell's first numbers and dimensions; &edit=1 the grid editable, with the changes
//   update_script would get under it)|vecviewer[&col=embedding|cuantizado|terminos] (a vector in the cell viewer)
// |designer&engine=pgvector|oracle[&new=1][&section=indexes][&alter=1][&vtype=<the vector column's type>] (vector indexes in the
//   table designer: preview/samples.ts; &alter=1 edits the table in place, &vtype=FLOAT[3]|Array(Float32)|float_vector(3)|… the types
//   other engines keep vectors in)
// |docs-edit&set=vectors (vector fields in the editable JSON tree)|adddoc&set=vectors ("Agregar documento" with vector fields)
// |nbwrite&set=vectors (a write cell's preview with a vector column)|datagen&set=vectors (test data's sample with a vector column)
// |vecwork[&tab=data|indexes|datacompare][&usage=vector][&vtype=…] (the workbench over a fake pgvector backend: preview/vecwork-samples.ts)
import './lang';
import I18NextVue from 'i18next-vue';
import i18next, { i18nReady } from '../i18n';
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
import JsonTreeView from '../components/JsonTreeView.vue';
import ResultsPane from '../components/ResultsPane.vue';
import ResultGrid from '../components/ResultGrid.vue';
import CompareView from '../views/CompareView.vue';
import DataCompareView from '../views/DataCompareView.vue';
import SupportReminder from '../components/SupportReminder.vue';
import FeedbackDialog from '../components/FeedbackDialog.vue';
import { compareMock } from './compare-samples';
import { matchesLocal, type FilterState } from '../composables/gridFilter';
import { sortRows, type SortKey } from '../composables/gridSort';
import ExportDialog from '../components/ExportDialog.vue';
import { samplePlans, sampleResult, parallelFkSchema } from './samples';
import DatabaseDiagram from '../components/DatabaseDiagram.vue';
import ConnectionView from '../views/ConnectionView.vue';
import { useTabsStore } from '../stores/tabs';
import StatusBar from '../components/StatusBar.vue';
import MonitorView from '../views/MonitorView.vue';
import { useConnectionsStore } from '../stores/connections';
import { sampleDrivers, sampleProfilerPoll, sampleProfilerStart, sampleSnapshot } from './monitor-samples';
import ProfilerView from '../views/ProfilerView.vue';
import ExplorerSidebar from '../components/ExplorerSidebar.vue';
import { installKeysPreview } from './keys-samples';
import EditorTabs from '../components/EditorTabs.vue';
import { installTabsPreview } from './tabs-samples';
import { installSplitPreview, setupSplitConnections, splitPreview } from './split-samples';
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
  sampleDesignerInitialPgvector, sampleDesignerOracleVector, sampleDesignerPgvector, sampleVectorDocuments, sampleVectorResult,
} from './samples';
import CellViewer from '../components/CellViewer.vue';
import { buildChanges, buildInserts } from '../composables/gridEdit';
import { rowsForScripts } from '../composables/vector';
import AddDocumentDialog from '../components/AddDocumentDialog.vue';
import GenerateDataDialog from '../components/GenerateDataDialog.vue';
import { installVectorWorkbench } from './vecwork-samples';
import type { Cell } from '../api/types';

import MultiDbRunDialog from '../components/MultiDbRunDialog.vue';
import App from '../App.vue';
import { installProjectsPreview } from './projects-samples';
import { installTimelinePreview, TimelinePreview } from './timeline-samples';
import { multiDbOutcome, runSummary } from '../composables/multiDb';
import { sampleLive, sampleMultiDbResponse, sampleTenantDatabases } from './multidb-samples';
import { EDITOR_SAMPLE_SQL, installEditorSample } from './editor-samples';
import { Q_SAMPLE, installQSample } from './q-samples';
import { DAX_SAMPLE, installDaxSample } from './dax-samples';
import { NAMED_SAMPLE_SQL, installNamedSample } from './named-samples';
import RenameDialog from '../components/RenameDialog.vue';
import { installRenamePreview } from './rename-samples';
import { applyThemeClass } from '../composables/appearance';
import { mapPreview } from './map-samples';
import { chartPreview, installWholeMock, notebookCellPreview } from './chart-samples';
import { notebookResultsPreview } from './nbx-samples';
import { notebookWritePreview } from './nbwrite-samples';
import { installNbPermMock, nbPermPreview } from './nbperm-samples';
import { installScheduleMock, installScheduledTaskMock, schedulePreview, scheduledTaskPreview, setupScheduleConnections } from './schedule-samples';
import { installJobsMock, jobsPreview, setupJobs } from './jobs-samples';
import { installSchedulerMock, schedulerJobTabPreview, schedulerLogPreview, schedulerPreview, schedulerTreePreview, setupScheduler } from './scheduler-samples';
import { notebookParamsPreview } from './nbparams-samples';
import { browseSample, sourceConnection, SAMPLE_ACCOUNTS } from './source-samples';
import ZohoConnectWizard from '../components/ZohoConnectWizard.vue';
import AccountConnectWizard from '../components/AccountConnectWizard.vue';
import { accountWizardSample } from './account-samples';
import { tb } from '../i18n/backend';
import { connStringPreview, driveConnStringPreview, installConnStringPreview } from './connstring-samples';

const params = new URLSearchParams(location.search);
// &theme=light previews the light theme.
applyThemeClass(params.get('theme') === 'light' ? 'light' : 'dark');
const view = params.get('view') ?? 'plan';

// view=script|import|run: dialogs that talk to the backend, over a mocked IPC.
// view=import&step=2[&target=new] jumps to the target step once the preview has loaded.
// &run=1 starts the job (script / run views): the mock keeps it running,
// except run_script_file, which ends with sample errors.
const renameDialog = ref<{ name: string } | null>(null);
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

// view=designer[&engine=mongo|pgvector|oracle][&new=1][&section=indexes|foreign_keys|options|script]
function designer() {
  const engine = params.get('engine');
  if (engine === 'pgvector' || engine === 'oracle') {
    // A table with a vector index (pgvector: HNSW / IVFFlat; Oracle: its own parameters and INCLUDE columns).
    // `window.__pvDesigner.table`: the TableSchema it builds (what table_ddl would get).
    // &vtype: the embedding's type (DuckDB's FLOAT[3], ClickHouse's Array(Float32)…); &alter=1: edit mode.
    const vtype = params.get('vtype');
    const initial = vtype
      ? { ...sampleDesignerInitialPgvector, columns: sampleDesignerInitialPgvector.columns.map((c) => (c.name === 'embedding' ? { ...c, data_type: vtype } : c)) }
      : sampleDesignerInitialPgvector;
    return h(TableDesignerView, {
      ref: (el: unknown) => { (window as unknown as Record<string, unknown>).__pvDesigner = el; },
      connectionId: 'x', database: 'docs', spec: engine === 'oracle' ? sampleDesignerOracleVector : sampleDesignerPgvector,
      schemas: ['public'], existingTables: [],
      initial: params.get('new') ? undefined : initial, alter: !!params.get('alter'),
      language: 'sql', dialect: engine === 'oracle' ? 'oracle' : 'postgres',
      initialSection: (params.get('section') as DesignerSection | null) ?? 'indexes',
    });
  }
  const mongo = engine === 'mongo';
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
// &set=vectors: documents with a vector field (4 numbers, the driver's
// mark); `#pv-edits` shows the edits the tree let through.
function docsEditView() {
  const vectors = params.get('set') === 'vectors';
  const docs = vectors ? sampleVectorDocuments : sampleDocuments;
  const tree = h(JsonTreeView, {
    columns: docs.columns, rows: docs.rows, edits: pvDocEdits, deleted: pvDocDeleted.value,
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
  if (!vectors) return tree;
  return h('div', { style: 'flex: 1; min-height: 0; display: flex; flex-direction: column; background: var(--ide-editor)' }, [
    h('div', { style: 'flex: 1; min-height: 0; display: flex; flex-direction: column' }, [tree]),
    h('pre', { id: 'pv-edits', style: 'height: 18vh; margin: 0; overflow: auto; padding: 6px 10px; font-size: 11px; border-top: 1px solid var(--nm-border); color: var(--nm-text)' },
      JSON.stringify(pvDocEdits)),
  ]);
}

// view=adddoc&set=vectors: "Agregar documento" over the vector documents;
// `window.__pvAdded`: what it let through.
function addDocView() {
  return h(AddDocumentDialog, {
    open: true, documents: true, fields: sampleVectorDocuments.columns.map((c) => c.name), columns: sampleVectorDocuments.columns,
    onAdd: (docs: unknown[]) => { (window as unknown as Record<string, unknown>).__pvAdded = docs; console.log('add', JSON.stringify(docs)); },
    onClose: () => console.log('close'),
  });
}

// view=datagen&set=vectors: "Generar datos de prueba…" of a table with a
// vector column, over a fake sample. &table=redis: a Redis search index,
// whose `VECTOR(3, FLOAT32)` field an older backend didn't read as a vector
// (no `vector` with the column, "auto" picks decimals), answered as
// datagen_preview does for what it's sent. `window.__dgCalls`: the columns each preview was sent;
// `window.__dgRuns`: what "Generar" sent (datagen_run, answered at once).
function dataGenView() {
  let cb = 0;
  const sent: unknown[] = [];
  const runs: unknown[] = [];
  (window as unknown as Record<string, unknown>).__dgCalls = sent;
  (window as unknown as Record<string, unknown>).__dgRuns = runs;
  (window as unknown as Record<string, unknown>).__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} };
  const redis = new URLSearchParams(location.search).get('table') === 'redis';
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => ++cb, unregisterCallback: () => {}, convertFileSrc: (p: string) => p,
    invoke: async (cmd: string, a?: { args?: { rows?: number; columns?: { name: string; generator: string; params: Record<string, string> }[] } }) => {
      if (cmd === 'plugin:event|listen' || cmd === 'plugin:event|unlisten') return 0;
      if (cmd === 'datagen_run') {
        runs.push(a?.args ?? null);
        return { rows: a?.args?.rows ?? 0 };
      }
      if (cmd !== 'datagen_preview') throw { kind: 'preview', message: `"${cmd}" no está disponible en la vista previa` };
      sent.push(a?.args?.columns ?? null);
      if (redis) {
        const c = (name: string, data_type: string, auto: string) => ({ name, data_type, nullable: name !== 'id', primary_key: name === 'id', auto_increment: false, foreign_key: null, auto });
        const asked = a?.args?.columns?.find((x) => x.name === 'embedding');
        const dims = Number(asked?.params.dims ?? 8);
        const value = (i: number) => (asked?.generator === 'vector'
          ? Array.from({ length: dims }, (_, k) => Math.round(Math.cos(i * 5 + k) * 1000) / 1000)
          : Math.round(Math.abs(Math.sin(i)) * 1e6) / 100);
        return {
          table_columns: [c('id', 'key', 'text'), c('title', 'TEXT', 'text'), c('embedding', 'VECTOR(3, FLOAT32)', 'decimal')],
          generators: ['skip', 'null', 'fixed', 'text', 'decimal', 'list'],
          columns: ['id', 'title', 'embedding'],
          rows: Array.from({ length: 8 }, (_, i) => [`k${i + 1}`, `palabra ${i + 1}`, value(i)]),
        };
      }
      const col = (name: string, data_type: string, auto: string) => ({ name, data_type, nullable: true, primary_key: name === 'id', auto_increment: name === 'id', foreign_key: null, auto });
      const { rows } = sampleVectorResult;
      // `resumen`: a vector field whose type doesn't say so (a document's
      // `array`); the backend's "auto" says it's filled with vectors.
      const small = (i: number) => JSON.stringify(Array.from({ length: 8 }, (_, k) => Math.round(Math.sin(i * 7 + k) * 1000) / 1000));
      return {
        table_columns: [col('id', 'int8', 'sequence'), col('titulo', 'text', 'text'), col('embedding', 'vector(1536)', 'vector'), col('resumen', 'array', 'vector')],
        generators: ['fixed', 'sequence', 'text', 'vector'],
        columns: ['id', 'titulo', 'embedding', 'resumen'],
        rows: rows.slice(0, 8).map((r, i) => [r[0], r[1], r[2], small(i)]),
      };
    },
  };
  const table = redis ? { kind: 'index', schema: null, name: 'idx:docs' } : { kind: 'table', schema: 'public', name: 'documentos' };
  return h(GenerateDataDialog, { connectionId: 'x', database: redis ? 'db0' : 'docs', table, onClose: () => console.log('close') });
}

// view=fkjump: a table's data with foreign keys from and to it ("Ir a la
// fila referenciada", "Filas que la referencian ›"); a jump is logged.
function fkJumpView() {
  const columns = [{ name: 'id', type_name: 'int' }, { name: 'cliente_id', type_name: 'int' }, { name: 'region', type_name: 'nvarchar' }, { name: 'sucursal', type_name: 'int' }, { name: 'total', type_name: 'decimal' }];
  const rows = [[1001, 17, 'sur', 3, 1520.5], [1002, null, 'norte', 1, 88], [1003, 4, 'sur', 1, 310]];
  const t = (name: string) => ({ kind: 'table', schema: 'dbo', name });
  const links = {
    parents: [
      { table: t('clientes'), cols: [1], otherColumns: ['id'], name: 'FK_pedidos_clientes' },
      { table: t('sucursales'), cols: [2, 3], otherColumns: ['region', 'num'], name: 'FK_pedidos_sucursales' },
    ],
    children: [
      { table: t('lineas'), cols: [0], otherColumns: ['pedido_id'], name: 'FK_lineas_pedidos' },
      { table: t('envios'), cols: [0], otherColumns: ['pedido_id'], name: 'FK_envios_pedidos' },
    ],
  };
  return h('div', { style: 'width: 760px; height: 300px; display: flex; flex-direction: column; background: var(--ide-editor)' }, [
    h(ResultGrid, { columns, rows, links, onFollow: (l: { table: { name: string } }, r: number) => console.log('follow', l.table.name, r) }),
  ]);
}

// view=results&set=vectors[&edit=1]: results with vector columns. With
// &edit=1 the grid alone, editable (a vector edit is checked: a JSON array
// of numbers with the column's dimensions), its edits applied here, and
// under it the changes and new rows as update_script gets them (vectors as
// arrays of numbers, never in the WHERE).
const pvVecEdits = reactive<Record<number, Record<number, Cell>>>({});
const pvVecAdded = reactive<Record<number, Cell>[]>([]);
function vectorsView() {
  const { columns, rows } = sampleVectorResult;
  if (!params.get('edit')) {
    return h(ResultsPane, {
      running: false, title: 'documentos', dialect: 'postgres',
      outcome: { results: [{ columns, rows, total_rows: rows.length, truncated: false, rows_affected: null }], messages: [], error: null, elapsed_ms: 21, plans: [] },
    });
  }
  const setup = { target: { kind: 'table', schema: 'public', name: 'documentos' }, keyColumns: ['id'], all: false, note: null, identity: [] };
  const all = [...rows, ...pvVecAdded.map(() => columns.map(() => null as Cell))];
  const edits: Record<number, Record<number, Cell>> = { ...pvVecEdits };
  pvVecAdded.forEach((cells, k) => { edits[rows.length + k] = cells; });
  let changes = '';
  try {
    changes = JSON.stringify({ changes: buildChanges(columns, rows, pvVecEdits, setup, 'postgres'), inserts: buildInserts(columns, pvVecAdded) }, null, 1);
  } catch (e) { changes = String(e); }
  return h('div', { style: 'flex: 1; min-height: 0; display: flex; flex-direction: column; background: var(--ide-editor)' }, [
    h('div', { style: 'flex: 1; min-height: 0; display: flex; flex-direction: column' }, [h(ResultGrid, {
      columns, rows: all, editable: true, insertable: true, added: pvVecAdded.length, edits,
      // "Copiar como SQL INSERTs / UPDATEs": a stand-in for the driver's script
      // (`window.__pvNoScript`: the driver can't, the generic SQL goes).
      copyContext: {
        table: { schema: 'public', name: 'documentos' }, dialect: 'postgres', keyColumns: ['id'],
        // `__pvScriptDelay`: the round trip takes that long; `__pvScriptThrow`: the driver fails.
        script: async (format: string, cols: typeof columns, rs: Cell[][]) => {
          const w = window as unknown as Record<string, unknown>;
          if (w.__pvScriptDelay) await new Promise((r) => setTimeout(r, Number(w.__pvScriptDelay)));
          w.__pvScriptDone = Date.now();
          if (w.__pvScriptThrow) throw new Error('driver failed');
          return w.__pvNoScript ? null : `-- ${format} (driver)\n${JSON.stringify(rowsForScripts(cols, rs))}`;
        },
      },
      onAdd: () => pvVecAdded.push({}),
      onEdit: (r: number, c: number, v: Cell | undefined) => {
        const set = r >= rows.length ? pvVecAdded[r - rows.length] : (pvVecEdits[r] ??= {});
        if (v === undefined) delete set[c]; else set[c] = v;
        if (r < rows.length && !Object.keys(set).length) delete pvVecEdits[r];
        console.log('edit', r, c, JSON.stringify(v));
      },
    })]),
    h('pre', { id: 'pv-changes', style: 'height: 30vh; margin: 0; overflow: auto; padding: 6px 10px; font-size: 11px; border-top: 1px solid var(--nm-border); color: var(--nm-text)' }, changes),
  ]);
}

// view=vecviewer[&col=embedding|cuantizado|terminos]: a vector in the cell viewer.
function vecViewerView() {
  const { columns, rows } = sampleVectorResult;
  const c = Math.max(0, columns.findIndex((x) => x.name === (params.get('col') ?? 'embedding')));
  return h(CellViewer, { title: columns[c].name, value: rows[1][c], vector: columns[c].vector ?? null, onClose: () => console.log('close') });
}

// view=filters: the column filter row, filtering the sample locally, and
// the header sort the way a table's data has it (the parent sorts).
const pvFilters = ref<Record<string, FilterState>>({});
const pvSort = ref<SortKey[]>([]);
function filtersView() {
  const list = Object.values(pvFilters.value).flatMap((f) => f.filters);
  const names = sampleResult.columns.map((c) => c.name);
  const cols = [...sampleResult.columns, { name: 'activo', type_name: 'bit' }, { name: 'alta', type_name: 'date' }];
  const rows = sampleResult.rows.map((r, i) => [...r, i % 3 === 0 ? null : i % 2 === 0, `2024-0${(i % 9) + 1}-1${i % 10}`]);
  return h(ResultsPane, {
    running: false, title: 'Ventas', dialect: 'mssql', filterable: true, filters: pvFilters.value, sort: pvSort.value,
    outcome: { results: [{ columns: cols, rows: sortRows(rows.filter((r) => matchesLocal(r, [...names, 'activo', 'alta'], list)), cols, pvSort.value), total_rows: rows.length, truncated: false, rows_affected: null }], messages: [], error: null, elapsed_ms: 5, plans: [] },
    onSort: (keys: SortKey[]) => {
      pvSort.value = keys;
      console.log('sort', JSON.stringify(keys));
    },
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
if (view === 'nbperm') installNbPermMock();
if (view === 'schedule') installScheduleMock(params);
if (view === 'schedtask') installScheduledTaskMock();
if (view === 'jobs') installJobsMock();
if (view.startsWith('scheduler')) installSchedulerMock();
if (view === 'nbcell' && params.get('set') === 'whole') installWholeMock(params);
if (view === 'feedback') {
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => 0,
    invoke: async (cmd: string) => (cmd === 'feedback_system_info' ? { app_version: '0.1.10', os: 'macOS 15.1', arch: 'aarch64' } : null),
  };
}

// view=fonts[&lang=ja|zh|ko|ru]: CJK and Cyrillic samples in the UI and mono
// fonts, with the font each one resolves to (`<html lang>` picks the glyph
// variant: 直 骨 角 differ between Japanese and Chinese).
function fontsPreview() {
  const samples = [
    ['ja', '日本語: 直 骨 角 誤 — 接続をテスト、クエリを実行'],
    ['zh', '中文: 直 骨 角 误 — 测试连接，运行查询'],
    ['ko', '한국어: 연결 테스트, 쿼리 실행'],
    ['ru', 'Русский: Проверить подключение, выполнить запрос'],
  ];
  const row = (font: string, text: string) => h('div', { style: `font-family: var(${font}); font-size: 22px; margin: 6px 0; color: var(--nm-text-strong)` }, text);
  return h('div', { style: 'padding: 20px; background: var(--ide-editor); height: 100vh; box-sizing: border-box' }, [
    h('div', { style: 'color: var(--nm-text-dim); margin-bottom: 10px' }, `<html lang="${document.documentElement.lang}">  ·  UI: ${i18next.t('common:save')} / ${i18next.t('common:cancel')}`),
    h('div', { style: 'color: var(--nm-text-dim)' }, '--nm-font'),
    ...samples.map(([, s]) => row('--nm-font', s)),
    h('div', { style: 'color: var(--nm-text-dim); margin-top: 14px' }, '--nm-mono'),
    ...samples.map(([, s]) => row('--nm-mono', `SELECT 1 -- ${s}`)),
  ]);
}

const app = createApp({
  render: () =>
    view === 'fonts' ? fontsPreview() :
    view === 'projects' || view === 'vecwork' ? h(App) : view === 'split' ? splitPreview() : view === 'timeline' ? h(TimelinePreview) : h('div', { style: 'height: 100vh; display: flex; flex-direction: column;' }, [
      view === 'tabs' ? h('div', { style: 'width: 1100px; background: var(--ide-editor)' }, [h(EditorTabs)]) :
      view === 'keys' ? h('div', { style: 'width: 340px; height: 640px; background: var(--ide-sidebar)' }, [h(ExplorerSidebar)]) :
      view === 'connection' ? connectionTab() : view === 'monitor'
        ? h(MonitorView, { tab: { id: 'm', kind: 'monitor', connectionId: 'c1', database: '', preview: false }, active: true }) :
      // view=query&w=<px>: the query editor's toolbar and the status bar at a given width.
      view === 'query' ? h('div', { style: `width: ${params.get('w') ?? 1300}px; height: ${params.get('h') ?? 260}px; display: flex; flex-direction: column; background: var(--ide-editor)` }, [
        h('div', { style: 'flex: 1; min-height: 0; display: flex; flex-direction: column' }, [
          h(QueryView, { tab: (useTabsStore().tabs.find((x) => x.id === 'q') ?? { id: 'q', kind: 'query', connectionId: 'c1', database: 'tenant-ventas', queryId: 'q1', preview: false, continueOnError: true }) as never }),
        ]),
        h(StatusBar, { panelOpen: false }),
      ]) :
      view === 'map' ? mapPreview(params) :
      view === 'connstring' ? connStringPreview() :
      view === 'support' ? h(SupportReminder) :
      view === 'feedback' ? h(FeedbackDialog, { modelValue: true }) :
      view === 'multidb' ? multiDbView() :
      view === 'rename' ? h(RenameDialog, {
        ref: renameDialog, onClose: () => console.log('close'),
        target: { connectionId: 'c1', database: 'ventas', target: { what: 'object', object: { kind: 'table', schema: 'dbo', name: 'Clientes' }, parent: null } },
      }) :
      view === 'dependencies' ? h(DependenciesView, { tab: { id: 'dep', kind: 'dependencies', connectionId: 'c1', database: 'ventas', object: { kind: 'table', schema: 'dbo', name: 'Clientes' }, column: 'Pepe', preview: false } }) :
      view === 'compare' ? h(CompareView, { tab: { id: 'cmp', kind: 'compare', connectionId: 'c1', database: 'ventas', preview: false } }) :
      view === 'datacompare' ? h(DataCompareView, { tab: {
        id: 'dc', kind: 'dataCompare', connectionId: 'c1', database: 'ventas', object: null, preview: false,
        picks: { left: { connectionId: 'c1', database: 'ventas', table: '' }, right: { connectionId: 'c1', database: 'ventas', table: '' }, mode: 'queries',
          queries: { left: { connectionId: 'c1', database: 'ventas', query: "select * from sise.simba_di\nwhere fec_emi >= '20260101'" }, right: { connectionId: 'c1', database: 'ventas', query: "select * from sise.simba_di\nwhere fec_emi >= '20260101'" } } },
      } }) :
      view === 'profiler' ? h(ProfilerView, { tab: { id: 'p', kind: 'profiler', connectionId: 'c1', database: 'ventas', preview: false } }) :
      view === 'zoho-wizard' ? zohoWizardPreview() :
      view === 'account-wizard' ? accountWizardPreview() :
      view === 'settings' ? h(SettingsDialog) : view.startsWith('diagram') ? diagram() : ['script', 'import', 'run'].includes(view) ? dialogView() : view === 'designer' ? designer() : view === 'chart'
        ? chartPreview(params)
        : view === 'nbcell' ? notebookCellPreview(params)
        : view === 'nbxresults' ? notebookResultsPreview(params)
        : view === 'nbwrite' ? notebookWritePreview(params)
        : view === 'nbperm' ? nbPermPreview(params)
        : view === 'schedule' ? schedulePreview(params)
        : view === 'schedtask' ? scheduledTaskPreview()
        : view === 'jobs' ? jobsPreview()
        : view === 'scheduler' ? schedulerPreview()
        : view === 'scheduler-tree' ? schedulerTreePreview()
        : view === 'scheduler-log' ? schedulerLogPreview()
        : view === 'scheduler-jobtab' ? schedulerJobTabPreview()
        : view === 'nbparams' ? notebookParamsPreview(params)
        : view === 'filters' ? filtersView()
        : view === 'fkjump' ? fkJumpView()
        : view === 'docs-edit' ? docsEditView()
        : view === 'adddoc' ? addDocView()
        : view === 'datagen' ? dataGenView()
        : view === 'docs'
          ? h(ResultsPane, {
            running: false, title: 'pedidos',
            outcome: { results: [{ ...sampleDocuments, total_rows: sampleDocuments.rows.length, truncated: false, rows_affected: null }], messages: [], error: null, elapsed_ms: 12, plans: [] },
          })
        : view === 'vecviewer' ? vecViewerView()
        : view === 'results' && params.get('set') === 'vectors' ? vectorsView()
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
if (view === 'connstring') {
  installConnStringPreview(params);
  driveConnStringPreview(params);
}
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
if (view === 'connection' || view === 'monitor' || view === 'profiler' || view === 'compare' || view === 'datacompare' || view === 'dependencies' || view === 'query') {
  const conns = useConnectionsStore();
  // Compare reads index usage and checks dependents before a drop (both faked in compare-samples).
  conns.drivers = view === 'compare' ? sampleDrivers.map((d) => ({ ...d, supports_index_usage: true, supports_dependencies: true }))
    : view === 'query' ? sampleDrivers.map((d) => ({ ...d, supports_manual_transactions: true }))
      // The monitor's "Procesos" with ending and cancelling.
      : view === 'monitor' ? sampleDrivers.map((d) => ({ ...d, capabilities: { ...d.capabilities, processes: true, kill_session: true, cancel_query: true } }))
        : sampleDrivers;
  conns.list = [{
    id: 'c1', name: 'Producción · ventas', color: '#3794ff', folder_id: null, tags: ['prod'], save_password: true, updated_at: '',
    config: { driver: 'postgres', host: 'db-prod-01', port: 5432, database: 'ventas', username: 'app', password: null,
      encrypt: true, trust_server_certificate: false, read_only: false, options: {} },
  }];
  if (view === 'query') {
    // A second connection for the status bar's connection picker.
    conns.list.push({ ...conns.list[0], id: 'c2', name: 'Desarrollo · ventas', tags: [], config: { ...conns.list[0].config, host: 'db-dev-01', database: 'ventas_dev' } });
    const tabs = useTabsStore();
    tabs.tabs = [{ id: 'q', kind: 'query', connectionId: 'c1', database: 'tenant-ventas', queryId: 'q1', preview: false, continueOnError: true }];
    tabs.activeId = 'q';
  }
  conns.live.c1 = { status: 'connected', serverVersion: 'PostgreSQL 16.4', databases: view === 'compare' ? ['ventas', 'ventas_qa'] : view === 'query' ? ['tenant-ventas', 'tenant-compras'] : ['ventas'], defaultDatabase: 'ventas', error: null };
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => 0,
    invoke: async (cmd: string, a?: { args?: Record<string, unknown> }) => {
      if (cmd === 'monitor_snapshot') return sampleSnapshot();
      // view=monitor: 30 sessions, so the list scrolls (the selected one's panel stays in view).
      if (cmd === 'monitor_processes') {
        return Array.from({ length: 30 }, (_, i) => ({
          id: String(4100 + i), status: i % 3 ? 'idle' : 'active', active: i % 3 === 0, system: false, own: i === 7,
          user: ['app', 'etl', 'reporting'][i % 3], host: `10.0.4.${20 + i}`, program: i % 2 ? 'psql' : 'ventas-api', database: 'ventas',
          command: i % 3 ? null : 'SELECT', elapsed_ms: i % 3 ? null : 1500 * (i + 1), cpu_ms: null, reads: null, writes: null,
          wait: i === 3 ? 'Lock: transactionid' : null, blocked_by: i === 3 ? '4100' : null,
          sql: `SELECT c.id, c.nombre, SUM(v.total) AS total\nFROM ventas v\nJOIN clientes c ON c.id = v.cliente_id\nWHERE v.fecha >= now() - interval '${i + 1} days'\nGROUP BY c.id, c.nombre\nORDER BY total DESC\nLIMIT 50;`,
        }));
      }
      if (cmd === 'profiler_start') return sampleProfilerStart(params.get('mode') === 'sampled' ? 'sampled' : 'complete');
      if (cmd === 'profiler_poll') return sampleProfilerPoll();
      if (cmd === 'profiler_stop') return null;
      if (cmd === 'get_dependents' && view !== 'compare') return sampleDependents;
      // …&testfail=1: the test times out, with its classification ("Ver qué pasa…").
      if (cmd === 'test_connection') {
        return params.get('testfail')
          ? { ok: false, message: 'no se pudo conectar a db.example.com:5432: se agotó el tiempo de espera (15 s)', detail: { kind: 'timeout', code: null, client_ip: null } }
          : { ok: true, message: 'PostgreSQL 16.4 · 38 ms' };
      }
      if (cmd === 'browse_secret_source') return browseSample(a?.args as { source: { kind: string }; item?: string; path?: string });
      if (cmd === 'test_secret_source') return { chars: 24, username: false };
      if (cmd === 'list_secret_accounts') return SAMPLE_ACCOUNTS;
      if (view === 'query') {
        if (cmd === 'save_query') return (a?.args as { query: unknown }).query;
        if (cmd === 'lint_script') return [];
        // …&tx=open: Manual leaves a transaction open.
        if (cmd === 'set_tab_autocommit' || cmd === 'tab_transaction_state') return params.get('tx') === 'open' && !(a?.args as { autocommit?: boolean }).autocommit ? 'open' : null;
        if (cmd === 'get_query') {
          const sample = params.get('sample');
          const sql = sample === 'editor' ? EDITOR_SAMPLE_SQL : sample === 'q' ? Q_SAMPLE : sample === 'dax' ? DAX_SAMPLE : sample === 'named' ? NAMED_SAMPLE_SQL : 'select top 10 * from ventas.clientes';
          return { id: 'q1', name: 'Query', sql, connection_id: 'c1', database: null, folder: null, updated_at: '' };
        }
        if (params.get('sample') === 'editor') return installEditorSample.invoke(cmd, a?.args);
        if (params.get('sample') === 'named') return installNamedSample.invoke(cmd, a?.args);
        if (params.get('sample') === 'q') return installQSample.invoke(cmd, a?.args);
        if (params.get('sample') === 'dax') return installDaxSample.invoke(cmd, a?.args);
        return null;
      }
      // view=datacompare: 40 columns on each side, so the column table is taller than the view.
      if (view === 'datacompare') {
        if (cmd === 'plugin:event|listen') return 1;
        if (cmd === 'plugin:event|unlisten') return null;
        if (cmd === 'data_compare_query_columns') {
          const columns = Array.from({ length: 40 }, (_, i) => ({ name: i < 4 ? ['COD_PAI', 'COD_MON', 'CAM_EMI', 'NUM_SEC'][i] : `COL_${String(i).padStart(2, '0')}`, type_name: 'varchar' }));
          return { columns, sample: columns.map(() => 'x') };
        }
      }
      const cmp = compareMock(cmd, a);
      if (cmp !== undefined) return cmp;
      throw { kind: 'preview', message: `"${cmd}" no está disponible en la vista previa` };
    },
  };
  if (view === 'profiler') profilerAutostart.add('p');
  if (view === 'datacompare') (window as unknown as Record<string, unknown>).__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} };
  // view=query&sample=editor: objects and columns for ⌘-click, unknown names, parameters and snippets.
  if (view === 'query' && params.get('sample') === 'editor') installEditorSample();
  // view=query&sample=q: a KDB+-like connection, for q's highlighting and completion.
  if (view === 'query' && params.get('sample') === 'q') installQSample();
  // view=query&sample=dax: a Power BI-like connection, for DAX's highlighting and completion.
  if (view === 'query' && params.get('sample') === 'dax') installDaxSample();
  // view=query&sample=named: commented statements, their lenses and named result tabs.
  if (view === 'query' && params.get('sample') === 'named') {
    installNamedSample();
    const click = (el: Element | null | undefined) => (el as HTMLElement | null)?.click();
    if (params.get('exec')) setTimeout(() => click(document.querySelector('.qv-run-wrap button')), 900);
    if (params.get('again')) {
      setTimeout(() => {
        const lens = [...document.querySelectorAll('.cm-run-lens')].find((l) => l.querySelector('.cm-run-lens-name')?.textContent === 'Pedidos del mes');
        click(lens?.querySelector('.cm-run-lens-action'));
      }, 2200);
    }
    const show = params.get('show');
    if (show) setTimeout(() => click([...document.querySelectorAll('.qv-rt-main')].find((b) => b.textContent?.includes(show))), 3200);
    if (params.get('rename')) setTimeout(() => document.querySelector('.qv-rt-main')?.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })), 3400);
  }
  if (view === 'connection') {
    const ui = useUiStore();
    const engine = params.get('engine');
    // &source=keepass|vault_db|bitwarden|…: a saved connection whose password
    // (and, &tab=ssh, the tunnel's password) comes from that source;
    // &browse=1 opens "Examinar".
    const source = params.get('source');
    if (source) {
      conns.list.push(sourceConnection(source, engine ?? 'postgres'));
      ui.editConnection('c-src');
    } else {
      ui.newConnection(null);
      if (engine) setTimeout(() => document.querySelector<HTMLButtonElement>(`[data-engine="${engine}"]`)?.click(), 200);
    }
    const tab = params.get('tab');
    if (tab) setTimeout(() => [...document.querySelectorAll<HTMLButtonElement>('.cd-tab')].find((b) => b.textContent?.trim().toLowerCase().startsWith(tab))?.click(), 400);
    if (params.get('browse')) setTimeout(() => document.querySelector<HTMLButtonElement>('.ss-browse:not(.is-disabled)')?.click(), 700);
  }
}
/** "Conectar Zoho Vault" at one step, with what that step would show. */
function zohoWizardPreview() {
  const step = Number(params.get('step') ?? 1) as 1 | 2 | 3 | 4;
  const filled = params.get('filled') === '1' || step > 2;
  const err = params.get('error');
  const preview = {
    ...(filled ? { client_id: '1000.XMDEG1KBDO9SE2IFTMDFSRPYYJ5L0U', client_secret: 'dummy-client-secret', code: '1000.6c1d4e0b9a7e1d2f.3c4b5a69' } : {}),
    ...(step >= 3 && err !== 'expired' ? { pending: 'p', data_center: 'eu', identity: 'ana@example.com', label: 'Zoho Vault · ana@example.com' } : {}),
    ...(step === 4 && filled ? { passphrase: 'dummy-passphrase' } : {}),
    result: params.get('result') === 'ok' ? 'ok' as const : undefined,
    error: err === 'expired' ? tb('el código de Zoho venció o ya se usó: generá un código nuevo en la pestaña «Generate Code»')
      : err === 'mismatch' ? tb('la frase no coincide con la frase de contraseña maestra de esta cuenta de Zoho Vault') : undefined,
  };
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = { transformCallback: () => 0, invoke: async () => null };
  return h(ZohoConnectWizard, { initialStep: step, preview });
}

/** "Conectar <gestor>" at one step, filled as the user would have it. */
function accountWizardPreview() {
  const { kind, step, preview } = accountWizardSample(params);
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = { transformCallback: () => 0, invoke: async () => null };
  return h(AccountConnectWizard, { kind, initialStep: step, preview });
}

function connectionTab() {
  const t = useTabsStore().tabs.find((x) => x.kind === 'connection');
  return t && t.kind === 'connection' ? h(ConnectionView, { tab: t }) : null;
}
if (view === 'keys') installKeysPreview();
if (view === 'rename') {
  installRenamePreview();
  setTimeout(() => { if (renameDialog.value) renameDialog.value.name = params.get('name') ?? 'ClientesActivos'; }, 300);
}
if (view === 'tabs') installTabsPreview();
if (view === 'split') { installSplitPreview(params); setupSplitConnections(); }
if (view === 'schedule') setupScheduleConnections();
if (view === 'jobs') setupJobs();
if (view.startsWith('scheduler')) setupScheduler(params);
if (view === 'projects') installProjectsPreview();
if (view === 'vecwork') installVectorWorkbench(params);
if (view === 'timeline') installTimelinePreview();
// ?vim=1: the editors in Vim mode. ?bind=view.ai=Mod-b,…: the user's own
// shortcuts. Again after a moment: the settings view loads its mock first.
function previewPrefs() {
  const st = useSettingsStore();
  if (params.get('vim')) st.values['editor.vim'] = true;
  const bind = params.get('bind');
  if (bind) st.values['keys.bindings'] = Object.fromEntries(bind.split(',').map((p) => p.split('=')));
}
previewPrefs();
setTimeout(previewPrefs, 300);
for (const [name, comp] of Object.entries(Icons)) app.component(`Ei${name}`, comp as never);
await i18nReady;
app.mount('#app');
