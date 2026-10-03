import { reactive } from 'vue';
import { ElMessageBox } from 'element-plus';
import { t } from '../i18n';
import { tb } from '../i18n/backend';
import { errorMessage, multiDbApi } from '../api/client';
import type { Message, MultiDbProgress, MultiDbResponse, MultiDbStatus, QueryOutcome, StatementResult } from '../api/types';
import { readJson, writeJson } from '../stores/storage';
import { startTask } from '../stores/tasks';

// "Ejecutar en varias bases…": the editor's script on several databases of a
// connection (src-tauri/src/commands/multi_db.rs). The run is a task
// (stores/tasks.ts): it goes on in the background when its dialog closes,
// and the Tareas panel can cancel it (no new database starts; the running
// ones are interrupted). Its results land in the editor's results pane.

/** Databases running at once (the backend's `MAX_PARALLEL`). */
export const MAX_PARALLEL = 4;

/** A name filter: `*` matches anything; without `*` it's a substring. Case aside. */
export function wildcardMatcher(filter: string): (name: string) => boolean {
  const f = filter.trim().toLowerCase();
  if (!f) return () => true;
  if (!f.includes('*')) return (n) => n.toLowerCase().includes(f);
  const re = new RegExp(`^${f.split('*').map((p) => p.replace(/[.+?^${}()|[\]\\]/g, '\\$&')).join('.*')}$`);
  return (n) => re.test(n.toLowerCase());
}

const selectionKey = (connectionId: string) => `dbine.multiDb.${connectionId}`;
/** The databases picked last time on this connection (null: never). */
export function recallSelection(connectionId: string): string[] | null {
  const v = readJson<unknown>(selectionKey(connectionId), null);
  return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : null;
}
export function rememberSelection(connectionId: string, databases: string[]) {
  writeJson(selectionKey(connectionId), databases);
}

/** A database's state during a run. */
export type LiveStatus = MultiDbStatus | 'pending';
export interface MultiDbLive {
  runId: string;
  taskId: string;
  databases: string[];
  states: Record<string, { status: LiveStatus; rows: number; elapsed_ms: number; error: string | null }>;
  done: number;
  running: boolean;
}

/** The editor's view of a finished run: a QueryOutcome for the results pane
 *  (the merged grid, or each database's result sets), their sub-tab labels,
 *  and the per-database summary in "Mensajes". */
export function multiDbOutcome(r: MultiDbResponse): { outcome: QueryOutcome; labels: string[] } {
  const results: StatementResult[] = [];
  const labels: string[] = [];
  if (r.merged) {
    results.push(r.merged);
    labels.push(t('multiDb:results.merged', { count: r.databases.filter((d) => d.status === 'ok').length }));
  }
  for (const d of r.databases) {
    const sets = d.results.filter((x) => x.columns.length);
    sets.forEach((s, i) => {
      results.push(s);
      labels.push(sets.length > 1 || r.merged ? `${d.database} (${i + (r.merged ? 2 : 1)})` : d.database);
    });
  }
  const log: Message[] = r.databases.map((d) => ({ level: d.status === 'ok' ? 'info' : d.status === 'error' ? 'error' : 'warning', text: summaryLine(d), statement: null, code: null, line: null }));
  return { outcome: { results, messages: [], error: null, elapsed_ms: r.elapsed_ms, plans: [], log, errors: [] }, labels };
}

function summaryLine(d: MultiDbResponse['databases'][number]): string {
  const ms = t('results:messages.elapsed', { ms: d.elapsed_ms.toLocaleString() });
  switch (d.status) {
    case 'ok': {
      const what = d.rows || !d.rows_affected
        ? t('results:messages.rows', { count: d.rows, rows: d.rows.toLocaleString() })
        : t('results:messages.affected', { count: d.rows_affected, rows: d.rows_affected.toLocaleString() });
      const extra = d.messages.length ? ` · ${d.messages.map((m) => tb(m)).join(' · ')}` : '';
      return `${d.database} · ${t('multiDb:status.ok')} · ${what} · ${ms}${extra}`;
    }
    case 'error': return `${d.database} · ${t('multiDb:status.error')}: ${tb(d.error ?? '')} · ${ms}`;
    default: return `${d.database} · ${t(`multiDb:status.${d.status}`)}`;
  }
}

