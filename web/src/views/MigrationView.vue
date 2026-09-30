<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import {
  migrationApi, type CopyOrder, type DeltaDepth, type MigrationEvent, type MigrationMode, type MigrationPlan, type MigrationTarget,
  type PlanOptions, type RunInfo, type RunResult, type RunStatus, type RunTable, type Severity, type SyncTable,
} from '../api/migration';
import { FAMILY_LABELS, type Family } from '../api/types';
import CodeEditor from '../components/CodeEditor.vue';
import { newQuery } from '../composables/actions';
import { saveTextFile } from '../composables/files';
import { dbKey, useConnectionsStore } from '../stores/connections';
import { useTabsStore, type MigrationTab } from '../stores/tabs';
import type { MigrationConfig, SavedMigration } from '../api/savedMigrations';
import { migrationSaves } from '../composables/savedMigrations';

// "Migrar…": a database's structure to another engine. Pick the target
// engine (and, optionally, a saved connection of it), the tables and the
// options; DBine converts the tables (types, defaults, keys, indexes, names),
// reports every change and writes the target's script. Nothing runs by
// itself: the script opens in a query of the target (docs/migracion.md).
// Between the same engine there are two more modes: clone (the driver's own
// script leaves the target identical) and sync (only the rows that changed,
// into tables that already exist).

const props = defineProps<{ tab: MigrationTab }>();
const conns = useConnectionsStore();
const { t } = useTranslation();

const source = computed(() => conns.byId(props.tab.connectionId));
const sourceDriver = computed(() => conns.driverOf(props.tab.connectionId));

// -- target ----------------------------------------------------------------------------------
const targets = ref<MigrationTarget[]>([]);
const targetDriver = ref('');
const targetConnection = ref('');
const targetDatabase = ref('');
onMounted(async () => {
  try { targets.value = await migrationApi.targets(props.tab.connectionId); } catch (e) { ElMessage.error(errorMessage(e)); }
  await loadTables();
  await restore();
});
const targetGroups = computed(() => {
  const by = new Map<string, MigrationTarget[]>();
  for (const t of targets.value) by.set(t.family, [...(by.get(t.family) ?? []), t]);
  return [...by.entries()].map(([f, list]) => ({
    label: FAMILY_LABELS[f as Family] ?? f,
    list: list.sort((a, b) => Number(b.supported) - Number(a.supported) || a.name.localeCompare(b.name)),
  }));
});
const targetInfo = computed(() => conns.drivers.find((d) => d.id === targetDriver.value));
const targetEntry = computed(() => targets.value.find((x) => x.id === targetDriver.value));
/** An engine can be picked when any mode works with it. */
function selectable(x: MigrationTarget) {
  return x.supported || x.clone.available || x.sync.available;
}

// -- mode --------------------------------------------------------------------------------------
const MODES: MigrationMode[] = ['convert', 'clone', 'sync'];
const mode = ref<MigrationMode>('convert');
/** Why a mode can't be used with the chosen engine (`null`: it can). */
function modeBlocked(m: MigrationMode): string | null {
  const x = targetEntry.value;
  if (!x) return t('migration:mode.pickEngineFirst');
  if (m === 'convert') return x.supported ? null : tb(x.reason) || t('migration:mode.noConvert');
  const s = m === 'clone' ? x.clone : x.sync;
  return s.available ? null : tb(s.reason) || t(`migration:mode.no${m === 'clone' ? 'Clone' : 'Sync'}`);
}
// The engine changed: keep the mode if it still works, else the first one that does.
watch(targetDriver, () => {
  if (modeBlocked(mode.value)) mode.value = MODES.find((m) => !modeBlocked(m)) ?? 'convert';
});
const sync = reactive<{ depth: DeltaDepth; max_cores: number }>({ depth: 'Full', max_cores: 0 });
const DEPTHS: { value: DeltaDepth; key: string }[] = [
  { value: 'Full', key: 'full' }, { value: 'Sizes', key: 'sizes' }, { value: 'Keys', key: 'keys' },
];
/** Keys picked per table for the sync (`schema.name` → columns joined by a comma). */
const syncKeys = ref<Record<string, string>>({});
/** Saved connections of the target engine (where the script can open). */
const targetConns = computed(() => conns.list.filter((c) => c.config.driver === targetDriver.value));
const targetDbs = computed(() => conns.live[targetConnection.value]?.databases ?? []);
// Choosing the engine doesn't pick (and connect to) a connection by itself:
// that could ask for a password out of the blue.
watch(targetDriver, () => {
  targetConnection.value = '';
  options.target_schema = '';
});
/** Restoring a saved migration: its target database, set without connecting (no password prompt on open). */
let restoringDatabase: string | null = null;
watch(targetConnection, async (id) => {
  targetDatabase.value = '';
  const kept = restoringDatabase;
  restoringDatabase = null;
  if (!id) return;
  if (kept !== null) { targetDatabase.value = kept; return; }
  if (!(await conns.ensureConnected(id))) return;
  // On the source's own server, never preselect the source database: that
  // would migrate a database onto itself. The user picks one.
  const pick = conns.live[id]?.defaultDatabase ?? targetDbs.value[0] ?? '';
  targetDatabase.value = id === props.tab.connectionId && pick === props.tab.database ? '' : pick;
});
/** The target's databases (the saved one while the target isn't connected yet). */
const targetDbChoices = computed(() => (targetDbs.value.length || !targetDatabase.value ? targetDbs.value : [targetDatabase.value]));

// -- tables ----------------------------------------------------------------------------------
const tables = ref<{ key: string; schema: string | null; name: string }[]>([]);
const picked = ref<Set<string>>(new Set());
const tableFilter = ref('');
const loadingTables = ref(false);
const tablesError = ref<string | null>(null);
async function loadTables(force = false) {
  loadingTables.value = true;
  tablesError.value = null;
  try {
    if (!(await conns.ensureConnected(props.tab.connectionId))) return;
    await conns.loadObjects(props.tab.connectionId, props.tab.database, force);
    const state = conns.objects[dbKey(props.tab.connectionId, props.tab.database)];
    if (state?.status === 'error') tablesError.value = state.error;
    tables.value = (conns.objects[dbKey(props.tab.connectionId, props.tab.database)]?.items ?? [])
      .filter((o) => o.kind === 'table' && !o.parent)
      .map((o) => ({ key: `${o.schema ?? ''}.${o.name}`, schema: o.schema, name: o.name }))
      .sort((a, b) => a.key.localeCompare(b.key));
    picked.value = new Set(tables.value.map((t) => t.key));
  } finally {
    loadingTables.value = false;
  }
}
const shownTables = computed(() => {
  const q = tableFilter.value.trim().toLowerCase();
  return q ? tables.value.filter((t) => t.key.toLowerCase().includes(q)) : tables.value;
});
function togglePick(key: string) {
  const s = new Set(picked.value);
  if (s.has(key)) s.delete(key);
  else s.add(key);
  picked.value = s;
}
function pickAll(on: boolean) {
  const s = new Set(picked.value);
  for (const t of shownTables.value) (on ? s.add(t.key) : s.delete(t.key));
  picked.value = s;
}

// -- options & plan ------------------------------------------------------------------------------
const options = reactive<PlanOptions>({ fold_case: true, target_schema: '', drop: false, if_exists: true, indexes: true, foreign_keys: true, data: true, keep_schemas: true });
function syncOptions() {
  const keys = Object.entries(syncKeys.value)
    .filter(([, cols]) => cols)
    .map(([k, cols]) => {
      const tab = tables.value.find((x) => x.key === k);
      return { schema: tab?.schema ?? null, name: tab?.name ?? k, columns: cols.split(',') };
    });
  return { depth: sync.depth, max_cores: sync.max_cores || 0, keys };
}
const plan = ref<MigrationPlan | null>(null);
const planning = ref(false);
const planError = ref<string | null>(null);
const script = ref('');

function chosenTables() {
  const all = picked.value.size === tables.value.length;
  return all ? [] : tables.value.filter((t) => picked.value.has(t.key)).map((t) => ({ schema: t.schema, name: t.name }));
}

async function generate() {
  if (!targetDriver.value) { ElMessage.warning(t('migration:pickEngine')); return; }
  if (!picked.value.size) { ElMessage.warning(t('migration:pickTable')); return; }
  if (mode.value === 'clone' && !targetConnection.value) { ElMessage.warning(t('migration:clone.needConnection')); return; }
  if (mode.value === 'clone' && !(await conns.ensureConnected(targetConnection.value))) return;
  pane.value = 'preview';
  planning.value = true;
  planError.value = null;
  try {
    plan.value = await migrationApi.plan({
      connectionId: props.tab.connectionId, database: props.tab.database, tables: chosenTables(), targetDriver: targetDriver.value,
      options: { ...options, target_schema: options.target_schema?.trim() || null },
      mode: mode.value, sync: syncOptions(),
      target: mode.value === 'clone' ? { connection_id: targetConnection.value, database: targetDatabase.value } : null,
    });
    script.value = plan.value.script;
  } catch (e) {
    plan.value = null;
    planError.value = errorMessage(e);
  } finally {
    planning.value = false;
  }
}
// Options changed after a plan: regenerate. Another mode: its own preview.
watch(() => ({ ...options }), () => { if (plan.value && mode.value === 'convert') generate(); }, { deep: true });
watch(mode, () => { plan.value = null; script.value = ''; planError.value = null; });

