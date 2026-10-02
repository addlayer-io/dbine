<script setup lang="ts">
import { computed, onBeforeUnmount, reactive, ref, watch } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import { dataCompareApi, type DataCompareResult, type DataScript, type DataSide, type Dir } from '../api/dataCompare';
import type { Cell, ObjectRef, QueryProgress } from '../api/types';
import { newQuery } from '../composables/actions';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import { dbKey, objKey, useConnectionsStore } from '../stores/connections';
import { useTabsStore, type DataCompareTab } from '../stores/tabs';
import { startTask, useTasksStore, type TaskHandle } from '../stores/tasks';

// Data compare (docs/comparacion-de-datos.md): a table's rows (left) against
// another table's (right), by key; then a script, in the target engine's
// language, that makes one side like the other. Nothing runs without the
// user's click.

const props = defineProps<{ tab: DataCompareTab }>();
const conns = useConnectionsStore();
const { t } = useTranslation();

interface Pick { connectionId: string; database: string; table: string }
const tableId = (o: ObjectRef | null) => (o ? `${o.schema ?? ''}\u0001${o.name}` : '');
/** Last time's picks (restored with the app), when their connections still exist. */
const kept = (s: 'left' | 'right') => {
  const p = props.tab.picks?.[s];
  return p && conns.byId(p.connectionId) ? { connectionId: p.connectionId, database: p.database, table: p.table ?? '' } : null;
};
const left = reactive<Pick>(kept('left') ?? { connectionId: props.tab.connectionId, database: props.tab.database, table: tableId(props.tab.object) });
const right = reactive<Pick>(kept('right') ?? { connectionId: props.tab.connectionId, database: props.tab.database, table: '' });
const limit = ref(200000);

/** The databases and tables to choose from, per side. */
function dbsOf(p: Pick): string[] {
  return conns.live[p.connectionId]?.databases ?? [];
}
function tablesOf(p: Pick) {
  const d = conns.driverOf(p.connectionId);
  const browsable = new Set(d?.object_kinds.filter((k) => k.browsable ?? true).map((k) => k.id) ?? []);
  return (conns.objects[dbKey(p.connectionId, p.database)]?.items ?? []).filter((o) => browsable.has(o.kind));
}
async function prepare(p: Pick) {
  if (!p.connectionId) return;
  if (!(await conns.ensureConnected(p.connectionId))) return;
  const live = conns.live[p.connectionId];
  // Another connection keeps the database if it has one by that name.
  if (live && live.databases.length && !live.databases.includes(p.database)) p.database = live.defaultDatabase || live.databases[0];
  await conns.loadObjects(p.connectionId, p.database);
}
watch(() => [left.connectionId, left.database], () => prepare(left), { immediate: true });
watch(() => [right.connectionId, right.database], () => prepare(right), { immediate: true });

/** The table in `list` that is `id` (schema and name; engines' default
 *  schemas count as the same, so dbo.X finds public.x). */
function sameTable(list: ReturnType<typeof tablesOf>, id: string) {
  const [schema, name] = id.split('\u0001');
  return list.find((o) => tableId(o) === id)
    ?? list.find((o) => o.name.toLowerCase() === (name ?? '').toLowerCase() && normSchema(o.schema) === normSchema(schema || null));
}
// Changing the connection or database keeps the chosen table when the new
// one has it; only a table that isn't there is cleared. Waits for the real
// list (a cached one may lack new tables).
for (const p of [left, right]) {
  watch(() => [p.connectionId, p.database, p.table, conns.objects[dbKey(p.connectionId, p.database)]?.status], () => {
    if (!p.table || conns.objects[dbKey(p.connectionId, p.database)]?.status !== 'ready') return;
    const o = sameTable(tablesOf(p), p.table);
    p.table = o ? tableId(o) : '';
  });
}
// Same table on the right by default (same schema first).
watch(() => [left.table, tablesOf(right).length], () => {
  if (right.table || !left.table) return;
  if (right.connectionId === left.connectionId && right.database === left.database) return;
  const list = tablesOf(right);
  const name = left.table.split('\u0001')[1] ?? '';
  const same = sameTable(list, left.table) ?? list.find((o) => o.name.toLowerCase() === name.toLowerCase());
  if (same) right.table = tableId(same);
});

function side(p: Pick): DataSide | null {
  const o = tablesOf(p).find((x) => tableId(x) === p.table);
  return o ? { connection_id: p.connectionId, database: p.database, object: { kind: o.kind, schema: o.schema, name: o.name } } : null;
}

// -- not the same table on both sides ------------------------------------------------
// Two tables with the same name in different schemas (dbo.X against sos.X)
// look alike in the pickers, and syncing them overwrites the wrong table.
// Engines' default schemas count as the same (dbo against public is fine).
const DEFAULT_SCHEMAS = new Set(['dbo', 'public', 'main']);
const normSchema = (s: string | null) => (!s || DEFAULT_SCHEMAS.has(s.toLowerCase()) ? '' : s.toLowerCase());
const qualified = (o: ObjectRef) => (o.schema ? `${o.schema}.${o.name}` : o.name);
const mismatch = computed<{ kind: 'name' | 'schema'; left: string; right: string } | null>(() => {
  const l = side(left);
  const r = side(right);
  if (!l || !r) return null;
  const names = { left: qualified(l.object), right: qualified(r.object) };
  if (l.object.name.toLowerCase() !== r.object.name.toLowerCase()) return { kind: 'name', ...names };
  if (normSchema(l.object.schema) !== normSchema(r.object.schema)) return { kind: 'schema', ...names };
  return null;
});
const mismatchText = computed(() => (mismatch.value ? t(`dataCompare:mismatch.${mismatch.value.kind}`, { left: mismatch.value.left, right: mismatch.value.right }) : ''));

