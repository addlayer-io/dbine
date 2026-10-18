<script setup lang="ts">
import { computed, h, markRaw, nextTick, onBeforeUnmount, onMounted, reactive, ref, shallowReactive, watch } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { save as saveDialog } from '@tauri-apps/plugin-dialog';
import { api, errorMessage } from '../api/client';
import { notebookApi, type PreviewLimits } from '../api/notebook';
import { projectsApi } from '../api/projects';
import type { FileStat, LoadedTable, LocalSource, Notebook, NotebookCell, NotebookTarget, QueryOutcome, TxState, UnsafeDml } from '../api/types';
import { t } from '../i18n';
import { tb } from '../i18n/backend';
import NotebookCellView, { type CellContext } from '../components/notebook/NotebookCell.vue';
import NotebookTargetDialog from '../components/notebook/NotebookTargetDialog.vue';
import { ensureConnectedOrTell } from '../composables/connectionGate';
import { saveTextFile } from '../composables/files';
import { chartImages } from '../composables/notebookExport';
import {
  BYTES_DEFAULT, BYTES_RANGE, BYTES_SETTING, EditVersion, PREVIEW_NOTICE_SETTING, RESULTS_CAP_DEFAULT, RESULTS_CAP_RANGE, RESULTS_CAP_SETTING,
  ROWS_DEFAULT, ROWS_RANGE, ROWS_SETTING, RunLock, applyProjectDefault, assignCellNames, cellNameProblem, clamp, completeRows, defaultCellName,
  describeResult, emptyNotebook, emptyTarget, isResultsTarget, newCell, outcomeToResult, outline, promptVariant, resultToOutcome,
} from '../composables/notebookModel';
import {
  moveFileAliases, projectDefaultTarget, rememberAlias, rememberFileAlias, resolveTarget, targetText, type FileKey, type ResolvedTarget,
} from '../composables/notebookTargets';
import { SNIPPETS_SETTING, snippetsFor, type UserSnippet } from '../composables/snippets';
import { fileDocs, type FileDocHandle } from '../composables/tabDocument';
import { notebookTitle } from '../composables/tabTitle';
import { registerEditor, useAiStore } from '../stores/ai';
import { dbKey, objKey, useConnectionsStore } from '../stores/connections';
import { useOutputStore } from '../stores/output';
import { useProjectsStore } from '../stores/projects';
import { useSettingsStore } from '../stores/settings';
import { sameJson } from '../composables/chartSettings';
import { readJson, writeJson } from '../stores/storage';
import { busyTabs, closeGuards, closedNotebooks, notebookDraftKey, notebookSessions, useTabsStore, type NotebookTab } from '../stores/tabs';
import { useUiStore } from '../stores/ui';

// A notebook (docs/notebooks.md): SQL and text cells one under another,
// each SQL cell with its result under it. It's a Jupyter .ipynb file: a
// standalone one, a project's (in a git repo: saved without results by
// default, also a foreign notebook that doesn't say), or a new one not
// saved yet (its draft survives a restart).
// A cell runs exactly as the query editor runs: same command, same
// confirmation of dangerous statements, same read-only rules (the backend's),
// same history. One backend session per connection and database.
// A cell on "Resultados del notebook" runs DuckDB SQL over the results of
// the cells above, named as tables by the cells' names, on the tab's own
// in-memory DuckDB session (notebook_run_local; docs/notebooks.md).

const props = defineProps<{ tab: NotebookTab }>();

const tabs = useTabsStore();
const ui = useUiStore();
const conns = useConnectionsStore();
const projects = useProjectsStore();
const settings = useSettingsStore();
const output = useOutputStore();
const ai = useAiStore();

const nb = ref<Notebook | null>(null);
const loadError = ref<string | null>(null);
type SaveState = 'saved' | 'dirty' | 'saving' | 'error';
const saveState = ref<SaveState>('saved');
/** A project file changed on disk while edited here (`null` disk: deleted). */
const conflict = ref<FileStat | null | false>(false);
/** The project's file as last read or written. */
let stat: FileStat | null = null;
const warningsOpen = ref(true);

const project = computed(() => (props.tab.projectId ? projects.byId(props.tab.projectId) : undefined));
const title = computed(() => notebookTitle(props.tab));
/** A project in a git repo (not known yet: taken as one): its notebooks are
 *  saved without results unless the file says otherwise. */
const gitLinked = computed(() => !!props.tab.projectId && (project.value?.is_repo ?? true));
/** This notebook on this machine (names several connections share are chosen per file). */
const fileKey = computed<FileKey>(() => (props.tab.projectId && props.tab.projectPath
  ? `p:${props.tab.projectId}:${props.tab.projectPath}`
  : props.tab.path ? `f:${props.tab.path}` : `u:${props.tab.id}`));

// -- loading -----------------------------------------------------------------------------------
/** Results the file kept are big arrays: never made reactive. */
function raw(n: Notebook): Notebook {
  for (const c of n.cells) if (c.result) c.result = markRaw(c.result);
  return n;
}

async function load() {
  loadError.value = null;
  try {
    if (props.tab.projectId && props.tab.projectPath) {
      const f = await projectsApi.readFile(props.tab.projectId, props.tab.projectPath);
      stat = { path: f.path, exists: true, mtime_ms: f.mtime_ms, size: f.size, hash: f.hash };
      // A file just created from the tree is empty: a new notebook.
      nb.value = raw(f.text.trim()
        ? applyProjectDefault(await notebookApi.parse(f.text), gitLinked.value)
        : emptyNotebook(gitLinked.value, projectDefaultTarget(props.tab.projectId, fileKey.value)));
    } else if (props.tab.path) {
      nb.value = raw((await notebookApi.read(props.tab.path)).notebook);
    } else {
      nb.value = readJson<Notebook | null>(notebookDraftKey(props.tab.id), null) ?? emptyNotebook(false);
    }
    // Files from before cell names: each SQL cell gets one (saved with the file).
    assignCellNames(nb.value.cells);
    // An untitled one's draft is unsaved work: closing it asks.
    saveState.value = props.tab.untitled && nb.value.cells.some((c) => c.source.trim()) ? 'dirty' : 'saved';
    edits = new EditVersion();
    conflict.value = false;
    for (const k of Object.keys(live)) delete live[k];
    counter = Math.max(0, ...nb.value.cells.map((c) => c.execution_count ?? 0));
    activeId.value = nb.value.cells[0]?.id ?? null;
    editing.clear();
    syncTabTarget();
  } catch (e) {
    loadError.value = errorMessage(e);
  }
}
onMounted(load);

// -- changes, the dirty dot and the draft --------------------------------------------------------
let draftTimer: ReturnType<typeof setTimeout> | null = null;
/** Edits so far: a save ends "saved" only if none came in while it ran. */
let edits = new EditVersion();
/** Closed for good (tabs.release): no draft, nothing unsaved, from now on. */
let closed = false;
const isClosed = () => closed || closedNotebooks.has(props.tab.id);
/** Every change goes through here (the notebook is too big to deep-watch). */
function touch() {
  if (!nb.value || isClosed()) return;
  edits.bump();
  if (saveState.value !== 'saving') saveState.value = 'dirty';
  if (props.tab.untitled) {
    if (draftTimer) clearTimeout(draftTimer);
    draftTimer = setTimeout(writeDraft, 500);
  }
}
/** An untitled notebook's draft: its cells and summaries, not the rows. */
function writeDraft() {
  draftTimer = null;
  const n = nb.value;
  if (!n || !props.tab.untitled || isClosed()) return;
  writeJson(notebookDraftKey(props.tab.id), {
    ...n,
    cells: n.cells.map((c) => ({ ...c, result: c.result ? { ...c.result, results: c.result.results.map((s) => ({ ...s, rows: [], rows_kept: 0 })) } : null })),
  });
}
onBeforeUnmount(() => {
  if (closedNotebooks.delete(props.tab.id)) closed = true;
  if (draftTimer) { clearTimeout(draftTimer); writeDraft(); }
});

