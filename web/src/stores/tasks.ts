import { effectScope, watch } from 'vue';
import { acceptHMRUpdate, defineStore } from 'pinia';
import { ElNotification } from 'element-plus';
import { listen, type EventCallback, type UnlistenFn } from '@tauri-apps/api/event';
import { getCurrentWindow, UserAttentionType } from '@tauri-apps/api/window';
import { t } from '../i18n';
import { errorMessage, windowApi } from '../api/client';
import { elapsedParts, estimate, etaKey, recordSample, roundEta, type EtaSeries } from '../composables/taskEta';

// Long operations (a schema sync, a script run, an import…) that keep going
// after their dialog closes. A dialog registers its run here when it starts
// (`startTask` / `runTask`); "Seguir en segundo plano" calls `background()`
// and closes the dialog. The task lives in this store, not in the component:
// listeners opened with the handle's `listen` stay alive until the task ends,
// so closing the dialog or its tab doesn't stop it. The status bar shows the
// running count and the Tareas panel (TasksPanel.vue) lists them.

export type TaskState = 'running' | 'done' | 'error' | 'cancelled';

export interface TaskProgress {
  /** Units done so far (statements, rows, bytes, tables…). */
  done?: number;
  /** Total units, when known. */
  total?: number;
  /** What `done`/`total` count: a `tasks:unit.*` key (statements, rows, bytes, tables, objects) or any text. */
  unit?: string;
  /** Current phase or step ("Creando índices", the statement running…). */
  phase?: string;
}

export interface TaskMessage {
  at: number;
  level: 'info' | 'warn' | 'error';
  text: string;
}

export interface Task {
  id: string;
  /** What kind of operation ('sync', 'script-run', 'import', 'export', 'clone-table', 'migration', 'restore'…). */
  kind: string;
  /** What it does and where ("Sync de bind", "Importar clientes.csv"). */
  title: string;
  connectionId?: string;
  database?: string;
  startedAt: number;
  endedAt?: number;
  state: TaskState;
  progress: TaskProgress;
  messages: TaskMessage[];
  result?: unknown;
  error?: string;
  /** Short outcome for the notice and the panel ("12 sentencias"). */
  summary?: string;
  /** Sent to the background: its dialog closed, so it notifies when it ends. */
  background: boolean;
  /** Cancel was asked and hasn't landed yet. */
  cancelling: boolean;
  /** Whether the panel can offer Cancelar / Ver detalle (the dialog's own one). */
  canCancel: boolean;
  canReopen: boolean;
}

export interface TaskOptions {
  kind: string;
  title: string;
  connectionId?: string;
  database?: string;
  /** The operation's existing cancel path (cancel_query, migration_cancel…). */
  cancel?: () => unknown;
  /** Reopen the dialog or a detail view of this task. Without it, "Ver
   *  detalle" shows the panel's own detail (messages, result or error). */
  reopen?: () => void;
  /** Start in the background (no dialog was ever shown). */
  background?: boolean;
}

export interface TaskHandle<R = unknown> {
  readonly id: string;
  /** Merge progress (`{ done: 4, total: 12, unit: 'statements' }`). */
  progress(p: TaskProgress): void;
  /** Add a line to the task's log. */
  log(text: string, level?: TaskMessage['level']): void;
  /** Listen to a backend event for as long as the task runs (survives the
   *  dialog and its tab). Resolves once the listener is in place. */
  listen<T>(event: string, handler: EventCallback<T>): Promise<void>;
  /** Hand it to the background (the dialog closes; a notice comes at the end). */
  background(): void;
  /** Replace the reopen hook (e.g. once the dialog that started it unmounted). */
  setReopen(fn: (() => void) | undefined): void;
  /** It ended well (or with errors, with `state: 'error'`). */
  finish(result?: R, summary?: string, state?: Exclude<TaskState, 'running'>): void;
  fail(error: unknown): void;
  cancelled(summary?: string): void;
  /** True once the user asked to cancel it. */
  readonly isCancelling: boolean;
}

export interface RunTaskOptions<R> extends TaskOptions {
  run: (task: TaskHandle<R>) => Promise<R>;
  /** Short outcome for the notice ("12 sentencias"). */
  summary?: (result: R) => string | undefined;
  /** Classify a result (default 'done'). */
  outcome?: (result: R) => Exclude<TaskState, 'running'>;
}

const MAX_MESSAGES = 1000;
/** Finished tasks kept for the panel; older ones are dropped with their hooks. */
const MAX_FINISHED = 50;
let seq = 0;