const running = ref(false);
const result = ref<DataCompareResult | null>(null);
const error = ref<string | null>(null);
const view = ref<'changed' | 'only_left' | 'only_right'>('changed');
const key = ref<string[]>(props.tab.picks?.key ?? []);
const tabs = useTabsStore();
watch(
  () => [left.connectionId, left.database, left.table, right.connectionId, right.database, right.table, key.value.join('\u0001')],
  () => tabs.setPicks(props.tab.id, { left: { ...left }, right: { ...right }, key: [...key.value] }),
);

/** The left table's columns, to choose the key before comparing (a table
 *  without a primary key needs one). */
const leftColumns = computed(() => {
  const o = tablesOf(left).find((x) => tableId(x) === left.table);
  if (!o) return [] as string[];
  return (conns.columns[objKey(left.connectionId, left.database, o.schema, o.name)]?.items ?? []).map((c) => c.name);
});
watch(() => [left.connectionId, left.database, left.table], () => { key.value = []; });
// Load its columns once the table (and the database's object list) is there.
watch(() => [left.connectionId, left.database, left.table, tablesOf(left).length], () => {
  const o = tablesOf(left).find((x) => tableId(x) === left.table);
  if (o) conns.loadColumns(left.connectionId, left.database, o);
}, { immediate: true });

async function compare() {
  const l = side(left);
  const r = side(right);
  if (!l || !r) { ElMessage.warning(t('dataCompare:pickBoth')); return; }
  running.value = true;
  error.value = null;
  try {
    result.value = await dataCompareApi.compare(l, r, key.value, limit.value);
    key.value = result.value.key;
    view.value = result.value.counts.changed ? 'changed' : result.value.counts.only_left ? 'only_left' : 'only_right';
  } catch (e) {
    error.value = errorMessage(e);
    result.value = null;
  } finally {
    running.value = false;
  }
}

const fmtN = (n: number) => n.toLocaleString(locale());
function show(v: Cell): string {
  if (v === null) return 'NULL';
  return typeof v === 'string' ? v : String(v);
}
const rows = computed(() => {
  const r = result.value;
  if (!r) return [];
  if (view.value === 'only_left') return r.only_left;
  if (view.value === 'only_right') return r.only_right;
  return [];
});
const shownOf = (n: number, list: unknown[]) => (n > list.length ? t('dataCompare:showingFirst', { shown: fmtN(list.length), total: fmtN(n) }) : '');

// -- what goes where: arrows per row, as in the schema compare ------------------

type Kind = 'changed' | 'only_left' | 'only_right';
/** Per kind: the direction of every row, and the rows set apart from it
 *  (by index, including rows past the ones shown). */
const choices = reactive<Record<Kind, { all: Dir; rows: Map<number, Dir> }>>({
  changed: { all: 'none', rows: new Map() },
  only_left: { all: 'none', rows: new Map() },
  only_right: { all: 'none', rows: new Map() },
});
function resetChoices() {
  for (const k of ['changed', 'only_left', 'only_right'] as Kind[]) {
    choices[k].all = 'none';
    choices[k].rows = new Map();
  }
}
watch(() => result.value?.id, resetChoices);
function dirOf(kind: Kind, i: number): Dir {
  return choices[kind].rows.get(i) ?? choices[kind].all;
}
/** A click on an arrow sends the row that way; a second click leaves it alone. */
function toggle(kind: Kind, i: number, d: Dir) {
  const c = choices[kind];
  const next = dirOf(kind, i) === d ? 'none' : d;
  if (next === c.all) c.rows.delete(i);
  else c.rows.set(i, next);
}
/** Every row of a kind (also those not shown) one way. */
function setAll(kind: Kind, d: Dir) {
  choices[kind].all = d;
  choices[kind].rows = new Map();
}
/** How many rows of a kind will change. */
function chosen(kind: Kind): number {
  const r = result.value;
  if (!r) return 0;
  const c = choices[kind];
  let n = c.all !== 'none' ? r.counts[kind] : 0;
  for (const d of c.rows.values()) {
    if (d === c.all) continue;
    if (c.all !== 'none') n--;
    if (d !== 'none') n++;
  }
  return n;
}
const totalChosen = computed(() => chosen('changed') + chosen('only_left') + chosen('only_right'));
/** What an arrow does on a row of this kind, for its tooltip. */
function arrowTip(kind: Kind, d: 'left' | 'right') {
  const what = kind === 'changed' ? 'update' : (kind === 'only_left') === (d === 'right') ? 'insert' : 'delete';
  return t(`dataCompare:arrow.${what}.${d}`);
}
/** "Make the right (left) match": changed and missing rows that way.
 *  Deleting is never picked for you: those rows are chosen one by one or
 *  with "all" in their own view. */