watch(saveState, (v) => {
  if (v === 'saved') delete ui.unsaved[props.tab.id];
  else ui.unsaved[props.tab.id] = v;
});
onBeforeUnmount(() => { delete ui.unsaved[props.tab.id]; });

// -- cells ----------------------------------------------------------------------------------------
const activeId = ref<string | null>(null);
/** Text cells being edited (the rest show rendered). */
const editing = reactive(new Set<string>());
const cellViews = new Map<string, InstanceType<typeof NotebookCellView>>();
function setCellView(id: string, v: unknown) {
  if (v) cellViews.set(id, v as InstanceType<typeof NotebookCellView>);
  else cellViews.delete(id);
}
const activeCell = computed(() => nb.value?.cells.find((c) => c.id === activeId.value) ?? null);

function indexOf(id: string) {
  return nb.value?.cells.findIndex((c) => c.id === id) ?? -1;
}
function focusCell(id: string) {
  activeId.value = id;
  void nextTick(() => cellViews.get(id)?.focus());
}
/** A new cell at `at` (default: after the active one, else at the end). */
function addCell(kind: 'sql' | 'markdown', at?: number, source = ''): NotebookCell {
  const n = nb.value!;
  const cell = newCell(kind, source);
  if (kind === 'sql') cell.language = languageOf(cell);
  const i = Math.max(0, Math.min(at ?? (activeId.value ? indexOf(activeId.value) + 1 : n.cells.length), n.cells.length));
  if (kind === 'sql') cell.name = defaultCellName(n.cells, i);
  n.cells.splice(i, 0, cell);
  if (kind === 'markdown' && !source) editing.add(cell.id);
  touch();
  return cell;
}
function setSource(cell: NotebookCell, text: string) {
  if (cell.source === text) return;
  cell.source = text;
  touch();
}
function move(cell: NotebookCell, by: -1 | 1) {
  const cells = nb.value!.cells;
  const i = indexOf(cell.id);
  const j = i + by;
  if (i < 0 || j < 0 || j >= cells.length) return;
  cells.splice(i, 1);
  cells.splice(j, 0, cell);
  touch();
}
function removeCell(cell: NotebookCell) {
  const cells = nb.value!.cells;
  const i = indexOf(cell.id);
  if (i < 0) return;
  cells.splice(i, 1);
  delete live[cell.id];
  if (activeId.value === cell.id) activeId.value = cells[Math.min(i, cells.length - 1)]?.id ?? null;
  touch();
}
function changeKind(cell: NotebookCell, kind: 'sql' | 'markdown') {
  cell.kind = kind;
  cell.language = kind === 'sql' ? languageOf(cell) : null;
  if (kind === 'sql' && !cell.name) cell.name = defaultCellName(nb.value!.cells, indexOf(cell.id));
  if (kind === 'markdown') { cell.result = null; cell.execution_count = null; delete live[cell.id]; editing.add(cell.id); }
  touch();
}
function setEditing(cell: NotebookCell, on: boolean) {
  if (on) editing.add(cell.id); else editing.delete(cell.id);
}
/** A new name (its table for "Resultados del notebook" cells): a plain identifier, unique in the notebook. */
function renameCell(cell: NotebookCell, name: string) {
  const problem = cellNameProblem(nb.value!.cells, cell.id, name);
  if (problem) { ElMessage.error({ message: t(`notebook:name.${problem}`, { name }), duration: 4000 }); return; }
  cell.name = name;
  touch();
}
function toggleKeep(cell: NotebookCell) {
  cell.keep_result = !cell.keep_result;
  touch();
}
/** The result's view or chart: stored in the cell (dirty only if it changed). */
function setChart(cell: NotebookCell, chart: Record<string, unknown>) {
  if (sameJson(chart, cell.chart)) return;
  cell.chart = chart;
  touch();
}
function clearResults() {
  for (const c of nb.value?.cells ?? []) { c.result = null; c.execution_count = null; }
  for (const k of Object.keys(live)) delete live[k];
  counter = 0;
  touch();
}
const saveWithout = computed({
  get: () => !(nb.value?.save_results ?? true),
  set: (v: boolean) => { if (nb.value) { nb.value.save_results = !v; touch(); } },
});

// -- targets ------------------------------------------------------------------------------------
/** The target a cell runs on as the file says it: its own, else the
 *  notebook's, else (in a project) the project's active base. */
function storedTarget(cell: NotebookCell | null): NotebookTarget | null {
  if (cell && !emptyTarget(cell.target)) return cell.target;
  if (!emptyTarget(nb.value?.target)) return nb.value!.target;
  return props.tab.projectId ? projectDefaultTarget(props.tab.projectId, fileKey.value) : null;
}
/** Re-read when connections, projects or aliases change. */
const aliasSeq = ref(0);
function resolved(cell: NotebookCell | null) {
  void aliasSeq.value;
  return resolveTarget(storedTarget(cell), props.tab.projectId, fileKey.value);
}
/** The cell runs on "Resultados del notebook" (its own target, else the notebook's). */
function isLocal(cell: NotebookCell | null): boolean {
  return isResultsTarget(storedTarget(cell));
}
/** The language a SQL cell is written in: its target driver's ("sql", "redis"…). */
function languageOf(cell: NotebookCell): string {
  if (isLocal(cell)) return 'sql';
  const r = resolved(cell);
  return (r.ok ? conns.driverOf(r.target.connectionId)?.language : null) ?? cell.language ?? 'sql';
}
/** The tab's own connection (its group, the assistant's context): the notebook's target. */
function syncTabTarget() {
  const r = resolved(null);
  if (r.ok) tabs.setNotebookTarget(props.tab.id, r.target.connectionId, r.target.database);
}
watch(() => [conns.list.length, projects.list.length] as const, syncTabTarget);

type TargetDialog =
  | { mode: 'pick'; cell: NotebookCell | null; resolve: (ok: boolean) => void }
  /** `candidates`: several local connections have that name (ids). */
  | { mode: 'map'; name: string; engine: string | null; candidates: string[] | null; resolve: (ok: boolean) => void };
const dialog = ref<TargetDialog | null>(null);
/** One dialog at a time: a second request while one is open is refused (false). */
function openDialog(d: TargetDialog) {
  if (dialog.value) d.resolve(false);
  else dialog.value = d;
}
function pickTarget(cell: NotebookCell | null): Promise<boolean> {
  return new Promise((resolve) => openDialog({ mode: 'pick', cell, resolve }));
}
function onPicked(target: NotebookTarget | null) {
  const d = dialog.value;
  if (d?.mode !== 'pick' || !nb.value) return;
  if (d.cell) d.cell.target = target;
  else nb.value.target = target;
  dialog.value = null;
  touch();
  syncTabTarget();
  d.resolve(true);
}
function onMapped(connectionId: string) {
  const d = dialog.value;
  if (d?.mode !== 'map') return;
  // A name several connections share: this file's choice; else the machine's.
  if (d.candidates) rememberFileAlias(fileKey.value, d.name, connectionId);
  else rememberAlias(d.name, connectionId);
  aliasSeq.value++;
  dialog.value = null;
  syncTabTarget();
  d.resolve(true);
}
function closeDialog() {
  const d = dialog.value;
  dialog.value = null;
  d?.resolve(false);
}
onBeforeUnmount(() => dialog.value?.resolve(false));

