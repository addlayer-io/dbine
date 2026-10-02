import { onScopeDispose } from 'vue';
import { ElMessageBox } from 'element-plus';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { t } from '../i18n';
import { useTasksStore } from '../stores/tasks';

// Every way out of the app goes through `requestQuit`:
// - the menu's Salir (⌘Q, `app.quit` in appMenu.ts, a custom item),
// - the main window's close button (close-requested, always held back here),
// - the OS asking to quit (Dock › Salir, app switcher, logout, the native
//   menu's quit): the backend holds it back and emits `quit-requested`.
// With tasks running it asks first; on "Cancelar y cerrar" it cancels them
// through their real cancel paths, waits a little for them to settle and
// quits anyway. `quit_app` saves the window state and exits the process.

/** How long to wait for cancelled tasks to settle before quitting anyway. */
const SETTLE_MS = 3000;
const POLL_MS = 100;

/** A quit is in progress (the box is open, or tasks are being cancelled). */
let quitting = false;

function appWindow() {
  try { return getCurrentWindow(); } catch { return null; }
}

/** True when the app may close now (nothing running, or the user chose to
 *  cancel them all). */
async function confirmQuit(): Promise<boolean> {
  const tasks = useTasksStore();
  const running = tasks.running;
  const count = running.length;
  if (!count) return true;
  const list = running.map((x) => x.title).join(', ');
  try {
    await ElMessageBox.confirm(t('tasks:quit.message', { count, list }), t('tasks:quit.title'), {
      type: 'warning',
      confirmButtonText: t('tasks:quit.confirm'),
      cancelButtonText: t('tasks:quit.stay'),
      confirmButtonClass: 'el-button--danger',
      distinguishCancelAndClose: true,
    });
  } catch {
    return false;
  }
  return true;
}

/** Cancel everything running and wait (up to `SETTLE_MS`) for it to end. */
async function cancelAndSettle() {
  const tasks = useTasksStore();
  const deadline = Date.now() + SETTLE_MS;
  void tasks.cancelAll(SETTLE_MS);
  while (tasks.runningCount && Date.now() < deadline) {
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

/** The single way to quit (menu, ⌘Q, close button, the OS's quit request). */
export async function requestQuit() {
  if (quitting) return;
  quitting = true;
  try {
    if (!(await confirmQuit())) return;
    if (useTasksStore().runningCount) await cancelAndSettle();
    await exitApp();
  } catch {
    /* nothing more to try: the app stays open */
  } finally {
    quitting = false;
  }
}

/** Route the window's close button and the OS's quit request through
 *  `requestQuit`. Call once from an always-mounted component. */
export function useQuitGuard() {
  if (!('__TAURI_INTERNALS__' in window)) return;
  const w = appWindow();
  if (!w) return;
  const offs: (() => void)[] = [];
  let disposed = false;
  const keep = (f: () => void) => { if (disposed) f(); else offs.push(f); };
  w.onCloseRequested((e) => {
    e.preventDefault();
    void requestQuit();
  }).then(keep).catch(() => {});
  listen('quit-requested', () => { void requestQuit(); }).then(keep).catch(() => {});
  onScopeDispose(() => { disposed = true; offs.splice(0).forEach((f) => f()); });
}
