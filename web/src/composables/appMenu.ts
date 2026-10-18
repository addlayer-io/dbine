import { onScopeDispose, watch } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';
import { windowApi } from '../api/client';
import i18next, { t } from '../i18n';
import { useTabsStore } from '../stores/tabs';
import { useUiStore } from '../stores/ui';
import { checkForUpdateNow } from './updates';
import { requestQuit } from './quitGuard';
import { bindingAccelerator, bindingOf, keyOfEvent, matchesCommand, recordingKeys, runCommand } from './commands';

// The native menu bar (macOS only: Windows and Linux have none, as VS Code).
// Its labels follow the app's language, so the UI builds it and sends it to
// the backend (`app_menu_set`) on start and on every language change. A click
// on one of its items comes back as the `app-menu` event with the item's id.
//
// Shortcuts: the menu shows the app's shortcuts (⌘N, ⌘W, ⌘B…) and, once it's
// installed, it's the menu that runs them. The window's own key handler asks
// `ownsShortcut` and leaves those keys alone, so nothing runs twice. Where
// there's no menu (Windows, Linux, a browser) the key handler keeps them all.
//
// Several windows share the one menu bar: each window sends its menu again
// when it gains focus, and the backend sends a click only to the window it
// belongs to. "Nueva ventana" (⌘⇧N) is handled in Rust; where there's no
// menu, this module takes Ctrl+Shift+N itself.
//
// Item ids are the action registry's command ids (composables/commands.ts):
// an item's accelerator is the keys the user gave that command, and the menu
// is sent again when they change. While Configuración records new keys the
// menu goes without accelerators, so ⌘N & co. reach the page.

/** What the menu runs that lives in App.vue's layout. */
export interface AppMenuActions {
  newQuery: () => void;
  toggleExplorer: () => void;
  toggleOutput: () => void;
}

type Entry =
  | { kind: 'item'; id: string; label: string; accelerator?: string }
  | { kind: 'separator' }
  | { kind: 'native'; item: string; label?: string };

const sep: Entry = { kind: 'separator' };
const native = (item: string, key?: string): Entry => ({ kind: 'native', item, label: key ? t(`menu:${key}`) : undefined });
/** `keys`: the item runs a registry command (its id), with its keys. */
const item = (id: string, keys = false): Entry => ({
  kind: 'item', id, label: t(`menu:${id}`), accelerator: keys && !recordingKeys.value ? bindingAccelerator(id) : undefined,
});

/** The menu, in the current language. Item ids are the labels' keys. */
function menus() {
  return [
    // macOS titles the application menu with the app's name.
    { label: 'DBine', items: [
      native('about', 'app.about'), item('app.settings', true), sep,
      native('services', 'app.services'), sep,
      native('hide', 'app.hide'), native('hideOthers', 'app.hideOthers'), native('showAll', 'app.showAll'), sep,
      // Ours, not the native one: quitting asks first when tasks are running.
      item('app.quit', true),
    ] },
    { label: t('menu:file.title'), items: [
      item('file.newConnection'), item('file.newQuery', true), item('file.newWindow', true), sep,
      item('file.closeTab', true),
    ] },
    // Without these, ⌘C / ⌘V / ⌘Z don't reach the webview on macOS.
    { label: t('menu:edit.title'), items: [
      native('undo', 'edit.undo'), native('redo', 'edit.redo'), sep,
      native('cut', 'edit.cut'), native('copy', 'edit.copy'), native('paste', 'edit.paste'), native('selectAll', 'edit.selectAll'),
    ] },
    { label: t('menu:view.title'), items: [
      item('palette.open', true), item('goto.open', true), sep,
      item('view.explorer', true), item('view.output', true), item('view.ai', true), sep,
      native('fullscreen', 'view.fullscreen'),
    ] },
    { label: t('menu:window.title'), items: [native('minimize', 'window.minimize'), native('zoom', 'window.zoom')] },
    { label: t('menu:help.title'), items: [item('help.checkUpdates'), sep, item('help.support')] },
  ] satisfies { label: string; items: Entry[] }[];
}

/** The commands whose menu items carry their keys (`item(id, true)` above). */
const menuCommands = ['palette.open', 'goto.open', 'app.settings', 'app.quit', 'file.newQuery', 'file.newWindow', 'file.closeTab', 'view.explorer', 'view.output', 'view.ai'];

let installed = false;

/** ⌘⇧N on macOS, Ctrl+Shift+N on Windows / Linux (or the user's keys): a new window. */
function isNewWindowKey(e: KeyboardEvent): boolean {
  return matchesCommand(e, 'file.newWindow');
}

/** True when the native menu, or this module, runs this key (the caller
 *  then ignores it). */
export function ownsShortcut(e: KeyboardEvent): boolean {
  if (isNewWindowKey(e)) return true;
  if (!installed) return false;
  const k = keyOfEvent(e);
  return !!k && menuCommands.some((id) => bindingOf(id) === k);
}

/** Install the menu and run its items. Call from App.vue's setup. */
export function useAppMenu(actions: AppMenuActions) {
  const ui = useUiStore();
  const tabs = useTabsStore();
  const run: Record<string, () => void> = {
    'app.settings': () => ui.openSettings(),
    'app.quit': () => { requestQuit(); },
    'file.newConnection': () => ui.newConnection(),
    'file.newQuery': actions.newQuery,
    'file.closeTab': () => { if (tabs.activeId) tabs.close(tabs.activeId); },
    'view.explorer': actions.toggleExplorer,
    'view.output': actions.toggleOutput,
    'view.ai': () => { ui.aiOpen = !ui.aiOpen; },
    // The command palette and "Ir a…" (composables/workbenchCommands.ts).
    'palette.open': () => { runCommand('palette.open'); },
    'goto.open': () => { runCommand('goto.open'); },
    'help.checkUpdates': () => { checkForUpdateNow(); },
    'help.support': () => { invoke('open_support_page').catch(() => {}); },
  };

  const send = () => {
    invoke<boolean>('app_menu_set', { args: { menus: menus(), dockNewWindow: t('menu:file.newWindow') } })
      .then((ok) => { installed = ok === true; })
      .catch(() => { installed = false; });
  };
  if (!('__TAURI_INTERNALS__' in window)) return;
  send();
  i18next.on('languageChanged', send);
  // New keys for a menu command, or recording started / ended.
  const stopWatch = watch(() => [recordingKeys.value, ...menuCommands.map((id) => bindingOf(id))].join('|'), send);

  // Without the menu (Windows, Linux) the new-window key is ours.
  const onKey = (e: KeyboardEvent) => {
    if (installed || recordingKeys.value || !isNewWindowKey(e)) return;
    e.preventDefault();
    windowApi.newWindow().catch(() => {});
  };
  window.addEventListener('keydown', onKey);

  const unlisteners: (() => void)[] = [];
  let disposed = false;
  const keep = (p: Promise<() => void>) =>
    p.then((f) => { if (disposed) f(); else unlisteners.push(f); }).catch(() => {});
  try {
    const win = getCurrentWebviewWindow();
    // This window's own listener: a global `listen` would also get the clicks
    // the backend sends to another window.
    keep(win.listen<string>('app-menu', (e) => run[e.payload]?.()));
    // The focused window's menu (and language) wins.
    keep(win.onFocusChanged(({ payload: focused }) => { if (focused) send(); }));
  } catch {
    // No window metadata (the dev-preview's fake backend).
  }
  onScopeDispose(() => {
    disposed = true;
    stopWatch();
    i18next.off('languageChanged', send);
    window.removeEventListener('keydown', onKey);
    unlisteners.forEach((f) => f());
  });
}
