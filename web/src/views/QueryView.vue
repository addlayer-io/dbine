<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { api, errorMessage } from '../api/client';
import { locale, t } from '../i18n';
import { tb } from '../i18n/backend';
import type { QueryMessage, QueryOutcome, QueryProgress, TxState, UnsafeDml } from '../api/types';
import CodeEditor from '../components/CodeEditor.vue';
import type { MenuItem } from '../components/ContextMenu.vue';
import ResultsPane from '../components/ResultsPane.vue';
import { dbKey, objKey, useConnectionsStore } from '../stores/connections';
import { useOutputStore } from '../stores/output';
import { useSettingsStore } from '../stores/settings';
import { baseName, busyTabs, closeGuards, useTabsStore, type FileTab, type QueryTab } from '../stores/tabs';
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
import {
  fillParams, findParams, guessType, hasNull, paramItems, paramMarkers, type ParamItem, type ParamTarget, type ParamValue,
} from '../composables/queryParams';
import { SNIPPETS_SETTING, snippetsFor, type UserSnippet } from '../composables/snippets';
import QueryParamsDialog from '../components/QueryParamsDialog.vue';

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
const outcome = ref<QueryOutcome | null>(null);
/** The script behind `outcome` (exports run it again for every row). */
const lastScript = ref('');
const running = ref(false);
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
async function onDatabaseMenu(visible: boolean) {
  if (!visible || unbound.value || conns.live[props.tab.connectionId]?.databases?.length) return;
  loadingDatabases.value = true;
  try { await conns.ensureConnected(props.tab.connectionId); } finally { loadingDatabases.value = false; }
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
  if (db === props.tab.database) return;
  // Another database is another session: the open transaction would go.
  if (!(await settleTransaction('database'))) return;
  await doc.changeDatabase(db);
  conns.loadObjects(props.tab.connectionId, db);
  txState.value = null;
}

/** A statement switched the database (`USE`): the tab follows it on the same
 *  session, so nothing to settle (the transaction and #temp tables stay). */
async function followDatabase(db: string | null | undefined) {
  if (!db || db === props.tab.database) return;
  await doc.followDatabase(db);
  conns.loadObjects(props.tab.connectionId, db);
}

// -- completion schema ------------------------------------------------------------
const schema = computed(() => {
  const objs = conns.objects[dbKey(props.tab.connectionId, props.tab.database)]?.items ?? [];
  const out: Record<string, string[]> = {};
  for (const o of objs) {
    const kind = driver.value?.object_kinds.find((k) => k.id === o.kind);
    if (!kind?.has_columns) continue;
    const cols = conns.columns[objKey(props.tab.connectionId, props.tab.database, o.schema, o.name)]?.items.map((c) => c.name) ?? [];
    out[o.name] = cols;
    if (o.schema) out[`${o.schema}.${o.name}`] = cols;
  }
  return out;
});

// Completion reads objects and columns from the store, which only the explorer
// filled. The tab loads its database's objects itself once the connection is
// open (it never connects just for this).
watch(() => [conns.live[props.tab.connectionId]?.status, props.tab.database] as const, ([status, db]) => {
  if (status === 'connected') conns.loadObjects(props.tab.connectionId, db);
}, { immediate: true });