// Sync: each table's key (the primary key, or a unique key the user picks).
function keyOf(s: SyncTable) {
  return `${s.schema ?? ''}.${s.name}`;
}
function keyChoices(s: SyncTable) {
  const out: { value: string; label: string }[] = [];
  if (s.primary_key) out.push({ value: s.primary_key.join(','), label: t('migration:sync.primaryKey', { columns: s.primary_key.join(', ') }) });
  for (const u of s.unique_keys) out.push({ value: u.join(','), label: t('migration:sync.uniqueKey', { columns: u.join(', ') }) });
  return out;
}
function chosenKey(s: SyncTable) {
  return syncKeys.value[keyOf(s)] ?? s.key?.join(',') ?? '';
}
function setKey(s: SyncTable, v: string) {
  syncKeys.value = { ...syncKeys.value, [keyOf(s)]: v };
}
/** Why the table can't be synced, unless the user already picked a key. */
function syncReason(s: SyncTable) {
  return syncKeys.value[keyOf(s)] ? null : s.reason;
}
const unsyncable = computed(() => (plan.value?.sync_tables ?? []).filter((s) => syncReason(s)).length);

// -- report ----------------------------------------------------------------------------------------
const SEVERITY = computed((): Record<Severity, { label: string; cls: string; hint: string }> => ({
  dropped: { label: t('migration:severity.dropped'), cls: 'dropped', hint: t('migration:severity.droppedHint') },
  loss: { label: t('migration:severity.loss'), cls: 'loss', hint: t('migration:severity.lossHint') },
  warning: { label: t('migration:severity.warning'), cls: 'warning', hint: t('migration:severity.warningHint') },
  info: { label: t('migration:severity.info'), cls: 'info', hint: t('migration:severity.infoHint') },
}));
const ORDER: Severity[] = ['dropped', 'loss', 'warning', 'info'];
const severityFilter = ref<Severity | 'all'>('all');
const counts = computed(() => {
  const c: Record<Severity, number> = { dropped: 0, loss: 0, warning: 0, info: 0 };
  for (const i of plan.value?.issues ?? []) c[i.severity]++;
  return c;
});
const issues = computed(() =>
  [...(plan.value?.issues ?? [])]
    .filter((i) => severityFilter.value === 'all' || i.severity === severityFilter.value)
    .sort((a, b) => ORDER.indexOf(a.severity) - ORDER.indexOf(b.severity) || a.table.localeCompare(b.table)),
);
const showColumns = ref(false);
const columnFilter = ref('');
const columns = computed(() => {
  const q = columnFilter.value.trim().toLowerCase();
  const all = plan.value?.columns ?? [];
  return q ? all.filter((c) => `${c.table} ${c.column} ${c.target_column}`.toLowerCase().includes(q)) : all;
});

// -- running the migration (on the chosen target) ------------------------------------------------------
// The data goes through the bulk transfer engine: one row per table with its
// state, rows, speed, path and bottleneck; the parallel limit changes live.
type Row = RunTable & { phase: string; rate: number };
const running = ref(false);
const runId = ref('');
const runStatus = ref<RunStatus | null>(null);
const rows = ref<Row[]>([]);
const step = ref<{ phase: string; table: string; done: number; total: number } | null>(null);
const logs = ref<{ level: string; text: string }[]>([]);
const result = ref<RunResult | null>(null);
const runError = ref<string | null>(null);
const parallel = ref(8);
const startedAt = ref(0);
const now = ref(Date.now());
const runTarget = ref<{ connectionId: string; database: string }>({ connectionId: '', database: '' });
/** The mode of the run shown (a resumed one keeps its own). */
const runMode = ref<MigrationMode>('convert');
let ticker: ReturnType<typeof setInterval> | null = null;
let unlisten: UnlistenFn | null = null;
onBeforeUnmount(() => { unlisten?.(); if (ticker) clearInterval(ticker); });
const targetName = computed(() => conns.byId(targetConnection.value)?.name ?? '');
const canRun = computed(() => !!targetDriver.value && !!targetConnection.value && !!picked.value.size && !running.value && !modeBlocked(mode.value));
/** Rows travel in this run (the parallel control and per-table actions apply). */
const movesData = computed(() => runMode.value === 'sync' || options.data);

// Advanced options of the transfer.
const transfer = reactive<{ parallel: number; order: CopyOrder; commit_rows: number }>({ parallel: 8, order: 'largest_first', commit_rows: 100000 });
const showAdvanced = ref(false);

function toRow(t: RunTable): Row {
  return { ...t, phase: '', rate: t.stats?.rows_per_s ?? 0 };
}
function rowOf(name: string): Row | undefined {
  return rows.value.find((r) => r.name === name);
}
function onEvent(e: MigrationEvent) {
  switch (e.event) {
    case 'plan': rows.value = e.tables.map(toRow); parallel.value = e.parallel; break;
    case 'step': step.value = e.phase === 'done' ? null : e; break;
    case 'run_started': parallel.value = e.parallel; step.value = null; break;
    case 'table_started': { const r = rowOf(e.table); if (r) { r.status = 'running'; r.attempts = e.attempt; r.error = null; } break; }
    case 'table_phase': { const r = rowOf(e.table); if (r) r.phase = e.phase; break; }
    case 'table_progress': {
      const r = rowOf(e.table);
      if (r) { r.rows_done = e.rows_done; r.rows_total = e.rows_total ?? r.rows_total; r.rate = e.rows_per_s; }
      break;
    }
    case 'table_done': {
      const r = rowOf(e.table);
      if (r) { r.status = 'done'; r.rows_done = e.rows; r.stats = e.stats; r.rate = e.stats.rows_per_s; if (e.stats.path) r.path = e.stats.path; }
      break;
    }
    case 'table_failed': { const r = rowOf(e.table); if (r) { r.status = 'failed'; r.error = e.error; } break; }
    case 'table_cancelled': { const r = rowOf(e.table); if (r) r.status = 'cancelled'; break; }
    case 'log': if (e.level !== 'info') logs.value = [...logs.value.slice(-49), { level: e.level, text: e.text }]; break;
  }
}

/** Run a migration command (run, resume, retry) while its events fill the grid. */
async function execute(id: string, target: { connectionId: string; database: string }, call: () => Promise<RunResult>, initial: RunTable[] = []) {
  stopAttach();
  runMissing.value = false;
  // The run goes into the saved migration (saved now if it was only a new tab).
  try {
    await persistNow();
    await conns.linkMigrationRun(migrationId, id);
  } catch (e) { ElMessage.error(errorMessage(e)); }
  pane.value = 'run';
  runFilter.value = 'all';
  running.value = true;
  runId.value = id;
  runStatus.value = 'running';
  runTarget.value = target;
  result.value = null;
  runError.value = null;
  step.value = null;
  logs.value = [];
  rows.value = initial.map(toRow);
  startedAt.value = Date.now();
  now.value = Date.now();
  if (ticker) clearInterval(ticker);
  ticker = setInterval(() => { now.value = Date.now(); }, 1000);
  try {
    unlisten?.();
    unlisten = await listen<MigrationEvent>('migration-progress', (e) => { if (e.payload.id === runId.value) onEvent(e.payload); });
    const r = await call();
    result.value = r;
    runStatus.value = r.status;
    rows.value = r.tables.map((t) => ({ ...toRow(t), rate: rowOf(t.name)?.rate ?? t.stats?.rows_per_s ?? 0 }));
    const failed = r.tables.filter((t) => t.status === 'failed').length + r.foreign_key_errors.length;
    runMode.value = r.mode ?? runMode.value;
    const failedAll = failed + (r.after_errors?.length ?? 0);
    if (r.cancelled) ElMessage.warning(t('migration:run.cancelled'));
    else if (failedAll) ElMessage.warning(t('migration:run.finishedWithErrors', { count: failedAll }));
    else if (failed) ElMessage.warning(t('migration:run.finishedWithErrors', { count: failed }));
    else ElMessage.success(t('migration:run.finished'));
    conns.loadObjects(target.connectionId, target.database, true);
  } catch (e) {
    runError.value = errorMessage(e);
    runStatus.value = 'failed';
  } finally {
    running.value = false;
    step.value = null;
    now.value = Date.now();
    if (ticker) { clearInterval(ticker); ticker = null; }
    unlisten?.();
    unlisten = null;
    loadRuns();
    refreshLinkedRun(id);
  }
}