function matchSide(d: 'left' | 'right') {
  setAll('changed', d);
  setAll(d === 'right' ? 'only_left' : 'only_right', d);
}

// -- the scripts -----------------------------------------------------------------

const sync = reactive({ open: false, tab: '' });
const scripts = ref<DataScript[]>([]);
const scriptError = ref<string | null>(null);
const building = ref(false);
const applying = ref(false);
function pickOf(kind: Kind): { all: Dir; rows: [number, Dir][] } {
  return { all: choices[kind].all, rows: [...choices[kind].rows.entries()] };
}
/** The build's task, while it runs (there's no backend cancel for it: it
 *  only reads the compare kept in memory, so it's short). */
let buildTaskHandle: TaskHandle | null = null;
/** The last build that ended well: "Ver detalle" reopens its scripts until
 *  another build or a run replaces them. */
let builtTaskHandle: TaskHandle | null = null;
async function openSync() {
  const res = result.value;
  if (!res) return;
  sync.open = true;
  // A run in progress keeps its scripts, and a build in progress fills them: the dialog shows it.
  if (applying.value || building.value) return;
  building.value = true;
  scripts.value = [];
  scriptError.value = null;
  stale.value = null;
  builtTaskHandle?.setReopen(undefined);
  builtTaskHandle = null;
  const task = track(startTask<DataScript[]>({
    kind: 'data-script',
    title: t('tasks:dataCompare.buildTitle', { table: tableName() }),
    connectionId: left.connectionId,
    database: left.database,
    reopen: reopenSync,
  }));
  buildTaskHandle = task;
  task.progress({ phase: t('tasks:dataCompare.buildPhase') });
  let ok = false;
  try {
    const built = await dataCompareApi.scripts(res.id, { changed: pickOf('changed'), only_left: pickOf('only_left'), only_right: pickOf('only_right') });
    const changing = built.filter((s) => s.script.trim());
    task.finish(built, changing.length
      ? changing.map((s) => `${sideName(s.side)}: ${summaryOf(s)}`).join(' · ')
      : t('dataCompare:noChanges'));
    // A compare reloaded meanwhile: these scripts belong to the old one.
    if (!alive || result.value?.id !== res.id) return;
    scripts.value = built;
    sync.tab = built[0]?.side ?? '';
    ok = true;
  } catch (e) {
    task.fail(e);
    if (alive) scriptError.value = errorMessage(e);
  } finally {
    building.value = false;
    buildTaskHandle = null;
    if (ok) builtTaskHandle = task;
    else task.setReopen(undefined);
  }
}
const tableName = () => (left.table.split('\u0001')[1] ?? '') || (right.table.split('\u0001')[1] ?? '');
function sideName(side: 'left' | 'right') {
  const p = side === 'right' ? right : left;
  return `${conns.byId(p.connectionId)?.name ?? ''} · ${p.table.split('\u0001')[1] ?? ''}`;
}
/** "1 inserción, 3 actualizaciones y 2 borrados" (only what the script does). */
function summaryOf(sc: DataScript) {
  const parts = ([['insert', sc.inserts], ['update', sc.updates], ['delete', sc.deletes]] as const)
    .filter(([, n]) => n > 0)
    .map(([k, n]) => t(`dataCompare:count.${k}`, { count: n, n: fmtN(n) }));
  return parts.length > 1 ? `${parts.slice(0, -1).join(', ')} ${t('dataCompare:count.and')} ${parts[parts.length - 1]}` : parts[0] ?? '';
}
const currentScript = computed(() => scripts.value.find((s) => s.side === sync.tab) ?? null);
async function copyScript() {
  if (!currentScript.value) return;
  await navigator.clipboard.writeText(currentScript.value.script);
  ElMessage.success(t('dataCompare:copied'));
}
async function openInQuery() {
  const s = currentScript.value;
  if (!s) return;
  await newQuery(s.connection_id, s.database, s.script, t('dataCompare:scriptName'));
}
// The run is a task (stores/tasks.ts): "Seguir en segundo plano" closes the
// dialog and the run goes on; closing the tab doesn't stop it either (its
// sessions are its own, not the tab's). The Tareas panel cancels it and,
// while this view lives, reopens this dialog.
const tasks = useTasksStore();
/** Every task this view started: when it unmounts, their "Ver detalle"
 *  falls back to the panel's own and the running ones notify at the end. */
const started = new Set<TaskHandle<any>>();
function track<R>(task: TaskHandle<R>): TaskHandle<R> {
  started.add(task as TaskHandle<any>);
  return task;
}
const isRunning = (task: TaskHandle<any> | null) => !!task && tasks.byId(task.id)?.state === 'running';
let runTaskHandle: TaskHandle | null = null;
/** The run's task id, reactive (for the footer's "Cancelando…"). */
const runTaskId = ref<string | null>(null);
const cancelling = computed(() => !!runTaskId.value && !!tasks.byId(runTaskId.value)?.cancelling);
/** These scripts can't run again until rebuilt: 'stopped' when a run
 *  stopped partway (cancel or error; the data may have changed), 'applied'
 *  once they ran (running them again would apply them twice). */