/** "3 de 4 bases bien, 1 con error". */
export function runSummary(r: MultiDbResponse): string {
  const count = (s: MultiDbStatus) => r.databases.filter((d) => d.status === s).length;
  const parts = [t('multiDb:summary.ok', { ok: count('ok'), total: r.databases.length })];
  if (count('error')) parts.push(t('multiDb:summary.error', { count: count('error') }));
  if (count('cancelled') + count('skipped')) parts.push(t('multiDb:summary.cancelled', { count: count('cancelled') + count('skipped') }));
  return parts.join(', ');
}

/** Whether the script may run: read-only, or the user confirmed it after
 *  being told how many databases it can change. Nothing runs here. */
export async function confirmMultiDb(a: { connectionId: string; databases: string[]; sql: string }): Promise<boolean> {
  const check = await multiDbApi.run({ runId: '', connectionId: a.connectionId, databases: a.databases, sql: a.sql, checkOnly: true });
  if (check.needs_confirmation == null) return true;
  const count = a.databases.length;
  const text = check.needs_confirmation
    ? t('multiDb:confirm.write', { keyword: check.needs_confirmation, count })
    : t('multiDb:confirm.unknown', { count });
  try {
    await ElMessageBox.confirm(text, t('multiDb:confirm.title'), {
      type: 'warning', confirmButtonText: t('multiDb:confirm.run', { count }), cancelButtonText: t('common:cancel'),
      confirmButtonClass: 'el-button--danger', distinguishCancelAndClose: true, closeOnClickModal: false,
    });
    return true;
  } catch {
    return false;
  }
}

/** Start the run as a task. `live` follows it (the dialog shows it);
 *  `onDone` gets the response (also when the dialog is gone). */
export function startMultiDbRun(a: {
  connectionId: string; connectionName: string; databases: string[]; sql: string;
  maxRows: number; continueOnError: boolean | null;
  reopen?: () => void;
  onDone: (r: MultiDbResponse | null, error: string | null) => void;
}): MultiDbLive {
  const runId = crypto.randomUUID();
  let settled = false;
  const task = startTask<MultiDbResponse>({
    kind: 'multi-db',
    title: t('multiDb:task.title', { count: a.databases.length, connection: a.connectionName }),
    connectionId: a.connectionId,
    // The command registers the run once it gets there: the cancel is
    // re-sent until the run ends.
    cancel: () => {
      const send = () => multiDbApi.cancel(runId).catch(() => {});
      const timer = setInterval(() => { if (settled) clearInterval(timer); else send(); }, 500);
      return send();
    },
    reopen: a.reopen,
  });
  const live = reactive<MultiDbLive>({
    runId, taskId: task.id, databases: [...a.databases], done: 0, running: true,
    states: Object.fromEntries(a.databases.map((d) => [d, { status: 'pending' as LiveStatus, rows: 0, elapsed_ms: 0, error: null }])),
  });
  task.log(a.sql);
  task.progress({ done: 0, total: a.databases.length, unit: t('multiDb:task.unit') });
  (async () => {
    try {
      await task.listen<MultiDbProgress>('multi-db-progress', ({ payload: p }) => {
        if (p.run_id !== runId || settled) return;
        live.states[p.database] = { status: p.status, rows: p.rows, elapsed_ms: p.elapsed_ms, error: p.error };
        live.done = p.done;
        task.progress({ done: p.done, phase: p.database });
        if (p.status === 'error') task.log(`${p.database}: ${tb(p.error ?? '')}`, 'error');
      });
    } catch { /* no live progress: the run still goes */ }
    try {
      const r = await multiDbApi.run({
        runId, connectionId: a.connectionId, databases: a.databases, sql: a.sql,
        maxRows: a.maxRows, continueOnError: a.continueOnError, confirmedWrite: true,
      });
      settled = true;
      for (const d of r.databases) live.states[d.database] = { status: d.status, rows: d.rows, elapsed_ms: d.elapsed_ms, error: d.error };
      live.done = r.databases.length;
      live.running = false;
      const summary = runSummary(r);
      if (r.cancelled) task.cancelled(summary);
      else task.finish(r, summary, r.databases.some((d) => d.status !== 'ok') ? 'error' : 'done');
      a.onDone(r, null);
    } catch (e) {
      settled = true;
      live.running = false;
      task.fail(e);
      a.onDone(null, errorMessage(e));
    }
  })();
  return live;
}