// Columns are loaded per table, only for the tables the text names.
let columnsTimer: ReturnType<typeof setTimeout> | null = null;
watch([text, () => conns.objects[dbKey(props.tab.connectionId, props.tab.database)]?.items], () => {
  if (columnsTimer) clearTimeout(columnsTimer);
  columnsTimer = setTimeout(loadReferencedColumns, 400);
});
onBeforeUnmount(() => { if (columnsTimer) clearTimeout(columnsTimer); });
function loadReferencedColumns() {
  const { connectionId, database } = props.tab;
  const objs = conns.objects[dbKey(connectionId, database)]?.items ?? [];
  if (!objs.length) return;
  const words = new Set(text.value.toLowerCase().match(/[\w$#]+/g) ?? []);
  let started = 0;
  for (const o of objs) {
    if (started >= 20) break;
    if (!words.has(o.name.toLowerCase()) || conns.columns[objKey(connectionId, database, o.schema, o.name)]) continue;
    if (!driver.value?.object_kinds.find((k) => k.id === o.kind)?.has_columns) continue;
    conns.loadColumns(connectionId, database, o);
    started++;
  }
}

/** The editor reached `table.` or `schema.table.` before the columns were
 *  loaded: load them now, without waiting for the text to settle. */
function loadColumnsFor(path: string[]) {
  const { connectionId, database } = props.tab;
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
const formatOff = computed(() => formatUnavailable(driver.value?.language));
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

type PlanMode = 'none' | 'estimated' | 'actual';

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
type TxReason = 'close' | 'database' | 'mode';
const txAsk = ref<{ reason: TxReason; resolve: (ok: boolean) => void } | null>(null);
const txError = ref<string | null>(null);
/** Resolves true when there's no open transaction left (or there was none). */
async function settleTransaction(reason: TxReason): Promise<boolean> {
  if (!txOpen.value && !manualTx.value) return true;
  try { txState.value = await api.tabTransactionState(props.tab.id); } catch { /* keep the last known state */ }
  if (!txOpen.value) return true;
  if (reason === 'close') tabs.activate(props.tab.id);
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
// A project's file tabs follow its active base, except while busy here.
watch(() => running.value || txOpen.value, (busy) => {
  if (busy) busyTabs.add(props.tab.id); else busyTabs.delete(props.tab.id);
}, { immediate: true });
onBeforeUnmount(() => busyTabs.delete(props.tab.id));

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
function confirmUnsafe(script: string, found: UnsafeDml[]): Promise<boolean> {
  const items = found.map((u) => ({ keyword: u.keyword, line: u.line + (lineOffset.value ?? 0), text: script.slice(u.start, u.end).trim() }));
  return new Promise<boolean>((resolve) => { unsafeAsk.value = { items, resolve }; });
}
function answerUnsafe(ok: boolean) {
  unsafeAsk.value?.resolve(ok);
  unsafeAsk.value = null;
}

// -- live progress (query-progress / query-message of this tab's session) --------------------
/** Where the run's sent text starts in the editor, and its first line minus one. */
const runBase = ref(0);
const lineOffset = ref<number | null>(null);
/** Messages streamed since the last statement ended (its progress event repeats them). */
let liveFrom = 0;
function onProgress(p: QueryProgress) {
  const o = outcome.value;
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
  const o = outcome.value;
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
});
onBeforeUnmount(() => {
  unmounted = true;
  unlisteners.forEach((u) => u());
});

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
  if (running.value) return;
  const picked = sqlText !== undefined ? { text: sqlText, from: from ?? 0 } : editor.value?.runnable() ?? { text: text.value, from: 0 };
  const script = picked.text.trim();
  if (!script) return;
  const base = picked.from + (picked.text.length - picked.text.trimStart().length);
  if (!runnable.value) return;
  if (unbound.value) { ElMessage.info({ message: t('projects:file.pickBase'), duration: 3000 }); return; }
  const filled = await withParams(script);
  if (filled === null) return;
  if (!(await confirmEnvironment())) return;
  if (!(await conns.ensureConnected(props.tab.connectionId))) return;
  if (doc.autosave && saveState.value === 'dirty') save();
  runBase.value = base;
  lineOffset.value = (editor.value?.lineAt(base) ?? 1) - 1;
  await execute(filled, plan, false);
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

async function execute(script: string, plan: PlanMode, confirmedUnsafe: boolean) {
  running.value = true;
  multiShown.value = null;
  startClock();
  const before = { outcome: outcome.value, script: lastScript.value };
  // What a cancel that closes the session would roll back: the transaction
  // open before the run.
  const txWasOpen = txOpen.value;
  lastScript.value = script;
  // Statements fill it as they end (query-progress); the answer replaces it.
  outcome.value = { results: [], messages: [], error: null, elapsed_ms: 0, plans: [], log: [], errors: [] };
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
    }).finally(() => { ui.historySeq++; });
    if (o.needs_confirmation?.length) {
      // Nothing ran: the previous results stay while the user decides.
      outcome.value = before.outcome;
      lastScript.value = before.script;
      unsafe = o.needs_confirmation;
    } else {
      outcome.value = o;
      // A cancel that closed the session took the open transaction with it:
      // the one open before the run, or the work of statements that ended.
      if (o.session_closed && manualTx.value && (txWasOpen || o.results.length > 0)) {
        ElMessage.warning({ message: t('query:tx.lostOnCancel'), duration: 5000 });
      }
      txState.value = o.transaction ?? null;
      await followDatabase(o.database);
      const firstLine = script.split('\n').find((l) => l.trim())?.trim().slice(0, 120) ?? '';
      if (o.error) output.add('error', `${firstLine}\n${tb(o.error)}`, { where, elapsedMs: o.elapsed_ms });
      else output.add('info', firstLine, { where, elapsedMs: o.elapsed_ms });
    }
  } catch (e) {
    outcome.value = { results: [], messages: [], error: errorMessage(e), elapsed_ms: Date.now() - startedAt.value, plans: [] };
    output.add('error', errorMessage(e), { where });
  } finally {
    running.value = false;
    stopClock();
  }
  if (unsafe && (await confirmUnsafe(script, unsafe))) await execute(script, plan, true);
}

/** ⌘⇧↵: the statement the cursor is in (or the last one before it). */
async function runStatement(doc: string, cursor: number) {
  if (running.value) return;
  let units;
  try {
    units = await api.splitScript({ connectionId: props.tab.connectionId, sql: doc, statements: true });
  } catch (e) {
    ElMessage.error(errorMessage(e));
    return;
  }
  // Client commands (DELIMITER, SET TERM) are the editor's, not the server's:
  // the nearest statement around them runs instead.
  units = units.filter((u) => u.kind !== 'client_command');
  const unit = units.find((u) => cursor >= u.start && cursor <= u.end)
    ?? [...units].reverse().find((u) => u.start <= cursor)
    ?? units[0];
  if (!unit) {
    ElMessage.info({ message: t('query:status.noStatement'), duration: 2000 });
    return;
  }
  await run(doc.slice(unit.start, unit.end), 'none', unit.start);
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
  editor.value?.goTo({
    pos: at.offset != null ? runBase.value + at.offset : null,
    line: at.line != null ? at.line + (lineOffset.value ?? 0) : null,
  });
}

// -- the tab's status bar ------------------------------------------------------------------------
const serverVersion = computed(() => conns.live[props.tab.connectionId]?.serverVersion ?? '');
const serverLabel = computed(() => conn.value?.config.host || conn.value?.name || '');
const userName = computed(() => conn.value?.config.username ?? '');
const statusLabel = computed(() => {
  if (running.value) return t('results:running');
  const o = outcome.value;
  if (!o) return t('query:status.ready');
  return o.error || o.errors?.length ? t('results:status.doneWithErrors') : t('results:status.done');
});
const statusKind = computed(() => (running.value ? 'running' : outcome.value?.error || outcome.value?.errors?.length ? 'error' : outcome.value ? 'ok' : ''));
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
/** The pane shows a multi-database run: its sub-tab labels and summary. */
const multiShown = ref<{ labels: string[]; summary: string } | null>(null);
const tasksStore = useTasksStore();
let viewAlive = true;
onBeforeUnmount(() => { viewAlive = false; });

async function openMultiDb() {
  if (multiLive.value?.running) { multiOpen.value = true; return; }
  multiLive.value = null;
  multiOpen.value = true;
  if (conns.live[props.tab.connectionId]?.databases?.length) return;
  multiLoading.value = true;
  try { await conns.ensureConnected(props.tab.connectionId); } finally { multiLoading.value = false; }
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
      outcome.value = o;
      lastScript.value = script;
      multiShown.value = { labels, summary: runSummary(r) };
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
    <div class="nm-toolbar qv-bar">
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
      <el-tooltip :content="formatOff ?? $t('query:formatTip')" placement="bottom" :show-after="400">
        <el-button :disabled="!!formatOff" :loading="formatting" :aria-label="$t('query:format')" @click="formatQuery">
          <el-icon v-if="!formatting"><ei-magic-stick /></el-icon><span class="qv-lbl">&nbsp;{{ $t('query:format') }}</span>
        </el-button>
      </el-tooltip>
      <el-tooltip v-if="runnable" :content="$t('optimizer:actionTip')" placement="bottom" :show-after="400">
        <el-button :disabled="unbound" :aria-label="$t('optimizer:action')" @click="optimize">
          <el-icon><ei-trend-charts /></el-icon><span class="qv-lbl">&nbsp;{{ $t('optimizer:action') }}</span>
        </el-button>
      </el-tooltip>
      <el-select
        v-if="!unbound && driver?.databases_label !== ''"
        :model-value="tab.database"
        filterable
        size="small"
        class="qv-db"
        :loading="loadingDatabases"
        :placeholder="driver?.databases_label ? tb(driver.databases_label) : $t('query:database')"
        :title="tab.database"
        @visible-change="onDatabaseMenu"
        @update:model-value="changeDatabase"
      >
        <el-option v-for="d in databases" :key="d" :label="d" :value="d" />
      </el-select>
      <el-tooltip v-if="multiDbAvailable" :content="$t('multiDb:actionTip')" placement="bottom" :show-after="300">
        <el-button size="small" :aria-label="$t('multiDb:action')" @click="openMultiDb">
          <el-icon :class="{ 'is-loading': multiLive?.running }"><ei-loading v-if="multiLive?.running" /><ei-files v-else /></el-icon><span class="qv-lbl">&nbsp;{{ $t('multiDb:action') }}</span>
        </el-button>
      </el-tooltip>
      <el-tooltip v-if="!unbound && (driver?.language === 'sql' || driver?.language === 'cql')" :content="$t('queryBuilder:toolbarTip')" placement="bottom" :show-after="300">
        <el-button size="small" :aria-label="$t('queryBuilder:toolbar')" @click="tabs.openQueryBuilder(tab.connectionId, tab.database)">
          <el-icon><ei-set-up /></el-icon><span class="qv-lbl">&nbsp;{{ $t('queryBuilder:toolbar') }}</span>
        </el-button>
      </el-tooltip>
      <el-tooltip v-if="perStatement" :content="$t('query:continueOnErrorTip')" placement="bottom" :show-after="400">
        <el-checkbox v-model="continueOnError" size="small" class="qv-check" :aria-label="$t('query:continueOnError')"><span class="qv-lbl2">{{ $t('query:continueOnError') }}</span></el-checkbox>
      </el-tooltip>
      <template v-if="offersManualTx">
        <el-radio-group
          size="small"
          :model-value="manualTx ? 'manual' : 'auto'"
          :aria-label="$t('query:tx.label')"
          :disabled="running || txBusy"
          @update:model-value="(v: string | number | boolean | undefined) => setManualTx(v === 'manual')"
        >
          <el-radio-button value="auto" :title="$t('query:tx.autoTip')">{{ $t('query:tx.auto') }}</el-radio-button>
          <el-radio-button value="manual" :title="$t('query:tx.manualTip')">{{ $t('query:tx.manual') }}</el-radio-button>
        </el-radio-group>
      </template>
      <span v-if="txOpen" class="qv-tx" :class="txState" role="status">
        <el-icon><ei-warning-filled /></el-icon>{{ txState === 'failed' ? $t('query:tx.failed') : $t('query:tx.open') }}
      </span>
      <template v-if="txOpen || manualTx">
        <el-button size="small" :disabled="running || txBusy" @click="endTransaction(true)">{{ $t('query:tx.commit') }}</el-button>
        <el-button size="small" :disabled="running || txBusy" @click="endTransaction(false)">{{ $t('query:tx.rollback') }}</el-button>
      </template>
      <el-tooltip :content="$t('query:saveToLibraryTip')" placement="bottom" :show-after="300">
        <el-button link :aria-label="$t('query:saveToLibrary')" @click="saveToLibrary"><el-icon :size="15"><ei-star /></el-icon></el-button>
      </el-tooltip>
      <el-popover v-if="lintSource" v-model:visible="problemsOpen" placement="bottom-start" :width="480" trigger="click">
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
      <div class="nm-spacer" />
      <el-popover v-if="driver?.query_help" placement="bottom-end" :width="520" trigger="click">
        <template #reference>
          <el-button link :title="$t('query:syntaxTitle')"><el-icon><ei-question-filled /></el-icon><span class="qv-lbl">&nbsp;{{ $t('query:syntax') }}</span></el-button>
        </template>
        <pre class="qv-help nm-selectable">{{ tb(driver.query_help) }}</pre>
      </el-popover>
      <span class="nm-muted qv-lbl2">{{ $t('query:maxRows') }}</span>
      <el-select v-model="maxRows" size="small" style="width: 96px" :title="$t('query:maxRows')">
        <el-option v-for="n in [100, 1000, 5000, 20000, 100000]" :key="n" :label="n.toLocaleString(locale())" :value="n" />
      </el-select>
    </div>
    <div ref="col" class="qv-body">
      <div class="qv-editor" :style="{ height: split * 100 + '%' }">
        <CodeEditor
          ref="editor"
          v-model="text"
          :language="editorLanguage"
          :dialect="driver?.dialect"
          :schema="schema"
          :placeholder="$t('query:editorPlaceholder')"
          :lint="lintSource"
          @run="(t: string, from: number) => run(t, 'none', from)"
          @run-statement="runStatement"
          @plan="(t: string, actual: boolean, from: number) => run(t, actual ? 'actual' : 'estimated', from)"
          @save="save"
          @format="formatQuery"
          @need-columns="loadColumnsFor"
          run-actions
          :menu-items="editorMenu"
          :snippets="snippets"
          :link-at="linkAt"
          @navigate="(d: string, pos: number) => openNameAt(d, pos, false)"
        />
      </div>
      <div class="qv-sash" @pointerdown="drag" />
      <div class="qv-results">
        <div v-if="multiShown" class="qv-multi" role="status">
          <el-icon><ei-files /></el-icon>
          <span>{{ $t('multiDb:results.bar', { summary: multiShown.summary }) }}</span>
          <span class="nm-muted">{{ $t('multiDb:summary.messages') }}</span>
        </div>
        <ResultsPane
          :outcome="outcome"
          :running="running"
          :source="lastScript && !multiShown ? { connectionId: tab.connectionId, database: tab.database, sql: lastScript } : null"
          :title="doc.title.value"
          :dialect="driver?.dialect ?? ''"
          :edit-source="lastScript && !multiShown ? { connectionId: tab.connectionId, database: tab.database, language: driver?.language ?? 'sql', script: lastScript } : null"
          :labels="multiShown?.labels ?? null"
          :line-offset="multiShown ? null : lineOffset ?? 0"
          hide-status
          @script="appendScript"
          @goto="goTo"
        />
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
.qv-results > :last-child { flex: 1; min-height: 0; }
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
.qv-file-ic { color: #c5a46d; }
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
/* The toolbar never wraps: groups (Auto/Manual, the plan buttons) keep one
   line, the database select gives up width first, then labels go and leave
   icon-only buttons (their name stays in the tooltip). The breakpoints are
   the toolbar's own width, so opening the AI sidebar counts. */
.qv-bar { container-type: inline-size; overflow: hidden; }
.qv-bar > * { flex-shrink: 0; }
.qv-bar :deep(.el-radio-group), .qv-bar :deep(.el-button-group) { display: inline-flex; flex-wrap: nowrap; flex-shrink: 0; }
.qv-bar :deep(.el-radio-button__inner), .qv-bar :deep(.el-button) { white-space: nowrap; }
.qv-bar .qv-db { flex: 0 1 234px; width: auto; min-width: 110px; }
@container (max-width: 1180px) { .qv-lbl { display: none; } }
@container (max-width: 960px) { .qv-lbl2 { display: none; } }
.qv-tx {
  display: inline-flex; align-items: center; gap: 4px; padding: 1px 8px; border-radius: 10px; font-size: 11.5px;
  color: var(--nm-warning); border: 1px solid color-mix(in srgb, var(--nm-warning) 50%, transparent);
  background: color-mix(in srgb, var(--nm-warning) 10%, transparent);
}
.qv-tx.failed { color: var(--nm-danger); border-color: color-mix(in srgb, var(--nm-danger) 50%, transparent); background: color-mix(in srgb, var(--nm-danger) 10%, transparent); }
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