const stale = ref<'stopped' | 'applied' | null>(null);
let alive = true;
onBeforeUnmount(() => {
  alive = false;
  for (const task of started) {
    // Without the view, "Ver detalle" shows the panel's own log…
    task.setReopen(undefined);
    // …and what's still running tells when it ends.
    if (isRunning(task)) task.background();
  }
  started.clear();
});
function reopenSync() {
  tabs.activate(props.tab.id);
  sync.open = true;
}
/** Closing the dialog while it runs (or builds) sends it to the background. */
watch(() => sync.open, (open) => {
  if (open) return;
  if (applying.value && isRunning(runTaskHandle)) runTaskHandle!.background();
  if (building.value && isRunning(buildTaskHandle)) buildTaskHandle!.background();
});
function toBackground() {
  runTaskHandle?.background();
  buildTaskHandle?.background();
  sync.open = false;
}
function cancelRun() {
  if (runTaskHandle) tasks.cancel(runTaskHandle.id);
}

/** `data_compare_script` also says how many statements each script has (the
 *  units its run reports as they end); typed here until api/dataCompare.ts
 *  carries it. */
const statementsOf = (s: DataScript) => Math.max(1, s.statements ?? 1);
/** The side runs inside one transaction (the engine has manual ones). */
const atomicOf = (s: DataScript) => s.atomic === true;