/** Hooks and listeners stay out of the reactive state. */
const hooks = new Map<string, { cancel?: () => unknown; reopen?: () => void; unlisten: UnlistenFn[] }>();

/** Remaining-time samples per task: outside the reactive state (they change
 *  on every progress update and only `etaOf` reads them). */
const etaSeries = new Map<string, EtaSeries>();

/** The one 1 s clock behind every live elapsed time and ETA (status bar,
 *  panel, detail). It only runs while some task does. */
let ticker: ReturnType<typeof setInterval> | null = null;

const pad2 = (n: number) => String(n).padStart(2, '0');

/** How long a cancel-all waits for the tasks to settle (the quit guard
 *  waits the same before quitting anyway). */
export const SETTLE_MS = 3000;
/** The running list is sent to the backend at most this often. */
const REPORT_DEBOUNCE_MS = 100;
let windowsBound = false;

/** "45 s", "12 min 03 s", "3 h 07 min", "1 d 2 h". */
export function formatElapsed(ms: number): string {
  const p = elapsedParts(ms);
  switch (p.kind) {
    case 'seconds': return t('tasks:elapsed.seconds', { n: p.s });
    case 'minutes': return t('tasks:elapsed.minutes', { n: p.m, s: pad2(p.s) });
    case 'hours': return t('tasks:elapsed.hours', { n: p.h, m: pad2(p.m) });
    case 'days': return t('tasks:elapsed.days', { n: p.d, h: p.h });
  }
}

/** A running task's remaining time: none (no total, or not running),
 *  still gathering samples, or an estimate. `scope: 'phase'` when the
 *  counter restarted during the task (the estimate covers this step only). */
export type TaskEta =
  | { status: 'none' }
  | { status: 'calculating' }
  | { status: 'eta'; remainingMs: number; confidence: 'low' | 'ok'; scope: 'task' | 'phase' };

/** "menos de 1 min", "≈ 4 min", "≈ 1 h 20 min", "≈ 1 d 2 h". */
export function formatEtaValue(ms: number): string {
  const r = roundEta(ms);
  switch (r.kind) {
    case 'lessThanMinute': return t('tasks:eta.lessThanMinute');
    case 'minutes': return t('tasks:eta.minutes', { n: r.m });
    case 'hours': return r.m ? t('tasks:eta.hours', { h: r.h, m: pad2(r.m) }) : t('tasks:eta.hoursExact', { h: r.h });
    case 'days': return t('tasks:eta.days', { d: r.d, h: r.h });
  }
}

/** For a row or the status bar: "≈ 4 min restantes", "≈ 3 min para esta
 *  etapa", "calculando…", or '' when there's no estimate to give. */
export function formatEta(e: TaskEta): string {
  if (e.status === 'calculating') return t('tasks:eta.calculating');
  if (e.status !== 'eta') return '';
  const eta = formatEtaValue(e.remainingMs);
  return e.scope === 'phase' ? t('tasks:eta.phase', { eta }) : t('tasks:eta.remaining', { eta });
}

function inTauri() {
  return '__TAURI_INTERNALS__' in window;
}

/** Ask the OS for attention (Dock bounce / taskbar flash) when the app isn't focused. */
function requestAttention() {
  if (!inTauri() || document.hasFocus()) return;
  try {
    getCurrentWindow().requestUserAttention(UserAttentionType.Informational).catch(() => {});
  } catch { /* no window */ }
}

