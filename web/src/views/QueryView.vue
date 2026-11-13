<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { api, errorMessage } from '../api/client';
import { locale, t } from '../i18n';
import { tb } from '../i18n/backend';
import type { QueryMessage, QueryOutcome, QueryProgress, ScriptUnit, TxState, UnsafeDml } from '../api/types';
import CodeEditor from '../components/CodeEditor.vue';
import ContextMenu, { type MenuItem } from '../components/ContextMenu.vue';
import ResultsPane from '../components/ResultsPane.vue';
import PinIcon from '../components/PinIcon.vue';
import ConnectionState from '../components/ConnectionState.vue';
import { useConnectionGate } from '../composables/connectionGate';
import { dbKey, objKey, useConnectionsStore } from '../stores/connections';
import { useOutputStore } from '../stores/output';
import { useSettingsStore } from '../stores/settings';
import { baseName, busyTabs, closeGuards, disconnectGuards, runningStatements, runningTabs, useTabsStore, type FileTab, type QueryTab } from '../stores/tabs';
import { useProjectsStore } from '../stores/projects';
import {
  extFitsDriver, extOf, isScriptFile, languageForExt, useFileDocument, useQueryDocument, type TabDocument,
} from '../composables/tabDocument';
import { useUiStore } from '../stores/ui';
import { registerEditor } from '../stores/ai';
import { useLibraryStore } from '../stores/library';
import { readJson, writeJson } from '../stores/storage';
import { formatCode, formatUnavailable } from '../composables/formatCode';
import MultiDbRunDialog from '../components/MultiDbRunDialog.vue';
import { confirmMultiDb, multiDbOutcome, rememberSelection, runSummary, startMultiDbRun, type MultiDbLive } from '../composables/multiDb';
import { useTasksStore } from '../stores/tasks';
import { lintSourceFor, type LintProblem } from '../composables/lint';
import { nameAt, unknownNames, type NameIndex } from '../composables/nameRefs';
import { useTableHover } from '../composables/tableHoverData';
import {
  fillParams, findParams, guessType, hasNull, paramItems, paramMarkers, type ParamItem, type ParamTarget, type ParamValue,
} from '../composables/queryParams';
import { SNIPPETS_SETTING, snippetsFor, type UserSnippet } from '../composables/snippets';
import QueryParamsDialog from '../components/QueryParamsDialog.vue';
import { queryControls, type QueryControls } from '../composables/queryControls';
import { runWhenOpened } from '../composables/openFiles';
import { useToolbarOverflow } from '../composables/toolbarOverflow';
import { bindingLabel } from '../composables/commands';
import {
  KEPT_RESULTS_SETTING, keptLimit, overflow, runLabel, scriptPreview, type PlanMode, type ResultRun,
} from '../composables/resultRuns';
import {
  KEEP_RESULTS_SETTING, RESULTS_SPACE_SETTING, forget, keepResults, loadRestored, restoredRun, resultsClears, resultsSpace, storeArgs,
} from '../composables/resultsStore';
import {
  LENS_MAX_CHARS, LENS_MAX_STATEMENTS, RUN_LENS_SETTING, commentMarkers, linesAt, numberedStatements, oneStatementEach, splitOutcome,
  statementHead, uniqueNames, type RunLens, type RunUnit,
} from '../composables/resultNames';

// A saved query, or a project's file, open in the editor. A query's text is
// saved as you type (to the state store, under its database in the
// explorer); a file is saved with ⌘S. Running uses the tab's own session,
// so SETs, temp tables and transactions persist between runs.

const props = defineProps<{ tab: QueryTab | FileTab }>();

const conns = useConnectionsStore();
const tabs = useTabsStore();
const ui = useUiStore();
const output = useOutputStore();
const projects = useProjectsStore();

// The document the editor shows: a saved query (autosaved) or a project's
// file (⌘S, composables/tabDocument.ts). A tab never changes kind.
const fileTab = props.tab.kind === 'file' ? props.tab : null;
const doc: TabDocument = props.tab.kind === 'file' ? useFileDocument(props.tab) : useQueryDocument(props.tab);
const { text, saveState, loadError } = doc;
// -- result tabs: each run keeps its results until it's closed (composables/resultRuns.ts) --
const runs = ref<ResultRun[]>([]);
const activeRunId = ref<number | null>(null);
let nextRunId = 1;
const activeRun = computed(() => runs.value.find((r) => r.id === activeRunId.value) ?? null);
/** The results shown: the active tab's. */
const outcome = computed(() => activeRun.value?.outcome ?? null);
/** The run in progress, which live progress fills (`liveRunId`: its tab, for the template). */
let liveRun: ResultRun | null = null;
const liveRunId = ref<number | null>(null);
const running = ref(false);
// A run connects first: the results pane shows "Conectando…", then the run
// ("Ejecutando…") or the connection's error with "Ver qué pasa…" and
// "Reintentar" — never nothing.
const { blocked: connBlocked, ensure: ensureConnection } = useConnectionGate(() => props.tab.connectionId);
const connecting = computed(() => connBlocked.value && conns.live[props.tab.connectionId]?.status === 'connecting');
/** What "Reintentar" runs again: the run that couldn't connect. */
let retryRun: (() => Promise<void>) | null = null;
function retryConnection() {
  if (retryRun) void retryRun();
  else void ensureConnection();
}
const settings = useSettingsStore();
const maxRows = ref(settings.get('query.maxRows', 5000));
watch(maxRows, (v) => { if (v !== settings.get('query.maxRows', 5000)) settings.set('query.maxRows', v); });
// Changed in Configuración or by a sync.
watch(() => settings.values['query.maxRows'], (v) => { if (typeof v === 'number') maxRows.value = v; });

/** A file tab with no base yet (its project has none active). */
const unbound = computed(() => !props.tab.connectionId);
const conn = computed(() => conns.byId(props.tab.connectionId));
const driver = computed(() => conns.driverOf(props.tab.connectionId));
/** The server's databases once connected; before that, just the tab's
 *  (opening the select connects and loads the rest). */
const databases = computed(() => {
  const live = conns.live[props.tab.connectionId]?.databases ?? [];
  return live.length ? live : props.tab.database ? [props.tab.database] : [];
});
const loadingDatabases = ref(false);
/** Opening the database picker connects (if needed) and loads the rest. */
async function loadDatabases() {
  if (unbound.value) return;
  // Already listed: read again, so a database created since shows up.
  if (conns.live[props.tab.connectionId]?.databases?.length) {
    conns.refreshDatabasesSoon(props.tab.connectionId);
    return;
  }
  loadingDatabases.value = true;
  try { await ensureConnection(); } finally { loadingDatabases.value = false; }
}

// -- file tabs: the project, its active base, the file's kind ------------------------------
const project = computed(() => (fileTab ? projects.byId(fileTab.projectId) : undefined));
const active = computed(() => (fileTab ? projects.activeTarget(fileTab.projectId) : null));
/** The environment the tab runs on (only while it follows the project). */
const runEnv = computed(() => (fileTab && !fileTab.pinnedTarget ? active.value?.env ?? null : null));
const ext = computed(() => (fileTab ? extOf(fileTab.path) : ''));
/** Not a script (a README, a YAML…): editable, never run. */
const runnable = computed(() => !fileTab || isScriptFile(fileTab.path));
const extMismatch = computed(() => !!fileTab && runnable.value && !!driver.value && !extFitsDriver(ext.value, driver.value));
const editorLanguage = computed(() => driver.value?.language ?? (fileTab ? languageForExt(ext.value) : undefined));
/** A `.q` file with no engine yet is still q (KDB+'s dialect). */
const editorDialect = computed(() => driver.value?.dialect ?? (fileTab && ext.value === 'q' ? 'q' : ''));
/** The tab picked its own database: back to the project's active base. */
function followProject() {
  if (!fileTab) return;
  fileTab.pinnedTarget = false;
  const target = projects.activeTarget(fileTab.projectId).target;
  tabs.retarget(props.tab.id, target?.connection_id ?? '', target?.database ?? '');
}
function pickProjectBase() {
  if (!fileTab) return;
  const a = projects.activeTarget(fileTab.projectId);
  projects.dialog = { kind: 'target', projectId: fileTab.projectId, alias: a.env?.name ?? a.alias ?? null };
}
const breadcrumbDir = computed(() => (fileTab && fileTab.path.includes('/') ? fileTab.path.slice(0, fileTab.path.lastIndexOf('/') + 1) : ''));

// The tab strip shows a dot while there are changes not saved.
watch(saveState, (v) => {
  if (v === 'saved') delete ui.unsaved[props.tab.id];
  else ui.unsaved[props.tab.id] = v;
});
onBeforeUnmount(() => { delete ui.unsaved[props.tab.id]; });

// The AI assistant reads this tab's editor and adds code to it (it never
// runs anything).
const unregisterAi = registerEditor(props.tab.id, {
  text: () => text.value,
  selection: () => editor.value?.selectionText() ?? '',
  lastError: () => outcome.value?.error ?? null,
  append: (code) => { if (editor.value) editor.value.appendText(code); else text.value = `${text.value.replace(/\s+$/, '')}\n\n${code}\n`; },
  replace: (code) => { if (editor.value) editor.value.replaceAll(code); else text.value = code; },
  rename: (name) => (doc.rename ? doc.rename(name) : Promise.resolve()),
});
onBeforeUnmount(unregisterAi);

async function save() {
  await doc.save();
}

async function changeDatabase(db: string) {
  if (db === props.tab.database || running.value) return;
  // Another database is another session: the open transaction would go.
  if (!(await settleTransaction('database'))) return;
  await doc.changeDatabase(db);
  conns.loadObjects(props.tab.connectionId, db);
  txState.value = null;
}

/** The status bar's connection picker: the tab (and its saved query) moves
 *  to the other connection, on its default database. */
async function changeConnection(connectionId: string) {
  if (connectionId === props.tab.connectionId || running.value) return;
  // Another connection is another session: the open transaction would go.
  if (!(await settleTransaction('connection'))) return;
  const db = conns.byId(connectionId)?.config.database ?? '';
  await doc.changeConnection(connectionId, db);
  txState.value = null;
  if (conns.live[connectionId]?.status === 'connected') conns.loadObjects(connectionId, db);
}

/** A statement switched the database (`USE`): the tab follows it on the same
 *  session, so nothing to settle (the transaction and #temp tables stay). */
async function followDatabase(db: string | null | undefined) {
  if (!db || db === props.tab.database) return;
  await doc.followDatabase(db);
  conns.loadObjects(props.tab.connectionId, db);
}

/** Statements that change the server's list of databases (keyspaces in
 *  CQL, schemas where those are the databases, as in Oracle's users). */
const CHANGES_DATABASES = /\b(?:create|drop|alter|rename)\s+(?:database|schema|keyspace|user)\b/i;

// -- completion schema ------------------------------------------------------------
type DbObject = NonNullable<(typeof conns.objects)[string]>['items'][number];
/** A database's tables and their loaded columns, as completion reads them. */
function namesIn(database: string, objs: DbObject[]): Record<string, string[]> {
  const out: Record<string, string[]> = {};
  for (const o of objs) {
    const kind = driver.value?.object_kinds.find((k) => k.id === o.kind);
    if (!kind?.has_columns) continue;
    const cols = conns.columns[objKey(props.tab.connectionId, database, o.schema, o.name)]?.items.map((c) => c.name) ?? [];
    // q names a namespace's table whole (`.ns.t`), never by its bare name.
    if (!o.schema || driver.value?.dialect !== 'q') out[o.name] = cols;
    if (o.schema) out[`${o.schema}.${o.name}`] = cols;
  }
  return out;
}
const schema = computed(() => namesIn(props.tab.database, conns.objects[dbKey(props.tab.connectionId, props.tab.database)]?.items ?? []));
/** q completes the connection's functions and variables too (they have no
 *  columns, so they aren't in `schema`). */
const otherNames = computed(() => {
  if (driver.value?.dialect !== 'q') return [];
  const objs = conns.objects[dbKey(props.tab.connectionId, props.tab.database)]?.items ?? [];
  return objs
    .filter((o) => !driver.value?.object_kinds.find((k) => k.id === o.kind)?.has_columns)
    .map((o) => ({ label: o.schema ? `${o.schema}.${o.name}` : o.name, kind: o.kind }));
});

/** On engines whose queries read the connection's other databases
 *  (`db.table`), those databases for completion: each one's names once
 *  loaded (on demand, when its name is typed with a dot), null before. */
const otherDatabases = computed(() => {
  if (!driver.value?.cross_database) return undefined;
  const { connectionId } = props.tab;
  const out: Record<string, Record<string, string[]> | null> = {};
  for (const db of conns.live[connectionId]?.databases ?? []) {
    const objs = conns.objects[dbKey(connectionId, db)]?.items;
    out[db] = objs ? namesIn(db, objs) : null;
  }
  return out;
});