const actionLabel = computed(() => t(`migration:mode.action.${mode.value}`));
async function runMigration() {
  if (!canRun.value) return;
  const where = `${targetName.value}${targetDatabase.value ? ` · ${targetDatabase.value}` : ''}`;
  const engine = targetInfo.value?.name;
  const count = picked.value.size;
  const message = mode.value === 'clone'
    ? t('migration:confirm.clone', { count, where, engine, data: options.data ? t('migration:confirm.cloneData') : '' })
    : mode.value === 'sync'
      ? t('migration:confirm.sync', { count, where, engine })
      : t('migration:confirm.message', {
        count, where, engine,
        data: options.data ? t('migration:confirm.data') : '',
        drop: options.drop ? t('migration:confirm.drop') : '',
      });
  try {
    await ElMessageBox.confirm(message, actionLabel.value, { confirmButtonText: actionLabel.value, cancelButtonText: t('common:cancel'), type: 'warning' });
  } catch { return; }
  if (!(await conns.ensureConnected(props.tab.connectionId)) || !(await conns.ensureConnected(targetConnection.value))) return;
  runMode.value = mode.value;
  const id = `${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`;
  const target = { connectionId: targetConnection.value, database: targetDatabase.value };
  parallel.value = transfer.parallel;
  await execute(id, target, () => migrationApi.run({
    migrationId: id, connectionId: props.tab.connectionId, database: props.tab.database, tables: chosenTables(),
    targetDriver: targetDriver.value, options: { ...options, target_schema: options.target_schema?.trim() || null },
    targetConnectionId: target.connectionId, targetDatabase: target.database,
    transfer: { parallel: transfer.parallel, order: transfer.order, commit_rows: transfer.commit_rows || null },
    mode: mode.value, sync: syncOptions(),
  }));
}
function cancelRun() {
  if (runId.value) migrationApi.cancel(runId.value).catch((e) => ElMessage.error(errorMessage(e)));
}
function cancelTable(name: string) {
  migrationApi.cancelTable(runId.value, name).catch((e) => ElMessage.error(errorMessage(e)));
}
function runNow(name: string) {
  migrationApi.runNow(runId.value, name).catch((e) => ElMessage.error(errorMessage(e)));
}
function changeParallel(n: number | undefined) {
  if (!n || !running.value) return;
  migrationApi.setParallel(runId.value, n).then((v) => { parallel.value = v; }).catch((e) => ElMessage.error(errorMessage(e)));
}
async function retryFailed() {
  const id = runId.value;
  if (!id || running.value) return;
  const target = runTarget.value;
  if (!(await conns.ensureConnected(props.tab.connectionId)) || !(await conns.ensureConnected(target.connectionId))) return;
  await execute(id, target, () => migrationApi.retryFailed(id), result.value?.tables ?? rows.value);
}

// Runs cut by closing the app (of this database), to resume.
const runs = ref<RunInfo[]>([]);
async function loadRuns() {
  try { runs.value = await migrationApi.runs(); } catch { /* the list is optional */ }
}
onMounted(loadRuns);
/** Runs of other saved migrations of this database (they resume from their own entry). */
const linkedElsewhere = computed(() => new Set((conns.migrations[dbKey(props.tab.connectionId, props.tab.database)]?.items ?? [])
  .filter((m) => m.id !== migrationId).flatMap((m) => m.run_ids)));
const interrupted = computed(() => runs.value.filter((r) =>
  r.status === 'interrupted' && r.source_connection_id === props.tab.connectionId && r.source_database === props.tab.database && r.id !== runId.value
  && !linkedElsewhere.value.has(r.id)));
function runLabel(r: RunInfo) {
  const name = conns.byId(r.target_connection_id)?.name ?? r.target_connection_id;
  return `${name}${r.target_database ? ` · ${r.target_database}` : ''}`;
}
function runDate(r: RunInfo) {
  return new Date(r.created_at).toLocaleString(locale());
}
async function resumeRun(r: RunInfo) {
  if (running.value) return;
  if (!(await conns.ensureConnected(r.source_connection_id)) || !(await conns.ensureConnected(r.target_connection_id))) return;
  parallel.value = r.parallel;
  runMode.value = r.mode ?? 'convert';
  await execute(r.id, { connectionId: r.target_connection_id, database: r.target_database }, () => migrationApi.resume(r.id), r.tables);
}
async function forgetRun(r: RunInfo) {
  try {
    await ElMessageBox.confirm(t('migration:interrupted.forgetConfirm'), t('migration:interrupted.forget'), {
      confirmButtonText: t('migration:interrupted.forget'), cancelButtonText: t('common:cancel'), type: 'warning',
    });
  } catch { return; }
  try { await migrationApi.forget(r.id); await loadRuns(); } catch (e) { ElMessage.error(errorMessage(e)); }
}

// Header figures.
const elapsed = computed(() => {
  const ms = result.value && !running.value ? result.value.elapsed_ms : now.value - startedAt.value;
  const secs = Math.max(0, Math.round(ms / 1000));
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  return `${h ? `${h}:` : ''}${String(m).padStart(h ? 2 : 1, '0')}:${String(secs % 60).padStart(2, '0')}`;
});
const finishedTables = computed(() => rows.value.filter((r) => ['done', 'failed', 'cancelled'].includes(r.status)).length);
const totalRows = computed(() => rows.value.reduce((n, r) => n + r.rows_done, 0));
const overall = computed(() => {
  if (!rows.value.length) return 0;
  const known = rows.value.every((r) => r.status === 'done' || r.rows_total != null);
  if (known) {
    const total = rows.value.reduce((n, r) => n + (r.status === 'done' ? r.rows_done : Math.max(r.rows_total ?? 0, r.rows_done)), 0);
    if (total > 0) return Math.min(100, Math.round((totalRows.value / total) * 100));
  }
  return Math.round((finishedTables.value / rows.value.length) * 100);
});
const hasFailed = computed(() => rows.value.some((r) => r.status === 'failed' || r.status === 'cancelled'));
const showRun = computed(() => running.value || !!result.value || !!runError.value || rows.value.length > 0);

// -- below the steps: the preview or the run, one at a time ---------------------------------
const pane = ref<'preview' | 'run'>('preview');

// -- run grid: by state and by name -------------------------------------------------------
type RunFilter = 'all' | 'pending' | 'active' | 'done' | 'failed' | 'cancelled';
const RUN_FILTERS: RunFilter[] = ['all', 'pending', 'active', 'done', 'failed', 'cancelled'];
const runFilter = ref<RunFilter>('all');
const runSearch = ref('');
/** Copying, or building its indexes after the copy. */
const bucket = (r: Row): Exclude<RunFilter, 'all'> =>
  r.status === 'running' || r.status === 'copied' ? 'active' : (r.status as Exclude<RunFilter, 'all'>);
const runCounts = computed(() => {
  const c: Record<RunFilter, number> = { all: rows.value.length, pending: 0, active: 0, done: 0, failed: 0, cancelled: 0 };
  for (const r of rows.value) c[bucket(r)]++;
  return c;
});
const shownRows = computed(() => {
  const q = runSearch.value.trim().toLowerCase();
  return rows.value.filter((r) => (runFilter.value === 'all' || bucket(r) === runFilter.value) && (!q || r.source.toLowerCase().includes(q) || r.target.toLowerCase().includes(q)));
});
const runTitle = computed(() => {
  if (running.value) return t('migration:run.running');
  if (runError.value) return t('migration:run.stopped');
  if (runStatus.value === 'cancelled') return t('migration:run.cancelled');
  if (runStatus.value === 'failed') return t('migration:run.withErrors');
  return runMode.value === 'sync' ? t('migration:run.synced') : runMode.value === 'clone' ? t('migration:run.cloned') : t('migration:run.finished');
});
const STEP = computed((): Record<string, string> => ({
  schemas: t('migration:phase.schemas'), drop: t('migration:phase.drop'), create: t('migration:phase.create'),
  foreign_keys: t('migration:phase.foreignKeys'), identity: t('migration:phase.identity'), done: t('migration:phase.done'),
  script: t('migration:phase.script'), before: t('migration:phase.before'), check: t('migration:phase.check'), after: t('migration:phase.after'),
}));
function statusLabel(r: Row) {
  if (r.status === 'running') {
    return ({
      check: t('migration:status.check'), truncate: t('migration:status.truncate'), indexes: t('migration:status.indexes'),
      summary: t('migration:status.summary'), compare: t('migration:status.compare'), apply: t('migration:status.apply'),
    } as Record<string, string>)[r.phase] ?? t(runMode.value === 'sync' ? 'migration:status.syncing' : 'migration:status.running');
  }
  if (r.status === 'done' && runMode.value === 'sync') return t('migration:status.synced');
  return t(`migration:status.${r.status}`);
}
const PATH = computed((): Record<string, string> => ({
  native: t('migration:path.native'), bulk_load: t('migration:path.bulkLoad'), insert_script: t('migration:path.insertScript'),
  delta: t('migration:path.delta'),
}));
function changes(r: Row) {
  const d = r.stats?.delta;
  return d ? t('migration:grid.changesValue', { inserted: fmt(d.inserted), updated: fmt(d.updated), deleted: fmt(d.deleted) }) : '—';
}
function pct(r: Row) {
  if (r.status === 'done') return 100;
  return r.rows_total ? Math.min(100, Math.round((r.rows_done / r.rows_total) * 100)) : 0;
}
function fmt(n: number) {
  return Math.round(n).toLocaleString(locale());
}

