import { onScopeDispose } from 'vue';
import { ElMessageBox } from 'element-plus';
import { invoke } from '@tauri-apps/api/core';
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';
import { t } from '../i18n';
import { api, windowApi } from '../api/client';
import { SETTLE_MS, useTasksStore } from '../stores/tasks';
import { useTabsStore } from '../stores/tabs';
import { initWindowRole, refreshWindowRole } from './windowRole';

// Every way out goes through here. Quitting the app (`requestQuit`):
// - the menu's Salir (⌘Q, `app.quit` in appMenu.ts, a custom item),
// - the close button of the last window,
// - the OS asking to quit (Dock › Salir, app switcher, logout, the native
//   menu's quit): with tasks running anywhere, the backend holds it back and
//   emits `quit-requested` to one window only (with none, it just quits).
// It asks once about the running tasks of every window; on "Cancelar y
// cerrar" it asks every window to cancel theirs, waits a little for them to
// settle and quits anyway. `quit_app` saves the window state and exits.
//
// Restarting to finish an update (`requestUpdateRestart`, from the update
// dialog) is a quit too: it asks once about every window's tasks, cancels
// them and then lets the backend install the update and relaunch.
//
// Closing one window while others stay (`requestCloseWindow`) asks only
// about this window's tasks, cancels them, ends its tabs' sessions and
// closes just this window.
//
// One prompt at a time across windows: both flows take the backend's quit
// lock (`quit_begin`) first. While another window's prompt is open, a new
// request only brings that window forward. The lock is released by
// `quit_end`, or by the backend when the window closes.

const POLL_MS = 100;

/** A quit or a close is in progress in this window (the box is open, or
 *  tasks are being cancelled). */
let quitting = false;

const inTauri = () => '__TAURI_INTERNALS__' in window;

function appWindow() {
  try { return getCurrentWebviewWindow(); } catch { return null; }
}

/** "main" is window 1, "win-3" is window 3. */
function windowNumber(label: string): string {
  const m = /^win-(\d+)$/.exec(label);
  return m ? m[1] : '1';
}

interface Running { label: string; title: string }

/** The running tasks of every window. This window's come from its own store
 *  (the backend's copy lags by the report's debounce); the rest from the
 *  backend. Without the backend, this window's only. */
async function runningEverywhere(): Promise<Running[]> {
  const { label } = await initWindowRole();
  const local = useTasksStore().running.map((x) => ({ label, title: x.title }));
  if (!inTauri()) return local;
  try {
    const all = await windowApi.runningTasksAll();
    if (!Array.isArray(all)) return local;
    return [...local, ...all.filter((x) => x.label !== label).map((x) => ({ label: x.label, title: x.title }))];
  } catch {
    return local;
  }
}

/** Ask before stopping running work; true to go ahead. */
async function confirmStop(prefix: 'quit' | 'closeWindow' | 'update', titles: string[]): Promise<boolean> {
  const count = titles.length;
  if (!count) return true;
  const list = titles.join(', ');
  try {
    await ElMessageBox.confirm(t(`tasks:${prefix}.message`, { count, list }), t(`tasks:${prefix}.title`), {
      type: 'warning',
      confirmButtonText: t(`tasks:${prefix}.confirm`),
      cancelButtonText: t(`tasks:${prefix}.stay`),
      confirmButtonClass: 'el-button--danger',
      distinguishCancelAndClose: true,
    });
  } catch {
    return false;
  }
  return true;
}

/** Cancel this window's running tasks and wait (up to `SETTLE_MS`) for them to end. */
async function cancelAndSettle() {
  const tasks = useTasksStore();
  const deadline = Date.now() + SETTLE_MS;
  void tasks.cancelAll(SETTLE_MS);
  while (tasks.runningCount && Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, POLL_MS));
  }
}

/** Ask every window to cancel its tasks and wait (up to `SETTLE_MS`) until
 *  none is running anywhere. */
async function cancelEverywhereAndSettle() {
  try {
    await windowApi.cancelAllBroadcast();
  } catch {
    // An older backend: at least this window's.
    await cancelAndSettle();
    return;
  }
  const deadline = Date.now() + SETTLE_MS;
  while (Date.now() < deadline && (await runningEverywhere()).length) {
    await new Promise((r) => setTimeout(r, POLL_MS));
  }
}

async function exitApp() {
  try {
    await invoke('quit_app');
  } catch {
    // An older backend without `quit_app`: closing the only window ends the app.
    await appWindow()?.destroy();
  }
}