/** The cell's target resolved, asking what's missing (once): null when the user gave up. */
async function ensureTarget(cell: NotebookCell): Promise<ResolvedTarget | null> {
  for (let tries = 0; tries < 3; tries++) {
    const r = resolved(cell);
    if (r.ok) return r.target;
    if (r.problem === 'none') {
      if (!(await pickTarget(null))) return null;
    } else if (r.problem === 'connection') {
      const engine = cell.result?.engine ?? null;
      const candidates = r.candidates ?? null;
      const ok = await new Promise<boolean>((resolve) => openDialog({ mode: 'map', name: r.name ?? '', engine, candidates, resolve }));
      if (!ok) return null;
    } else {
      // An environment not bound on this machine: the project says which base it is.
      ElMessage.info({ message: t('notebook:target.bindEnvironment', { env: r.name ?? '' }), duration: 4500 });
      if (props.tab.projectId) projects.dialog = { kind: 'target', projectId: props.tab.projectId, alias: r.name };
      return null;
    }
  }
  return null;
}

/** An environment marked `confirm_run` asks before running on it (as the
 *  editor does: once per run; "Ejecutar todo" asks once per target, kept in `asked`). */
async function confirmEnvironment(target: ResolvedTarget, asked: Set<string> | null): Promise<boolean> {
  if (!target.env?.confirm_run) return true;
  const key = `${target.env.name}\u0000${target.connectionId}\u0000${target.database}`;
  if (asked?.has(key)) return true;
  try {
    await ElMessageBox.confirm(
      t('projects:file.confirmRun', { env: target.env.name, where: target.label }),
      t('projects:file.confirmRunTitle'),
      { type: 'warning', confirmButtonText: t('common:run'), cancelButtonText: t('common:cancel'), confirmButtonClass: 'el-button--danger' },
    );
    asked?.add(key);
    return true;
  } catch {
    return false;
  }
}