// -- the saved migration ("Migraciones" node) ---------------------------------------------------
// The form is auto-saved into the tab's entry (from the first change on: opening the screen
// alone saves nothing); each run started here is linked to it and shows when it opens again.
const tabs = useTabsStore();
if (!props.tab.migrationId) {
  props.tab.migrationId = crypto.randomUUID();
  tabs.persist();
}
const migrationId = props.tab.migrationId;
const entry = computed(() => conns.migrationById(migrationId));
/** The last automatic name given: while the entry keeps it, it follows the configuration. */
let lastAuto = '';
let ready = false;
let restoring = false;
/** The configuration as last saved (or as it opened): nothing to save while it's the same. */
let baseline = '';
/** Its current run isn't on this machine (a synced entry). */
const runMissing = ref(false);

function currentConfig(): MigrationConfig {
  return {
    target_driver: targetDriver.value, mode: mode.value, target_connection_id: targetConnection.value, target_database: targetDatabase.value,
    tables: tables.value.length && picked.value.size === tables.value.length ? null : [...picked.value].sort(),
    options: { ...options }, sync: { ...sync }, sync_keys: { ...syncKeys.value }, transfer: { ...transfer },
  };
}
function autoName(c: MigrationConfig): string {
  if (!c.target_driver) return t('migration:saved.untitled');
  const where = conns.byId(c.target_connection_id)?.name || conns.drivers.find((d) => d.id === c.target_driver)?.name || c.target_driver;
  return [`→ ${where}`, c.target_database, t(`migration:saved.mode.${c.mode}`)].filter(Boolean).join(' · ');
}
const title = computed(() => entry.value?.name ?? autoName(currentConfig()));

async function applyConfig(c: MigrationConfig) {
  targetDriver.value = c.target_driver ?? '';
  await nextTick(); // the engine's watchers (connection, schema, mode) go first
  if (c.mode && !modeBlocked(c.mode)) mode.value = c.mode;
  Object.assign(options, c.options ?? {});
  Object.assign(sync, c.sync ?? {});
  syncKeys.value = { ...(c.sync_keys ?? {}) };
  Object.assign(transfer, c.transfer ?? {});
  if (c.target_connection_id && conns.byId(c.target_connection_id)) {
    restoringDatabase = c.target_database ?? '';
    targetConnection.value = c.target_connection_id;
    await nextTick();
    restoringDatabase = null;
  }
  if (c.tables) {
    // Tables that no longer exist drop out; without the list (it didn't load), the choice stays.
    const known = new Set(tables.value.map((x) => x.key));
    picked.value = new Set(tables.value.length ? c.tables.filter((k) => known.has(k)) : c.tables);
  }
}

async function restore() {
  restoring = true;
  try {
    await conns.loadMigrations(props.tab.connectionId, props.tab.database);
    const m = entry.value;
    if (m?.config) {
      await applyConfig(m.config);
      lastAuto = autoName(m.config);
    }
    baseline = JSON.stringify(currentConfig());
    if (m) await showLinkedRun(m);
  } finally {
    restoring = false;
    ready = true;
  }
}

let saving: Promise<SavedMigration | null> = Promise.resolve(null);
function persistNow(): Promise<SavedMigration | null> {
  if (saveTimer) { clearTimeout(saveTimer); saveTimer = null; }
  const next = saving.catch(() => null).then(async () => {
    const cfg = currentConfig();
    baseline = JSON.stringify(cfg);
    const auto = autoName(cfg);
    const current = entry.value;
    // A name the user wrote stays; the automatic one follows the configuration.
    const name = current && current.name !== lastAuto ? current.name : auto;
    lastAuto = auto;
    return conns.saveMigration({
      id: migrationId, connection_id: props.tab.connectionId, database: props.tab.database, name, config: cfg,
      run_ids: current?.run_ids ?? [], created_at: current?.created_at ?? '', updated_at: '',
    });
  });
  saving = next;
  migrationSaves.set(migrationId, next.catch(() => null));
  return next;
}
let saveTimer: ReturnType<typeof setTimeout> | null = null;
watch(() => JSON.stringify(currentConfig()), (json) => {
  if (!ready || restoring) return;
  // Not an entry until the first real change; after that, only changes are saved.
  if (json === baseline) return;
  if (saveTimer) clearTimeout(saveTimer);
  saveTimer = setTimeout(() => { persistNow().catch((e) => ElMessage.error(errorMessage(e))); }, 800);
});
onBeforeUnmount(() => {
  if (saveTimer) persistNow().catch(() => {});
  stopAttach();
});

function onTitle(e: Event) {
  const el = e.target as HTMLInputElement;
  if (!el.value.trim()) el.value = title.value;
  else rename(el.value);
}
/** The title at the top: renaming (a new tab is saved first). */
async function rename(value: string) {
  const name = value.trim();
  if (!name || name === entry.value?.name) return;
  try {
    if (!entry.value) await persistNow();
    await saving.catch(() => null);
    await conns.renameMigration(migrationId, name);
  } catch (e) { ElMessage.error(errorMessage(e)); }
}

// The linked run: shown in "Ejecución"; still going on in this app, followed live.
const linkedRun = ref<RunInfo | null>(null);
/** Its runs, newest first, with what this machine knows of them. */
const runHistory = computed(() => [...(entry.value?.run_ids ?? [])].reverse());
const historyRuns = ref<Record<string, RunInfo>>({});
async function loadHistory() {
  const ids = entry.value?.run_ids ?? [];
  if (!ids.length) return;
  try { historyRuns.value = Object.fromEntries((await migrationApi.runs(undefined, ids)).map((r) => [r.id, r])); } catch { /* optional */ }
}
function showRunInfo(r: RunInfo) {
  linkedRun.value = r;
  runId.value = r.id;
  runStatus.value = r.status;
  runTarget.value = { connectionId: r.target_connection_id, database: r.target_database };
  runMode.value = r.mode ?? 'convert';
  parallel.value = r.parallel;
  runError.value = null;
  rows.value = r.tables.map((x) => ({ ...toRow(x), rate: rowOf(x.name)?.rate ?? x.stats?.rows_per_s ?? 0 }));
  const ms = r.finished_at ? Math.max(0, Date.parse(r.finished_at) - Date.parse(r.created_at)) : 0;
  result.value = r.status === 'running' ? null : {
    run_id: r.id, status: r.status, tables: r.tables, foreign_key_errors: r.foreign_key_errors, after_errors: r.after_errors,
    notes: r.notes, elapsed_ms: ms, cancelled: r.status === 'cancelled', mode: r.mode,
  };
  pane.value = 'run';
}
async function fetchRun(id: string): Promise<RunInfo | null> {
  try { return (await migrationApi.runs(undefined, [id]))[0] ?? null; } catch { return null; }
}
async function showLinkedRun(m: SavedMigration, id = m.run_ids[m.run_ids.length - 1]) {
  if (!id) return;
  loadHistory();
  const r = await fetchRun(id);
  if (!r) { runMissing.value = true; return; }
  runMissing.value = false;
  showRunInfo(r);
  if (r.status === 'running') await attach(r);
}
/** After a run (or resume, retry) ends: its record as saved. */
async function refreshLinkedRun(id: string) {
  const r = await fetchRun(id);
  if (r) {
    linkedRun.value = r;
    if (entry.value) conns.loadMigrationRuns([entry.value]);
  }
  loadHistory();
}
/** "Ejecuciones": look at an earlier run of this migration. */
async function viewRun(id: string) {
  if (running.value || !entry.value) return;
  await showLinkedRun(entry.value, id);
}

// A run started by an earlier tab of this migration is still going on: follow its events and
// its record until it ends.
let poll: ReturnType<typeof setInterval> | null = null;
function stopAttach() {
  if (poll) { clearInterval(poll); poll = null; }
}
async function attach(r: RunInfo) {
  stopAttach();
  running.value = true;
  startedAt.value = Date.parse(r.created_at) || Date.now();
  now.value = Date.now();
  if (ticker) clearInterval(ticker);
  ticker = setInterval(() => { now.value = Date.now(); }, 1000);
  unlisten?.();
  unlisten = await listen<MigrationEvent>('migration-progress', (e) => { if (e.payload.id === runId.value) onEvent(e.payload); });
  poll = setInterval(async () => {
    const x = await fetchRun(r.id);
    if (x && x.status === 'running') return;
    stopAttach();
    running.value = false;
    step.value = null;
    if (ticker) { clearInterval(ticker); ticker = null; }
    unlisten?.();
    unlisten = null;
    if (x) showRunInfo(x);
    loadRuns();
    if (entry.value) conns.loadMigrationRuns([entry.value]);
  }, 2000);
}
/** "Retomar": the linked run, cut by closing the app or cancelled. */
const canResume = computed(() => !running.value && !!linkedRun.value && linkedRun.value.id === runId.value
  && (linkedRun.value.status === 'interrupted' || linkedRun.value.status === 'cancelled'));
function runOption(id: string) {
  const r = historyRuns.value[id] ?? (linkedRun.value?.id === id ? linkedRun.value : null);
  return r ? `${runDate(r)} · ${t(`migration:saved.state.${r.status}`)}` : t('migration:saved.state.elsewhere');
}