export const useTasksStore = defineStore('tasks', {
  state: () => ({
    tasks: [] as Task[],
    panelOpen: false,
    /** The task whose built-in detail is open (no reopen hook). */
    detailId: null as string | null,
    /** Ticks every second while a task runs (see `syncTicker`). */
    now: Date.now(),
  }),
  getters: {
    running: (s) => s.tasks.filter((x) => x.state === 'running'),
    runningCount: (s) => s.tasks.filter((x) => x.state === 'running').length,
    byId: (s) => (id: string) => s.tasks.find((x) => x.id === id),
    /** Live elapsed time of a task (to its end once it ended). */
    elapsedOf: (s) => (x: Task) => (x.endedAt ?? Math.max(s.now, x.startedAt)) - x.startedAt,
    /** Remaining-time estimate of a running task (live: reads `now`). */
    etaOf: (s) => (x: Task): TaskEta => {
      const { done, total } = x.progress;
      if (x.state !== 'running' || !total || total <= 0 || (done ?? 0) >= total) return { status: 'none' };
      const series = etaSeries.get(x.id);
      const e = estimate(series, total, Math.max(s.now, series?.samples[series.samples.length - 1].t ?? 0));
      if (!e) return { status: 'calculating' };
      return { status: 'eta', ...e, scope: series && series.resets > 0 ? 'phase' : 'task' };
    },
  },
  actions: {
    /** Register a long operation that's starting. Returns its handle. */
    start<R = unknown>(opts: TaskOptions): TaskHandle<R> {
      const id = `task-${Date.now()}-${++seq}`;
      this.tasks.unshift({
        id, kind: opts.kind, title: opts.title, connectionId: opts.connectionId, database: opts.database,
        startedAt: Date.now(), state: 'running', progress: {}, messages: [],
        background: !!opts.background, cancelling: false, canCancel: !!opts.cancel, canReopen: !!opts.reopen,
      });
      hooks.set(id, { cancel: opts.cancel, reopen: opts.reopen, unlisten: [] });
      this.syncTicker();
      const task = () => this.byId(id);
      const end = (state: Exclude<TaskState, 'running'>, fields: Partial<Task>) => {
        const x = task();
        if (!x || x.state !== 'running') return;
        Object.assign(x, fields, { state, endedAt: Date.now(), cancelling: false, canCancel: false });
        const h = hooks.get(id);
        h?.unlisten.splice(0).forEach((f) => f());
        etaSeries.delete(id);
        this.syncTicker();
        this.notifyEnd(x);
        this.pruneFinished();
      };
      const store = this;
      return {
        id,
        progress(p) {
          const x = task();
          if (!x) return;
          const next = { ...x.progress, ...p };
          x.progress = next;
          if (x.state !== 'running' || next.done == null) return;
          // Feed the ETA. A first report that already has work done counts
          // from the task's start; one at 0 is the start itself.
          const prev = etaSeries.get(id);
          const origin = !prev && next.done > 0 ? { t: x.startedAt, done: 0 } : undefined;
          etaSeries.set(id, recordSample(prev, etaKey(next.unit, next.total), next.done, Date.now(), origin));
        },
        log(text, level = 'info') {
          const x = task();
          if (!x) return;
          x.messages.push({ at: Date.now(), level, text });
          if (x.messages.length > MAX_MESSAGES) x.messages.splice(0, x.messages.length - MAX_MESSAGES);
        },
        async listen<T>(event: string, handler: EventCallback<T>) {
          if (!inTauri()) return;
          const off = await listen<T>(event, handler);
          const h = hooks.get(id);
          if (h && task()?.state === 'running') h.unlisten.push(off); else off();
        },
        background() { store.toBackground(id); },
        setReopen(fn) {
          const h = hooks.get(id);
          if (h) h.reopen = fn;
          const x = task();
          if (x) x.canReopen = !!fn;
        },
        finish(result, summary, state = 'done') { end(state, { result, summary }); },
        fail(error) { end('error', { error: errorMessage(error) }); },
        cancelled(summary) { end('cancelled', { summary }); },
        get isCancelling() { return !!task()?.cancelling; },
      };
    },

    /** Run `run` as a task: it ends when the promise settles. A rejection
     *  after Cancelar counts as cancelled. `outcome` classifies a result
     *  (e.g. a sync that ran with errors → 'error'; one stopped → 'cancelled'). */
    run<R>(opts: RunTaskOptions<R>): { task: TaskHandle<R>; promise: Promise<R> } {
      const task = this.start<R>(opts);
      const promise = (async () => {
        try {
          const r = await opts.run(task);
          task.finish(r, opts.summary?.(r), opts.outcome?.(r) ?? 'done');
          return r;
        } catch (e) {
          if (task.isCancelling) task.cancelled(); else task.fail(e);
          throw e;
        }
      })();
      // The caller may not await it once it's in the background.
      promise.catch(() => {});
      return { task, promise };
    },

    /** Run the shared clock only while something runs. */
    syncTicker() {
      this.now = Date.now();
      const on = this.tasks.some((x) => x.state === 'running');
      if (on && !ticker) ticker = setInterval(() => { this.now = Date.now(); }, 1000);
      if (!on && ticker) { clearInterval(ticker); ticker = null; }
    },

    toBackground(id: string) {
      const x = this.byId(id);
      if (x) x.background = true;
    },

    async cancel(id: string) {
      const x = this.byId(id);
      const h = hooks.get(id);
      // Already asked (dialog and panel can both ask): one cancel per request.
      if (!x || x.state !== 'running' || !h?.cancel || x.cancelling) return;
      x.cancelling = true;
      try {
        await h.cancel();
      } catch (e) {
        // The cancel didn't land: re-enable the button, and a later failure
        // is an error again, not a cancellation.
        x.messages.push({ at: Date.now(), level: 'error', text: errorMessage(e) });
        if (x.state === 'running') x.cancelling = false;
      }
    },

    /** Cancel every running task (the quit guard). Waits up to `timeoutMs`. */
    async cancelAll(timeoutMs = SETTLE_MS) {
      const all = this.running.map((x) => this.cancel(x.id));
      await Promise.race([Promise.allSettled(all), new Promise((r) => setTimeout(r, timeoutMs))]);
    },

    /** Share this window's running tasks with the backend (so quitting from
     *  any window asks about all of them) and cancel them when some window
     *  quits ("tasks-cancel-all"). Once per window; outlives the caller. */
    bindWindows() {
      if (windowsBound || !inTauri()) return;
      windowsBound = true;
      let timer: ReturnType<typeof setTimeout> | null = null;
      const report = () => {
        timer = null;
        const tasks = this.running.map((x) => ({ id: x.id, title: x.title }));
        windowApi.reportTasks(tasks).catch(() => {});
      };
      // Detached: the component that asked may unmount, the reporting stays.
      effectScope(true).run(() => {
        watch(
          () => this.running.map((x) => `${x.id}\u0000${x.title}`).join('\u0001'),
          () => {
            if (timer) clearTimeout(timer);
            timer = setTimeout(report, REPORT_DEBOUNCE_MS);
          },
          { immediate: true },
        );
      });
      listen('tasks-cancel-all', () => { void this.cancelAll(SETTLE_MS); }).catch(() => {});
    },

    /** "Ver detalle": the dialog's own view, or the panel's detail. */
    reopen(id: string) {
      // A notice can outlive its task (pruned or removed): nothing to show.
      if (!this.byId(id)) return;
      const h = hooks.get(id);
      if (h?.reopen) { this.panelOpen = false; h.reopen(); } else this.detailId = id;
    },

    remove(id: string) {
      const x = this.byId(id);
      if (!x || x.state === 'running') return;
      hooks.delete(id);
      etaSeries.delete(id);
      this.tasks = this.tasks.filter((y) => y.id !== id);
      if (this.detailId === id) this.detailId = null;
    },

    /** Keep only the newest `MAX_FINISHED` finished tasks (the list is newest first). */
    pruneFinished() {
      let kept = 0;
      const drop = new Set<string>();
      for (const x of this.tasks) {
        if (x.state === 'running') continue;
        if (++kept > MAX_FINISHED) drop.add(x.id);
      }
      if (!drop.size) return;
      for (const id of drop) { hooks.delete(id); etaSeries.delete(id); }
      this.tasks = this.tasks.filter((x) => !drop.has(x.id));
      if (this.detailId && drop.has(this.detailId)) this.detailId = null;
    },

    clearFinished() {
      for (const x of this.tasks) if (x.state !== 'running') { hooks.delete(x.id); etaSeries.delete(x.id); }
      this.tasks = this.tasks.filter((x) => x.state === 'running');
      if (this.detailId && !this.tasks.some((x) => x.id === this.detailId)) this.detailId = null;
    },

    /** The end notice: in-app for tasks in the background, plus the OS's
     *  attention request when the app isn't focused. */
    notifyEnd(x: Task) {
      requestAttention();
      if (!x.background) return;
      const elapsed = formatElapsed((x.endedAt ?? Date.now()) - x.startedAt);
      const type = x.state === 'done' ? 'success' : x.state === 'error' ? 'error' : 'warning';
      const message = x.state === 'error' && x.error
        ? t('tasks:notify.failed', { title: x.title, elapsed, error: x.error })
        : x.summary
          ? t('tasks:notify.withSummary', { title: x.title, elapsed, summary: x.summary })
          : t(`tasks:notify.${x.state}`, { title: x.title, elapsed });
      ElNotification({
        type,
        title: t(`tasks:notify.title.${x.state}`),
        message,
        duration: x.state === 'done' ? 8000 : 0,
        onClick: () => this.reopen(x.id),
      });
    },
  },
});

// Development: swap the store's code in place when it changes (no reload).
if (import.meta.hot) import.meta.hot.accept(acceptHMRUpdate(useTasksStore, import.meta.hot));

/** The API dialogs use (outside a component, too). */
export function startTask<R = unknown>(opts: TaskOptions): TaskHandle<R> {
  return useTasksStore().start<R>(opts);
}

export function runTask<R>(opts: RunTaskOptions<R>) {
  return useTasksStore().run<R>(opts);
}
