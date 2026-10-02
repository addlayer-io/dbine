import { acceptHMRUpdate, defineStore } from 'pinia';
import { ElNotification } from 'element-plus';
import { listen, type EventCallback, type UnlistenFn } from '@tauri-apps/api/event';
import { getCurrentWindow, UserAttentionType } from '@tauri-apps/api/window';
import { t } from '../i18n';
import { errorMessage } from '../api/client';

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

/** "42 s", "58 min", "1 h 03 min". */
export function formatElapsed(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return t('tasks:elapsed.seconds', { n: s });
  const m = Math.floor(s / 60);
  if (m < 60) return t('tasks:elapsed.minutes', { n: m, s: String(s % 60).padStart(2, '0') });
  return t('tasks:elapsed.hours', { n: Math.floor(m / 60), m: String(m % 60).padStart(2, '0') });
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
  }),
  getters: {
    running: (s) => s.tasks.filter((x) => x.state === 'running'),
    runningCount: (s) => s.tasks.filter((x) => x.state === 'running').length,
    byId: (s) => (id: string) => s.tasks.find((x) => x.id === id),
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
      const task = () => this.byId(id);
      const end = (state: Exclude<TaskState, 'running'>, fields: Partial<Task>) => {
        const x = task();
        if (!x || x.state !== 'running') return;
        Object.assign(x, fields, { state, endedAt: Date.now(), cancelling: false, canCancel: false });
        const h = hooks.get(id);
        h?.unlisten.splice(0).forEach((f) => f());
        this.notifyEnd(x);
        this.pruneFinished();
      };
      const store = this;
      return {
        id,
        progress(p) { const x = task(); if (x) x.progress = { ...x.progress, ...p }; },
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
    async cancelAll(timeoutMs = 3000) {
      const all = this.running.map((x) => this.cancel(x.id));
      await Promise.race([Promise.allSettled(all), new Promise((r) => setTimeout(r, timeoutMs))]);
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
      for (const id of drop) hooks.delete(id);
      this.tasks = this.tasks.filter((x) => !drop.has(x.id));
      if (this.detailId && drop.has(this.detailId)) this.detailId = null;
    },

    clearFinished() {
      for (const x of this.tasks) if (x.state !== 'running') hooks.delete(x.id);
      this.tasks = this.tasks.filter((x) => x.state === 'running');
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