// -- script (preview) -------------------------------------------------------------------------------
async function openInQuery() {
  if (!targetConnection.value) {
    ElMessage.warning(t('migration:script.pickConnection'));
    return;
  }
  await newQuery(targetConnection.value, targetDatabase.value, script.value, t('migration:script.queryTitle', { name: props.tab.database || source.value?.name }));
}
async function copyScript() {
  try { await navigator.clipboard.writeText(script.value); ElMessage.success(t('migration:script.copied')); } catch { /* ignore */ }
}
function saveScript() {
  saveTextFile(script.value, `${t('migration:script.fileName', { database: props.tab.database || t('migration:script.fileNameDatabase'), engine: targetDriver.value })}.sql`, [{ name: 'SQL', extensions: ['sql', 'txt'] }]);
}
</script>

<template>
  <div class="mg nm-content">
    <header class="mg-head">
      <el-icon :size="20"><ei-switch /></el-icon>
      <div class="mg-head-main">
        <div class="mg-title-row">
          <span class="mg-kicker">{{ $t('migration:migrate') }}</span>
          <input
            class="mg-title"
            :value="title"
            :title="$t('migration:saved.renameHint')"
            spellcheck="false"
            @change="onTitle"
            @keydown.enter="($event.target as HTMLInputElement).blur()"
          />
          <span v-if="!entry" class="mg-muted">{{ $t('migration:saved.notSaved') }}</span>
        </div>
        <p>
          <i18next :translation="$t('migration:intro', { engine: sourceDriver?.name })"><template #database><b>{{ tab.database || source?.name }}</b></template></i18next>
        </p>
      </div>
    </header>

    <div class="mg-grid">
      <!-- 1. Target -->
      <section class="mg-card">
        <h3><span class="mg-step">1</span> {{ $t('migration:target.title') }}</h3>
        <label>{{ $t('migration:target.engine') }}</label>
        <el-select v-model="targetDriver" filterable :placeholder="$t('migration:target.pickEngine')" style="width: 100%">
          <el-option-group v-for="g in targetGroups" :key="g.label" :label="g.label">
            <el-option v-for="t in g.list" :key="t.id" :value="t.id" :label="t.name" :disabled="!selectable(t)">
              <span>{{ t.name }}</span>
              <span v-if="!selectable(t)" class="mg-opt-why" :title="tb(t.reason)">{{ tb(t.reason) }}</span>
            </el-option>
          </el-option-group>
        </el-select>
        <template v-if="targetDriver">
          <label>{{ $t('migration:mode.title') }}</label>
          <el-radio-group v-model="mode" size="small" class="mg-modes">
            <el-tooltip v-for="m in MODES" :key="m" :content="modeBlocked(m) ?? $t(`migration:mode.hint.${m}`)" placement="top" :show-after="300">
              <el-radio-button :value="m" :disabled="!!modeBlocked(m)">{{ $t(`migration:mode.label.${m}`) }}</el-radio-button>
            </el-tooltip>
          </el-radio-group>
          <label>{{ $t('migration:target.connection') }} <span class="mg-muted">{{ $t('migration:target.connectionHint') }}</span></label>
          <el-select v-model="targetConnection" clearable filterable :placeholder="targetConns.length ? $t('migration:target.connectionPlaceholder') : $t('migration:target.noConnections', { engine: targetInfo?.name ?? $t('migration:target.thatEngine') })" style="width: 100%">
            <el-option v-for="c in targetConns" :key="c.id" :label="c.name" :value="c.id" />
          </el-select>
          <template v-if="targetConnection && targetDbChoices.length">
            <label>{{ targetInfo?.databases_label ? tb(targetInfo.databases_label) : $t('migration:target.database') }}</label>
            <el-select v-model="targetDatabase" filterable style="width: 100%" @visible-change="(open: boolean) => open && !targetDbs.length && conns.ensureConnected(targetConnection)">
              <el-option v-for="d in targetDbChoices" :key="d" :label="d" :value="d" />
            </el-select>
          </template>
          <template v-if="targetInfo?.has_schemas && mode === 'convert'">
            <el-checkbox v-model="options.keep_schemas" :disabled="!!options.target_schema?.trim()" style="margin-top: 6px">
              {{ $t('migration:target.keepSchemas') }}
            </el-checkbox>
            <label>{{ $t('migration:target.singleSchema') }} <span class="mg-muted">{{ $t('migration:target.singleSchemaHint') }}</span></label>
            <el-input v-model="options.target_schema" clearable :placeholder="options.keep_schemas ? $t('migration:target.eachInOwnSchema') : $t('migration:target.engineDefault')" />
          </template>
        </template>
      </section>

      <!-- 2. Tables -->
      <section class="mg-card">
        <h3><span class="mg-step">2</span> {{ $t('migration:tables.title') }} <span class="mg-muted">{{ $t('migration:tables.pickedOf', { picked: picked.size, total: tables.length }) }}</span></h3>
        <div class="mg-row">
          <el-input v-model="tableFilter" clearable :placeholder="$t('common:filter')" size="small" />
          <el-button size="small" text @click="pickAll(true)">{{ $t('migration:tables.all') }}</el-button>
          <el-button size="small" text @click="pickAll(false)">{{ $t('migration:tables.none') }}</el-button>
        </div>
        <div class="mg-tables">
          <div v-if="loadingTables" class="mg-muted"><el-icon class="is-loading"><ei-loading /></el-icon> {{ $t('migration:tables.loading') }}</div>
          <div v-else-if="tablesError" class="mg-err">
            {{ tb(tablesError) }}
            <el-button size="small" style="margin-top: 6px" @click="loadTables(true)">{{ $t('common:retry') }}</el-button>
          </div>
          <div v-else-if="!tables.length" class="mg-muted">{{ $t('migration:tables.empty') }}</div>
          <label v-for="t in shownTables" :key="t.key" class="mg-table">
            <el-checkbox :model-value="picked.has(t.key)" size="small" @change="togglePick(t.key)" />
            <span :title="t.key.replace(/^\./, '')">{{ t.schema ? `${t.schema}.` : '' }}<b>{{ t.name }}</b></span>
          </label>
        </div>
      </section>

      <!-- 3. Options -->
      <section class="mg-card">
        <h3><span class="mg-step">3</span> {{ $t('migration:options.title') }}</h3>
        <template v-if="mode === 'convert'">
          <el-checkbox v-model="options.data">{{ $t('migration:options.data') }}</el-checkbox>
          <el-checkbox v-model="options.indexes">{{ $t('migration:options.indexes') }}</el-checkbox>
          <el-checkbox v-model="options.foreign_keys">{{ $t('migration:options.foreignKeys') }}</el-checkbox>
          <el-checkbox v-model="options.fold_case">{{ $t('migration:options.foldCase') }}</el-checkbox>
          <el-checkbox v-model="options.drop">{{ $t('migration:options.drop') }}</el-checkbox>
          <el-checkbox v-model="options.if_exists">{{ $t('migration:options.ifExists') }}</el-checkbox>
        </template>
        <template v-else-if="mode === 'clone'">
          <p class="mg-muted mg-small mg-mode-help">{{ $t('migration:clone.help') }}</p>
          <el-checkbox v-model="options.data">{{ $t('migration:options.data') }}</el-checkbox>
        </template>
        <template v-else>
          <p class="mg-muted mg-small mg-mode-help">{{ $t('migration:sync.help') }}</p>
          <label>{{ $t('migration:sync.depth') }}</label>
          <el-radio-group v-model="sync.depth" class="mg-depths">
            <el-radio v-for="d in DEPTHS" :key="d.value" :value="d.value">
              <span class="mg-depth">{{ $t(`migration:sync.depths.${d.key}`) }}</span>
              <span class="mg-muted mg-depth-why">{{ $t(`migration:sync.depths.${d.key}Hint`) }}</span>
            </el-radio>
          </el-radio-group>
          <label>{{ $t('migration:sync.maxCores') }} <span class="mg-muted">{{ $t('migration:sync.maxCoresHint') }}</span></label>
          <el-input-number v-model="sync.max_cores" :min="0" :max="256" size="small" style="width: 140px" />
        </template>
        <div class="mg-adv-head" @click="showAdvanced = !showAdvanced">
          <el-icon class="mg-caret" :class="{ open: showAdvanced }"><ei-arrow-right /></el-icon>
          {{ $t('migration:advanced.title') }}
        </div>
        <div v-if="showAdvanced" class="mg-adv">
          <label>{{ $t('migration:advanced.parallel') }} <span class="mg-muted">{{ $t('migration:advanced.parallelHint') }}</span></label>
          <el-input-number v-model="transfer.parallel" :min="1" :max="32" size="small" :disabled="!options.data && mode !== 'sync'" />
          <label>{{ $t('migration:advanced.order') }}</label>
          <el-select v-model="transfer.order" size="small" :disabled="!options.data && mode !== 'sync'">
            <el-option value="largest_first" :label="$t('migration:advanced.largestFirst')" />
            <el-option value="smallest_first" :label="$t('migration:advanced.smallestFirst')" />
            <el-option value="alphabetical" :label="$t('migration:advanced.alphabetical')" />
          </el-select>
          <label>{{ $t('migration:advanced.commitRows') }} <span class="mg-muted">{{ $t('migration:advanced.commitRowsHint') }}</span></label>
          <el-input-number v-model="transfer.commit_rows" :min="1000" :step="10000" size="small" :disabled="!options.data || mode === 'sync'" />
        </div>
        <div class="mg-actions">
          <el-button :loading="planning" :disabled="!targetDriver || !picked.size || running" @click="generate">
            <el-icon><ei-view /></el-icon>&nbsp;{{ $t('migration:preview') }}
          </el-button>
          <el-button type="primary" :disabled="!canRun" :loading="running" @click="runMigration">
            <el-icon><ei-switch /></el-icon>&nbsp;{{ actionLabel }}
          </el-button>
        </div>
        <p v-if="targetDriver && !targetConnection" class="mg-muted mg-small">{{ $t(`migration:options.needConnection${mode === 'convert' ? '' : 'Same'}`) }}</p>
      </section>
    </div>

    <el-alert v-if="planError" type="error" :title="planError" :closable="false" style="margin-top: 14px" />
    <el-alert v-if="runMissing && !showRun" type="info" :title="$t('migration:saved.runElsewhere')" :closable="false" style="margin-top: 14px" />

    <!-- Interrupted runs (the app closed while they ran) -->
    <section v-if="interrupted.length" class="mg-card mg-wide">
      <h3>
        <el-icon style="color: var(--nm-warning)"><ei-warning-filled /></el-icon>
        {{ $t('migration:interrupted.title') }}
        <span class="mg-muted">{{ $t('migration:interrupted.hint') }}</span>
      </h3>
      <div v-for="r in interrupted" :key="r.id" class="mg-irun">
        <span class="mg-muted">{{ runDate(r) }}</span>
        <span>→ <b>{{ runLabel(r) }}</b></span>
        <span class="mg-muted">{{ $t('migration:interrupted.tables', { done: r.tables.filter((x) => x.status === 'done').length, total: r.tables.length }) }}</span>
        <div style="flex: 1" />
        <el-button size="small" type="primary" :disabled="running" @click="resumeRun(r)">
          <el-icon><ei-video-play /></el-icon>&nbsp;{{ $t('migration:interrupted.resume') }}
        </el-button>
        <el-button size="small" text :disabled="running" @click="forgetRun(r)">{{ $t('migration:interrupted.forget') }}</el-button>
      </div>
    </section>

    <!-- The preview and the run: one at a time, the run's tab shows by itself when it starts -->
    <div v-if="plan || showRun" class="mg-panes">
      <button class="mg-pane" :class="{ on: pane === 'preview' }" :disabled="!plan" @click="pane = 'preview'">
        <el-icon><ei-view /></el-icon> {{ $t('migration:panes.preview') }}
      </button>
      <button class="mg-pane" :class="{ on: pane === 'run' }" :disabled="!showRun" @click="pane = 'run'">
        <el-icon v-if="running" class="is-loading"><ei-loading /></el-icon><el-icon v-else><ei-switch /></el-icon>
        {{ $t('migration:panes.run') }}
        <span v-if="showRun" class="mg-muted">{{ finishedTables }}/{{ rows.length }}</span>
        <span v-if="runCounts.failed" class="mg-pane-err">{{ runCounts.failed }}</span>
      </button>
    </div>

    <!-- Running / result -->
    <section v-if="showRun && pane === 'run'" class="mg-card mg-wide">
      <h3>
        <el-icon v-if="running" class="is-loading"><ei-loading /></el-icon>
        <el-icon v-else-if="runError || runStatus === 'failed'" style="color: var(--nm-danger)"><ei-circle-close-filled /></el-icon>
        <el-icon v-else-if="runStatus === 'cancelled'" style="color: var(--nm-warning)"><ei-warning-filled /></el-icon>
        <el-icon v-else style="color: var(--nm-success)"><ei-circle-check-filled /></el-icon>
        {{ runTitle }}
        <span class="mg-muted">{{ $t('migration:run.progress', { done: finishedTables, total: rows.length, rows: fmt(totalRows), elapsed }) }}</span>
        <div style="flex: 1" />
        <template v-if="running && movesData">
          <span class="mg-muted">{{ $t('migration:run.parallel') }}</span>
          <el-input-number v-model="parallel" :min="1" :max="32" size="small" style="width: 96px" @change="changeParallel" />
        </template>
        <el-button v-if="running" size="small" type="danger" plain @click="cancelRun">{{ $t('migration:run.cancelAll') }}</el-button>
        <el-button v-if="canResume" size="small" type="primary" @click="resumeRun(linkedRun!)">
          <el-icon><ei-video-play /></el-icon>&nbsp;{{ $t('migration:saved.resume') }}
        </el-button>
        <el-button v-if="!running && hasFailed && result" size="small" @click="retryFailed">
          <el-icon><ei-refresh-right /></el-icon>&nbsp;{{ $t('migration:run.retryFailed') }}
        </el-button>
        <el-select
          v-if="runHistory.length > 1"
          :model-value="runId"
          size="small"
          :disabled="running"
          class="mg-run-history"
          :title="$t('migration:saved.history')"
          @update:model-value="viewRun"
        >
          <el-option v-for="id in runHistory" :key="id" :value="id" :label="runOption(id)" />
        </el-select>
      </h3>
      <el-progress :percentage="overall" :stroke-width="6" :status="running ? undefined : runStatus === 'done' ? 'success' : 'exception'" />
      <div v-if="running && step" class="mg-progress">
        <span>{{ STEP[step.phase] ?? step.phase }}</span>
        <b v-if="step.table">{{ step.table }}</b>
        <span v-if="step.total" class="mg-muted">({{ $t('migration:run.stepOf', { step: Math.min(step.done + 1, step.total), total: step.total }) }})</span>
      </div>
      <pre v-if="runError" class="mg-runerr nm-selectable">{{ tb(runError) }}</pre>
      <div v-if="rows.length" class="mg-chips mg-run-filters">
        <button
          v-for="f in RUN_FILTERS"
          :key="f"
          class="mg-chip"
          :class="[`st-${f}`, { on: runFilter === f }]"
          :disabled="f !== 'all' && !runCounts[f]"
          @click="runFilter = f"
        >{{ $t(`migration:runFilter.${f}`) }} ({{ runCounts[f] }})</button>
        <el-input v-model="runSearch" clearable size="small" :placeholder="$t('migration:runFilter.search')" class="mg-run-search" />
      </div>
      <div v-if="rows.length" class="mg-grid-t" :class="{ sync: runMode === 'sync' }">
        <div class="mg-tr mg-col-h">
          <span>{{ $t('migration:cols.table') }}</span><span>{{ $t('migration:grid.status') }}</span>
          <span>{{ runMode === 'sync' ? $t('migration:grid.reviewed') : $t('migration:cols.rows') }}</span>
          <span>{{ $t('migration:grid.speed') }}</span><span>{{ $t('migration:grid.path') }}</span>
          <span>{{ runMode === 'sync' ? $t('migration:grid.changes') : $t('migration:grid.bottleneck') }}</span>
          <span>{{ $t('common:error') }}</span><span />
        </div>
        <div v-for="r in shownRows" :key="r.name" class="mg-tr">
          <span class="mg-tname" :title="`${r.source} → ${r.target}`">{{ r.source }}<span v-if="r.target !== r.source" class="mg-muted"> → {{ r.target }}</span></span>
          <span><span class="mg-st" :class="r.status">{{ statusLabel(r) }}</span></span>
          <span class="mg-rows">
            <span>{{ fmt(r.rows_done) }}<span v-if="r.rows_total != null" class="mg-muted"> / {{ fmt(r.rows_total) }}</span></span>
            <el-progress :percentage="pct(r)" :show-text="false" :stroke-width="4" :status="r.status === 'failed' ? 'exception' : r.status === 'done' ? 'success' : undefined" />
          </span>
          <span class="mono">{{ r.rate ? $t('migration:grid.perSecond', { n: fmt(r.rate) }) : '—' }}</span>
          <span>{{ PATH[r.stats?.path ?? r.path] ?? '—' }}</span>
          <span v-if="runMode === 'sync'" class="mg-changes">{{ changes(r) }}</span>
          <span v-else>{{ r.stats?.bottleneck ? $t(`migration:grid.${r.stats.bottleneck}`) : '—' }}</span>
          <span class="mg-reserr">
            <el-tooltip v-if="r.error" :content="tb(r.error)" placement="top" :show-after="300" popper-class="mg-tip">
              <span class="mg-errtext">{{ tb(r.error) }}</span>
            </el-tooltip>
          </span>
          <span class="mg-tact">
            <template v-if="running && movesData">
              <el-tooltip v-if="r.status === 'pending'" :content="$t('migration:grid.runNow')" placement="top" :show-after="300">
                <el-button size="small" text @click="runNow(r.name)"><el-icon><ei-video-play /></el-icon></el-button>
              </el-tooltip>
              <el-tooltip v-if="r.status === 'pending' || r.status === 'running' || r.status === 'copied'" :content="$t('migration:grid.cancel')" placement="top" :show-after="300">
                <el-button size="small" text @click="cancelTable(r.name)"><el-icon><ei-close /></el-icon></el-button>
              </el-tooltip>
            </template>
          </span>
        </div>
        <div v-if="!shownRows.length" class="mg-muted mg-none">{{ $t('migration:runFilter.none') }}</div>
      </div>
      <div v-if="result?.foreign_key_errors.length" class="mg-runerr">
        {{ $t('migration:run.fkErrors') }}
        <div v-for="e in result.foreign_key_errors" :key="e">· {{ tb(e) }}</div>
      </div>
      <div v-if="result?.after_errors?.length" class="mg-runerr">
        {{ $t('migration:run.afterErrors') }}
        <div v-for="e in result.after_errors" :key="e">· {{ tb(e) }}</div>
      </div>
      <div v-if="result?.notes.length" class="mg-notes">
        <div v-for="n in result.notes" :key="n">· {{ tb(n) }}</div>
      </div>
      <div v-if="logs.length" class="mg-notes">
        <div v-for="(l, k) in logs" :key="k" :class="{ 'mg-reserr': l.level === 'error' }">· {{ tb(l.text) }}</div>
      </div>
    </section>

    <!-- 4. Sync: each table's key -->
    <section v-if="plan && mode === 'sync' && pane === 'preview'" class="mg-card mg-wide">
      <h3>
        <span class="mg-step">4</span> {{ $t('migration:sync.keysTitle') }}
        <span class="mg-muted">{{ $t('migration:sync.keysSummary', { tables: plan.sync_tables.length, unsyncable }) }}</span>
      </h3>
      <p class="mg-muted mg-small">{{ $t('migration:sync.keysHelp') }}</p>
      <div class="mg-keys">
        <div v-for="s in plan.sync_tables" :key="keyOf(s)" class="mg-key">
          <span class="mg-where" :title="keyOf(s).replace(/^\./, '')">{{ s.schema ? `${s.schema}.` : '' }}<b>{{ s.name }}</b></span>
          <el-select
            v-if="keyChoices(s).length"
            :model-value="chosenKey(s)"
            size="small"
            :placeholder="$t('migration:sync.pickKey')"
            @update:model-value="(v: string) => setKey(s, v)"
          >
            <el-option v-for="c in keyChoices(s)" :key="c.value" :value="c.value" :label="c.label" />
          </el-select>
          <span v-else />
          <span v-if="syncReason(s)" class="mg-reserr">{{ tb(syncReason(s)) }}</span>
          <span v-else class="mg-ok-text"><el-icon><ei-circle-check-filled /></el-icon></span>
        </div>
      </div>
    </section>

    <!-- 4. Result -->
    <template v-if="plan && mode !== 'sync' && pane === 'preview'">
      <section class="mg-card mg-wide">
        <h3>
          <span class="mg-step">4</span> {{ mode === 'clone' ? $t('migration:clone.reportTitle') : $t('migration:report.title') }}
          <span class="mg-muted">{{ $t('migration:report.summary', { tables: plan.tables.length, issues: plan.issues.length }) }}</span>
        </h3>
        <div class="mg-chips">
          <button class="mg-chip" :class="{ on: severityFilter === 'all' }" @click="severityFilter = 'all'">{{ $t('migration:tables.all') }} ({{ plan.issues.length }})</button>
          <button
            v-for="s in ORDER"
            :key="s"
            class="mg-chip"
            :class="[SEVERITY[s].cls, { on: severityFilter === s }]"
            :title="SEVERITY[s].hint"
            :disabled="!counts[s]"
            @click="severityFilter = s"
          >{{ SEVERITY[s].label }} ({{ counts[s] }})</button>
        </div>
        <div v-if="!plan.issues.length" class="mg-ok"><el-icon><ei-circle-check-filled /></el-icon> {{ mode === 'clone' ? $t('migration:clone.clean') : $t('migration:report.clean') }}</div>
        <div v-else class="mg-issues">
          <div v-for="(i, k) in issues" :key="k" class="mg-issue">
            <span class="mg-sev" :class="SEVERITY[i.severity].cls">{{ SEVERITY[i.severity].label }}</span>
            <span class="mg-where">{{ i.table }}{{ i.object ? ` · ${i.object}` : '' }}</span>
            <span class="mg-msg">{{ tb(i.message) }}</span>
          </div>
        </div>

        <div v-if="plan.columns.length" class="mg-cols-head" @click="showColumns = !showColumns">
          <el-icon class="mg-caret" :class="{ open: showColumns }"><ei-arrow-right /></el-icon>
          {{ $t('migration:report.byColumn', { count: plan.columns.length }) }}
        </div>
        <template v-if="showColumns && plan.columns.length">
          <el-input v-model="columnFilter" clearable size="small" :placeholder="$t('migration:report.filterColumns')" style="max-width: 320px; margin-bottom: 6px" />
          <div class="mg-cols">
            <div class="mg-col mg-col-h"><span>{{ $t('migration:cols.table') }}</span><span>{{ $t('migration:cols.column') }}</span><span>{{ $t('migration:cols.sourceType') }}</span><span /><span>{{ $t('migration:cols.targetType') }}</span></div>
            <div v-for="(c, k) in columns" :key="k" class="mg-col">
              <span>{{ c.table }}</span>
              <span>{{ c.column || $t('migration:cols.new') }}<template v-if="c.column && c.target_column !== c.column"> → {{ c.target_column }}</template></span>
              <span class="mono">{{ c.source_type || '—' }}</span>
              <span class="mg-arrow">→</span>
              <span class="mono">{{ c.target_type }}</span>
            </div>
          </div>
        </template>
      </section>

      <section class="mg-card mg-wide">
        <h3>
          <span class="mg-step">5</span> {{ mode === 'clone' ? $t('migration:clone.scriptTitle', { engine: targetInfo?.name }) : $t('migration:script.title', { engine: targetInfo?.name }) }}
          <span class="mg-muted">{{ $t('migration:script.previewTag') }}</span>
          <div style="flex: 1" />
          <el-button size="small" type="primary" :disabled="!script" @click="openInQuery">{{ $t('migration:script.openInQuery') }}{{ targetConnection ? '' : '…' }}</el-button>
          <el-button size="small" :disabled="!script" @click="copyScript">{{ $t('common:copy') }}</el-button>
          <el-button size="small" :disabled="!script" @click="saveScript">{{ $t('migration:script.saveAs') }}</el-button>
        </h3>
        <p class="mg-muted mg-small">{{ mode === 'clone' ? $t('migration:clone.scriptHelp') : $t('migration:script.help') }}</p>
        <div class="mg-script">
          <CodeEditor v-model="script" :language="targetInfo?.language ?? 'sql'" :dialect="targetInfo?.dialect ?? ''" />
        </div>
      </section>
    </template>
  </div>