// -- the editor's context per cell: language, completion -------------------------------------------
function schemaFor(connectionId: string, database: string): Record<string, string[]> {
  const d = conns.driverOf(connectionId);
  const out: Record<string, string[]> = {};
  for (const o of conns.objects[dbKey(connectionId, database)]?.items ?? []) {
    if (!d?.object_kinds.find((k) => k.id === o.kind)?.has_columns) continue;
    const cols = conns.columns[objKey(connectionId, database, o.schema, o.name)]?.items.map((c) => c.name) ?? [];
    out[o.name] = cols;
    if (o.schema) out[`${o.schema}.${o.name}`] = cols;
  }
  return out;
}
const userSnippets = computed(() => settings.get<UserSnippet[]>(SNIPPETS_SETTING, []));
/** A "Resultados del notebook" cell's completion: the cells above as tables, with their columns. */
function localSchema(cell: NotebookCell): Record<string, string[]> {
  const out: Record<string, string[]> = {};
  const cells = nb.value?.cells ?? [];
  for (const c of cells.slice(0, Math.max(0, cells.indexOf(cell)))) {
    if (c.kind !== 'sql' || !c.name) continue;
    const cols = live[c.id]?.outcome.results.find((r) => r.columns.length)?.columns.map((x) => x.name)
      ?? c.result?.results.find((r) => r.columns.length)?.columns.map((x) => x.name) ?? [];
    out[c.name] = cols;
  }
  return out;
}
const contexts = computed<Record<string, CellContext>>(() => {
  const out: Record<string, CellContext> = {};
  for (const c of nb.value?.cells ?? []) {
    if (c.kind !== 'sql') continue;
    if (isLocal(c)) {
      const duck = conns.drivers.find((d) => d.id === 'duckdb');
      out[c.id] = {
        language: 'sql', dialect: duck?.dialect, schema: localSchema(c), snippets: snippetsFor(duck, userSnippets.value),
        target: t('notebook:target.results'), overridden: !emptyTarget(c.target), unresolved: false,
      };
      continue;
    }
    const r = resolved(c);
    const d = r.ok ? conns.driverOf(r.target.connectionId) : undefined;
    out[c.id] = {
      language: d?.language ?? 'sql',
      dialect: d?.dialect,
      schema: r.ok ? schemaFor(r.target.connectionId, r.target.database) : {},
      snippets: snippetsFor(d, userSnippets.value),
      target: r.ok ? r.target.label : r.problem === 'none' ? t('notebook:target.none') : r.problem === 'connection' ? t('notebook:target.unknown', { name: r.name ?? '' }) : t('notebook:target.unboundEnv', { env: r.name ?? '' }),
      overridden: !emptyTarget(c.target),
      unresolved: !r.ok,
    };
  }
  return out;
});
// Completion reads the explorer's cache: load each target's objects once connected.
watch(() => [...new Set((nb.value?.cells ?? []).flatMap((c) => {
  const r = c.kind === 'sql' ? resolved(c) : null;
  return r?.ok && conns.isConnected(r.target.connectionId) ? [`${r.target.connectionId}\u0000${r.target.database}`] : [];
}))].join('\u0001'), (keys) => {
  for (const k of keys ? keys.split('\u0001') : []) {
    const [c, d] = k.split('\u0000');
    if (!conns.objects[dbKey(c, d)]) conns.loadObjects(c, d);
  }
}, { immediate: true });
function needNames(cell: NotebookCell) {
  const r = resolved(cell);
  if (r.ok && !conns.isConnected(r.target.connectionId)) void conns.ensureConnected(r.target.connectionId, { auto: true });
}
function needColumns(cell: NotebookCell, path: string[]) {
  const r = resolved(cell);
  if (!r.ok) return;
  const { connectionId, database } = r.target;
  const [schemaName, name] = path.length > 1 ? path : [null, path[0]];
  const obj = (conns.objects[dbKey(connectionId, database)]?.items ?? []).find((o) => o.name === name && (schemaName === null || o.schema === schemaName));
  if (obj && !conns.columns[objKey(connectionId, database, obj.schema, obj.name)]) conns.loadColumns(connectionId, database, obj);
}
// The tables the active cell names get their columns (as the editor does).
let columnsTimer: ReturnType<typeof setTimeout> | null = null;
watch(() => activeCell.value?.kind === 'sql' ? activeCell.value.source : '', (text) => {
  if (columnsTimer) clearTimeout(columnsTimer);
  columnsTimer = setTimeout(() => {
    const cell = activeCell.value;
    const r = cell ? resolved(cell) : null;
    if (!r?.ok) return;
    const { connectionId, database } = r.target;
    const words = new Set(text.toLowerCase().match(/[\w$#]+/g) ?? []);
    const d = conns.driverOf(connectionId);
    let started = 0;
    for (const o of conns.objects[dbKey(connectionId, database)]?.items ?? []) {
      if (started >= 20) break;
      if (!words.has(o.name.toLowerCase()) || conns.columns[objKey(connectionId, database, o.schema, o.name)]) continue;
      if (!d?.object_kinds.find((k) => k.id === o.kind)?.has_columns) continue;
      conns.loadColumns(connectionId, database, o);
      started++;
    }
  }, 400);
});
onBeforeUnmount(() => { if (columnsTimer) clearTimeout(columnsTimer); });

// -- running ----------------------------------------------------------------------------------------
/** This session's runs, with every row (the file keeps only a preview). Not deep. */
/** `source`: null for a "Resultados del notebook" cell (no connection);
 *  `run`: the run counter it got; `loaded`: such a cell's tables. */
interface LiveRun { outcome: QueryOutcome; source: { connectionId: string; database: string; sql: string } | null; run?: number; loaded?: LoadedTable[] }
const live = shallowReactive<Record<string, LiveRun>>({});
/** A run's id, unique in this app session (unlike the [n] counter, never
 *  reset nor repeated when the view mounts again): what the backend's cached
 *  tables of "Resultados del notebook" cells are keyed by. Microseconds. */
let lastRunId = 0;
function nextRunId(): number {
  lastRunId = Math.max(lastRunId + 1, Math.floor((performance.timeOrigin + performance.now()) * 1000));
  return lastRunId;
}
const runningId = ref<string | null>(null);
let runningSession: string | null = null;
const runAll = ref(false);
let stopRequested = false;
let counter = 0;
const settingsMaxRows = computed(() => settings.get<number>('query.maxRows', 5000));

const sessionOf = (connectionId: string, database: string) => `${props.tab.id}:${connectionId}:${database}`;

function confirmUnsafe(script: string, found: UnsafeDml[]): Promise<boolean> {
  const items = found.map((u) => h('div', { class: 'nb-unsafe' }, [
    h('div', { class: 'nb-unsafe-line' }, t('results:messages.line', { line: u.line })),
    h('pre', { class: 'nb-unsafe-code' }, script.slice(u.start, u.end).trim()),
  ]));
  return ElMessageBox.confirm(h('div', [h('p', t('query:unsafe.intro', { count: found.length })), ...items]), t('query:unsafe.title'), {
    type: 'warning', confirmButtonText: t('query:unsafe.run'), cancelButtonText: t('common:cancel'), confirmButtonClass: 'el-button--danger', customClass: 'nb-unsafe-box',
  }).then(() => true, () => false);
}

// -- transactions: per session, as the editor tracks its tab's ------------------------------------
interface TxInfo { state: TxState; connectionId: string; database: string; label: string }
/** Sessions with a transaction open (or failed), as their last run left them. */
const txs = reactive<Record<string, TxInfo>>({});
const openTxs = computed(() => Object.entries(txs).map(([sessionId, x]) => ({ sessionId, ...x })));
const txBusy = ref(false);
function setTx(sessionId: string, state: TxState | null, info: Omit<TxInfo, 'state'> | undefined) {
  if ((state === 'open' || state === 'failed') && info) txs[sessionId] = { ...info, state };
  else delete txs[sessionId];
}
/** Confirmar / Deshacer one session's transaction. True when it went through. */
async function endTransaction(sessionId: string, commit: boolean): Promise<boolean> {
  const info = txs[sessionId];
  txBusy.value = true;
  try {
    setTx(sessionId, await (commit ? api.commitTab(sessionId) : api.rollbackTab(sessionId)), info);
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
// Closing with a transaction open asks, one session at a time: Confirmar / Deshacer / Cancelar.
const txAsk = ref<{ sessionId: string; label: string; resolve: (ok: boolean) => void } | null>(null);
const txError = ref<string | null>(null);
/** True when no transaction is left open (the user settled each one). */
async function settleTransactions(): Promise<boolean> {
  for (const [id, info] of Object.entries(txs)) {
    try { setTx(id, await api.tabTransactionState(id), info); } catch { /* keep the last known state */ }
  }
  for (const tx of openTxs.value) {
    if (!txs[tx.sessionId]) continue;
    tabs.activate(props.tab.id);
    txError.value = null;
    const ok = await new Promise<boolean>((resolve) => { txAsk.value = { sessionId: tx.sessionId, label: tx.label, resolve }; });
    if (!ok) return false;
  }
  return true;
}
async function answerTx(choice: 'commit' | 'rollback' | 'cancel') {
  const a = txAsk.value;
  if (!a) return;
  if (choice !== 'cancel' && !(await endTransaction(a.sessionId, choice === 'commit'))) return;
  txAsk.value = null;
  a.resolve(choice !== 'cancel');
}
onBeforeUnmount(() => txAsk.value?.resolve(false));

/** One cell at a time: taken before the first await (dialogs included), so
 *  ⌘↵, ⇧↵, the buttons and "Ejecutar todo" can't start a second run. */
const lock = reactive(new RunLock());

/** Run a SQL cell (`sql`: a selection of it). True when it ran without errors. */
async function runCell(cell: NotebookCell, sql: string | null = null): Promise<boolean> {
  if (cell.kind !== 'sql' || !lock.take()) return false;
  try {
    return await runHeld(cell, sql, null);
  } finally {
    lock.release();
  }
}

/** The run itself, with the lock held. `asked`: the confirm_run targets
 *  already confirmed in this "Ejecutar todo". */
async function runHeld(cell: NotebookCell, sql: string | null, asked: Set<string> | null): Promise<boolean> {
  const script = (sql ?? cell.source).trim();
  if (!script) return true;
  if (isLocal(cell)) return runLocal(cell, script, asked);
  const target = await ensureTarget(cell);
  if (!target || !(await confirmEnvironment(target, asked))) return false;
  if (!(await ensureConnectedOrTell(target.connectionId))) return false;
  const { connectionId, database } = target;
  const sessionId = sessionOf(connectionId, database);
  let set = notebookSessions.get(props.tab.id);
  if (!set) notebookSessions.set(props.tab.id, (set = new Set()));
  set.add(sessionId);
  runningId.value = cell.id;
  runningSession = sessionId;
  const ranAt = new Date();
  const where = target.label;
  const driver = conns.driverOf(connectionId);
  // The cell is in its target's language (the file says so).
  if (driver?.language) cell.language = driver.language;
  // What a cancel that closes the session would roll back.
  const txWasOpen = !!txs[sessionId];
  const txInfo = { connectionId, database, label: where };
  let ok = false;
  try {
    let confirmed = false;
    for (;;) {
      const o = await api.executeQuery({
        sessionId, connectionId, database, sql: script, maxRows: settingsMaxRows.value, record: true, mode: 'auto', confirmedUnsafe: confirmed,
        autocommit: driver?.supports_manual_transactions ? true : null,
      }).finally(() => { ui.historySeq++; });
      if (o.needs_confirmation?.length && !confirmed) {
        // Nothing ran: the previous result stays while the user decides.
        if (!(await confirmUnsafe(script, o.needs_confirmation))) return false;
        confirmed = true;
        continue;
      }
      // A cancel that closed the session took its open transaction with it.
      if (o.session_closed && txWasOpen) ElMessage.warning({ message: t('query:tx.lostOnCancel'), duration: 5000 });
      setTx(sessionId, o.transaction ?? null, txInfo);
      cell.execution_count = ++counter;
      live[cell.id] = { outcome: markRaw(o), source: { connectionId, database, sql: script }, run: nextRunId() };
      cell.result = markRaw(outcomeToResult(o, { script, ranAt, engine: driver?.id ?? null, targetLabel: where }));
      const firstLine = script.split('\n').find((l) => l.trim())?.trim().slice(0, 120) ?? '';
      const failed = !!o.error || !!o.errors?.length;
      if (failed) output.add('error', `${firstLine}\n${tb(o.error ?? o.errors?.[0]?.message ?? '')}`, { where, elapsedMs: o.elapsed_ms });
      else output.add('info', firstLine, { where, elapsedMs: o.elapsed_ms });
      ok = !failed;
      break;
    }
  } catch (e) {
    const message = errorMessage(e);
    const o: QueryOutcome = { results: [], messages: [], error: message, elapsed_ms: Date.now() - ranAt.getTime(), plans: [] };
    cell.execution_count = ++counter;
    live[cell.id] = { outcome: markRaw(o), source: { connectionId, database, sql: script }, run: nextRunId() };
    cell.result = markRaw(outcomeToResult(o, { script, ranAt, engine: driver?.id ?? null, targetLabel: where }));
    output.add('error', message, { where });
    void conns.recheckAfter(connectionId, e);
  } finally {
    runningId.value = null;
    runningSession = null;
    touch();
  }
  return ok;
}

/** A "Resultados del notebook" cell: its DuckDB SQL over the cells above,
 *  each one it names loaded as a table (the backend says which; their
 *  targets are resolved and connected here first). A source's rows come
 *  from its complete run here, else its query runs again on its own
 *  connection (read-only; a write is refused there, never re-run). */
async function runLocal(cell: NotebookCell, script: string, asked: Set<string> | null): Promise<boolean> {
  const cells = nb.value!.cells;
  const above = cells.slice(0, indexOf(cell.id)).filter((c) => c.kind === 'sql' && c.name && c.source.trim());
  const base: LocalSource[] = above.map((c) => ({ name: c.name!, local: isLocal(c), sql: c.source.trim(), connection_id: null, database: null, run: null, rows: null }));
  const sessionId = `nbx:${props.tab.id}`;
  let set = notebookSessions.get(props.tab.id);
  if (!set) notebookSessions.set(props.tab.id, (set = new Set()));
  set.add(sessionId);
  runningId.value = cell.id;
  runningSession = sessionId;
  const ranAt = new Date();
  const where = t('notebook:target.results');
  cell.language = 'sql';
  let ok = false;
  try {
    const needed = new Set((await notebookApi.localRefs(script, base)).map((n) => n.toLowerCase()));
    const sources: LocalSource[] = [];
    for (const [k, c] of above.entries()) {
      const s = { ...base[k] };
      if (!s.local && needed.has(s.name.toLowerCase())) {
        const target = await ensureTarget(c);
        if (!target || !(await confirmEnvironment(target, asked))) return false;
        if (!(await ensureConnectedOrTell(target.connectionId))) return false;
        s.connection_id = target.connectionId;
        s.database = target.database;
        // Its run here, if it was of this very query on this target and nothing cut it.
        const l = live[c.id];
        const rows = l?.source && l.source.sql === s.sql && l.source.connectionId === target.connectionId && l.source.database === target.database ? completeRows(l.outcome) : null;
        if (rows) { s.rows = rows; s.run = l!.run ?? null; }
      }
      sources.push(s);
    }
    const rowCap = clamp(settings.get<number>(RESULTS_CAP_SETTING, RESULTS_CAP_DEFAULT), RESULTS_CAP_RANGE);
    const r = await notebookApi.runLocal({ tabId: props.tab.id, sql: script, maxRows: settingsMaxRows.value, rowCap, sources, notebookPath: props.tab.path ?? null });
    const o = r.outcome;
    cell.execution_count = ++counter;
    live[cell.id] = { outcome: markRaw(o), source: null, run: nextRunId(), loaded: markRaw(r.loaded) };
    cell.result = markRaw(outcomeToResult(o, { script, ranAt, engine: 'duckdb', targetLabel: where }));
    const firstLine = script.split('\n').find((l) => l.trim())?.trim().slice(0, 120) ?? '';
    const failed = !!o.error || !!o.errors?.length;
    if (failed) output.add('error', `${firstLine}\n${tb(o.error ?? o.errors?.[0]?.message ?? '')}`, { where, elapsedMs: o.elapsed_ms });
    else output.add('info', firstLine, { where, elapsedMs: o.elapsed_ms });
    ok = !failed;
  } catch (e) {
    const message = errorMessage(e);
    const o: QueryOutcome = { results: [], messages: [], error: message, elapsed_ms: Date.now() - ranAt.getTime(), plans: [] };
    cell.execution_count = ++counter;
    live[cell.id] = { outcome: markRaw(o), source: null, run: nextRunId() };
    cell.result = markRaw(outcomeToResult(o, { script, ranAt, engine: 'duckdb', targetLabel: where }));
    output.add('error', message, { where });
  } finally {
    runningId.value = null;
    runningSession = null;
    touch();
  }
  return ok;
}

/** Run from cell `from` to the end; stops at the first failure (or Cancel). */
async function runFrom(from: number) {
  if (!nb.value || !lock.take()) return;
  runAll.value = true;
  stopRequested = false;
  const asked = new Set<string>();
  try {
    for (const cell of nb.value.cells.slice(from)) {
      if (cell.kind !== 'sql' || !cell.source.trim()) continue;
      activeId.value = cell.id;
      if (!(await runHeld(cell, null, asked)) || stopRequested) break;
    }
  } finally {
    runAll.value = false;
    lock.release();
  }
}

/** ⇧↵: run, then the next cell (a new one at the end). */
async function runAndNext(cell: NotebookCell) {
  if (lock.held) return;
  const i = indexOf(cell.id);
  const next = nb.value!.cells[i + 1] ?? addCell('sql', i + 1);
  focusCell(next.id);
  await runCell(cell);
}
function textNext(cell: NotebookCell) {
  editing.delete(cell.id);
  const i = indexOf(cell.id);
  const next = nb.value!.cells[i + 1] ?? addCell('sql', i + 1);
  focusCell(next.id);
}

function cancel() {
  stopRequested = true;
  if (runningSession) api.cancelQuery(runningSession).catch(() => {});
}
/** Resolves once no run holds the lock. */
function whenIdle(): Promise<void> {
  if (!lock.held) return Promise.resolve();
  return new Promise((resolve) => {
    const stop = watch(() => lock.held, (held) => { if (!held) { stop(); resolve(); } });
  });
}

// Busy (running, or a transaction open): the close guard asks, and the tab
// is "running" for the rest of the app.
watch(() => lock.held || openTxs.value.length > 0, (on) => { if (on) busyTabs.add(props.tab.id); else busyTabs.delete(props.tab.id); });
onBeforeUnmount(() => busyTabs.delete(props.tab.id));

// -- saving ---------------------------------------------------------------------------------------
function limits(): PreviewLimits {
  return {
    rows: clamp(settings.get<number>(ROWS_SETTING, ROWS_DEFAULT), ROWS_RANGE),
    bytes: clamp(settings.get<number>(BYTES_SETTING, BYTES_DEFAULT), BYTES_RANGE),
  };
}

/** The first save of a notebook with results says the file has data in it (once). */
async function previewNotice() {
  const n = nb.value!;
  if (!n.save_results || settings.get<boolean>(PREVIEW_NOTICE_SETTING, false)) return;
  if (!n.cells.some((c) => c.keep_result && c.result?.results.some((s) => s.rows.length))) return;
  settings.set(PREVIEW_NOTICE_SETTING, true);
  await ElMessageBox.alert(t('notebook:notice.body'), t('notebook:notice.title'), { confirmButtonText: t('notebook:notice.ok') }).catch(() => {});
}

let pending: Promise<boolean> | null = null;
function save(): Promise<boolean> {
  if (pending) return pending;
  pending = write(true).finally(() => { pending = null; });
  return pending;
}

async function write(check: boolean): Promise<boolean> {
  const n = nb.value;
  if (!n) return false;
  await previewNotice();
  const before = saveState.value;
  saveState.value = 'saving';
  try {
    // Edits from here on (while it saves) leave it unsaved.
    let started = edits.start();
    if (props.tab.projectId && props.tab.projectPath) {
      const text = await notebookApi.serialize(n, limits());
      const out = await projectsApi.writeFile({
        id: props.tab.projectId, path: props.tab.projectPath, text, eol: 'lf', bom: false,
        expectedHash: check && stat?.exists ? stat.hash : null,
      });
      if (out.conflict || !out.written) {
        conflict.value = out.stat.exists ? out.stat : null;
        saveState.value = 'dirty';
        ElMessage.warning({ message: t('notebook:save.conflict'), duration: 5000 });
        return false;
      }
      stat = out.stat;
      conflict.value = false;
      projects.refreshStatusSoon(props.tab.projectId);
    } else {
      let path = props.tab.path;
      if (!path) {
        const picked = await saveDialog({ defaultPath: `${title.value}.ipynb`, filters: [{ name: t('notebook:save.filter'), extensions: ['ipynb'] }] });
        if (!picked) { saveState.value = edits.end(started) === 'saved' ? before : 'dirty'; return false; }
        path = /\.ipynb$/i.test(picked) ? picked : `${picked}.ipynb`;
        // What's written is the notebook as it is now.
        started = edits.start();
      }
      await notebookApi.write(path, n, limits());
      if (!props.tab.path) {
        moveFileAliases(fileKey.value, `f:${path}`);
        tabs.notebookSaved(props.tab.id, path);
      }
    }
    saveState.value = edits.end(started);
    return true;
  } catch (e) {
    saveState.value = 'error';
    ElMessage.error(t('notebook:save.failed', { name: title.value, error: errorMessage(e) }));
    return false;
  }
}

async function reloadFromDisk() {
  await load();
}

// The quit guard and the project watcher see this tab like a project file.
const handle: FileDocHandle = {
  tabId: props.tab.id,
  title: () => (project.value ? `${project.value.name} › ${title.value}` : title.value),
  projectId: () => props.tab.projectId ?? '',
  path: () => props.tab.projectPath ?? props.tab.path ?? '',
  dirty: () => saveState.value === 'dirty' || saveState.value === 'error',
  known: () => stat,
  save,
  external(disk) {
    if (saveState.value === 'saving' || !props.tab.projectId) return;
    if (!disk.exists) { conflict.value = null; return; }
    if (stat && disk.hash === stat.hash) return;
    if (saveState.value === 'saved') void load();
    else conflict.value = disk;
  },
  projectGone() { /* saving says so */ },
};
fileDocs.set(props.tab.id, handle);
onBeforeUnmount(() => { if (fileDocs.get(props.tab.id) === handle) fileDocs.delete(props.tab.id); });

// Closing asks first, and only then stops anything: a cell running (stop it
// and close?), each open transaction (Confirmar / Deshacer / Cancelar) and
// unsaved changes (Guardar / No guardar / Cancelar).
async function guardClose(): Promise<boolean> {
  if (lock.held) {
    tabs.activate(props.tab.id);
    try {
      await ElMessageBox.confirm(t('notebook:close.running', { name: title.value }), t('notebook:close.runningTitle'), {
        type: 'warning', confirmButtonText: t('notebook:close.stop'), cancelButtonText: t('common:cancel'),
      });
    } catch {
      return false;
    }
    cancel();
    closeDialog();
    await whenIdle();
  }
  if (!(await settleTransactions())) return false;
  if (saveState.value !== 'dirty' && saveState.value !== 'error') return true;
  tabs.activate(props.tab.id);
  try {
    await ElMessageBox.confirm(t('notebook:close.ask', { name: title.value }), t('notebook:close.title'), {
      type: 'warning', confirmButtonText: t('common:save'), cancelButtonText: t('notebook:close.dontSave'), distinguishCancelAndClose: true,
    });
  } catch (action) {
    return action === 'cancel';
  }
  return save();
}
watch(() => saveState.value === 'dirty' || saveState.value === 'error' || lock.held || openTxs.value.length > 0, (on) => {
  if (on) closeGuards.set(props.tab.id, guardClose);
  else closeGuards.delete(props.tab.id);
}, { immediate: true });
onBeforeUnmount(() => closeGuards.delete(props.tab.id));

// -- export ---------------------------------------------------------------------------------------
async function exportAs(format: 'html' | 'markdown') {
  if (!nb.value) return;
  try {
    const n = nb.value;
    const base = title.value.replace(/\.ipynb$/i, '');
    // Each cell's charts as images (PNG, light theme).
    const images = chartImages(n, (id) => live[id]?.outcome ?? null);
    if (format === 'markdown' && images.length) {
      // The .md and its images in `<name>_files/`: the command writes both.
      const picked = await saveDialog({ defaultPath: `${base}.md`, filters: [{ name: 'Markdown', extensions: ['md'] }] });
      if (!picked) return;
      const path = /\.(md|markdown)$/i.test(picked) ? picked : `${picked}.md`;
      await notebookApi.export(n, format, limits(), images, path);
      ElMessage.success({ message: t('notebook:toolbar.exportedWithCharts', { count: images.length }), duration: 2500 });
      return;
    }
    const text = await notebookApi.export(n, format, limits(), format === 'html' ? images : []);
    await saveTextFile(text, `${base}.${format === 'html' ? 'html' : 'md'}`, [{ name: format === 'html' ? 'HTML' : 'Markdown', extensions: [format === 'html' ? 'html' : 'md'] }]);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

// -- the assistant: it proposes, the user runs (docs/ai-assistant.md). Rows never go. ----------------
const activeSql = () => (activeCell.value?.kind === 'sql' ? activeCell.value : null);
const unregisterAi = registerEditor(props.tab.id, {
  // The active cell's target (each cell may run elsewhere), else the notebook's.
  target: () => {
    const r = resolved(activeSql());
    return r.ok ? { connectionId: r.target.connectionId, database: r.target.database } : null;
  },
  text: () => activeSql()?.source ?? '',
  selection: () => (activeSql() ? cellViews.get(activeSql()!.id)?.selectionText() ?? '' : ''),
  lastError: () => activeSql()?.result?.error ?? null,
  append: (code) => focusCell(addCell('sql', undefined, code).id),
  replace: (code) => {
    const c = activeSql();
    if (!c) { focusCell(addCell('sql', undefined, code).id); return; }
    const v = cellViews.get(c.id);
    if (v) v.replaceAll(code); else setSource(c, code);
  },
  appendText: (md) => { const c = addCell('markdown', undefined, md); editing.delete(c.id); activeId.value = c.id; },
});
onBeforeUnmount(unregisterAi);

function ask(prompt: string) {
  ui.aiOpen = true;
  if (ai.running) { ElMessage.info({ message: t('notebook:ai.busy'), duration: 2500 }); return; }
  void ai.send(prompt);
}
/** The cell's target engine, for the prompts: SQL engines are asked for
 *  SQL (GROUP BY…), the others in their own language. */
function promptArgs(cell: NotebookCell) {
  if (isLocal(cell.kind === 'sql' ? cell : activeSql())) return { lang: 'sql', engine: 'DuckDB', native: false };
  const r = resolved(cell.kind === 'sql' ? cell : activeSql());
  const d = r.ok ? conns.driverOf(r.target.connectionId) : undefined;
  const lang = d?.language ?? (cell.kind === 'sql' ? cell.language : null) ?? 'sql';
  return { lang, engine: d?.name ?? lang, native: promptVariant(lang) === 'native' };
}
function explain(cell: NotebookCell) {
  activeId.value = cell.id;
  const p = promptArgs(cell);
  ask(t(p.native ? 'notebook:ai.explainPromptNative' : 'notebook:ai.explainPrompt', { sql: cell.source.trim(), result: describeResult(cell.result), lang: p.lang, engine: p.engine }));
}
function propose(cell: NotebookCell) {
  activeId.value = cell.id;
  const p = promptArgs(cell);
  ask(t(p.native ? 'notebook:ai.proposePromptNative' : 'notebook:ai.proposePrompt', { outline: outline(nb.value!.cells, indexOf(cell.id)), lang: p.lang, engine: p.engine }));
}
function group(cell: NotebookCell) {
  activeId.value = cell.id;
  const p = promptArgs(cell);
  ask(t(p.native ? 'notebook:ai.groupPromptNative' : 'notebook:ai.groupPrompt', { sql: cell.source.trim(), result: describeResult(cell.result), lang: p.lang, engine: p.engine }));
}

// -- keys --------------------------------------------------------------------------------------------
function onKey(e: KeyboardEvent) {
  if ((e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey && e.key.toLowerCase() === 's') {
    e.preventDefault();
    void save();
  }
}

function fallbackOf(cell: NotebookCell): QueryOutcome | null {
  return cell.result ? resultToOutcome(cell.result) : null;
}
/** A full export runs the query again: only the query that ran here (a
 *  stored preview exports just its rows; the cell's text may have changed). */
function sourceOf(cell: NotebookCell) {
  return live[cell.id]?.source ?? null;
}
const notebookTarget = computed(() => {
  void aliasSeq.value;
  const r = resolved(null);
  return { text: r.ok ? r.target.label : targetText(storedTarget(null)), ok: r.ok || isLocal(null), env: r.ok ? r.target.env : null };
});
const breadcrumbDir = computed(() => {
  const p = props.tab.projectPath ?? '';
  return p.includes('/') ? p.slice(0, p.lastIndexOf('/') + 1) : '';
});
</script>

<template>
  <div v-if="loadError" class="nm-content">
    <el-alert type="error" :title="loadError" :closable="false" />
  </div>
  <div v-else class="nb" @keydown="onKey">
    <!-- Where it is: the project and path, or the file. -->
    <div class="nb-file">
      <el-icon class="nb-file-ic"><ei-notebook /></el-icon>
      <template v-if="tab.projectId">
        <button class="nb-crumb" :title="$t('projects:file.showInProjects')" @click="projects.focusProject(tab.projectId!)">{{ project?.name ?? '…' }}</button>
        <span class="nb-sep">›</span>
        <span class="nb-path nm-selectable" :title="tab.projectPath"><span class="nm-muted">{{ breadcrumbDir }}</span>{{ title }}</span>
      </template>
      <span v-else class="nb-path nm-selectable" :title="tab.path ?? ''">{{ tab.path ?? title }}</span>
      <div class="nm-spacer" />
      <span v-if="saveState === 'dirty'" class="nm-muted nb-lbl">{{ $t('projects:file.unsaved') }}</span>
    </div>

    <div v-if="conflict !== false" class="nb-banner" role="alert">
      <el-icon><ei-warning-filled /></el-icon>
      <span>{{ conflict ? $t('projects:file.changedOnDisk') : $t('projects:file.deleted') }}</span>
      <div class="nm-spacer" />
      <el-button v-if="conflict" size="small" @click="reloadFromDisk">{{ $t('projects:file.reload') }}</el-button>
      <el-button size="small" type="warning" @click="write(false)">{{ conflict ? $t('projects:file.overwrite') : $t('projects:file.saveAgain') }}</el-button>
    </div>
    <div v-if="nb?.warnings.length && warningsOpen" class="nb-banner info" role="status">
      <el-icon><ei-info-filled /></el-icon>
      <div class="nb-warnings"><div v-for="(w, i) in nb.warnings" :key="i">{{ tb(w) }}</div></div>
      <div class="nm-spacer" />
      <el-button size="small" link :aria-label="$t('common:close')" @click="warningsOpen = false"><el-icon><ei-close /></el-icon></el-button>
    </div>

    <div class="nm-toolbar nb-bar">
      <el-tooltip :content="$t('notebook:toolbar.targetTip')" placement="bottom" :show-after="300">
        <button class="nb-target" :class="{ warn: !notebookTarget.ok, env: !!notebookTarget.env, danger: notebookTarget.env?.confirm_run }" @click="pickTarget(null)">
          <el-icon><ei-coin /></el-icon><span>{{ notebookTarget.text }}</span><el-icon><ei-arrow-down /></el-icon>
        </button>
      </el-tooltip>
      <el-button v-if="!runningId && !runAll" type="primary" :disabled="!nb || lock.held" @click="runFrom(0)">
        <el-icon><ei-video-play /></el-icon>&nbsp;{{ $t('notebook:toolbar.runAll') }}
      </el-button>
      <el-button v-else type="danger" @click="cancel">
        <el-icon><ei-video-pause /></el-icon>&nbsp;{{ $t('common:cancel') }}
      </el-button>
      <el-button :disabled="lock.held" @click="clearResults">
        <el-icon><ei-delete /></el-icon><span class="nb-lbl">&nbsp;{{ $t('notebook:toolbar.clear') }}</span>
      </el-button>
      <el-tooltip :content="$t('notebook:toolbar.withoutResultsTip')" placement="bottom" :show-after="300">
        <el-checkbox v-model="saveWithout" size="small" class="nb-check"><span class="nb-lbl">{{ $t('notebook:toolbar.withoutResults') }}</span></el-checkbox>
      </el-tooltip>
      <!-- Open transactions (a cell ran BEGIN…): where, and Confirmar / Deshacer at hand. -->
      <div v-for="tx in openTxs" :key="tx.sessionId" class="nb-tx" :class="{ failed: tx.state === 'failed' }" role="status">
        <el-icon><ei-warning-filled /></el-icon>
        <span class="nb-tx-text" :title="tx.label">{{ tx.state === 'failed' ? $t('query:tx.failed') : $t('query:tx.open') }} · {{ tx.label }}</span>
        <el-button size="small" :disabled="lock.held || txBusy" :title="$t('query:tx.commitTip')" @click="endTransaction(tx.sessionId, true)">{{ $t('query:tx.commit') }}</el-button>
        <el-button size="small" :disabled="lock.held || txBusy" :title="$t('query:tx.rollbackTip')" @click="endTransaction(tx.sessionId, false)">{{ $t('query:tx.rollback') }}</el-button>
      </div>
      <div class="nm-spacer" />
      <el-dropdown trigger="click" placement="bottom-end" @command="exportAs">
        <el-button :disabled="!nb"><el-icon><ei-download /></el-icon><span class="nb-lbl">&nbsp;{{ $t('notebook:toolbar.export') }}</span></el-button>
        <template #dropdown>
          <el-dropdown-menu>
            <el-dropdown-item command="html">{{ $t('notebook:toolbar.exportHtml') }}</el-dropdown-item>
            <el-dropdown-item command="markdown">{{ $t('notebook:toolbar.exportMarkdown') }}</el-dropdown-item>
          </el-dropdown-menu>
        </template>
      </el-dropdown>
      <el-tooltip :content="$t('notebook:toolbar.saveTip')" placement="bottom" :show-after="300">
        <el-button :type="saveState === 'dirty' || tab.untitled ? 'primary' : undefined" :disabled="!nb || saveState === 'saving' || (saveState === 'saved' && !tab.untitled)" :loading="saveState === 'saving'" @click="save">
          {{ $t('common:save') }}
        </el-button>
      </el-tooltip>
    </div>

    <div v-if="nb" class="nb-body">
      <template v-for="(cell, i) in nb.cells" :key="cell.id">
        <div class="nb-add">
          <button @click="focusCell(addCell('sql', i).id)"><el-icon><ei-plus /></el-icon>{{ $t('notebook:add.sql') }}</button>
          <button @click="focusCell(addCell('markdown', i).id)"><el-icon><ei-plus /></el-icon>{{ $t('notebook:add.text') }}</button>
        </div>
        <NotebookCellView
          :ref="(v: unknown) => setCellView(cell.id, v)"
          :cell="cell"
          :index="i"
          :count="nb.cells.length"
          :active="activeId === cell.id"
          :running="runningId === cell.id"
          :busy="lock.held"
          :editing="editing.has(cell.id)"
          :ctx="contexts[cell.id] ?? { schema: {}, snippets: [], target: '', overridden: false, unresolved: false }"
          :live="live[cell.id]?.outcome ?? null"
          :fallback="live[cell.id] ? null : fallbackOf(cell)"
          :source="cell.kind === 'sql' ? sourceOf(cell) : null"
          :title="`${title.replace(/\.ipynb$/i, '')} ${i + 1}`"
          :loaded="live[cell.id]?.loaded ?? null"
          @rename="(name: string) => renameCell(cell, name)"
          @activate="activeId = cell.id"
          @source="(v: string) => setSource(cell, v)"
          @run="(sql: string | null, next: boolean) => next ? runAndNext(cell) : runCell(cell, sql)"
          @run-from="runFrom(i)"
          @cancel="cancel"
          @move="(by: -1 | 1) => move(cell, by)"
          @remove="removeCell(cell)"
          @kind="(k: 'sql' | 'markdown') => changeKind(cell, k)"
          @target="pickTarget(cell)"
          @keep="toggleKeep(cell)"
          @chart="(c: Record<string, unknown>) => setChart(cell, c)"
          @explain="explain(cell)"
          @propose="propose(cell)"
          @group="group(cell)"
          @edit="(on: boolean) => setEditing(cell, on)"
          @next="textNext(cell)"
          @save="save"
          @need-columns="(p: string[]) => needColumns(cell, p)"
          @need-names="needNames(cell)"
        />
      </template>
      <div class="nb-add last">
        <button @click="focusCell(addCell('sql', nb.cells.length).id)"><el-icon><ei-plus /></el-icon>{{ $t('notebook:add.sql') }}</button>
        <button @click="focusCell(addCell('markdown', nb.cells.length).id)"><el-icon><ei-plus /></el-icon>{{ $t('notebook:add.text') }}</button>
      </div>
    </div>
    <div v-else class="nb-loading nm-muted"><el-icon class="is-loading"><ei-loading /></el-icon> {{ $t('common:loading') }}</div>

    <NotebookTargetDialog
      v-if="dialog?.mode === 'pick'"
      mode="pick"
      :initial="dialog.cell ? dialog.cell.target : nb?.target ?? null"
      :project-id="tab.projectId ?? null"
      :inherit="!!dialog.cell"
      :file="fileKey"
      @pick="onPicked"
      @close="closeDialog"
    />
    <NotebookTargetDialog
      v-else-if="dialog?.mode === 'map'"
      mode="map"
      :name="dialog.name"
      :engine="dialog.engine"
      :candidates="dialog.candidates"
      :file="fileKey"
      @map="onMapped"
      @close="closeDialog"
    />

    <el-dialog
      :model-value="!!txAsk"
      :title="$t('query:tx.dialogTitle')"
      width="460px"
      append-to-body
      :close-on-click-modal="false"
      @update:model-value="(v: boolean) => { if (!v) answerTx('cancel'); }"
    >
      <p class="nb-dialog-text">{{ txAsk ? $t('notebook:tx.askClose', { where: txAsk.label }) : '' }}</p>
      <div v-if="txError" class="nb-dialog-error" role="alert">{{ txError }}</div>
      <template #footer>
        <el-button :disabled="txBusy" @click="answerTx('cancel')">{{ $t('common:cancel') }}</el-button>
        <el-button :loading="txBusy" @click="answerTx('rollback')">{{ $t('query:tx.rollback') }}</el-button>
        <el-button type="primary" :loading="txBusy" @click="answerTx('commit')">{{ $t('query:tx.commit') }}</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.nb { display: flex; flex-direction: column; height: 100%; min-height: 0; }
.nb-file {
  display: flex; align-items: center; gap: 6px; height: 26px; flex-shrink: 0; padding: 0 10px; overflow: hidden; white-space: nowrap;
  font-size: 12px; color: var(--nm-text); border-bottom: 1px solid var(--nm-border-soft);
}
.nb-file > * { flex-shrink: 0; }
.nb-file-ic { color: var(--nm-accent); }
.nb-path { flex-shrink: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; color: var(--nm-text-strong); }
.nb-sep { color: var(--nm-text-muted); }
.nb-crumb { border: 0; padding: 0; background: none; font: inherit; color: var(--nm-text-dim); cursor: pointer; }
.nb-crumb:hover { color: var(--nm-text-strong); text-decoration: underline; }
.nb-banner {
  display: flex; align-items: center; gap: 8px; flex-shrink: 0; padding: 4px 10px; font-size: 12px; color: var(--nm-text);
  border-bottom: 1px solid color-mix(in srgb, var(--nm-warning) 45%, transparent); background: color-mix(in srgb, var(--nm-warning) 12%, transparent);
}
.nb-banner > .el-icon { color: var(--nm-warning); }
.nb-banner.info { border-bottom-color: color-mix(in srgb, var(--nm-info) 45%, transparent); background: color-mix(in srgb, var(--nm-info) 10%, transparent); }
.nb-banner.info > .el-icon { color: var(--nm-info); }
.nb-warnings { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
.nb-bar { container-type: inline-size; overflow: hidden; }
.nb-bar > * { flex-shrink: 0; }
.nb-bar :deep(.el-button) { white-space: nowrap; }
.nb-bar > .el-button + .el-button { margin-left: 0; }
.nb-check { margin: 0 4px; }
@container (max-width: 760px) { .nb-lbl { display: none; } }
.nb-target {
  display: inline-flex; align-items: center; gap: 5px; max-width: 320px; height: 26px; padding: 0 8px; overflow: hidden;
  border: 1px solid var(--nm-border); border-radius: 4px; background: none; font: inherit; font-size: 12px; color: var(--nm-text); cursor: pointer;
}
.nb-target > span { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.nb-target:hover { border-color: var(--nm-border-hover); color: var(--nm-text-strong); }
.nb-target.warn { color: var(--nm-warning); }
.nb-target.env { border-color: color-mix(in srgb, var(--nm-accent) 55%, transparent); background: color-mix(in srgb, var(--nm-accent) 10%, transparent); }
.nb-target.danger { border-color: color-mix(in srgb, var(--nm-danger) 60%, transparent); background: color-mix(in srgb, var(--nm-danger) 12%, transparent); }
.nb-body { flex: 1; min-height: 0; overflow: auto; padding: 8px 18px 40px 6px; background: var(--ide-editor, var(--nm-bg)); }
.nb-add { display: flex; justify-content: center; gap: 8px; height: 14px; margin: 2px 0; opacity: 0; transition: opacity 0.1s; }
.nb-add:hover, .nb-add:focus-within, .nb-add.last { opacity: 1; }
.nb-add.last { height: auto; margin-top: 10px; }
.nb-add button {
  display: inline-flex; align-items: center; gap: 3px; padding: 0 8px; height: 18px; border: 1px solid var(--nm-border); border-radius: 9px;
  background: var(--nm-bg-elev, var(--nm-bg)); font: inherit; font-size: 11px; color: var(--nm-text-dim); cursor: pointer;
}
.nb-add.last button { height: 24px; padding: 0 12px; border-radius: 4px; font-size: 12px; }
.nb-add button:hover { color: var(--nm-text-strong); border-color: var(--nm-border-hover); }
.nb-loading { padding: 20px; }
.nb-tx {
  display: inline-flex; align-items: center; gap: 6px; max-width: 420px; height: 26px; padding: 0 4px 0 8px; font-size: 12px; color: var(--nm-text);
  border: 1px solid color-mix(in srgb, var(--nm-warning) 55%, transparent); border-radius: 4px; background: color-mix(in srgb, var(--nm-warning) 12%, transparent);
}
.nb-tx > .el-icon { color: var(--nm-warning); }
.nb-tx.failed { border-color: color-mix(in srgb, var(--nm-danger) 60%, transparent); background: color-mix(in srgb, var(--nm-danger) 12%, transparent); }
.nb-tx.failed > .el-icon { color: var(--nm-danger); }
.nb-tx-text { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.nb-tx .el-button + .el-button { margin-left: 0; }
.nb-dialog-text { margin: 0; line-height: 1.5; color: var(--nm-text); }
.nb-dialog-error { margin-top: 10px; padding: 6px 10px; border-radius: 3px; font-size: 12px; white-space: pre-wrap; color: var(--nm-text); border: 1px solid color-mix(in srgb, var(--nm-danger) 50%, transparent); background: color-mix(in srgb, var(--nm-danger) 10%, transparent); }
</style>

<style>
/* The UPDATE / DELETE without WHERE confirmation (an ElMessageBox, outside this view). */
.nb-unsafe-box { width: 620px; max-width: 92vw; }
.nb-unsafe { margin-bottom: 8px; }
.nb-unsafe-line { font-size: 11.5px; color: var(--nm-text-dim); margin-bottom: 2px; }
.nb-unsafe-code { margin: 0; padding: 8px 10px; max-height: 30vh; overflow: auto; border: 1px solid var(--nm-border); border-radius: 4px; font-family: var(--nm-mono); font-size: 12.5px; white-space: pre-wrap; }
</style>