// Completion reads objects and columns from the store, which only the explorer
// filled. The tab loads its database's objects itself once the connection is
// open.
watch(() => [conns.live[props.tab.connectionId]?.status, props.tab.database] as const, ([status, db]) => {
  if (status === 'connected') conns.loadObjects(props.tab.connectionId, db);
}, { immediate: true });

/** The editor asked for names and has none: the connection isn't open (a tab
 *  restored at startup, nothing expanded or run yet). Open it, as a run
 *  would; the watch above then loads the names and the list opens. After a
 *  failed connect it doesn't try again ("Reintentar" does). */
function loadNames() {
  if (unbound.value || conns.isConnected(props.tab.connectionId)) return;
  void ensureConnection({ auto: true });
}

// Columns are loaded per table, only for the tables the text names.
let columnsTimer: ReturnType<typeof setTimeout> | null = null;
watch([text, () => conns.objects[dbKey(props.tab.connectionId, props.tab.database)]?.items, otherDatabases], () => {
  if (columnsTimer) clearTimeout(columnsTimer);
  columnsTimer = setTimeout(loadReferencedColumns, 400);
});
onBeforeUnmount(() => { if (columnsTimer) clearTimeout(columnsTimer); });
function loadReferencedColumns() {
  const { connectionId, database } = props.tab;
  const words = new Set(text.value.toLowerCase().match(/[\w$#]+/g) ?? []);
  let started = 0;
  const loadIn = (db: string, objs: DbObject[]) => {
    for (const o of objs) {
      if (started >= 20) break;
      if (!words.has(o.name.toLowerCase()) || conns.columns[objKey(connectionId, db, o.schema, o.name)]) continue;
      if (!driver.value?.object_kinds.find((k) => k.id === o.kind)?.has_columns) continue;
      conns.loadColumns(connectionId, db, o);
      started++;
    }
  };
  loadIn(database, conns.objects[dbKey(connectionId, database)]?.items ?? []);
  // Other databases the text names with a dot (`other_db.orders`): their
  // objects first, then the columns of the tables named.
  const lower = text.value.toLowerCase();
  for (const db of Object.keys(otherDatabases.value ?? {})) {
    if (db === database || !qualifiedRe(db).test(lower)) continue;
    const objs = conns.objects[dbKey(connectionId, db)]?.items;
    if (objs) loadIn(db, objs);
    else void conns.loadObjects(connectionId, db);
  }
}
/** `db.` in lower-case text, bare or quoted (`"db".`, `[db].`, \`db\`.). */
function qualifiedRe(db: string) {
  const name = db.toLowerCase().replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return new RegExp(`(?:^|[^\\w$#])["\\[\x60]?${name}["\\]\x60]?\\s*\\.`);
}

/** The editor reached `table.` or `schema.table.` before the columns were
 *  loaded: load them now, without waiting for the text to settle. */
function loadColumnsFor(path: string[], database = props.tab.database) {
  const { connectionId } = props.tab;
  const [schemaName, name] = path.length > 1 ? path : [null, path[0]];
  const obj = (conns.objects[dbKey(connectionId, database)]?.items ?? [])
    .find((o) => o.name === name && (schemaName === null || o.schema === schemaName));
  if (obj && !conns.columns[objKey(connectionId, database, obj.schema, obj.name)]) conns.loadColumns(connectionId, database, obj);
}

/** ⭐ Save the selection (or the whole query) as a Library script. */
function saveToLibrary() {
  const sel = editor.value?.selectionText() ?? '';
  const lib = useLibraryStore();
  lib.newScript(sel.trim() ? sel : text.value, driver.value ? [driver.value.id] : [], sel.trim() ? '' : doc.title.value);
}

/** UPDATE code from edited result cells: at the end of the query, selected. */
function appendScript(code: string) {
  if (editor.value) editor.value.appendText(code);
  else text.value = `${text.value.replace(/\s+$/, '')}\n\n${code}\n`;
  ElMessage.success(t('query:scriptAppended'));
}

// -- run ----------------------------------------------------------------------------
const editor = ref<InstanceType<typeof CodeEditor> | null>(null);

// -- the editor's right-click menu: what this view adds to the editor's own --------------------
const problemsOpen = ref(false);
function editorMenu(): MenuItem[] {
  const items: MenuItem[] = [];
  if (!unbound.value) {
    const at = editor.value?.cursor() ?? 0;
    const known = !!(nameIndex.value && nameAt(nameIndex.value, text.value, at));
    items.push({ label: t('editor:menu.goToDefinition'), shortcut: `${navigator.platform.toLowerCase().includes('mac') ? '⌘' : 'Ctrl+'}${t('editor:menu.click')}`, disabled: !known, action: () => openNameAt(text.value, at, false) });
    items.push({ label: t('editor:menu.revealInExplorer'), disabled: !known, action: () => openNameAt(text.value, at, true) });
  }
  if (runnable.value && !unbound.value) items.push({ label: t('editor:menu.optimize'), divided: true, action: optimize });
  if (lintSource.value) items.push({ label: t('editor:menu.problems'), action: () => { problemsOpen.value = true; } });
  if (runnable.value && paramTarget.value && paramMarkers(paramTarget.value)) {
    items.push({ label: t('editor:menu.detectParams'), checked: !props.tab.paramsOff, divided: true, action: () => tabs.setQueryOptions(props.tab.id, { paramsOff: !props.tab.paramsOff }) });
  }
  return items;
}

// -- names: ⌘/Ctrl+click, "Ir a la definición", "Mostrar en el explorador", unknown names ----
/** The tab's database as the explorer knows it (its cache first): empty
 *  objects while not loaded, so nothing is resolved nor marked. */
const nameIndex = computed<NameIndex | null>(() => {
  const d = driver.value;
  if (!d || unbound.value) return null;
  const { connectionId, database } = props.tab;
  const k = dbKey(connectionId, database);
  const loaded = conns.objects[k];
  const objects = loaded && (loaded.status === 'ready' || loaded.status === 'stale') ? loaded.items : [];
  return {
    language: d.language, dialect: d.dialect, database, objects, kinds: d.object_kinds,
    schemas: (conns.schemas[k] ?? []).map((s) => s.name),
    columns: (o) => {
      const c = conns.columns[objKey(connectionId, database, o.schema, o.name)];
      return c && (c.status === 'ready' || c.status === 'stale') ? c.items.map((x) => x.name) : null;
    },
  };
});
function linkAt(doc: string, pos: number) {
  const hit = nameIndex.value ? nameAt(nameIndex.value, doc, pos) : null;
  return hit ? { from: hit.from, to: hit.to } : null;
}
/** Open the object named at `pos` (its structure, data or code), or show it
 *  in the explorer. */
function openNameAt(doc: string, pos: number, reveal: boolean) {
  const idx = nameIndex.value;
  const hit = idx ? nameAt(idx, doc, pos) : null;
  if (!hit) {
    ElMessage.info({ message: t(idx?.objects.length ? 'editor:nav.notFound' : 'editor:nav.notLoaded'), duration: 2500 });
    return;
  }
  const { connectionId, database } = props.tab;
  const ref = { kind: hit.object.kind, schema: hit.object.schema, name: hit.object.name };
  if (reveal) ui.revealInExplorer({ connectionId, database, object: ref });
  else tabs.openObject(connectionId, database, ref, hit.view, false);
}

// -- hovering a table's name: its columns (composables/tableHover.ts) ------------------------
const { tableHover } = useTableHover(() => props.tab, () => driver.value, () => !unbound.value);

// -- snippets (Configuración › Snippets adds the user's) -----------------------------------
const snippets = computed(() => snippetsFor(driver.value, settings.get<UserSnippet[]>(SNIPPETS_SETTING, [])));

// -- Calidad de código: marks while typing, and "Ver problemas" ----------------------------
const problems = ref<LintProblem[]>([]);
const lintSource = computed(() => {
  // Rebuilt (and the text checked again) when objects or columns arrive.
  void schema.value;
  const idx = nameIndex.value;
  return lintSourceFor(props.tab.connectionId, (p) => { problems.value = p; }, idx?.objects.length ? (doc) => unknownNames(idx, doc) : null);
});
watch(lintSource, (s) => { if (!s) problems.value = []; });
/** The worst severity found, for the button's color. */
const worstProblem = computed(() => (['error', 'warning', 'info'] as const).find((s) => problems.value.some((p) => p.severity === s)) ?? '');

// -- Formatear (⇧⌥F): the selection, or everything --------------------------------------
const formatting = ref(false);
const formatOff = computed(() => formatUnavailable(driver.value?.language, driver.value?.dialect));
async function formatQuery() {
  if (formatOff.value) {
    ElMessage.info({ message: formatOff.value, duration: 3000 });
    return;
  }
  formatting.value = true;
  try {
    await editor.value?.format((t) => formatCode(t, driver.value?.language, driver.value?.dialect ?? ''));
  } catch (e) {
    ElMessage.warning({ message: t('query:formatFailed', { error: errorMessage(e) }), duration: 4500 });
  } finally {
    formatting.value = false;
  }
}

// -- run options: "Seguir si hay un error" and transactions (per tab) ----------------------
/** The driver runs editor scripts statement by statement (or batch by batch). */
const perStatement = computed(() => (driver.value?.script_mode ?? 'whole') !== 'whole');
const continueOnError = computed({
  get: () => props.tab.continueOnError ?? driver.value?.script_defaults?.continue_on_error ?? false,
  set: (v: boolean) => tabs.setQueryOptions(props.tab.id, { continueOnError: v }),
});
const offersManualTx = computed(() => !!driver.value?.supports_manual_transactions);
const manualTx = computed(() => offersManualTx.value && !!props.tab.manualTx);
/** The tab's transaction, as the last run (or Confirmar / Deshacer) left it. */
const txState = ref<TxState | null>(null);
const txOpen = computed(() => txState.value === 'open' || txState.value === 'failed');
const txBusy = ref(false);

async function setManualTx(on: boolean) {
  if (on === manualTx.value) return;
  // Back to automatic: the driver may commit what's pending; the user decides first.
  if (!on && !(await settleTransaction('mode'))) return;
  const before = props.tab.manualTx;
  tabs.setQueryOptions(props.tab.id, { manualTx: on });
  // Not connected yet: the next run switches the session.
  if (conns.live[props.tab.connectionId]?.status !== 'connected') return;
  try {
    txState.value = await api.setTabAutocommit(props.tab.id, props.tab.connectionId, props.tab.database, !on);
  } catch (e) {
    tabs.setQueryOptions(props.tab.id, { manualTx: before });
    ElMessage.error(errorMessage(e));
  }
}

/** Confirmar / Deshacer. True when it went through. */
async function endTransaction(commit: boolean): Promise<boolean> {
  txBusy.value = true;
  try {
    txState.value = await (commit ? api.commitTab(props.tab.id) : api.rollbackTab(props.tab.id));
    ElMessage.success({ message: commit ? t('query:tx.committed') : t('query:tx.rolledBack'), duration: 1500 });
    return true;
  } catch (e) {
    txError.value = errorMessage(e);
    if (!txAsk.value) ElMessage.error(txError.value);
    return false;
  } finally {
    txBusy.value = false;
  }
}

// The dialog when an open transaction would be lost (closing the tab,
// another database, back to automatic): Confirmar / Deshacer / Cancelar.
type TxReason = 'close' | 'database' | 'connection' | 'mode' | 'disconnect';
const txAsk = ref<{ reason: TxReason; resolve: (ok: boolean) => void } | null>(null);
const txError = ref<string | null>(null);
/** Resolves true when there's no open transaction left (or there was none). */
async function settleTransaction(reason: TxReason): Promise<boolean> {
  if (!txOpen.value && !manualTx.value) return true;
  try { txState.value = await api.tabTransactionState(props.tab.id); } catch { /* keep the last known state */ }
  if (!txOpen.value) return true;
  if (reason === 'close' || reason === 'disconnect') tabs.activate(props.tab.id);
  txError.value = null;
  return new Promise<boolean>((resolve) => { txAsk.value = { reason, resolve }; });
}
async function answerTx(choice: 'commit' | 'rollback' | 'cancel') {
  const ask = txAsk.value;
  if (!ask) return;
  if (choice !== 'cancel' && !(await endTransaction(choice === 'commit'))) return;
  txAsk.value = null;
  ask.resolve(choice !== 'cancel');
}
// Closing the tab asks while a transaction is open, and (file tabs) while
// there are changes not saved: Guardar / No guardar / Cancelar.
async function settleUnsaved(): Promise<boolean> {
  if (!fileTab || (saveState.value !== 'dirty' && saveState.value !== 'error')) return true;
  tabs.activate(props.tab.id);
  try {
    await ElMessageBox.confirm(t('projects:file.closeAsk', { name: baseName(fileTab.path) }), t('projects:file.closeAskTitle'), {
      type: 'warning', confirmButtonText: t('common:save'), cancelButtonText: t('projects:file.dontSave'), distinguishCancelAndClose: true,
    });
  } catch (action) {
    return action === 'cancel';
  }
  return doc.save();
}
const guardClose = async () => (await settleUnsaved()) && (await settleTransaction('close'));
const needsGuard = computed(() => txOpen.value || (!!fileTab && (saveState.value === 'dirty' || saveState.value === 'error')));
watch(needsGuard, (on) => {
  if (on) closeGuards.set(props.tab.id, guardClose);
  else closeGuards.delete(props.tab.id);
}, { immediate: true });
// Disconnecting its connection asks too while a transaction is open.
const guardDisconnect = () => settleTransaction('disconnect');
watch(txOpen, (on) => {
  if (on) disconnectGuards.set(props.tab.id, guardDisconnect);
  else disconnectGuards.delete(props.tab.id);
}, { immediate: true });
watch(running, (on) => {
  if (on) runningTabs.add(props.tab.id); else runningTabs.delete(props.tab.id);
  if (on) runningStatements.set(props.tab.id, { connectionId: props.tab.connectionId, sql: liveRun?.script ?? '', since: Date.now() });
  else runningStatements.delete(props.tab.id);
}, { immediate: true });
// A project's file tabs follow its active base, except while busy here.
watch(() => running.value || txOpen.value, (busy) => {
  if (busy) busyTabs.add(props.tab.id); else busyTabs.delete(props.tab.id);
}, { immediate: true });
onBeforeUnmount(() => {
  busyTabs.delete(props.tab.id);
  runningTabs.delete(props.tab.id);
  runningStatements.delete(props.tab.id);
  disconnectGuards.delete(props.tab.id);
});

/** An environment marked `confirm_run` in .dbine.json asks before each run. */
async function confirmEnvironment(): Promise<boolean> {
  const env = runEnv.value;
  if (!env?.confirm_run) return true;
  try {
    await ElMessageBox.confirm(
      t('projects:file.confirmRun', { env: env.name, where: `${conn.value?.name ?? ''} › ${props.tab.database || t('query:defaultDatabase')}` }),
      t('projects:file.confirmRunTitle'),
      { type: 'warning', confirmButtonText: t('common:run'), cancelButtonText: t('common:cancel'), confirmButtonClass: 'el-button--danger' },
    );
    return true;
  } catch {
    return false;
  }
}
onBeforeUnmount(() => {
  closeGuards.delete(props.tab.id);
  txAsk.value?.resolve(false);
});

// -- UPDATE / DELETE without WHERE: "Ejecutar igual" / "Cancelar" ---------------------------
const unsafeAsk = ref<{ items: { keyword: string; line: number; text: string }[]; resolve: (ok: boolean) => void } | null>(null);
function confirmUnsafe(script: string, found: UnsafeDml[], offset: number | null): Promise<boolean> {
  const items = found.map((u) => ({ keyword: u.keyword, line: u.line + (offset ?? 0), text: script.slice(u.start, u.end).trim() }));
  return new Promise<boolean>((resolve) => { unsafeAsk.value = { items, resolve }; });
}
function answerUnsafe(ok: boolean) {
  unsafeAsk.value?.resolve(ok);
  unsafeAsk.value = null;
}

// -- live progress (query-progress / query-message of this tab's session) --------------------
/** Where the next run's sent text starts in the editor, and its first line
 *  minus one (each result tab keeps its own). */
let runBase = 0;
let lineOffset: number | null = null;
/** Messages streamed since the last statement ended (its progress event repeats them). */
let liveFrom = 0;
function onProgress(p: QueryProgress) {
  const o = liveRun?.outcome;
  if (!running.value || p.session_id !== props.tab.id || !o) return;
  const log = (o.log ??= []);
  const streamed = log.splice(liveFrom);
  const incoming = [...p.log];
  // What was streamed and isn't in the statement's own log stays (a GO N's
  // "Inicio del ciclo de ejecución").
  const kept = streamed.filter((m) => {
    const k = incoming.findIndex((x) => x.level === m.level && x.text === m.text);
    if (k < 0) return true;
    incoming.splice(k, 1);
    return false;
  });
  log.push(...kept, ...p.log);
  liveFrom = log.length;
  o.results.push(...p.results);
  (o.errors ??= []).push(...p.errors);
  if (!o.error && p.errors.length) o.error = p.errors[0].message;
}
function onMessage(p: QueryMessage) {
  const o = liveRun?.outcome;
  if (!running.value || p.session_id !== props.tab.id || !o) return;
  (o.log ??= []).push(p.message);
}
const unlisteners: UnlistenFn[] = [];
let unmounted = false;
onMounted(async () => {
  try {
    const subs = await Promise.all([
      listen<QueryProgress>('query-progress', (e) => onProgress(e.payload)),
      listen<QueryMessage>('query-message', (e) => onMessage(e.payload)),
    ]);
    if (unmounted) subs.forEach((u) => u());
    else unlisteners.push(...subs);
  } catch { /* outside Tauri */ }
  void restoreRuns();
});
onBeforeUnmount(() => {
  unmounted = true;
  unlisteners.forEach((u) => u());
});
// A data file just opened (composables/openFiles.ts): its first query runs
// once its text is in the editor.
if (props.tab.kind === 'query' && runWhenOpened.has(props.tab.queryId)) {
  const queryId = props.tab.queryId;
  const stopAutoRun = watch(text, (v) => {
    if (!v.trim() || !runWhenOpened.delete(queryId)) return;
    queueMicrotask(() => stopAutoRun());
    void run(v);
  }, { immediate: true });
}

// -- elapsed time while running ----------------------------------------------------------------
const startedAt = ref(0);
const now = ref(0);
let clock: ReturnType<typeof setInterval> | null = null;
function startClock() {
  startedAt.value = now.value = Date.now();
  if (clock) clearInterval(clock);
  clock = setInterval(() => { now.value = Date.now(); }, 200);
}
function stopClock() {
  if (clock) clearInterval(clock);
  clock = null;
}
onBeforeUnmount(stopClock);

/** Run `sqlText` (default: the selection, or everything). `from`: where it
 *  starts in the editor, so the lines of messages and errors map to it. */
async function run(sqlText?: string, plan: PlanMode = 'none', from?: number) {
  if (running.value || connecting.value) return;
  // The editor's text as it is now: the comments that name the results.
  const docText = text.value;
  const picked = sqlText !== undefined ? { text: sqlText, from: from ?? 0 } : editor.value?.runnable() ?? { text: text.value, from: 0 };
  const script = picked.text.trim();
  if (!script) return;
  const base = picked.from + (picked.text.length - picked.text.trimStart().length);
  if (!runnable.value) return;
  if (unbound.value) { ElMessage.info({ message: t('projects:file.pickBase'), duration: 3000 }); return; }
  const filled = await withParams(script);
  if (filled === null) return;
  if (!(await confirmEnvironment())) return;
  const naming = await nameRun(docText, base, script, filled, plan);
  // Where the confirmation was given: a retry after the tab moved to another
  // connection or database asks again before running there.
  const confirmedFor = `${props.tab.connectionId}\u0000${props.tab.database}`;
  const go = async () => {
    retryRun = null;
    if (`${props.tab.connectionId}\u0000${props.tab.database}` !== confirmedFor && !(await confirmEnvironment())) return;
    if (!(await ensureConnection())) {
      // The pane shows the error; "Reintentar" connects and runs this again.
      retryRun = go;
      return;
    }
    if (doc.autosave && saveState.value === 'dirty') save();
    runBase = base;
    lineOffset = (editor.value?.lineAt(base) ?? 1) - 1;
    await execute(filled, plan, false, undefined, naming);
  };
  await go();
}

// -- named results: the comment above a statement names its result tab (composables/resultNames.ts) --
/** The engine's line-comment markers. */
const commentMarks = computed(() => commentMarkers(driver.value?.language, driver.value?.dialect));
/** Where a run's results go by name. `whole`: the run is one named
 *  statement, which runs in its name's tab. `units`: the app runs the
 *  script's units one by one (a result's `statement` is its unit's index),
 *  some named; their parts move to their tabs when it ends. `numbered`: the
 *  engine gets the text whole (or a plan runs it whole), so its statements
 *  are the ones its outcome numbers, found when it ends. */
interface RunNaming { whole: string | null; units: RunUnit[] | null; numbered: Numbered | null }
interface Numbered {
  /** The text sent. */
  sent: string;
  /** The statements the outcome numbers (`numberedStatements`) as units, named. */
  resolve: (statements: { statement: number; start: number; end: number }[]) => RunUnit[];
  /** An outcome that numbers nothing: the name the whole run takes when it
   *  was one statement (null: none). */
  single: string | null;
}

/** "Volver a ejecutar" on a script's tab whose named statements went to
 *  their tabs: the same split again, with the names they had (by statement). */
async function rerunNaming(r: ResultRun): Promise<RunNaming | null> {
  const parts = r.parts;
  if (!parts?.some(Boolean)) return null;
  if (!(perStatement.value && r.plan === 'none')) {
    // The engine numbers the same statements the same way.
    const resolve = (statements: { statement: number; start: number; end: number }[]) => {
      const lines = linesAt(r.script, statements.map((s) => s.start));
      return statements.map((s, i): RunUnit => ({
        start: s.start, end: s.end, line: lines[i], text: r.script.slice(s.start, s.end),
        editorStart: r.runBase + s.start, editorLine: (r.lineOffset ?? 0) + lines[i], client: false, name: parts[s.statement] ?? null,
      }));
    };
    return { whole: null, units: null, numbered: { sent: r.script, resolve, single: null } };
  }
  let units: ScriptUnit[];
  try {
    units = await api.splitScript({ connectionId: r.connectionId, sql: r.script });
  } catch {
    return null;
  }
  if (units.length !== parts.length) return null;
  return {
    whole: null,
    numbered: null,
    units: units.map((u, i) => ({
      start: u.start, end: u.end, line: u.line, text: u.text, editorStart: r.runBase + u.start, editorLine: (r.lineOffset ?? 0) + u.line,
      client: u.kind === 'client_command', name: parts[i],
    })),
  };
}

/** The names of a run, read from `doc` (the editor's text): `base` is where
 *  `script` (as written) starts in it, `sent` the text that runs (its
 *  parameters filled). Null: nothing named. */
async function nameRun(doc: string, base: number, script: string, sent: string, plan: PlanMode): Promise<RunNaming | null> {
  const marks = commentMarks.value;
  const dialect = driver.value?.dialect;
  if (!marks.some((m) => doc.includes(m))) return null;
  const split = (sql: string, statements = false) => api.splitScript({ connectionId: props.tab.connectionId, sql, statements });
  // The units the app runs one by one (`statement` indexes into them).
  let units: ScriptUnit[];
  let written: ScriptUnit[];
  try {
    units = await split(sent);
    written = sent === script ? units : await split(script);
  } catch {
    return null;
  }
  // Where an offset of the text sent is in the editor: by the text as
  // written (a filled parameter moves what follows it).
  const same = written.length === units.length;
  const at = (offset: number) => {
    if (!same || sent === script) return base + offset;
    // The last unit that starts at or before it.
    let lo = 0;
    let hi = units.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (units[mid].start <= offset) lo = mid;
      else hi = mid - 1;
    }
    const k = lo;
    const u = units[k];
    const w = written[k];
    return u && offset >= u.start ? base + w.start + Math.min(offset - u.start, w.end - w.start) : base + offset;
  };
  const head = (start: number, end: number) => statementHead(doc, at(start), at(end), marks, dialect);
  // A batch (SQL Server's, Sybase's) may hold several statements: a name
  // is a statement's, so only a unit of one statement takes it.
  let starts: number[] | null = null;
  if (units.some((u) => u.kind === 'batch')) {
    try {
      starts = (await split(sent, true)).filter((u) => u.kind !== 'client_command')
        .map((u) => statementHead(sent, u.start, u.end, marks, dialect).start).filter((s): s is number => s !== null);
    } catch {
      return null;
    }
  }
  const single = (us: RunUnit[]) => (starts ? oneStatementEach(us, starts) : us);
  const lines = linesAt(doc, units.map((u) => at(u.start)));
  const named = single(units.map((u, i): RunUnit => {
    const h = u.kind === 'client_command' ? null : head(u.start, u.end);
    return {
      start: u.start, end: u.end, line: u.line, text: u.text, editorStart: at(u.start), editorLine: lines[i],
      client: h?.start == null, name: h?.name ?? null,
    };
  }));
  const statements = named.filter((u) => !u.client);
  if (perStatement.value) {
    if (statements.length === 1) return statements[0].name ? { whole: statements[0].name, units: null, numbered: null } : null;
    // Several, run one by one by the app.
    if (plan === 'none') return named.some((u) => u.name) ? { whole: null, units: uniqueNames(named), numbered: null } : null;
  }
  // The engine gets the text whole (or its plan does): when the run ends,
  // its outcome says where each statement starts, and the editor names it.
  const resolve = (found: { statement: number; start: number; end: number }[]) => {
    const inSent = linesAt(sent, found.map((s) => s.start));
    const inDoc = linesAt(doc, found.map((s) => at(s.start)));
    return uniqueNames(single(found.map((s, i): RunUnit => ({
      start: s.start, end: s.end, line: inSent[i], text: sent.slice(s.start, s.end),
      editorStart: at(s.start), editorLine: inDoc[i], client: false, name: head(s.start, s.end).name,
    }))));
  };
  return { whole: null, units: null, numbered: { sent, resolve, single: statements.length === 1 ? statements[0].name : null } };
}

/** A script with named statements ended: each one's results, messages and
 *  errors go to its name's tab (refreshed in place, or a new one after the
 *  run's), the rest stays in the run's tab, which closes when nothing is
 *  left (the run's first named tab shows instead). `clears`: the store's
 *  clears when it started (one since: nothing is stored). */
function splitRun(r: ResultRun, o: QueryOutcome, naming: RunNaming, clears: number) {
  // Each statement of the outcome as its unit: by index where the app ran
  // them one by one, or by where the engine's numbering puts them.
  let byStatement: (RunUnit | undefined)[] = [];
  let whole: RunUnit | null = null;
  if (naming.units) byStatement = naming.units;
  else if (naming.numbered) {
    const n = naming.numbered;
    const found = numberedStatements(o, n.sent, commentMarks.value);
    n.resolve(found).forEach((u, i) => { byStatement[found[i].statement] = u; });
    // An engine that numbers nothing: only a run of one statement is named.
    if (!found.length && n.single) {
      whole = {
        start: 0, end: n.sent.length, line: 1, text: n.sent, editorStart: r.runBase, editorLine: (r.lineOffset ?? 0) + 1, client: false, name: n.single,
      };
    }
  }
  // The run's own tab may have been given a statement's name: that one stays in it.
  if (r.name) byStatement = Array.from(byStatement, (u) => (u && u.name === r.name ? { ...u, name: null } : u));
  if (whole?.name === r.name) whole = null;
  const { rest, parts } = whole ? { rest: null, parts: [{ name: whole.name!, unit: 0, outcome: o }] } : splitOutcome(o, byStatement);
  if (whole) byStatement = [whole];
  if (!parts.length) {
    persistRun(r, clears);
    return;
  }
  let at = runs.value.indexOf(r) + 1;
  const tabs: ResultRun[] = [];
  for (const p of parts) {
    const u = byStatement[p.unit]!;
    // The tab holds its statement: "Volver a ejecutar" and exports run only
    // it (a whole run's plan with it; a part's plans stay in the run's tab).
    const fields = {
      outcome: p.outcome, script: u.text, plan: whole ? r.plan : 'none' as PlanMode, connectionId: r.connectionId, database: r.database,
      language: r.language, multi: null, lineOffset: u.editorLine - 1, runBase: u.editorStart, parts: null, label: null, resultSql: null,
    };
    let tab = runs.value.find((x) => x !== r && x.name === p.name);
    if (tab) Object.assign(tab, fields, { at: Date.now(), restored: false });
    else {
      runs.value.splice(at, 0, { ...fields, name: p.name, id: nextRunId++, key: crypto.randomUUID(), at: Date.now(), pinned: false });
      tab = runs.value[at++];
    }
    // Its stored copy is replaced in place (same key): never two of one name.
    persistRun(tab, clears);
    tabs.push(tab);
  }
  if (rest) {
    r.outcome = rest;
    // "Volver a ejecutar" runs the script again and splits it the same way;
    // the tab reads as what stayed in it.
    r.parts = Array.from(byStatement, (u) => u?.name ?? null);
    const first = byStatement.find((u) => u && !u.name && !u.client);
    r.label = first ? runLabel(first.text) : null;
    // Its results are some of the script's: export, editing and the chart
    // read each one's own statement, not the whole script.
    r.resultSql = rest.results.map((x) => (x.statement != null ? byStatement[x.statement]?.text ?? null : null));
    persistRun(r, clears);
  } else {
    const shown = activeRunId.value === r.id;
    dropRun(r);
    if (shown) activeRunId.value = tabs[0].id;
  }
}

// -- parameters (`:name`, `?`, `@name`): asked before every kind of run ------------------------
const paramTarget = computed<ParamTarget | null>(() => (driver.value ? { language: driver.value.language, dialect: driver.value.dialect, driverId: driver.value.id } : null));
const paramAsk = ref<{ items: ParamItem[]; values: Record<string, ParamValue>; resolve: (v: Record<string, ParamValue> | 'off' | null) => void } | null>(null);

/** `script` with its parameters' values, after asking for them; the text as
 *  it is when it has none (or the tab doesn't look for them); null: cancelled. */
async function withParams(script: string): Promise<string | null> {
  const target = paramTarget.value;
  if (!target || props.tab.paramsOff) return script;
  const spots = findParams(script, target);
  if (!spots.length) return script;
  const items = paramItems(spots);
  const last = props.tab.params ?? {};
  const values = Object.fromEntries(items.map((i) => [i.key, last[i.key] ? { ...last[i.key] } : { type: guessType(i.key), value: '' }]));
  const answer = await new Promise<Record<string, ParamValue> | 'off' | null>((resolve) => { paramAsk.value = { items, values, resolve }; });
  paramAsk.value = null;
  if (answer === null) return null;
  if (answer === 'off') {
    tabs.setQueryOptions(props.tab.id, { paramsOff: true });
    return script;
  }
  tabs.setQueryOptions(props.tab.id, { params: { ...last, ...answer } });
  return fillParams(script, spots, answer, target);
}
onBeforeUnmount(() => paramAsk.value?.resolve(null));

/** `into`: "Volver a ejecutar" on a result tab, whose results this run
 *  replaces; otherwise the run opens a new tab, or (`naming`) runs in its
 *  name's tab, or splits into named tabs when it ends. */
async function execute(script: string, plan: PlanMode, confirmedUnsafe: boolean, into?: ResultRun, naming?: RunNaming | null) {
  running.value = true;
  startClock();
  // What a cancel that closes the session would roll back: the transaction
  // open before the run.
  const txWasOpen = txOpen.value;
  // Statements fill it as they end (query-progress); the answer replaces it.
  const blank: QueryOutcome = { results: [], messages: [], error: null, elapsed_ms: 0, plans: [], log: [], errors: [] };
  const shownBefore = activeRunId.value;
  // "Borrar ahora" while it runs: what it splits into named tabs isn't stored.
  const clears = resultsClears();
  // A named statement runs in its name's tab, the one it has or a new one.
  const named = !into && naming?.whole ? naming.whole : null;
  const target = into ?? (named ? runs.value.find((x) => x.name === named) : undefined);
  const again = target && runs.value.includes(target) ? target : null;
  const before = again ? { ...again } : null;
  const fields = {
    script, plan, connectionId: props.tab.connectionId, database: props.tab.database,
    language: driver.value?.language ?? 'sql', multi: null, lineOffset, runBase, parts: null, label: null, resultSql: null,
  };
  let r: ResultRun;
  if (again) {
    r = again;
    r.restored = false;
    r.outcome = blank;
    r.at = Date.now();
    // Whatever split it had is the end's to say again.
    r.parts = null;
    r.label = null;
    r.resultSql = null;
    // Its name's statement, from where it is now in the editor.
    if (!into) Object.assign(r, fields);
  } else {
    r = addRun({ ...fields, outcome: blank, name: named });
  }
  // Named statements in a script: their parts go to their tabs at the end,
  // and each tab's results are stored then (not the whole run first).
  const split = naming?.units || naming?.numbered ? naming : null;
  activeRunId.value = r.id;
  liveRun = r;
  liveRunId.value = r.id;
  liveFrom = 0;
  const where = `${conn.value?.name ?? ''} · ${props.tab.database || t('query:defaultDatabase')}`;
  let unsafe: UnsafeDml[] | null = null;
  try {
    const o = await api.executeQuery({
      sessionId: props.tab.id, connectionId: props.tab.connectionId, database: props.tab.database,
      sql: script, maxRows: maxRows.value, queryId: doc.queryId ?? null, plan, record: true,
      mode: 'auto',
      continueOnError: perStatement.value ? continueOnError.value : null,
      confirmedUnsafe,
      autocommit: offersManualTx.value ? !manualTx.value : null,
      store: storing.value && !split ? storeArgs(r, props.tab.id, resultsSpace(settings.values[RESULTS_SPACE_SETTING])) : null,
    }).finally(() => { ui.historySeq++; });
    if (o.needs_confirmation?.length) {
      // Nothing ran: the previous results stay while the user decides.
      if (before) Object.assign(r, before);
      else dropRun(r);
      if (runs.value.some((x) => x.id === shownBefore)) activeRunId.value = shownBefore;
      unsafe = o.needs_confirmation;
    } else {
      r.outcome = o;
      // A cancel that closed the session took the open transaction with it:
      // the one open before the run, or the work of statements that ended.
      if (o.session_closed && manualTx.value && (txWasOpen || o.results.length > 0)) {
        ElMessage.warning({ message: t('query:tx.lostOnCancel'), duration: 5000 });
      }
      txState.value = o.transaction ?? null;
      await followDatabase(o.database);
      // A database created, dropped or renamed here: the explorer and the
      // pickers list it right away.
      if (CHANGES_DATABASES.test(script)) void conns.refreshDatabases(props.tab.connectionId).catch(() => {});
      const firstLine = script.split('\n').find((l) => l.trim())?.trim().slice(0, 120) ?? '';
      if (o.error) output.add('error', `${firstLine}\n${tb(o.error)}`, { where, elapsedMs: o.elapsed_ms });
      else output.add('info', firstLine, { where, elapsedMs: o.elapsed_ms });
      if (split) splitRun(r, o, split, clears);
    }
  } catch (e) {
    r.outcome = { results: [], messages: [], error: errorMessage(e), elapsed_ms: Date.now() - startedAt.value, plans: [] };
    persistRun(r, clears);
    output.add('error', errorMessage(e), { where });
    // A connection error: check the connection (the explorer and this pane
    // then show its state).
    void conns.recheckAfter(props.tab.connectionId, e);
  } finally {
    running.value = false;
    liveRun = null;
    liveRunId.value = null;
    stopClock();
  }
  if (!unsafe) trimRuns(r);
  if (unsafe && (await confirmUnsafe(script, unsafe, into ? into.lineOffset : lineOffset))) await execute(script, plan, true, into, naming);
}

/** A statement of the editor, with where its code starts (past the
 *  comment lines it starts with) and its name. */
type EditorStatement = ScriptUnit & { code: number; name: string | null };
/** The statements the engine cuts `doc` into. Client commands (DELIMITER,
 *  SET TERM) are the editor's, not the server's, and a unit of nothing but
 *  comments isn't one: neither is listed. */
function editorStatements(doc: string, units: ScriptUnit[]): EditorStatement[] {
  const out: EditorStatement[] = [];
  for (const u of units) {
    if (u.kind === 'client_command') continue;
    const h = statementHead(doc, u.start, u.end, commentMarks.value, driver.value?.dialect);
    if (h.start !== null) out.push({ ...u, code: h.start, name: h.name });
  }
  return out;
}
/** The statement of `doc` that `pick` chooses among those the engine cuts
 *  it into, or null (said why). */
async function statementOf(doc: string, pick: (units: EditorStatement[]) => EditorStatement | undefined): Promise<EditorStatement | null> {
  let units;
  try {
    units = await api.splitScript({ connectionId: props.tab.connectionId, sql: doc, statements: true });
  } catch (e) {
    ElMessage.error(errorMessage(e));
    return null;
  }
  // Around a client command, the nearest statement runs instead.
  const unit = pick(editorStatements(doc, units));
  if (!unit) ElMessage.info({ message: t('query:status.noStatement'), duration: 2000 });
  return unit ?? null;
}

/** ⌘⇧↵: the statement the cursor is in (or the last one before it). */
async function runStatement(doc: string, cursor: number) {
  if (running.value) return;
  const unit = await statementOf(doc, (units) => units.find((u) => cursor >= u.start && cursor <= u.end)
    ?? [...units].reverse().find((u) => u.start <= cursor)
    ?? units[0]);
  if (unit) await run(doc.slice(unit.start, unit.end), 'none', unit.start);
}

// -- the run lens: "▶ Ejecutar" over each statement (setting `editor.runLens`) --------------
const lensOn = computed(() => settings.values[RUN_LENS_SETTING] !== false);
/** The statements' starts and names, from the engine's own split; null: no lens. */
const lenses = ref<RunLens[] | null>(null);
let lensTimer: ReturnType<typeof setTimeout> | null = null;
let lensSeq = 0;
// Again a moment after typing stops; the old lenses stay (moved with the
// text) until the new ones arrive.
watch([text, lensOn, unbound, runnable, () => props.tab.connectionId, commentMarks], () => {
  if (lensTimer) clearTimeout(lensTimer);
  lensTimer = setTimeout(() => void refreshLenses(), 400);
}, { immediate: true });
onBeforeUnmount(() => { if (lensTimer) clearTimeout(lensTimer); });
async function refreshLenses() {
  const seq = ++lensSeq;
  const doc = text.value;
  if (!lensOn.value || unbound.value || !runnable.value || !driver.value || doc.length > LENS_MAX_CHARS) {
    lenses.value = null;
    return;
  }
  let units: ScriptUnit[] | null;
  try {
    units = await api.splitScript({ connectionId: props.tab.connectionId, sql: doc, statements: true });
  } catch {
    units = null;
  }
  // A newer text has its own on the way.
  if (seq !== lensSeq || doc !== text.value) return;
  const found = !units || units.some((u) => u.error) || units.length > 2 * LENS_MAX_STATEMENTS ? [] : editorStatements(doc, units);
  // Each lens over its statement's code (past the comment lines a unit may start with).
  lenses.value = found.length >= 2 && found.length <= LENS_MAX_STATEMENTS ? found.map((u) => ({ start: u.code, name: u.name })) : null;
}
/** A lens was clicked (`lineFrom`: its line): the statement whose code
 *  starts on that line runs, or its estimated plan, as "Ejecutar sentencia"
 *  would. */
async function runLens(lineFrom: number, plan: boolean) {
  if (running.value || connecting.value) return;
  const doc = text.value;
  const nl = doc.indexOf('\n', lineFrom);
  const lineEnd = nl < 0 ? doc.length : nl;
  const unit = await statementOf(doc, (units) => units.find((u) => u.code >= lineFrom && u.code <= lineEnd)
    ?? units.find((u) => lineFrom >= u.start && lineFrom <= u.end));
  if (unit) await run(doc.slice(unit.start, unit.end), plan ? 'estimated' : 'none', unit.start);
}

/** "Optimizar consulta" (OptimizerView): the selection, or the statement at the cursor. */
async function optimize() {
  if (!editor.value || unbound.value) return;
  const doc = text.value;
  let from = 0;
  let to = 0;
  if (editor.value.selectionText()) {
    ({ from } = editor.value.runnable());
    to = from + editor.value.selectionText().length;
  } else {
    const cursor = editor.value.cursor();
    let units;
    try { units = (await api.splitScript({ connectionId: props.tab.connectionId, sql: doc, statements: true })).filter((u) => u.kind !== 'client_command'); } catch (e) { ElMessage.error(errorMessage(e)); return; }
    const unit = units.find((u) => cursor >= u.start && cursor <= u.end) ?? [...units].reverse().find((u) => u.start <= cursor) ?? units[0];
    if (unit) ({ start: from, end: to } = unit);
  }
  // Without the spaces around it, so the editor still finds it where it was.
  const raw = doc.slice(from, to);
  from += raw.length - raw.trimStart().length;
  to -= raw.length - raw.trimEnd().length;
  if (to <= from) { ElMessage.info({ message: t('optimizer:noStatement'), duration: 2500 }); return; }
  tabs.openOptimizer(props.tab.connectionId, props.tab.database, doc.slice(from, to), props.tab.id, from, to);
}

/** A message's line was clicked: the cursor goes there in the editor. */
function goTo(at: { offset: number | null; line: number | null }) {
  const r = activeRun.value;
  editor.value?.goTo({
    pos: at.offset != null ? (r?.runBase ?? 0) + at.offset : null,
    line: at.line != null ? at.line + (r?.lineOffset ?? 0) : null,
  });
}

// -- result tabs -------------------------------------------------------------------------------
/** Unpinned results kept (Configuración › General). */
const keptResults = computed(() => keptLimit(settings.values[KEPT_RESULTS_SETTING]));

function addRun(fields: Omit<ResultRun, 'id' | 'key' | 'at' | 'pinned'>): ResultRun {
  runs.value.push({ ...fields, id: nextRunId++, key: crypto.randomUUID(), at: Date.now(), pinned: false });
  // The reactive one, so live progress and the answer show as they arrive.
  return runs.value[runs.value.length - 1];
}

/** Closes a result tab; the one shown moves to its neighbour. */
function dropRun(r: ResultRun) {
  const i = runs.value.indexOf(r);
  if (i < 0) return;
  runs.value.splice(i, 1);
  if (activeRunId.value === r.id) activeRunId.value = (runs.value[i] ?? runs.value[i - 1])?.id ?? null;
  if (storing.value) forget([r.key]);
}

/** After a run: the oldest unpinned results beyond the setting close. */
function trimRuns(fresh: ResultRun) {
  for (const r of overflow(runs.value, fresh, keptResults.value, liveRun)) dropRun(r);
}

function closeRun(r: ResultRun) {
  if (r === liveRun) return;
  dropRun(r);
}
/** "Cerrar las no fijadas" (`except`: "Cerrar las demás no fijadas"). */
function closeUnpinned(except?: ResultRun) {
  for (const r of runs.value.filter((x) => !x.pinned && x !== except && x !== liveRun)) dropRun(r);
}
function togglePin(r: ResultRun) {
  r.pinned = !r.pinned;
  if (storing.value) void api.resultsPin(r.key, r.pinned).catch(() => {});
}

// -- results kept on disk (composables/resultsStore.ts, docs/results-store.md) --
/** Results go to the store too ("Conservar resultados al cerrar DBine"). */
const storing = computed(() => keepResults(settings.values[KEEP_RESULTS_SETTING]));
/** Keep a tab's outcome as it is now: one the editor put together (several
 *  databases), changed (saved edits) or that failed before reaching the backend. */
function persistRun(r: ResultRun, clears?: number) {
  if (!storing.value || !r.outcome || r.restored) return;
  // The store was cleared since the run began: as `execute_query` does, it isn't kept.
  if (clears !== undefined && clears !== resultsClears()) return;
  void api.resultsSave(storeArgs(r, props.tab.id, resultsSpace(settings.values[RESULTS_SPACE_SETTING])), r.outcome).catch(() => {});
}
/** A restored editor shows its stored result tabs again, the newest one active. */
async function restoreRuns() {
  if (!storing.value) return;
  let stored;
  try {
    stored = await api.resultsRuns(props.tab.id);
  } catch {
    return;
  }
  if (unmounted || !stored.length) return;
  const known = new Set(runs.value.map((r) => r.key));
  const back = stored.filter((m) => !known.has(m.id)).map((m) => restoredRun(m, nextRunId++));
  if (!back.length) return;
  runs.value.unshift(...back);
  if (activeRunId.value == null) activeRunId.value = back[back.length - 1].id;
}
// A restored tab's rows load when it's shown.
watch(activeRun, (r) => { if (r?.restored) void loadRestored(r); });

/** Why "Volver a ejecutar" can't run a tab's script here, or null. */
function rerunBlocked(r: ResultRun): string | null {
  if (r.multi) return t('query:runs.rerunMulti');
  if (r.connectionId !== props.tab.connectionId || r.database !== props.tab.database) {
    return t('query:runs.rerunMoved', { database: r.database || t('query:defaultDatabase') });
  }
  return null;
}
/** "Volver a ejecutar": the tab's script again, its results replacing the tab's. */
async function rerun(r: ResultRun) {
  if (running.value || connecting.value || rerunBlocked(r)) return;
  if (!(await confirmEnvironment())) return;
  const go = async () => {
    retryRun = null;
    if (rerunBlocked(r) || !runs.value.includes(r)) return;
    if (!(await ensureConnection())) {
      retryRun = go;
      return;
    }
    await execute(r.script, r.plan, false, r, await rerunNaming(r));
  };
  await go();
}

/** hh:mm:ss, 24-hour so the tab stays short (as the Output panel's). */
function runTime(r: ResultRun) {
  return new Date(r.at).toLocaleTimeString(locale(), { hour: '2-digit', minute: '2-digit', second: '2-digit', hourCycle: 'h23' });
}
function runTitle(r: ResultRun) {
  const where = `${conns.byId(r.connectionId)?.name ?? ''} · ${r.database || t('query:defaultDatabase')}`;
  const what = r.multi ? r.multi.summary : scriptPreview(r.script);
  // A named one: its whole name first (the tab may show it shortened).
  const name = r.name ? `${r.name}\n\n` : '';
  return `${name}${what}\n\n${where}\n${t('query:runs.ranAt', { time: new Date(r.at).toLocaleString(locale()) })}`;
}

// -- renaming a result tab: double-click (or F2, or its menu), Enter keeps, Esc cancels ----
const renamingId = ref<number | null>(null);
const renameText = ref('');
let renameInput: HTMLInputElement | null = null;
function setRenameInput(el: unknown) {
  renameInput = el instanceof HTMLInputElement ? el : null;
}
function startRename(r: ResultRun) {
  // The one running gets its name when it ends (its stored copy is on the way).
  if (r === liveRun) return;
  renamingId.value = r.id;
  renameText.value = r.name ?? '';
  void nextTick(() => { renameInput?.focus(); renameInput?.select(); });
}
/** The name typed becomes the tab's: a statement with that name runs in it
 *  from then on (another tab that had it goes back to unnamed). Empty: no
 *  name. */
function commitRename(r: ResultRun) {
  if (renamingId.value !== r.id) return;
  renamingId.value = null;
  const name = renameText.value.trim().replace(/\s+/g, ' ') || null;
  if (name === (r.name ?? null)) return;
  for (const x of runs.value) {
    if (x !== r && name && x.name === name) {
      x.name = null;
      void storeName(x);
    }
  }
  r.name = name;
  void storeName(r);
}
function cancelRename() {
  renamingId.value = null;
}
/** The stored copy takes the new name (a restored tab's rows load first;
 *  rows that can't be read leave the stored copy as it is, never empty). */
async function storeName(r: ResultRun) {
  if (!storing.value || r === liveRun) return;
  if (r.restored && !(await loadRestored(r))) return;
  persistRun(r);
}
const runMenu = ref<{ x: number; y: number; items: MenuItem[] } | null>(null);
function openRunMenu(e: MouseEvent | KeyboardEvent, r: ResultRun) {
  e.preventDefault();
  let x: number;
  let y: number;
  if (e instanceof MouseEvent && e.clientX) ({ clientX: x, clientY: y } = e);
  else {
    const b = (e.currentTarget as HTMLElement).getBoundingClientRect();
    x = b.left;
    y = b.bottom;
  }
  const blocked = rerunBlocked(r);
  const others = runs.value.some((x) => !x.pinned && x !== r && x !== liveRun);
  runMenu.value = {
    x, y,
    items: [
      { label: r.pinned ? t('query:runs.unpin') : t('query:runs.pin'), action: () => togglePin(r) },
      { label: t('query:runs.rerun'), action: () => void rerun(r), disabled: !!blocked || running.value, hint: blocked ?? undefined },
      { label: t('query:runs.rename'), shortcut: 'F2', action: () => startRename(r), disabled: r === liveRun },
      { label: t('query:runs.copySql'), action: () => void copyRunSql(r) },
      { label: t('common:close'), action: () => closeRun(r), disabled: r === liveRun, divided: true },
      { label: t('query:runs.closeOthers'), action: () => closeUnpinned(r), disabled: !others },
      { label: t('query:runs.closeUnpinned'), action: () => closeUnpinned(), disabled: !runs.value.some((x) => !x.pinned && x !== liveRun) },
    ],
  };
}
async function copyRunSql(r: ResultRun) {
  try {
    await navigator.clipboard.writeText(r.script);
    ElMessage.success({ message: t('common:copied'), duration: 1500 });
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}
/** The strip shows with more than one result or a pinned or named one; a
 *  single result offers "Fijar" in the results' own bar. */
const showRunStrip = computed(() => runs.value.length > 1 || runs.value.some((r) => r.pinned || r.name));

// -- the tab's status bar ------------------------------------------------------------------------
const serverVersion = computed(() => conns.live[props.tab.connectionId]?.serverVersion ?? '');
const serverLabel = computed(() => conn.value?.config.host || conn.value?.name || '');
const userName = computed(() => conn.value?.config.username ?? '');
const statusLabel = computed(() => {
  if (connecting.value) return t('explorer:tree.connecting');
  if (connBlocked.value) {
    const kind = conns.live[props.tab.connectionId]?.detail?.kind;
    return t(`connection:problem.kind.${kind && kind !== 'unknown' ? kind : 'unknown'}.title`);
  }
  if (running.value) return t('results:running');
  const o = outcome.value;
  if (!o) return t('query:status.ready');
  return o.error || o.errors?.length ? t('results:status.doneWithErrors') : t('results:status.done');
});
const statusKind = computed(() => (connecting.value || running.value ? 'running' : connBlocked.value ? 'error' : outcome.value?.error || outcome.value?.errors?.length ? 'error' : outcome.value ? 'ok' : ''));
/** hh:mm:ss, live while it runs. */
const clockText = computed(() => {
  const ms = running.value ? now.value - startedAt.value : outcome.value?.elapsed_ms ?? 0;
  const sec = Math.floor(ms / 1000);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${pad(Math.floor(sec / 3600))}:${pad(Math.floor(sec / 60) % 60)}:${pad(sec % 60)}`;
});
const totalRows = computed(() => (outcome.value?.results ?? []).reduce((n, r) => n + (r.columns.length ? r.total_rows : 0), 0));

function cancel() {
  api.cancelQuery(props.tab.id).catch(() => {});
}

// -- "Ejecutar en varias bases…" (engines with several databases) ---------------------------
// The script (selection or everything) runs on the databases picked in the
// dialog, as a task; its results replace the pane's, merged into one grid
// when they share columns (composables/multiDb.ts).
const multiDbAvailable = computed(() => !!driver.value?.databases_label);
const multiOpen = ref(false);
const multiLoading = ref(false);
const multiLive = ref<MultiDbLive | null>(null);
const tasksStore = useTasksStore();
let viewAlive = true;
onBeforeUnmount(() => { viewAlive = false; });

async function openMultiDb() {
  if (multiLive.value?.running) { multiOpen.value = true; return; }
  multiLive.value = null;
  multiOpen.value = true;
  if (conns.live[props.tab.connectionId]?.databases?.length) return;
  multiLoading.value = true;
  try {
    // Couldn't connect: the results pane shows why, not an empty dialog.
    if (!(await ensureConnection())) multiOpen.value = false;
  } finally { multiLoading.value = false; }
}

async function runMultiDb(databases: string[]) {
  const picked = editor.value?.runnable() ?? { text: text.value, from: 0 };
  let script = picked.text.trim();
  if (!script) { ElMessage.info({ message: t('multiDb:dialog.empty'), duration: 2000 }); return; }
  const filled = await withParams(script);
  if (filled === null) return;
  script = filled;
  const connectionId = props.tab.connectionId;
  try {
    if (!(await confirmMultiDb({ connectionId, databases, sql: script }))) return;
  } catch (e) {
    ElMessage.error(errorMessage(e));
    return;
  }
  rememberSelection(connectionId, databases);
  if (doc.autosave && saveState.value === 'dirty') save();
  multiLive.value = startMultiDbRun({
    connectionId, connectionName: conn.value?.name ?? '', databases, sql: script,
    maxRows: maxRows.value, continueOnError: perStatement.value ? continueOnError.value : null,
    reopen: () => { if (viewAlive) multiOpen.value = true; },
    onDone: (r, error) => {
      if (!viewAlive) return;
      multiOpen.value = false;
      if (!r) { ElMessage.error(error ?? ''); return; }
      const { outcome: o, labels } = multiDbOutcome(r);
      const kept = addRun({
        outcome: o, script, plan: 'none', connectionId, database: '', language: driver.value?.language ?? 'sql',
        multi: { labels, summary: runSummary(r) }, lineOffset: null, runBase: 0,
      });
      activeRunId.value = kept.id;
      persistRun(kept);
      trimRuns(kept);
      output.add(r.databases.some((d) => d.status === 'error') ? 'error' : 'info', `${script.split('\n').find((l) => l.trim())?.trim().slice(0, 120) ?? ''}\n${runSummary(r)}`, {
        where: t('multiDb:task.title', { count: databases.length, connection: conn.value?.name ?? '' }), elapsedMs: r.elapsed_ms,
      });
    },
  });
}

// -- editor / results split -----------------------------------------------------------
const split = ref(readJson('dbine.querySplit', 0.45));
const col = ref<HTMLDivElement | null>(null);
function drag(e: PointerEvent) {
  const box = col.value!.getBoundingClientRect();
  const move = (ev: PointerEvent) => {
    split.value = Math.min(0.85, Math.max(0.12, (ev.clientY - box.top) / box.height));
  };
  const up = () => {
    window.removeEventListener('pointermove', move);
    window.removeEventListener('pointerup', up);
    writeJson('dbine.querySplit', split.value);
  };
  window.addEventListener('pointermove', move);
  window.addEventListener('pointerup', up);
  e.preventDefault();
}

// -- the status bar: connection, database, transactions and row limit ----------------------
const controls: QueryControls = reactive({
  running, txBusy, unbound, databases, loadingDatabases, offersManualTx, manualTx, txOpen, txState, maxRows,
  showDatabase: computed(() => !unbound.value && driver.value?.databases_label !== ''),
  loadDatabases, changeConnection, changeDatabase, setManualTx,
  setMaxRows: (n: number) => { maxRows.value = n; },
  // The command palette's "Editor: …" (composables/workbenchCommands.ts).
  editor: {
    runnable: computed(() => runnable.value && !unbound.value),
    multiDb: multiDbAvailable, lint: computed(() => !!lintSource.value), perStatement, continueOnError,
    hasSelection: () => !!editor.value?.selectionText().trim(),
    runSelection: () => { if (editor.value?.selectionText().trim()) void run(); },
    cancel,
    optimize: () => { void optimize(); },
    runMultiDb: () => { void openMultiDb(); },
    saveToLibrary,
    showProblems: () => { problemsOpen.value = true; },
    endTransaction: (commit: boolean) => { void endTransaction(commit); },
    toggleContinueOnError: () => { continueOnError.value = !continueOnError.value; },
  },
});
queryControls.set(props.tab.id, controls);
onBeforeUnmount(() => { if (queryControls.get(props.tab.id) === controls) queryControls.delete(props.tab.id); });

// -- the toolbar: what doesn't fit goes to "…" ---------------------------------------------
const bar = ref<HTMLElement | null>(null);
const { hidden: overflowed, isHidden } = useToolbarOverflow(bar);
const syntaxOpen = ref(false);
/** The collapsed actions, as the "…" menu shows them. */
const overflowItems = computed<MenuItem[]>(() => {
  const all: Record<string, () => MenuItem> = {
    format: () => ({ label: t('query:format'), shortcut: bindingLabel('editor.format'), disabled: !!formatOff.value || formatting.value, action: formatQuery }),
    optimize: () => ({ label: t('optimizer:action'), disabled: unbound.value, action: optimize }),
    multiDb: () => ({ label: t('multiDb:action'), action: openMultiDb }),
    builder: () => ({ label: t('queryBuilder:toolbar'), action: () => tabs.openQueryBuilder(props.tab.connectionId, props.tab.database) }),
    continueOnError: () => ({ label: t('query:continueOnError'), checked: continueOnError.value, action: () => { continueOnError.value = !continueOnError.value; } }),
    library: () => ({ label: t('query:saveToLibrary'), action: saveToLibrary }),
    problems: () => ({ divided: true, label: problems.value.length ? t('lint:problems.count', { count: problems.value.length }) : t('lint:problems.none'), action: () => { problemsOpen.value = true; } }),
    syntax: () => ({ label: t('query:syntax'), action: () => { syntaxOpen.value = true; } }),
  };
  return overflowed.value.filter((k) => all[k]).map((k) => all[k]());
});
function runOverflow(i: number) {
  const item = overflowItems.value[i];
  // After the dropdown's click is over (a popover would take it as an outside click).
  if (item && !item.disabled) setTimeout(() => item.action?.(), 0);
}
</script>

<template>
  <div v-if="loadError" class="nm-content">
    <el-alert type="error" :title="loadError" :closable="false" />
  </div>
  <div v-else class="qv">
    <!-- A project's file: where it is, and where it runs. -->
    <div v-if="fileTab" class="qv-file">
      <el-icon class="qv-file-ic"><ei-folder-opened /></el-icon>
      <button class="qv-crumb" :title="$t('projects:file.showInProjects')" @click="projects.focusProject(fileTab.projectId)">{{ project?.name ?? '…' }}</button>
      <span class="qv-sep">›</span>
      <span class="qv-path nm-selectable" :title="fileTab.path"><span class="nm-muted">{{ breadcrumbDir }}</span>{{ baseName(fileTab.path) }}</span>
      <span class="qv-sep">·</span>
      <button v-if="runEnv" class="qv-env" :class="{ warn: runEnv.confirm_run }" :title="$t('projects:file.envTip', { env: runEnv.name })" @click="pickProjectBase">{{ runEnv.name }}</button>
      <span v-if="!unbound" class="qv-target" :title="`${conn?.name ?? ''} › ${tab.database}`">{{ conn?.name }}<template v-if="tab.database"> › {{ tab.database }}</template></span>
      <button v-else class="qv-crumb warn" @click="pickProjectBase">{{ active?.problem === 'missing-connection' ? $t('projects:base.missingConnection') : active?.problem === 'missing-env' ? $t('projects:base.missingEnv', { env: active?.alias ?? '' }) : $t('projects:file.noBase') }}</button>
      <button v-if="fileTab.pinnedTarget" class="qv-crumb" :title="$t('projects:file.followProjectTip')" @click="followProject">{{ $t('projects:file.followProject') }}</button>
      <span v-if="extMismatch" class="qv-chip warn" :title="$t('projects:file.extMismatchTip', { ext: ext, engine: driver?.name ?? '' })"><el-icon><ei-warning-filled /></el-icon>{{ $t('projects:file.extMismatch', { ext }) }}</span>
      <div class="nm-spacer" />
      <span v-if="saveState === 'dirty'" class="nm-muted qv-lbl2">{{ $t('projects:file.unsaved') }}</span>
      <el-tooltip :content="$t('projects:file.saveTip')" placement="bottom" :show-after="300">
        <el-button size="small" :type="saveState === 'dirty' ? 'primary' : undefined" :disabled="saveState === 'saved' || saveState === 'saving' || doc.missing.value === 'project'" :loading="saveState === 'saving'" @click="save">
          {{ $t('common:save') }}
        </el-button>
      </el-tooltip>
    </div>
    <div v-if="doc.missing.value === 'project'" class="qv-banner err" role="alert">
      <el-icon><ei-warning-filled /></el-icon><span>{{ $t('projects:file.projectGone') }}</span>
    </div>
    <div v-else-if="doc.missing.value === 'file'" class="qv-banner err" role="alert">
      <el-icon><ei-warning-filled /></el-icon><span>{{ $t('projects:file.deleted') }}</span>
      <div class="nm-spacer" />
      <el-button size="small" @click="doc.keepMine?.()">{{ $t('projects:file.saveAgain') }}</el-button>
      <el-button size="small" @click="tabs.close(tab.id, true)">{{ $t('common:close') }}</el-button>
    </div>
    <div v-else-if="doc.conflict.value" class="qv-banner" role="alert">
      <el-icon><ei-warning-filled /></el-icon><span>{{ $t('projects:file.changedOnDisk') }}</span>
      <div class="nm-spacer" />
      <el-button size="small" @click="doc.reloadFromDisk?.()">{{ $t('projects:file.reload') }}</el-button>
      <el-button size="small" type="warning" @click="doc.keepMine?.()">{{ $t('projects:file.overwrite') }}</el-button>
    </div>
    <!-- The connection, database, transaction mode and row limit are in the
         status bar (StatusBar.vue). What doesn't fit here goes to "…". -->
    <div ref="bar" class="nm-toolbar qv-bar">
      <div class="qv-group" data-ov-fixed>
        <template v-if="!runnable" />
        <template v-else-if="!running">
          <el-tooltip :disabled="!unbound" :content="$t('projects:file.pickBase')" placement="bottom">
            <span class="qv-run-wrap">
              <el-button type="primary" :title="unbound ? undefined : $t('query:runTitle')" :disabled="unbound" @click="run()">
                <el-icon><ei-video-play /></el-icon>&nbsp;{{ $t('common:run') }}
              </el-button>
            </span>
          </el-tooltip>
          <el-tooltip :content="$t('query:runStatementTip')" placement="bottom" :show-after="300">
            <el-button :aria-label="$t('query:runStatement')" :disabled="unbound" @click="editor && runStatement(text, editor.cursor())"><el-icon><ei-caret-right /></el-icon></el-button>
          </el-tooltip>
          <el-button-group v-if="driver?.supports_explain">
            <el-tooltip :content="$t('query:estimatedPlanTip')" placement="bottom" :show-after="300">
              <el-button :aria-label="$t('query:estimatedPlan')" @click="run(undefined, 'estimated')"><el-icon><ei-share /></el-icon></el-button>
            </el-tooltip>
            <el-tooltip :content="$t('query:actualPlanTip')" placement="bottom" :show-after="300">
              <el-button :aria-label="$t('query:runWithPlan')" @click="run(undefined, 'actual')"><el-icon><ei-data-analysis /></el-icon></el-button>
            </el-tooltip>
          </el-button-group>
        </template>
        <el-button v-else type="danger" @click="cancel">
          <el-icon><ei-video-pause /></el-icon>&nbsp;{{ $t('common:cancel') }}
        </el-button>
        <!-- Manual transactions: Confirmar / Deshacer stay at hand (the mode is in the status bar). -->
        <template v-if="txOpen || manualTx">
          <el-button size="small" class="qv-txbtn" :disabled="running || txBusy" :title="$t('query:tx.commitTip')" @click="endTransaction(true)">{{ $t('query:tx.commit') }}</el-button>
          <el-button size="small" class="qv-txbtn" :disabled="running || txBusy" :title="$t('query:tx.rollbackTip')" @click="endTransaction(false)">{{ $t('query:tx.rollback') }}</el-button>
        </template>
      </div>
      <div class="qv-ov" data-ov="format" :class="{ 'qv-ov-off': isHidden('format') }">
        <el-tooltip :content="formatOff ?? $t('query:formatTip')" placement="bottom" :show-after="400">
          <el-button :disabled="!!formatOff" :loading="formatting" :aria-label="$t('query:format')" @click="formatQuery">
            <el-icon v-if="!formatting"><ei-magic-stick /></el-icon><span class="qv-lbl">&nbsp;{{ $t('query:format') }}</span>
          </el-button>
        </el-tooltip>
      </div>
      <div v-if="runnable" class="qv-ov" data-ov="optimize" :class="{ 'qv-ov-off': isHidden('optimize') }">
        <el-tooltip :content="$t('optimizer:actionTip')" placement="bottom" :show-after="400">
          <el-button :disabled="unbound" :aria-label="$t('optimizer:action')" @click="optimize">
            <el-icon><ei-trend-charts /></el-icon><span class="qv-lbl">&nbsp;{{ $t('optimizer:action') }}</span>
          </el-button>
        </el-tooltip>
      </div>
      <div v-if="multiDbAvailable" class="qv-ov" data-ov="multiDb" :class="{ 'qv-ov-off': isHidden('multiDb') }">
        <el-tooltip :content="$t('multiDb:actionTip')" placement="bottom" :show-after="300">
          <el-button size="small" :aria-label="$t('multiDb:action')" @click="openMultiDb">
            <el-icon :class="{ 'is-loading': multiLive?.running }"><ei-loading v-if="multiLive?.running" /><ei-files v-else /></el-icon><span class="qv-lbl">&nbsp;{{ $t('multiDb:action') }}</span>
          </el-button>
        </el-tooltip>
      </div>
      <div v-if="!unbound && (driver?.language === 'sql' || driver?.language === 'cql')" class="qv-ov" data-ov="builder" :class="{ 'qv-ov-off': isHidden('builder') }">
        <el-tooltip :content="$t('queryBuilder:toolbarTip')" placement="bottom" :show-after="300">
          <el-button size="small" :aria-label="$t('queryBuilder:toolbar')" @click="tabs.openQueryBuilder(tab.connectionId, tab.database)">
            <el-icon><ei-set-up /></el-icon><span class="qv-lbl">&nbsp;{{ $t('queryBuilder:toolbar') }}</span>
          </el-button>
        </el-tooltip>
      </div>
      <div v-if="perStatement" class="qv-ov" data-ov="continueOnError" :class="{ 'qv-ov-off': isHidden('continueOnError') }">
        <el-tooltip :content="$t('query:continueOnErrorTip')" placement="bottom" :show-after="400">
          <el-checkbox v-model="continueOnError" size="small" class="qv-check" :aria-label="$t('query:continueOnError')"><span class="qv-lbl2">{{ $t('query:continueOnError') }}</span></el-checkbox>
        </el-tooltip>
      </div>
      <div class="qv-ov" data-ov="library" :class="{ 'qv-ov-off': isHidden('library') }">
        <el-tooltip :content="$t('query:saveToLibraryTip')" placement="bottom" :show-after="300">
          <el-button link :aria-label="$t('query:saveToLibrary')" @click="saveToLibrary"><el-icon :size="15"><ei-star /></el-icon></el-button>
        </el-tooltip>
      </div>
      <div v-if="lintSource" class="qv-ov" data-ov="problems" :class="{ 'qv-ov-off': isHidden('problems') }">
        <el-popover v-model:visible="problemsOpen" placement="bottom-start" :width="480" trigger="click">
          <template #reference>
            <el-button link class="qv-problems" :class="worstProblem" :title="$t('lint:problems.button')" :aria-label="$t('lint:problems.button')">
              <el-icon><ei-warning /></el-icon><span class="qv-lbl2">&nbsp;{{ problems.length ? $t('lint:problems.count', { count: problems.length }) : $t('lint:problems.none') }}</span>
            </el-button>
          </template>
          <div class="qv-plist">
            <div class="qv-plist-head">
              <strong>{{ $t('lint:problems.title') }}</strong>
              <el-button link size="small" @click="ui.openSettings('lint')">{{ $t('lint:problems.settings') }}</el-button>
            </div>
            <p v-if="!problems.length" class="nm-muted">{{ $t('lint:problems.none') }}</p>
            <button v-for="(p, n) in problems" :key="n" class="qv-problem" :title="p.rule" @click="editor?.goTo({ pos: p.start })">
              <span class="qv-sev" :class="p.severity" />
              <span class="qv-pmsg">{{ p.message }}</span>
              <span class="nm-muted">{{ $t('lint:problems.line', { line: p.line }) }}</span>
            </button>
          </div>
        </el-popover>
      </div>
      <div class="nm-spacer" />
      <div v-if="driver?.query_help" class="qv-ov" data-ov="syntax" :class="{ 'qv-ov-off': isHidden('syntax') }">
        <el-popover v-model:visible="syntaxOpen" placement="bottom-end" :width="520" trigger="click">
          <template #reference>
            <el-button link :title="$t('query:syntaxTitle')"><el-icon><ei-question-filled /></el-icon><span class="qv-lbl">&nbsp;{{ $t('query:syntax') }}</span></el-button>
          </template>
          <pre class="qv-help nm-selectable">{{ tb(driver.query_help) }}</pre>
        </el-popover>
      </div>
      <el-dropdown data-ov-more class="qv-more" :class="{ 'qv-ov-off': !overflowItems.length }" trigger="click" placement="bottom-end" @command="runOverflow">
        <el-button link :title="$t('query:moreActions')" :aria-label="$t('query:moreActions')" :tabindex="overflowItems.length ? 0 : -1">
          <el-icon><ei-more-filled /></el-icon>
        </el-button>
        <template #dropdown>
          <el-dropdown-menu>
            <el-dropdown-item v-for="(item, i) in overflowItems" :key="item.label" :command="i" :disabled="item.disabled" :divided="i > 0 && item.divided">
              <span class="qv-more-check"><el-icon v-if="item.checked"><ei-check /></el-icon></span>{{ item.label }}
              <span v-if="item.shortcut" class="qv-more-key">{{ item.shortcut }}</span>
            </el-dropdown-item>
          </el-dropdown-menu>
        </template>
      </el-dropdown>
    </div>
    <div ref="col" class="qv-body">
      <div class="qv-editor" :style="{ height: split * 100 + '%' }">
        <CodeEditor
          ref="editor"
          v-model="text"
          :language="editorLanguage"
          :dialect="editorDialect"
          :schema="schema"
          :other-names="otherNames"
          :databases="otherDatabases"
          :placeholder="$t('query:editorPlaceholder')"
          :lint="lintSource"
          @run="(t: string, from: number) => run(t, 'none', from)"
          @run-statement="runStatement"
          @plan="(t: string, actual: boolean, from: number) => run(t, actual ? 'actual' : 'estimated', from)"
          @save="save"
          @format="formatQuery"
          @need-columns="loadColumnsFor"
          @need-database="(db: string) => conns.loadObjects(props.tab.connectionId, db)"
          @need-names="loadNames"
          run-actions
          :menu-items="editorMenu"
          :snippets="snippets"
          :link-at="linkAt"
          :table-hover="tableHover"
          :lenses="lenses"
          :lens-plan="!!driver?.supports_explain"
          :lens-busy="running || connecting"
          @navigate="(d: string, pos: number) => openNameAt(d, pos, false)"
          @lens="runLens"
        />
      </div>
      <div class="qv-sash" @pointerdown="drag" />
      <div class="qv-results">
        <ConnectionState v-if="connBlocked" :connection-id="tab.connectionId" @retry="retryConnection" />
        <template v-else>
          <!-- Each run's results stay in a tab of their own until closed. -->
          <div v-if="showRunStrip" class="qv-rts" role="tablist" :aria-label="$t('query:runs.label')">
            <div
              v-for="r in runs"
              :key="r.id"
              class="qv-rt"
              :class="{ active: r.id === activeRunId, pinned: r.pinned }"
            >
              <!-- Renaming: the name in a box in place of the tab's label. -->
              <input
                v-if="r.id === renamingId"
                :ref="setRenameInput"
                v-model="renameText"
                class="qv-rt-input"
                :size="Math.min(40, Math.max(20, renameText.length + 1))"
                :placeholder="$t('query:runs.namePlaceholder')"
                :aria-label="$t('query:runs.rename')"
                @keydown.enter.prevent="commitRename(r)"
                @keydown.esc.stop.prevent="cancelRename"
                @blur="commitRename(r)"
              >
              <button
                v-else
                role="tab"
                class="qv-rt-main"
                :aria-selected="r.id === activeRunId"
                :title="runTitle(r)"
                @click="activeRunId = r.id"
                @dblclick="startRename(r)"
                @auxclick.middle="closeRun(r)"
                @contextmenu="openRunMenu($event, r)"
                @keydown.shift.f10="openRunMenu($event, r)"
                @keydown.context-menu="openRunMenu($event, r)"
                @keydown.f2.prevent="startRename(r)"
              >
                <el-icon v-if="r.id === liveRunId" class="is-loading qv-rt-state"><ei-loading /></el-icon>
                <el-icon v-else-if="r.outcome?.error || r.outcome?.errors?.length" class="qv-rt-state qv-rt-error"><ei-circle-close-filled /></el-icon>
                <el-icon v-else-if="r.multi" class="qv-rt-state"><ei-files /></el-icon>
                <span v-if="r.name" class="qv-rt-name named">{{ r.name }}</span>
                <span v-else class="qv-rt-name">{{ r.label || runLabel(r.script) }}</span>
                <span class="qv-rt-time">{{ runTime(r) }}</span>
              </button>
              <button
                class="qv-rt-pin ide-icon-btn"
                :aria-pressed="r.pinned"
                :aria-label="r.pinned ? $t('query:runs.unpin') : $t('query:runs.pin')"
                :title="r.pinned ? $t('query:runs.unpin') : $t('query:runs.pinTip')"
                @click="togglePin(r)"
              >
                <el-icon><PinIcon /></el-icon>
              </button>
              <button
                v-if="r.id !== liveRunId"
                class="qv-rt-close ide-icon-btn"
                :aria-label="$t('common:close')"
                :title="$t('common:close')"
                @click="closeRun(r)"
              >
                <el-icon><ei-close /></el-icon>
              </button>
            </div>
          </div>
          <div v-if="activeRun?.multi" class="qv-multi" role="status">
            <el-icon><ei-files /></el-icon>
            <span>{{ $t('multiDb:results.bar', { summary: activeRun.multi.summary }) }}</span>
            <span class="nm-muted">{{ $t('multiDb:summary.messages') }}</span>
          </div>
          <div class="qv-panes">
            <!-- Kept mounted (v-show): each tab keeps its sort, view, chart and pending edits. -->
            <ResultsPane
              v-for="r in runs"
              v-show="r.id === activeRunId"
              :key="r.id"
              :outcome="r.outcome"
              :running="r.id === liveRunId"
              :source="r.multi ? null : { connectionId: r.connectionId, database: r.database, sql: r.script }"
              :title="doc.title.value"
              :dialect="driver?.dialect ?? ''"
              :edit-source="r.multi ? null : { connectionId: r.connectionId, database: r.database, language: r.language, script: r.script }"
              :result-sql="r.resultSql ?? null"
              :labels="r.multi?.labels ?? null"
              :line-offset="r.multi ? null : r.lineOffset ?? 0"
              hide-status
              @script="appendScript"
              @goto="goTo"
              @applied="persistRun(r)"
            >
              <template v-if="!showRunStrip" #lead>
                <button
                  class="qv-pin-one ide-icon-btn"
                  :aria-label="$t('query:runs.pin')"
                  :title="$t('query:runs.pinTip')"
                  @click="togglePin(r)"
                >
                  <el-icon><PinIcon /></el-icon>
                </button>
              </template>
            </ResultsPane>
            <ResultsPane v-if="!runs.length" :outcome="null" :running="false" hide-status />
          </div>
          <ContextMenu v-if="runMenu" :x="runMenu.x" :y="runMenu.y" :items="runMenu.items" @close="runMenu = null" />
        </template>
      </div>
    </div>
    <div class="qv-status">
      <span class="qv-st" :class="statusKind">
        <el-icon v-if="statusKind === 'running'" class="is-loading"><ei-loading /></el-icon>
        <el-icon v-else-if="statusKind === 'error'"><ei-circle-close-filled /></el-icon>
        <el-icon v-else-if="statusKind === 'ok'"><ei-circle-check-filled /></el-icon>
        {{ statusLabel }}
      </span>
      <span v-if="serverLabel" class="qv-st qv-st-trim" :title="serverVersion ? `${$t('query:status.server')}: ${serverVersion}` : $t('query:status.server')">{{ serverLabel }}</span>
      <span v-if="userName" class="qv-st" :title="$t('query:status.user')">{{ userName }}</span>
      <span v-if="tab.database" class="qv-st qv-st-trim" :title="$t('query:database')">{{ tab.database }}</span>
      <div class="nm-spacer" />
      <span class="qv-st" :title="$t('query:status.elapsed')">{{ clockText }}</span>
      <span class="qv-st" :title="$t('query:status.rowsTip')">{{ $t('results:messages.rows', { count: totalRows, rows: totalRows.toLocaleString(locale()) }) }}</span>
    </div>

    <QueryParamsDialog
      v-if="paramAsk && paramTarget"
      :items="paramAsk.items"
      :values="paramAsk.values"
      :target="paramTarget"
      :allow-null="hasNull(paramTarget)"
      @submit="(v: Record<string, ParamValue>) => paramAsk?.resolve(v)"
      @cancel="paramAsk?.resolve(null)"
      @disable="paramAsk?.resolve('off')"
    />

    <MultiDbRunDialog
      v-if="multiOpen"
      :connection-id="tab.connectionId"
      :current-database="tab.database"
      :databases="conns.live[tab.connectionId]?.databases ?? []"
      :loading="multiLoading"
      :read-only="!!conn?.config.read_only"
      :live="multiLive"
      @run="runMultiDb"
      @close="multiOpen = false"
      @background="multiOpen = false"
      @cancel="multiLive && tasksStore.cancel(multiLive.taskId)"
    />

    <el-dialog
      :model-value="!!txAsk"
      :title="$t('query:tx.dialogTitle')"
      width="460px"
      append-to-body
      :close-on-click-modal="false"
      @update:model-value="(v: boolean) => { if (!v) answerTx('cancel'); }"
    >
      <p class="qv-dialog-text">{{ txAsk ? $t(`query:tx.ask.${txAsk.reason}`) : '' }}</p>
      <div v-if="txError" class="qv-dialog-error" role="alert">{{ txError }}</div>
      <template #footer>
        <el-button :disabled="txBusy" @click="answerTx('cancel')">{{ $t('common:cancel') }}</el-button>
        <el-button :loading="txBusy" @click="answerTx('rollback')">{{ $t('query:tx.rollback') }}</el-button>
        <el-button type="primary" :loading="txBusy" @click="answerTx('commit')">{{ $t('query:tx.commit') }}</el-button>
      </template>
    </el-dialog>

    <el-dialog
      :model-value="!!unsafeAsk"
      :title="$t('query:unsafe.title')"
      width="620px"
      append-to-body
      :close-on-click-modal="false"
      @update:model-value="(v: boolean) => { if (!v) answerUnsafe(false); }"
    >
      <p class="qv-dialog-text">{{ $t('query:unsafe.intro', { count: unsafeAsk?.items.length ?? 0 }) }}</p>
      <div v-for="(u, i) in unsafeAsk?.items ?? []" :key="i" class="qv-unsafe">
        <div class="qv-unsafe-line">{{ $t('results:messages.line', { line: u.line }) }}</div>
        <pre class="qv-unsafe-code nm-selectable">{{ u.text }}</pre>
      </div>
      <template #footer>
        <el-button @click="answerUnsafe(false)">{{ $t('common:cancel') }}</el-button>
        <el-button type="danger" @click="answerUnsafe(true)">{{ $t('query:unsafe.run') }}</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.qv { display: flex; flex-direction: column; height: 100%; min-height: 0; }
.qv-help { margin: 0; max-height: 60vh; overflow: auto; white-space: pre-wrap; font-family: var(--nm-mono); font-size: 12px; line-height: 1.5; }
.qv-body { flex: 1; min-height: 0; display: flex; flex-direction: column; }
.qv-editor { min-height: 60px; overflow: hidden; }
.qv-sash { height: 5px; flex-shrink: 0; cursor: row-resize; border-top: 1px solid var(--nm-border-soft); }
.qv-sash:hover { background: var(--ide-focus); }
.qv-results { flex: 1; min-height: 0; display: flex; flex-direction: column; }
.qv-results > :last-child, .qv-panes { flex: 1; min-height: 0; }
.qv-panes { display: flex; flex-direction: column; }
.qv-panes > * { flex: 1; min-height: 0; }
/* The result tabs: one per run, like the editor's tabs but smaller. */
.qv-rts {
  display: flex; flex-shrink: 0; height: 26px; overflow-x: auto; scrollbar-width: thin;
  border-bottom: 1px solid var(--nm-border-soft); background: var(--ide-tabs, var(--nm-bg-elev));
}
.qv-rt {
  position: relative; display: flex; align-items: center; flex-shrink: 0; max-width: 300px;
  border-right: 1px solid var(--nm-border-soft); color: var(--nm-text-dim);
}
.qv-rt.active { background: var(--ide-editor); color: var(--nm-text-strong); }
.qv-rt.active::before { content: ''; position: absolute; left: 0; right: 0; top: 0; height: 1px; background: var(--ide-focus); }
.qv-rt-main {
  display: flex; align-items: center; gap: 6px; min-width: 0; height: 100%; padding: 0 2px 0 10px;
  border: 0; background: none; color: inherit; font: inherit; font-size: 12px; cursor: pointer;
}
.qv-rt-main:focus-visible { outline: 1px solid var(--ide-focus); outline-offset: -1px; }
.qv-rt-name { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-family: var(--nm-mono); font-size: 11.5px; }
/* A named result: its name, in the UI's font (it's a title, not code). */
.qv-rt-name.named { font-family: inherit; font-size: 12px; font-weight: 600; }
.qv-rt-input {
  align-self: center; height: 20px; margin: 0 4px 0 6px; padding: 0 6px; min-width: 0; box-sizing: border-box;
  border: 1px solid var(--ide-focus); border-radius: 3px; outline: none;
  background: var(--ide-editor, var(--nm-bg-elev)); color: var(--nm-text-strong); font: inherit; font-size: 12px;
}
.qv-rt-time { flex-shrink: 0; color: var(--nm-text-muted); font-size: 11px; font-variant-numeric: tabular-nums; }
.qv-rt-state { flex-shrink: 0; font-size: 12px; }
.qv-rt-error { color: var(--nm-danger); }
.qv-rt-pin, .qv-rt-close { flex-shrink: 0; width: 20px; height: 20px; font-size: 12px; color: inherit; }
/* Unpinned: the pin shows on hover, like the editor tabs' close. */
.qv-rt:not(.pinned) .qv-rt-pin { opacity: 0; }
.qv-rt:hover .qv-rt-pin, .qv-rt-pin:focus-visible { opacity: 1; }
.qv-rt.pinned .qv-rt-pin { color: var(--ide-focus); }
.qv-rt-close { margin-right: 4px; opacity: 0; }
.qv-rt:hover .qv-rt-close, .qv-rt.active .qv-rt-close, .qv-rt-close:focus-visible { opacity: 1; }
.qv-pin-one { align-self: center; width: 22px; height: 22px; margin-left: 4px; font-size: 13px; color: var(--nm-text-dim); }
.qv-multi {
  display: flex; align-items: center; gap: 6px; flex-shrink: 0; padding: 3px 10px; font-size: 12px;
  border-bottom: 1px solid var(--nm-border-soft); background: color-mix(in srgb, var(--ide-focus, var(--nm-primary)) 8%, transparent);
}
.qv-check { margin: 0 4px; }
/* "Ver problemas": the button takes the worst severity's color. */
.qv-problems.error { color: var(--nm-danger); }
.qv-problems.warning { color: var(--nm-warning); }
.qv-problems.info { color: var(--nm-info); }
.qv-plist { display: flex; flex-direction: column; max-height: 360px; overflow: auto; font-size: 12.5px; }
.qv-plist-head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 6px; }
.qv-plist p { margin: 4px 0; }
.qv-problem {
  display: flex; align-items: baseline; gap: 8px; padding: 4px 6px; border: 0; border-radius: 3px; background: none;
  color: var(--nm-text); font: inherit; text-align: left; cursor: pointer;
}
.qv-problem:hover { background: var(--ide-hover); }
.qv-pmsg { flex: 1; min-width: 0; }
.qv-sev { flex: none; width: 8px; height: 8px; border-radius: 50%; background: var(--nm-info); }
.qv-sev.error { background: var(--nm-danger); }
.qv-sev.warning { background: var(--nm-warning); }
.qv-run-wrap { display: inline-flex; }
/* A project file's header: project › path · environment, base. */
.qv-file {
  display: flex; align-items: center; gap: 6px; height: 26px; flex-shrink: 0; padding: 0 10px; overflow: hidden; white-space: nowrap;
  font-size: 12px; color: var(--nm-text); border-bottom: 1px solid var(--nm-border-soft);
}
.qv-file > * { flex-shrink: 0; }
.qv-file-ic { color: var(--nm-folder); }
.qv-path { flex-shrink: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; color: var(--nm-text-strong); }
.qv-sep { color: var(--nm-text-muted); }
.qv-crumb { border: 0; padding: 0; background: none; font: inherit; color: var(--nm-text-dim); cursor: pointer; }
.qv-crumb:hover { color: var(--nm-text-strong); text-decoration: underline; }
.qv-crumb.warn { color: var(--nm-warning); }
.qv-target { flex-shrink: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; color: var(--nm-text-dim); }
.qv-env {
  border: 1px solid color-mix(in srgb, var(--nm-accent) 55%, transparent); border-radius: 9px; padding: 0 7px; line-height: 16px;
  background: color-mix(in srgb, var(--nm-accent) 14%, transparent); color: var(--nm-text-strong); font: inherit; font-size: 11px; cursor: pointer;
}
.qv-env.warn { border-color: color-mix(in srgb, var(--nm-danger) 60%, transparent); background: color-mix(in srgb, var(--nm-danger) 16%, transparent); }
.qv-chip { display: inline-flex; align-items: center; gap: 3px; font-size: 11px; }
.qv-chip.warn { color: var(--nm-warning); }
.qv-banner {
  display: flex; align-items: center; gap: 8px; flex-shrink: 0; padding: 4px 10px; font-size: 12px; color: var(--nm-text);
  border-bottom: 1px solid color-mix(in srgb, var(--nm-warning) 45%, transparent); background: color-mix(in srgb, var(--nm-warning) 12%, transparent);
}
.qv-banner > .el-icon { color: var(--nm-warning); }
.qv-banner.err { border-bottom-color: color-mix(in srgb, var(--nm-danger) 45%, transparent); background: color-mix(in srgb, var(--nm-danger) 12%, transparent); }
.qv-banner.err > .el-icon { color: var(--nm-danger); }
/* The toolbar never wraps nor clips: labels go first (icon-only buttons,
   the name stays in the tooltip), then the actions that still don't fit go
   to "…" (composables/toolbarOverflow.ts). The breakpoints are the
   toolbar's own width, so opening the AI sidebar counts. */
.qv-bar { container-type: inline-size; overflow: hidden; position: relative; }
.qv-bar > * { flex-shrink: 0; }
.qv-group, .qv-ov { display: inline-flex; align-items: center; gap: 6px; }
.qv-group > .el-button + .el-button { margin-left: 0; }
.qv-bar :deep(.el-button-group) { display: inline-flex; flex-wrap: nowrap; flex-shrink: 0; }
.qv-bar :deep(.el-button) { white-space: nowrap; }
/* Collapsed into "…": out of the line and invisible, still measurable (and
   an anchor for its popover, at the toolbar's right end). */
.qv-bar > .qv-ov-off { position: absolute; right: 8px; top: 0; height: 100%; visibility: hidden; pointer-events: none; }
.qv-more-check { display: inline-flex; width: 16px; }
.qv-more-key { margin-left: 18px; color: var(--nm-text-dim); font-size: 11.5px; }
@container (max-width: 1180px) { .qv-lbl { display: none; } }
@container (max-width: 960px) { .qv-lbl2 { display: none; } }
.qv-status {
  display: flex; align-items: center; height: 22px; flex-shrink: 0; padding: 0 4px; overflow: hidden; white-space: nowrap;
  border-top: 1px solid var(--nm-border-soft); font-size: 11.5px; color: var(--nm-text-dim); font-variant-numeric: tabular-nums;
}
.qv-st { display: inline-flex; align-items: center; gap: 4px; padding: 0 8px; border-right: 1px solid var(--nm-border-soft); }
.qv-st:last-child { border-right: none; }
.qv-st-trim { max-width: 240px; overflow: hidden; text-overflow: ellipsis; display: inline-block; }
.qv-st.ok .el-icon { color: var(--nm-success); }
.qv-st.error { color: var(--nm-danger); }
.qv-dialog-text { margin: 0 0 10px; color: var(--nm-text); }
.qv-dialog-error { margin-top: 8px; padding: 6px 10px; border-radius: 3px; color: var(--nm-text); white-space: pre-wrap; border: 1px solid color-mix(in srgb, var(--nm-danger) 50%, transparent); background: color-mix(in srgb, var(--nm-danger) 10%, transparent); }
.qv-unsafe { margin-bottom: 8px; }
.qv-unsafe-line { font-size: 11.5px; color: var(--nm-text-dim); margin-bottom: 2px; }
.qv-unsafe-code { margin: 0; padding: 8px 10px; max-height: 30vh; overflow: auto; border: 1px solid var(--nm-border); border-radius: 4px; background: var(--ide-editor, var(--nm-bg-elev)); font-family: var(--nm-mono); font-size: 12.5px; color: var(--nm-text-strong); white-space: pre-wrap; }
</style>