</template>

<style scoped>
.mg { height: 100%; overflow: auto; padding: 18px 22px 30px; box-sizing: border-box; }
.mg-head { display: flex; gap: 12px; align-items: flex-start; margin-bottom: 16px; color: var(--nm-accent); }
.mg-head h2 { margin: 0 0 4px; font-size: 17px; color: var(--nm-text-strong); }
.mg-head-main { flex: 1; min-width: 0; }
.mg-title-row { display: flex; align-items: baseline; gap: 10px; margin-bottom: 4px; min-width: 0; }
.mg-kicker { flex: none; font-size: 12px; text-transform: uppercase; letter-spacing: 0.04em; color: var(--nm-text-dim); }
.mg-title { flex: 1; min-width: 120px; max-width: 640px; padding: 1px 4px; margin-left: -4px; border: 1px solid transparent; border-radius: 4px; background: transparent; color: var(--nm-text-strong); font: inherit; font-size: 17px; font-weight: 600; }
.mg-title:hover { border-color: var(--nm-border); }
.mg-title:focus { outline: none; border-color: var(--nm-accent); background: var(--nm-bg-elev, var(--ide-editor)); }
.mg-run-history { width: 230px; }
.mg-head p { margin: 0; max-width: 900px; font-size: 13px; line-height: 1.5; color: var(--nm-text); }
.mg-grid { display: grid; grid-template-columns: repeat(3, minmax(260px, 1fr)); gap: 14px; align-items: start; }
.mg-card { min-width: 0; display: flex; flex-direction: column; gap: 6px; padding: 12px 14px; border: 1px solid var(--nm-border); border-radius: 6px; background: var(--nm-bg-card, transparent); }
.mg-wide { margin-top: 14px; }
.mg-card h3 { display: flex; align-items: center; gap: 8px; margin: 0 0 6px; font-size: 13.5px; color: var(--nm-text-strong); }
.mg-card label { font-size: 12px; color: var(--nm-text-dim); margin-top: 4px; }
.mg-step { display: inline-flex; align-items: center; justify-content: center; width: 20px; height: 20px; border-radius: 50%; background: var(--nm-accent); color: #fff; font-size: 11px; }
.mg-muted { color: var(--nm-text-dim); font-size: 12px; font-weight: normal; }
.mg-small { font-size: 11.5px; line-height: 1.45; margin: 4px 0 0; }
.mg-opt-why { float: right; margin-left: 12px; max-width: 260px; overflow: hidden; text-overflow: ellipsis; font-size: 11px; color: var(--nm-text-dim); }
.mg-actions { display: flex; gap: 8px; margin-top: 8px; flex-wrap: wrap; }
.mg-actions .el-button + .el-button { margin-left: 0; }
.mg-progress { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; font-size: 12.5px; }
.mg-runerr { margin: 6px 0 0; white-space: pre-wrap; font-size: 12px; color: var(--nm-danger); font-family: inherit; }
.mg-adv-head { display: flex; align-items: center; gap: 5px; margin-top: 6px; font-size: 12.5px; color: var(--nm-text-strong); cursor: pointer; user-select: none; }
.mg-adv { display: flex; flex-direction: column; gap: 4px; padding-left: 16px; }
.mg-adv .el-select, .mg-adv .el-input-number { width: 180px; }
.mg-irun { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; padding: 4px 0; border-bottom: 1px solid var(--nm-border-soft, var(--nm-border)); font-size: 12.5px; }
.mg-irun .el-button + .el-button { margin-left: 0; }
.mg-grid-t { max-height: 420px; overflow: auto; font-size: 12px; margin-top: 6px; }
.mg-tr { display: grid; grid-template-columns: minmax(160px, 2fr) 96px minmax(130px, 1.2fr) 90px 100px 72px minmax(120px, 1.6fr) 60px; gap: 8px; align-items: center; padding: 3px 0; border-bottom: 1px solid var(--nm-border-soft, var(--nm-border)); }
.mg-tname { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--nm-text-strong); }
.mg-rows { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
.mg-tr .mono { font-family: var(--nm-mono); }
.mg-st { display: inline-block; padding: 0 7px; border-radius: 9px; border: 1px solid var(--nm-border); font-size: 11px; white-space: nowrap; color: var(--nm-text-dim); }
.mg-st.running, .mg-st.copied { color: var(--nm-accent); border-color: color-mix(in srgb, var(--nm-accent) 60%, transparent); }
.mg-st.done { color: var(--nm-success); border-color: color-mix(in srgb, var(--nm-success) 60%, transparent); }
.mg-st.failed { color: var(--nm-danger); border-color: color-mix(in srgb, var(--nm-danger) 60%, transparent); }
.mg-st.cancelled { color: var(--nm-warning); border-color: color-mix(in srgb, var(--nm-warning) 60%, transparent); }
.mg-errtext { display: block; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; cursor: default; }
.mg-tact { display: flex; justify-content: flex-end; }
.mg-tact .el-button { padding: 2px 4px; margin-left: 0; }
.mg-notes { margin-top: 6px; font-size: 12px; color: var(--nm-text-dim); line-height: 1.5; }
.mg-reserr { color: var(--nm-danger); overflow-wrap: anywhere; }
.mg-err { display: flex; flex-direction: column; align-items: flex-start; font-size: 12px; color: var(--nm-danger); line-height: 1.45; }
.mg-row { display: flex; gap: 6px; align-items: center; min-width: 0; }
.mg-row .el-input { flex: 1; min-width: 0; }
.mg-row .el-button { margin-left: 0; flex: none; }
/* Long option labels wrap inside the card. */
.mg-card :deep(.el-checkbox) { height: auto; min-height: 22px; margin-right: 0; white-space: normal; align-items: flex-start; }
.mg-card :deep(.el-checkbox__input) { margin-top: 3px; }
.mg-card :deep(.el-checkbox__label) { white-space: normal; line-height: 1.45; color: var(--nm-text); }
.mg-tables { max-height: 260px; overflow-y: auto; overflow-x: hidden; border: 1px solid var(--nm-border); border-radius: 4px; padding: 4px 6px; }
.mg-table > span { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.mg-table :deep(.el-checkbox) { align-items: center; flex: none; }
.mg-table { display: flex; align-items: center; gap: 6px; min-width: 0; padding: 1px 0; font-size: 12.5px; color: var(--nm-text); cursor: pointer; margin: 0 !important; }
.mg-chips { display: flex; flex-wrap: wrap; gap: 6px; margin-bottom: 8px; }
.mg-panes { display: flex; gap: 2px; margin-top: 14px; border-bottom: 1px solid var(--nm-border); }
.mg-pane { display: inline-flex; align-items: center; gap: 6px; padding: 6px 14px; border: 1px solid transparent; border-bottom: none; border-radius: 4px 4px 0 0; background: transparent; color: var(--nm-text-dim); font: inherit; font-size: 12.5px; cursor: pointer; margin-bottom: -1px; }
.mg-pane.on { background: var(--nm-bg-elev, var(--ide-editor)); border-color: var(--nm-border); color: var(--nm-text-strong); }
.mg-pane:disabled { opacity: 0.4; cursor: default; }
.mg-pane-err { min-width: 16px; padding: 0 5px; border-radius: 8px; background: var(--nm-danger); color: #fff; font-size: 11px; text-align: center; }
.mg-panes + .mg-card { margin-top: 0; border-top-left-radius: 0; }
.mg-run-filters { align-items: center; margin-top: 8px; }
.mg-run-search { width: 220px; margin-left: auto; }
.mg-chip.st-failed.on { border-color: var(--nm-danger); color: var(--nm-danger); }
.mg-none { padding: 12px 4px; }
.mg-chip { padding: 2px 10px; border-radius: 12px; border: 1px solid var(--nm-border); background: transparent; color: var(--nm-text); font: inherit; font-size: 12px; cursor: pointer; }
.mg-chip:disabled { opacity: 0.4; cursor: default; }
.mg-chip.on { background: var(--ide-selection); color: var(--nm-text-strong); }
.mg-chip.dropped, .mg-sev.dropped { border-color: color-mix(in srgb, var(--nm-danger) 60%, transparent); }
.mg-chip.loss, .mg-sev.loss { border-color: color-mix(in srgb, var(--nm-warning) 70%, transparent); }
.mg-chip.warning, .mg-sev.warning { border-color: color-mix(in srgb, var(--nm-info, #4fc1ff) 60%, transparent); }
.mg-issues { max-height: 320px; overflow: auto; }
.mg-issue { display: grid; grid-template-columns: 72px minmax(120px, 240px) 1fr; gap: 10px; align-items: baseline; padding: 4px 0; border-bottom: 1px solid var(--nm-border-soft, var(--nm-border)); font-size: 12.5px; }
.mg-sev { justify-self: start; padding: 0 7px; border-radius: 9px; border: 1px solid var(--nm-border); font-size: 11px; }
.mg-sev.dropped { color: var(--nm-danger); }
.mg-sev.loss { color: var(--nm-warning); }
.mg-sev.warning { color: var(--nm-info, #4fc1ff); }
.mg-sev.info { color: var(--nm-text-dim); }
.mg-where { color: var(--nm-text-strong); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.mg-msg { color: var(--nm-text); }
.mg-ok { display: flex; align-items: center; gap: 6px; color: var(--nm-success); font-size: 12.5px; }
.mg-cols-head { display: flex; align-items: center; gap: 5px; margin-top: 12px; font-size: 12.5px; color: var(--nm-text-strong); cursor: pointer; user-select: none; }
.mg-caret { transition: transform 0.1s; font-size: 11px; }
.mg-caret.open { transform: rotate(90deg); }
.mg-cols { max-height: 320px; overflow: auto; font-size: 12px; }
.mg-col { display: grid; grid-template-columns: minmax(100px, 1fr) minmax(120px, 1.2fr) minmax(110px, 1fr) 18px minmax(110px, 1fr); gap: 8px; padding: 2px 0; border-bottom: 1px solid var(--nm-border-soft, var(--nm-border)); }
.mg-col-h { color: var(--nm-text-dim); font-size: 11px; text-transform: uppercase; }
.mg-col .mono { font-family: var(--nm-mono); }
.mg-arrow { color: var(--nm-text-dim); }
.mg-modes { display: flex; flex-wrap: wrap; }
.mg-mode-help { margin: 0 0 4px; }
.mg-depths { display: flex; flex-direction: column; align-items: flex-start; gap: 2px; }
.mg-depths :deep(.el-radio) { height: auto; min-height: 22px; margin-right: 0; white-space: normal; align-items: flex-start; }
.mg-depths :deep(.el-radio__input) { margin-top: 3px; }
.mg-depths :deep(.el-radio__label) { display: flex; flex-direction: column; line-height: 1.4; }
.mg-depth { color: var(--nm-text); font-size: 12.5px; }
.mg-depth-why { font-size: 11.5px; }
.mg-grid-t.sync .mg-tr { grid-template-columns: minmax(160px, 2fr) 110px minmax(130px, 1.2fr) 90px 100px minmax(200px, 1.4fr) minmax(120px, 1.6fr) 60px; }
.mg-changes { font-family: var(--nm-mono); font-size: 11.5px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.mg-keys { max-height: 360px; overflow: auto; }
.mg-key { display: grid; grid-template-columns: minmax(160px, 1fr) minmax(200px, 1.2fr) minmax(160px, 1.6fr); gap: 10px; align-items: center; padding: 3px 0; border-bottom: 1px solid var(--nm-border-soft, var(--nm-border)); font-size: 12.5px; }
.mg-ok-text { color: var(--nm-success); display: flex; align-items: center; }
.mg-script { height: 380px; border: 1px solid var(--nm-border); border-radius: 4px; overflow: hidden; }
@media (max-width: 1100px) { .mg-grid { grid-template-columns: 1fr; } }
</style>