/** Runs every script, one side after the other; stops at the first error. */
async function apply() {
  if (!scripts.value.length || applying.value || building.value || stale.value) return;
  if (mismatch.value) {
    try {
      await ElMessageBox.confirm(mismatchText.value, t('dataCompare:mismatch.title'), {
        confirmButtonText: t('dataCompare:mismatch.runAnyway'), cancelButtonText: t('common:cancel'), type: 'warning',
      });
    } catch {
      return;
    }
  }
  const toRun = scripts.value.filter((s) => s.script.trim());
  const first = toRun[0];
  if (!first) return;
  const table = tableName();
  let session: string | null = null;
  const task = startTask({
    kind: 'data-sync',
    title: t('tasks:dataSync.title', { table, where: toRun.map((s) => sideName(s.side)).join(' / ') }),
    connectionId: first.connection_id,
    database: first.database,
    cancel: () => (session ? api.cancelQuery(session) : undefined),
    reopen: reopenSync,
  });
  track(task);
  builtTaskHandle?.setReopen(undefined);
  builtTaskHandle = null;
  runTaskHandle = task;
  runTaskId.value = task.id;
  // Progress in statements across every side: each side's run reports its
  // statements as they end (query-progress, as in the editor) where the
  // engine allows; the others advance when their side ends.
  const total = toRun.reduce((n, s) => n + statementsOf(s), 0);
  task.progress({ done: 0, total, unit: t('tasks:dataSync.unit') });
  /** Statements of the sides already run, the running side's, and its session. */
  let base = 0;
  let sideTotal = 0;
  let liveSession: string | null = null;
  // At most one update every 200 ms: a side can end thousands of statements.
  let shown = 0;
  let lastShown = 0;
  let pending: ReturnType<typeof setTimeout> | null = null;
  const flush = () => {
    if (pending) clearTimeout(pending);
    pending = null;
    lastShown = Date.now();
    task.progress({ done: shown });
  };
  const report = (done: number, now = false) => {
    shown = Math.max(shown, Math.min(done, total));
    if (now) flush();
    else pending ??= setTimeout(flush, Math.max(0, lastShown + 200 - Date.now()));
  };
  applying.value = true;
  scriptError.value = null;
  /** The sides whose script ran to the end. */
  const applied: DataScript[] = [];
  /** What already changed, for the task's summary: one side may be applied
   *  and the other not, and the Tareas panel has to say so. */
  const appliedText = (stoppedOn?: DataScript) => [
    applied.length
      ? t('tasks:dataSync.applied', { sides: applied.map((s) => `${sideName(s.side)}: ${summaryOf(s)}`).join(' · ') })
      : t('tasks:dataSync.nothingApplied'),
    ...(stoppedOn ? [t('tasks:dataSync.stoppedOn', { side: sideName(stoppedOn.side) })] : []),
  ].join('. ');
  /** Stopped partway: the data may no longer match the scripts. They can't
   *  run again (that would apply some changes twice) and the compare reloads. */
  const stopped = async (how: 'cancel' | 'error', stoppedOn?: DataScript, error?: string) => {
    stale.value = 'stopped';
    task.setReopen(undefined);
    const summary = appliedText(stoppedOn);
    task.log(summary, how === 'error' ? 'error' : 'info');
    if (how === 'cancel') task.cancelled(summary);
    else task.fail(`${error ?? ''} ${summary}.`.trim());
    if (alive) await compare();
  };
  try {
    await task.listen<QueryProgress>('query-progress', ({ payload: p }) => {
      if (p.session_id !== liveSession) return;
      // `total` is 0 when the driver splits the script itself: then it's
      // the count from data_compare_script (the same cut).
      const n = p.total > 0 ? Math.round(((p.statement + 1) * sideTotal) / p.total) : p.statement + 1;
      report(base + Math.min(n, sideTotal));
    });
    for (const s of toRun) {
      // Cancelar between two sides: the next one doesn't start.
      if (task.isCancelling) { await stopped('cancel'); return; }
      const sessionId = `dsync:${s.side}:${Date.now()}`;
      session = sessionId;
      liveSession = sessionId;
      sideTotal = statementsOf(s);
      task.progress({ phase: t('tasks:dataSync.phase', { side: sideName(s.side), parts: summaryOf(s) }) });
      task.log(t('tasks:dataSync.phase', { side: sideName(s.side), parts: summaryOf(s) }));
      // The backend only sees a cancel once the session is connected and the
      // script has started (it clears earlier ones), so a Cancelar pressed
      // while connecting would be lost: keep sending it until the script ends.
      const retry = setInterval(() => { if (task.isCancelling) api.cancelQuery(sessionId).catch(() => {}); }, 500);
      let failure: string | null = null;
      const atomic = atomicOf(s);
      /** The side may hold part of its changes: false once its transaction
       *  is known to be rolled back (or never opened). */
      let partial = !atomic;
      try {
        // Where the engine has manual transactions, as the editor runs it
        // ('auto': statement by statement, with progress events) inside one
        // transaction that commits only when every statement ran: a failure
        // or a cancel leaves the side as it was. Elsewhere, the whole script
        // in one call ('whole'), as before: the engine's own atomicity (a
        // multi-statement text is one implicit transaction on some) stays.
        // Stops at the first error. The script is generated (every
        // UPDATE/DELETE goes by key), so no WHERE check.
        const o = await api.executeQuery({
          sessionId, connectionId: s.connection_id, database: s.database, sql: s.script, maxRows: 10, record: true,
          ...(atomic
            ? { mode: 'auto' as const, autocommit: false, continueOnError: false, confirmedUnsafe: true }
            : { mode: 'whole' as const }),
        });
        clearInterval(retry);
        if (o.error) failure = tb(o.error);
        else if (atomic && task.isCancelling) failure = '';
        else if (atomic) {
          // A commit that fails is an error of the side; whether anything
          // stayed is then unknown.
          // Past this point Cancelar must not reach the session: a cancel
          // that interrupts the COMMIT would leave its outcome unknown. A
          // cancel now stops before the next side instead.
          session = null;
          partial = true;
          await api.commitTab(sessionId);
          partial = false;
        }
      } catch (e) {
        failure = errorMessage(e);
      } finally {
        clearInterval(retry);
        session = null;
        liveSession = null;
        if (failure !== null && atomic) {
          // Closing the session would roll it back too; this says so first.
          await api.rollbackTab(sessionId).catch(() => {});
        }
        api.closeSession(sessionId).catch(() => {});
      }
      if (failure !== null) {
        const stoppedOn = partial ? s : undefined;
        if (task.isCancelling) { await stopped('cancel', stoppedOn); return; }
        sync.tab = s.side;
        scriptError.value = t('dataCompare:failedOn', { side: sideName(s.side), error: failure });
        task.log(scriptError.value, 'error');
        await stopped('error', stoppedOn, scriptError.value);
        return;
      }
      applied.push(s);
      base += sideTotal;
      report(base, true);
    }
    // A cancel that arrived after the last script ended changed nothing: it's applied.
    if (task.isCancelling) task.log(t('tasks:dataSync.lateCancel'));
    // In the background the store's notice says it; here, the usual message.
    const inBackground = !!tasks.byId(task.id)?.background;
    // Applied: these scripts can't run again, and the finished task keeps
    // only the panel's detail (reopening the dialog would offer Ejecutar).
    stale.value = 'applied';
    task.setReopen(undefined);
    task.finish(undefined, toRun.map((s) => summaryOf(s)).join(' · '));
    runTaskHandle = null;
    runTaskId.value = null;
    sync.open = false;
    if (!alive) return;
    if (!inBackground) ElMessage.success(t('dataCompare:applied'));
    await compare();
  } catch (e) {
    // Only compare() or the store can throw here; the run itself is handled above.
    stale.value ??= 'stopped';
    task.setReopen(undefined);
    if (task.isCancelling) task.cancelled(appliedText());
    else {
      scriptError.value = errorMessage(e);
      task.fail(e);
    }
  } finally {
    if (pending) clearTimeout(pending);
    applying.value = false;
    task.setReopen(undefined);
    if (runTaskHandle === task) runTaskHandle = null;
    if (runTaskId.value === task.id) runTaskId.value = null;
  }
}
// A new compare (the user's or the one after a run): scripts built from the
// old one no longer match the data, so the last build's "Ver detalle" can't
// reopen them with Ejecutar enabled. A run in progress keeps its own.
watch(() => result.value?.id, () => {
  builtTaskHandle?.setReopen(undefined);
  builtTaskHandle = null;
  if (!applying.value && !building.value) scripts.value = [];
});
</script>

