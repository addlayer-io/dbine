import { readonly, ref } from 'vue';
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';
import { windowApi } from '../api/client';

// Which window this is. Several windows share one backend: only the primary
// one ("main", or the oldest left when it closes) restores and saves the tabs
// and the AI conversation; the rest start empty. When the primary closes,
// the backend promotes another one with a "window-role" event.

export interface WindowRoleState {
  label: string;
  primary: boolean;
  windowCount: number;
}

const inTauri = () => typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

// Outside Tauri (dev-preview, tests) there's one window, and it's the primary.
const state = ref<WindowRoleState>({ label: 'main', primary: true, windowCount: 1 });

/** This window's role. Reactive; filled by `initWindowRole`. */
export const windowRole = readonly(state);

/** Asks the backend again (the window count changes as windows open and
 *  close; a quit guard should call this right before deciding). */
export async function refreshWindowRole(): Promise<WindowRoleState> {
  if (!inTauri()) return state.value;
  try {
    const r = await windowApi.role();
    // The dev-preview's fake backend answers null.
    if (r) state.value = { label: r.label, primary: r.primary, windowCount: r.window_count };
  } catch {
    // Keep what we had.
  }
  return state.value;
}

let ready: Promise<WindowRoleState> | null = null;

/** Reads the role once and follows promotions. Idempotent: every caller gets
 *  the same promise, so code that depends on the role (restoring tabs) can
 *  await it. */
export function initWindowRole(): Promise<WindowRoleState> {
  if (ready) return ready;
  ready = refreshWindowRole();
  if (inTauri()) {
    try {
      // The window's own listener: "window-role" is emitted to one window
      // only, and a global `listen` would hear events sent to the others too.
      getCurrentWebviewWindow()
        .listen<{ primary: boolean }>('window-role', (e) => {
          state.value = { ...state.value, primary: e.payload.primary };
          void refreshWindowRole();
        })
        .catch(() => {});
    } catch {
      // No window metadata (the dev-preview's fake backend).
    }
  }
  return ready;
}

let claim: Promise<boolean> | null = null;

/** True in the one window that does the once-per-run work (update check,
 *  app-start telemetry): the first to ask in this run. Asked once per window. */
export function startupClaim(): Promise<boolean> {
  if (claim) return claim;
  claim = inTauri()
    // The dev-preview's fake backend answers null: one window, so it's ours.
    ? windowApi.claimStartup().then((r) => r !== false).catch(() => false)
    : Promise.resolve(true);
  return claim;
}