/** Takes the app-wide quit lock; false when another window's prompt is
 *  open (the backend brings it forward). Without the command (an older
 *  backend, the dev-preview), go ahead. */
async function quitBegin(): Promise<boolean> {
  if (!inTauri()) return true;
  try {
    return (await invoke<boolean | null>('quit_begin')) !== false;
  } catch {
    return true;
  }
}

function quitEnd() {
  if (inTauri()) invoke('quit_end').catch(() => {});
}

/** Run `flow` holding this window's flag and the app-wide quit lock. The
 *  flag is set before any await, so a double click starts one flow. */
async function exclusive(flow: () => Promise<void>) {
  if (quitting) return;
  quitting = true;
  try {
    if (!(await quitBegin())) return;
    try {
      await flow();
    } finally {
      quitEnd();
    }
  } catch {
    /* nothing more to try: the app or the window stays open */
  } finally {
    quitting = false;
  }
}

/** The running tasks' titles; with several windows, each says which one it's in. */
async function titlesEverywhere(running: Running[]): Promise<string[]> {
  const { windowCount } = await refreshWindowRole();
  return windowCount > 1 || new Set(running.map((x) => x.label)).size > 1
    ? running.map((x) => t('tasks:quit.window', { title: x.title, n: windowNumber(x.label) }))
    : running.map((x) => x.title);
}

/** Asks about every window's tasks and quits. */
async function quitFlow() {
  try {
    const running = await runningEverywhere();
    if (!(await confirmStop('quit', await titlesEverywhere(running)))) return;
    if (running.length) await cancelEverywhereAndSettle();
    await exitApp();
  } catch {
    /* nothing more to try: the app stays open */
  }
}

/** The single way to quit (menu, ⌘Q, the last window's close button, the OS's quit request). */
export function requestQuit(): Promise<void> {
  return exclusive(quitFlow);
}

/** Install the downloaded update and relaunch, after asking about every
 *  window's running tasks (and cancelling them). Resolves without doing
 *  anything when the user stays or another window's prompt is open; throws
 *  the backend's error when the install fails (the app stays open). */
export async function requestUpdateRestart(): Promise<void> {
  const failures: unknown[] = [];
  await exclusive(async () => {
    const running = await runningEverywhere();
    if (!(await confirmStop('update', await titlesEverywhere(running)))) return;
    if (running.length) await cancelEverywhereAndSettle();
    try {
      await api.updateInstallAndRestart();
    } catch (e) {
      failures.push(e);
    }
  });
  if (failures.length) throw failures[0];
}

/** This window's close button: closes only this window, or quits when it's
 *  the last one. The count is read holding the lock, so two windows closing
 *  at once can't both see the other one still open. */
export function requestCloseWindow(): Promise<void> {
  return exclusive(async () => {
    const { windowCount } = await refreshWindowRole();
    await (windowCount <= 1 ? quitFlow() : closeFlow());
  });
}

/** Asks about this window's tasks and closes just this window. */
async function closeFlow() {
  try {
    const tasks = useTasksStore();
    if (!(await confirmStop('closeWindow', tasks.running.map((x) => x.title)))) return;
    if (tasks.runningCount) await cancelAndSettle();
    // End the tabs' backend sessions without touching the saved tabs (the
    // window goes away; another one may be the primary by now).
    const tabs = useTabsStore();
    for (const tab of tabs.tabs) tabs.release(tab);
    try {
      await windowApi.close();
    } catch {
      await appWindow()?.destroy();
    }
  } catch {
    /* the window stays open */
  }
}

/** Route the window's close button and the OS's quit request, and share
 *  this window's tasks with the others. Call once from an always-mounted
 *  component. */
export function useQuitGuard() {
  if (!inTauri()) return;
  useTasksStore().bindWindows();
  const w = appWindow();
  if (!w) return;
  const offs: (() => void)[] = [];
  let disposed = false;
  const keep = (f: () => void) => { if (disposed) f(); else offs.push(f); };
  w.onCloseRequested((e) => {
    e.preventDefault();
    void requestCloseWindow();
  }).then(keep).catch(() => {});
  // The window's own listener: `quit-requested` is sent to one window only,
  // and a global `listen` also hears events sent to the others.
  w.listen('quit-requested', () => { void requestQuit(); }).then(keep).catch(() => {});
  onScopeDispose(() => { disposed = true; offs.splice(0).forEach((f) => f()); });
}