<template>
  <div class="dc">
    <div class="dc-pick">
      <div v-for="(p, i) in [left, right]" :key="i" class="dc-side">
        <span class="dc-side-label">{{ i === 0 ? $t('dataCompare:left') : $t('dataCompare:right') }}</span>
        <el-select v-model="p.connectionId" filterable size="small" class="dc-conn">
          <el-option v-for="c in conns.list" :key="c.id" :label="c.name" :value="c.id" />
        </el-select>
        <el-select v-if="dbsOf(p).length" v-model="p.database" filterable size="small" class="dc-db">
          <el-option v-for="d in dbsOf(p)" :key="d" :label="d" :value="d" />
        </el-select>
        <el-select v-model="p.table" filterable size="small" class="dc-table" :placeholder="$t('dataCompare:table')">
          <el-option v-for="o in tablesOf(p)" :key="tableId(o)" :label="o.schema ? `${o.schema}.${o.name}` : o.name" :value="tableId(o)" />
        </el-select>
      </div>
      <div class="dc-actions">
        <span class="dc-side-label">{{ $t('dataCompare:keyLabel') }}</span>
        <el-select v-model="key" multiple filterable collapse-tags size="small" class="dc-key" :placeholder="$t('dataCompare:key')">
          <el-option v-for="c in (result?.columns.length ? result.columns : leftColumns)" :key="c" :label="c" :value="c" />
        </el-select>
        <el-button :type="result ? 'default' : 'primary'" size="small" :loading="running" @click="compare">{{ $t('dataCompare:compare') }}</el-button>
        <!-- Same as the schema compare: choose with the arrows, then sync. -->
        <el-button v-if="result" size="small" :type="totalChosen ? 'primary' : 'default'" :disabled="!totalChosen" @click="openSync">
          {{ $t('compare:sync.button') }}<span v-if="totalChosen" class="dc-count">{{ fmtN(totalChosen) }}</span>
        </el-button>
      </div>
    </div>

    <div v-if="mismatch" class="dc-mismatch" role="alert"><el-icon><ei-warning-filled /></el-icon>{{ mismatchText }}</div>
    <div v-if="error" class="dc-error" role="alert">{{ error }}</div>

    <template v-if="result">
      <div class="dc-summary">
        <span class="dc-same"><b>{{ fmtN(result.counts.same) }}</b> {{ $t('dataCompare:same') }}</span>
        <button :class="{ on: view === 'changed' }" @click="view = 'changed'"><b>{{ fmtN(result.counts.changed) }}</b> {{ $t('dataCompare:changed') }}</button>
        <button :class="{ on: view === 'only_left' }" @click="view = 'only_left'"><b>{{ fmtN(result.counts.only_left) }}</b> {{ $t('dataCompare:onlyLeft') }}</button>
        <button :class="{ on: view === 'only_right' }" @click="view = 'only_right'"><b>{{ fmtN(result.counts.only_right) }}</b> {{ $t('dataCompare:onlyRight') }}</button>
        <span style="flex: 1" />
        <el-button size="small" :disabled="!result.counts.changed && !result.counts.only_left" :title="$t('dataCompare:matchHint')" @click="matchSide('right')">{{ $t('dataCompare:syncRight') }}</el-button>
        <el-button size="small" :disabled="!result.counts.changed && !result.counts.only_right" :title="$t('dataCompare:matchHint')" @click="matchSide('left')">{{ $t('dataCompare:syncLeft') }}</el-button>
      </div>
      <div v-if="view === 'changed' ? result.counts.changed : view === 'only_left' ? result.counts.only_left : result.counts.only_right" class="dc-bulk">
        <span>{{ $t('dataCompare:bulk', { count: chosen(view), n: fmtN(chosen(view)) }) }}</span>
        <button :class="{ on: choices[view].all === 'left' && !choices[view].rows.size }" :title="arrowTip(view, 'left')" @click="setAll(view, 'left')">{{ $t('dataCompare:allLeft') }}</button>
        <button :class="{ on: choices[view].all === 'right' && !choices[view].rows.size }" :title="arrowTip(view, 'right')" @click="setAll(view, 'right')">{{ $t('dataCompare:allRight') }}</button>
        <button :class="{ on: choices[view].all === 'none' && !choices[view].rows.size }" @click="setAll(view, 'none')">{{ $t('dataCompare:allNone') }}</button>
      </div>
      <div v-if="result.truncated_left || result.truncated_right || result.duplicate_keys || result.only_left_columns.length || result.only_right_columns.length" class="dc-notes">
        <div v-if="result.truncated_left || result.truncated_right">{{ $t('dataCompare:truncated', { limit: fmtN(limit) }) }}</div>
        <div v-if="result.duplicate_keys">{{ $t('dataCompare:duplicates', { count: result.duplicate_keys }) }}</div>
        <div v-if="result.only_left_columns.length">{{ $t('dataCompare:onlyLeftColumns', { list: result.only_left_columns.join(', ') }) }}</div>
        <div v-if="result.only_right_columns.length">{{ $t('dataCompare:onlyRightColumns', { list: result.only_right_columns.join(', ') }) }}</div>
      </div>
      <div class="dc-grid-wrap">
        <table class="dc-grid">
          <thead><tr><th class="dc-arrows-h" /><th v-for="(c, i) in result.columns" :key="c" :class="{ key: i < result.key.length }">{{ c }}</th></tr></thead>
          <tbody v-if="view === 'changed'">
            <tr v-for="(r, i) in result.changed" :key="i" :class="`go-${dirOf('changed', i)}`">
              <td class="dc-arrows">
                <button :class="{ on: dirOf('changed', i) === 'left' }" :title="arrowTip('changed', 'left')" @click="toggle('changed', i, 'left')">←</button>
                <button :class="{ on: dirOf('changed', i) === 'right' }" :title="arrowTip('changed', 'right')" @click="toggle('changed', i, 'right')">→</button>
              </td>
              <td v-for="(c, j) in result.columns" :key="c" :class="{ diff: r.diff.includes(j), key: j < result.key.length }">
                <template v-if="r.diff.includes(j)">
                  <div class="dc-l" :title="$t('dataCompare:left')">{{ show(r.left[j]) }}</div>
                  <div class="dc-r" :title="$t('dataCompare:right')">{{ show(r.right[j]) }}</div>
                </template>
                <template v-else>{{ show(r.left[j]) }}</template>
              </td>
            </tr>
          </tbody>
          <tbody v-else>
            <tr v-for="(r, i) in rows" :key="i" :class="`go-${dirOf(view, i)}`">
              <td class="dc-arrows">
                <button :class="{ on: dirOf(view, i) === 'left' }" :title="arrowTip(view, 'left')" @click="toggle(view, i, 'left')">←</button>
                <button :class="{ on: dirOf(view, i) === 'right' }" :title="arrowTip(view, 'right')" @click="toggle(view, i, 'right')">→</button>
              </td>
              <td v-for="(c, j) in result.columns" :key="c" :class="{ key: j < result.key.length }">{{ show(r[j]) }}</td>
            </tr>
          </tbody>
        </table>
        <div v-if="view === 'changed' ? !result.changed.length : !rows.length" class="dc-empty">{{ $t('dataCompare:nothing') }}</div>
        <div class="dc-more">
          {{ view === 'changed' ? shownOf(result.counts.changed, result.changed) : view === 'only_left' ? shownOf(result.counts.only_left, result.only_left) : shownOf(result.counts.only_right, result.only_right) }}
        </div>
      </div>
    </template>
    <div v-else-if="!running && !error" class="dc-hint">{{ $t('dataCompare:hint') }}</div>

    <el-dialog v-model="sync.open" :title="$t('dataCompare:syncTitle')" width="760px" append-to-body>
      <div v-if="building" v-loading="true" class="dc-building" />
      <el-tabs v-if="scripts.length" v-model="sync.tab">
        <el-tab-pane v-for="sc in scripts" :key="sc.side" :name="sc.side" :label="`${sc.side === 'left' ? $t('dataCompare:left') : $t('dataCompare:right')} · ${sideName(sc.side)}`">
          <p class="dc-hint2">{{ $t('dataCompare:scriptSummary', { parts: summaryOf(sc) }) }}</p>
          <pre class="dc-script nm-selectable">{{ sc.script || $t('dataCompare:noChanges') }}</pre>
        </el-tab-pane>
      </el-tabs>
      <p v-if="scripts.length > 1" class="dc-hint2">{{ $t('dataCompare:bothSides') }}</p>
      <div v-if="mismatch" class="dc-mismatch in-dialog" role="alert"><el-icon><ei-warning-filled /></el-icon>{{ mismatchText }}</div>
      <div v-if="scriptError" class="dc-error" role="alert">{{ scriptError }}</div>
      <div v-if="stale && !applying" class="dc-mismatch in-dialog" role="alert">
        <el-icon><ei-warning-filled /></el-icon>{{ stale === 'applied' ? $t('tasks:dataCompare.applied') : $t('tasks:dataSync.stale') }}
        <el-button link type="primary" :disabled="!result || running || building" @click="openSync">{{ $t('tasks:dataSync.regenerate') }}</el-button>
      </div>
      <template #footer>
        <div class="dc-foot">
          <el-button :disabled="!currentScript?.script" @click="copyScript">{{ $t('common:copy') }}</el-button>
          <el-button :disabled="!currentScript?.script" @click="openInQuery">{{ $t('dataCompare:openInQuery') }}</el-button>
          <span style="flex: 1" />
          <template v-if="building">
            <el-button @click="sync.open = false">{{ $t('common:close') }}</el-button>
            <el-button @click="toBackground">{{ $t('tasks:panel.background') }}</el-button>
          </template>
          <template v-else-if="applying">
            <el-button :disabled="cancelling" @click="cancelRun">{{ cancelling ? $t('tasks:panel.cancelling') : $t('tasks:panel.cancel') }}</el-button>
            <el-button @click="toBackground">{{ $t('tasks:panel.background') }}</el-button>
          </template>
          <el-button v-else @click="sync.open = false">{{ $t('common:cancel') }}</el-button>
          <el-button type="primary" :loading="applying" :disabled="!!stale || building || !scripts.some((x) => x.script.trim())" @click="apply">{{ $t('dataCompare:run') }}</el-button>
        </div>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.dc { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--ide-editor); }
