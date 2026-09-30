import { ElMessageBox } from 'element-plus';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { ask } from '@tauri-apps/plugin-dialog';
import { t } from './i18n';

// What makes the webview behave like a desktop app instead of a web page:
// no browser shortcuts or context menu, the OS's own confirm dialogs.

/** This app's window; null outside Tauri (the UI opened in a browser). */
function currentWindow() {
  try { return getCurrentWindow(); } catch { return null; }
}

/** The window starts hidden (tauri.conf.json) so it never shows a blank
 *  page: shown once the UI has rendered. Not from requestAnimationFrame: a
 *  hidden WKWebView doesn't paint, so the frame never comes. The Rust side
 *  shows it anyway after a few seconds if this never runs. */
export function showWindow() {
  setTimeout(() => {
    currentWindow()?.show().catch(() => {});
  }, 0);
}

const editable = (t: EventTarget | null) =>
  t instanceof HTMLElement && (t.isContentEditable || !!t.closest('input, textarea, [contenteditable], .cm-editor'));

/** Uncaught UI errors go to app.log (see `log_ui_error`). */
function reportErrors() {
  const send = (message: string) => {
    import('@tauri-apps/api/core')
      .then(({ invoke }) => invoke('log_ui_error', { message }))
      .catch(() => {});
  };
  window.addEventListener('error', (e) => send(`${e.message} at ${e.filename}:${e.lineno}:${e.colno}`));
  window.addEventListener('unhandledrejection', (e) => {
    const r = e.reason;
    // WebKit's stack doesn't include the message: log both.
    send(`unhandled rejection: ${r instanceof Error ? `${r.name}: ${r.message}\n${r.stack ?? ''}` : JSON.stringify(r)}`);
  });
}

/** Turn off what only makes sense in a browser. */
export function installNativeBehavior() {
  reportErrors();
  // Right click: only text fields keep the system menu (copy / paste).
  // Components with their own menu handle the event before it gets here.
  window.addEventListener('contextmenu', (e) => {
    if (!editable(e.target) && !window.getSelection()?.toString()) e.preventDefault();
  });

  // Reload (would drop open sessions), print, find in page, view source,
  // caret browsing, history back / forward. App shortcuts are handled
  // elsewhere; ⌘F stays available inside the SQL editor (its own search).
  window.addEventListener('keydown', (e) => {
    const mod = e.ctrlKey || e.metaKey;
    const k = e.key.toLowerCase();
    const inEditor = e.target instanceof HTMLElement && !!e.target.closest('.cm-editor');
    const blocked =
      e.key === 'F5' || e.key === 'F3' || e.key === 'F7' ||
      (mod && ['r', 'p', 'g', 'u'].includes(k)) ||
      (mod && k === 'f' && !inEditor) ||
      (e.altKey && (e.key === 'ArrowLeft' || e.key === 'ArrowRight') && !inEditor) ||
      e.key === 'BrowserBack' || e.key === 'BrowserForward' || e.key === 'BrowserRefresh';
    if (blocked) e.preventDefault();
  }, { capture: true });

  // Mouse back / forward buttons navigate the webview's history.
  window.addEventListener('mouseup', (e) => {
    if (e.button === 3 || e.button === 4) e.preventDefault();
  });

  // Dragging a link or image out of the window. The app's own drag and
  // drop (explorer folders) marks its draggables with data-dnd.
  window.addEventListener('dragstart', (e) => {
    const own = e.target instanceof HTMLElement && !!e.target.closest('[data-dnd]');
    if (!editable(e.target) && !own) e.preventDefault();
  });
}

/** The OS's confirm dialog. Resolves true on OK. */
export async function confirmNative(
  message: string,
  opts: { title: string; okLabel: string; cancelLabel?: string; kind?: 'info' | 'warning' | 'error' },
): Promise<boolean> {
  const cancelLabel = opts.cancelLabel ?? t('common:cancel');
  try {
    return await ask(message, { title: opts.title, kind: opts.kind ?? 'warning', okLabel: opts.okLabel, cancelLabel });
  } catch {
    // No native dialog (outside Tauri, or refused): the in-app one.
    return ElMessageBox.confirm(message, opts.title, {
      type: opts.kind ?? 'warning', confirmButtonText: opts.okLabel, cancelButtonText: cancelLabel,
    }).then(() => true, () => false);
  }
}