.dc-pick { display: flex; flex-wrap: wrap; gap: 8px 18px; align-items: center; padding: 10px 14px; border-bottom: 1px solid var(--nm-border); }
/* The key and the button always on their own line, at the left. */
.dc-side { display: flex; align-items: center; gap: 6px; }
.dc-side-label { font-size: 11px; font-weight: 600; letter-spacing: .05em; text-transform: uppercase; color: var(--nm-text-dim); }
.dc-conn { width: 180px; }
.dc-db { width: 150px; }
.dc-table { width: 220px; }
.dc-actions { display: flex; gap: 6px; align-items: center; flex-basis: 100%; }
.dc-key { width: 320px; }
.dc-count { margin-left: 6px; padding: 0 6px; border-radius: 8px; font-size: 11px; line-height: 16px; background: rgba(255, 255, 255, 0.22); }
.dc-bulk { display: flex; align-items: center; gap: 6px; padding: 6px 14px; border-bottom: 1px solid var(--nm-border); font-size: 12px; color: var(--nm-text-dim); }
.dc-bulk button { border: 1px solid var(--nm-border); border-radius: 3px; background: none; color: var(--nm-text); padding: 2px 8px; font: inherit; cursor: pointer; }
.dc-bulk button.on { background: var(--ide-selection); border-color: var(--ide-focus, var(--el-color-primary)); }
.dc-grid th.dc-arrows-h { width: 52px; }
.dc-arrows { white-space: nowrap; padding: 0 4px !important; }
.dc-arrows button { width: 20px; height: 18px; padding: 0; border: 1px solid transparent; border-radius: 3px; background: none; color: var(--nm-text-dim); font: inherit; cursor: pointer; }
.dc-arrows button:hover { border-color: var(--nm-border); color: var(--nm-text-strong); }
.dc-arrows button.on { background: var(--el-color-primary); color: #fff; }
.dc-grid tr.go-left td:not(.dc-arrows), .dc-grid tr.go-right td:not(.dc-arrows) { background: color-mix(in srgb, var(--el-color-primary) 8%, transparent); }
.dc-building { height: 80px; }
.dc-mismatch { display: flex; align-items: center; gap: 8px; margin: 10px 14px 0; padding: 8px 10px; border-radius: 3px; border: 1px solid color-mix(in srgb, var(--nm-warning) 55%, transparent); background: color-mix(in srgb, var(--nm-warning) 12%, transparent); color: var(--nm-text-strong); font-size: 12.5px; }
.dc-mismatch .el-icon { color: var(--nm-warning); flex: none; }
.dc-mismatch.in-dialog { margin: 8px 0 0; }
.dc-error { margin: 10px 14px; padding: 8px 10px; border-radius: 3px; border: 1px solid color-mix(in srgb, var(--nm-danger) 50%, transparent); background: color-mix(in srgb, var(--nm-danger) 10%, transparent); color: var(--nm-text); white-space: pre-wrap; }
.dc-summary { display: flex; align-items: center; gap: 6px; padding: 8px 14px; border-bottom: 1px solid var(--nm-border); font-size: 12.5px; }
.dc-summary button { border: 1px solid var(--nm-border); border-radius: 12px; background: none; color: var(--nm-text); padding: 3px 10px; font: inherit; cursor: pointer; }
.dc-summary button.on { background: var(--ide-selection); border-color: var(--ide-focus, var(--el-color-primary)); color: var(--nm-text-strong); }
.dc-same { color: var(--nm-text-dim); margin-right: 6px; }
.dc-notes { padding: 6px 14px; font-size: 12px; color: var(--nm-warning); border-bottom: 1px solid var(--nm-border); }
.dc-grid-wrap { flex: 1; min-height: 0; overflow: auto; }
.dc-grid { border-collapse: collapse; font-size: 12px; font-family: var(--nm-mono); }
.dc-grid th { position: sticky; top: 0; background: var(--nm-bg-elev); color: var(--nm-text-dim); font-weight: 500; text-align: left; padding: 4px 10px; white-space: nowrap; border-bottom: 1px solid var(--nm-border); }
.dc-grid th.key { color: var(--nm-text-strong); }
.dc-grid td { padding: 3px 10px; border-bottom: 1px solid var(--nm-border-soft); white-space: nowrap; max-width: 320px; overflow: hidden; text-overflow: ellipsis; color: var(--nm-text); }
.dc-grid td.key { color: var(--nm-text-strong); }
.dc-grid td.diff { background: color-mix(in srgb, var(--nm-warning) 10%, transparent); }
.dc-l { color: var(--nm-danger); }
.dc-r { color: var(--nm-success); }
.dc-empty, .dc-hint { padding: 30px 14px; color: var(--nm-text-dim); font-size: 12.5px; }
.dc-more { padding: 6px 14px; color: var(--nm-text-dim); font-size: 11.5px; }
.dc-opts { display: flex; gap: 16px; margin-bottom: 8px; }
.dc-hint2 { margin: 0 0 6px; color: var(--nm-text-dim); font-size: 12.5px; }
.dc-script { margin: 0; padding: 10px 12px; max-height: 45vh; overflow: auto; border: 1px solid var(--nm-border); border-radius: 4px; background: var(--ide-editor, var(--nm-bg-elev)); font-family: var(--nm-mono); font-size: 12px; color: var(--nm-text-strong); white-space: pre-wrap; }
.dc-foot { display: flex; align-items: center; gap: 8px; }
.dc-foot .el-button { margin: 0; }
</style>
