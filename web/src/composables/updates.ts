import { onScopeDispose, ref } from 'vue';
import { ElMessage } from 'element-plus';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';
import { api, errorKind, errorMessage } from '../api/client';
import type { UpdateInfo, UpdateProgress, WhatsNew } from '../api/types';
import { language, locale, t } from '../i18n';
import { useSettingsStore } from '../stores/settings';
import { readJson, writeJson } from '../stores/storage';
import { requestUpdateRestart } from './quitGuard';
import { initWindowRole, startupClaim } from './windowRole';

// Updates (docs/updates.md). Like VS Code, always automatic where the
// install can update itself (`installable` and `auto`):
// - checks: at launch (a few seconds after start, the first window only)
//   and every 6 hours in every window (the backend lets one of them through
//   per period); errors are silent;
// - a new version downloads and verifies in the background, no dialog;
// - once ready, the status bar shows "Reiniciar para actualizar"
//   (`updateReady`, in every window); a normal quit installs it too (the
//   backend's `install_on_quit`), and the next launch is the new version;
// - the first launch of a new version shows what's new (`whatsNew`).
// Where it can't (deb/rpm, MSI, the app outside Applications, unsigned
// builds) or `DBINE_AUTO_UPDATE=0` turns it off, the notice (UpdateNotice.vue)
// offers the version as before, with "Omitir esta versión". The Flathub
// build checks nothing.
// By hand (Ayuda › Buscar actualizaciones…, Configuración › General): always
// answers; with an update ready, the dialog offers the restart.

/** Preference: the version "Omitir esta versión" was pressed for (the notice only). */
export const UPDATES_SKIPPED = 'updates.skipped';
/** This machine's last version that ran (localStorage: the settings travel
 *  with the cloud backup to other machines, this must not). */
const LAST_RUN_VERSION = 'dbine.lastRunVersion';
/** Between periodic checks. */
const PERIOD_MS = 6 * 60 * 60 * 1000;

export type UpdateStage = 'offer' | 'downloading' | 'verifying' | 'ready' | 'error';

/** The release the dialog shows; null while there's nothing to show. */
export const updateOffer = ref<UpdateInfo | null>(null);
/** Where the dialog is. */
export const updateStage = ref<UpdateStage>('offer');
/** Bytes downloaded so far, and the size when the server says it. */
export const updateProgress = ref<{ downloaded: number; total: number | null }>({ downloaded: 0, total: null });
/** The message of the `error` stage. */
export const updateError = ref('');
/** A manual check is running (its button shows loading). */
export const checkingForUpdate = ref(false);
/** A downloaded, verified update waiting for the restart (the status bar's
 *  button), in every window. */
export const updateReady = ref<UpdateInfo | null>(null);
/** What's new after an update, while its dialog is due. */
export const whatsNew = ref<WhatsNew | null>(null);

let launchDone = false;
let periodicTimer: ReturnType<typeof setInterval> | undefined;
/** The notice (no self-update) shows a version once per run on its own. */
const noticed = new Set<string>();

/** Open the dialog on `info`, in the stage the backend reports. */
function show(info: UpdateInfo) {
  updateOffer.value = info;
  updateStage.value = info.phase === 'ready' ? 'ready' : info.phase === 'downloading' ? 'downloading' : 'offer';
  if (info.phase === 'idle') updateProgress.value = { downloaded: 0, total: null };
}

/** Download and verify in the background; the backend tells every window
 *  when it's ready (`update-state-changed`). Failures are silent: the next
 *  check tries again. */
function downloadSilently() {
  api.updateDownload().catch(() => {});
}

/** What a background check does with its answer. */
function onBackgroundInfo(info: UpdateInfo) {
  if (info.reason === 'flatpak') {
    clearInterval(periodicTimer);
    periodicTimer = undefined;
    return;
  }
  if (!info.available) return;
  if (info.installable && info.auto) {
    if (info.phase === 'idle') downloadSilently();
    return;
  }
  const skipped = useSettingsStore().get<string | null>(UPDATES_SKIPPED, null);
  if (info.latest === skipped || noticed.has(info.latest) || updateOffer.value) return;
  noticed.add(info.latest);
  show(info);
}

async function backgroundCheck(periodic: boolean) {
  try {
    onBackgroundInfo(await api.checkForUpdate(false, language.value, periodic));
  } catch {
    // Offline, rate-limited, blocked: not worth bothering anyone.
  }
}

/** The launch check (once per run, the first window) and the periodic one
 *  (every window; the backend throttles). Call once the settings are loaded. */
export async function startUpdateChecks() {
  if (launchDone) return;
  launchDone = true;
  periodicTimer = setInterval(() => { void backgroundCheck(true); }, PERIOD_MS);
  void refreshUpdateReady();
  if (!(await startupClaim())) return;
  void showWhatsNewOnce();
  await backgroundCheck(false);
}

/** The status bar's state, from the backend. */
export async function refreshUpdateReady() {
  try {
    updateReady.value = await api.updateState();
  } catch {
    updateReady.value = null;
  }
}

/** First launch of a new version: the changelog since the one that ran
 *  before. A failure (offline) keeps the old version, so the next launch
 *  tries again. A first install records the version and shows nothing. */
async function showWhatsNewOnce() {
  let current: string;
  try {
    const { getVersion } = await import('@tauri-apps/api/app');
    current = await getVersion();
  } catch {
    return;
  }
  const last = readJson<string | null>(LAST_RUN_VERSION, null);
  if (last === current) return;
  if (!last) {
    writeJson(LAST_RUN_VERSION, current);
    return;
  }
  try {
    const news = await api.whatsNew(last, language.value);
    writeJson(LAST_RUN_VERSION, current);
    if (news?.notes.trim()) whatsNew.value = news;
  } catch {
    // Next launch.
  }
}

/** "main" is window 1, "win-3" is window 3. */
function windowNumber(label: string): string {
  const m = /^win-(\d+)$/.exec(label);
  return m ? m[1] : '1';
}

/** A failed check or download: no connection reads as such, not as the
 *  network library's error chain. */
function updateErrorText(e: unknown): string {
  return errorKind(e) === 'connect' ? t('updates:offline') : errorMessage(e);
}

/** The manual check: the dialog, "you're up to date" or the error. With
 *  automatic updates an update found starts downloading in the dialog. */
export async function checkForUpdateNow() {
  if (checkingForUpdate.value) return;
  checkingForUpdate.value = true;
  try {
    const info = await api.checkForUpdate(true, language.value);
    const { label } = await initWindowRole();
    if (info.reason === 'flatpak') {
      ElMessage.info(t('updates:managedByFlatpak', { version: info.current }));
    } else if (!info.available) {
      ElMessage.success(t('updates:upToDate', { version: info.current }));
    } else if (info.owner && info.owner !== label && info.phase === 'downloading') {
      ElMessage.info(t('updates:busyInWindow', { n: windowNumber(info.owner) }));
    } else {
      show(info);
      if (info.installable && info.auto && info.phase === 'idle') void startUpdateDownload();
    }
  } catch (e) {
    ElMessage.error(errorKind(e) === 'connect' ? t('updates:offline') : t('updates:failed', { error: errorMessage(e) }));
  } finally {
    checkingForUpdate.value = false;
  }
}

/** "Actualizar ahora": download and verify; the dialog follows the progress. */
export async function startUpdateDownload() {
  if (!updateOffer.value || updateStage.value === 'downloading' || updateStage.value === 'verifying') return;
  updateStage.value = 'downloading';
  updateProgress.value = { downloaded: 0, total: null };
  try {
    await api.updateDownload();
    if (updateOffer.value) updateStage.value = 'ready';
  } catch (e) {
    if (!updateOffer.value) return;
    if (errorKind(e) === 'cancelled') {
      updateStage.value = 'offer';
    } else {
      updateError.value = updateErrorText(e);
      updateStage.value = 'error';
    }
  }
}

export function cancelUpdateDownload() {
  api.updateCancel().catch(() => {});
}

/** "Reiniciar para terminar" / the status bar's "Reiniciar para actualizar":
 *  through the quit guard (asks about running tasks). Staying changes
 *  nothing; a failed install shows the error with the release page. */
export async function restartToUpdate() {
  try {
    await requestUpdateRestart();
  } catch (e) {
    if (!updateOffer.value && updateReady.value) updateOffer.value = updateReady.value;
    updateError.value = updateErrorText(e);
    updateStage.value = 'error';
  }
}

/** "940 KB", "12,3 MB". */
export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ['KB', 'MB', 'GB'];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v.toLocaleString(locale(), { maximumFractionDigits: 1, minimumFractionDigits: v < 100 ? 1 : 0 })} ${units[i]}`;
}

/** The backend's events for this window: download progress, and another
 *  window taking the update over (this one's dialog closes) and how a
 *  download ended. The window's
 *  own listener: they're sent to one window only. Plus the app-wide
 *  `update-state-changed` (an update ready or gone: the status bar). Call
 *  once from the always-mounted dialog. */
export function useUpdateEvents() {
  if (!('__TAURI_INTERNALS__' in window)) return;
  let w;
  try { w = getCurrentWebviewWindow(); } catch { return; }
  const offs: (() => void)[] = [];
  let disposed = false;
  const keep = (f: () => void) => { if (disposed) f(); else offs.push(f); };
  w.listen<UpdateProgress>('update-progress', ({ payload }) => {
    if (!updateOffer.value) return;
    if (payload.phase === 'verifying') {
      updateStage.value = 'verifying';
    } else if (updateStage.value === 'downloading') {
      updateProgress.value = { downloaded: payload.downloaded, total: payload.total };
    }
  }).then(keep).catch(() => {});
  // How the download ended. The window awaiting `updateDownload` gets the
  // same from the command; this covers a window that took over the download
  // of another one that was closed (else its dialog would stay "downloading"),
  // and a dialog opened on a download that started in the background.
  w.listen<{ ok: boolean; cancelled: boolean; error: unknown }>('update-finished', ({ payload }) => {
    if (!updateOffer.value || (updateStage.value !== 'downloading' && updateStage.value !== 'verifying')) return;
    if (payload.ok) {
      updateStage.value = 'ready';
    } else if (payload.cancelled) {
      updateStage.value = 'offer';
    } else {
      updateError.value = updateErrorText(payload.error);
      updateStage.value = 'error';
    }
  }).then(keep).catch(() => {});
  w.listen('update-owner-changed', () => {
    if (updateStage.value !== 'downloading' && updateStage.value !== 'verifying') updateOffer.value = null;
  }).then(keep).catch(() => {});
  // Sent to every window; one listener per window (this composable runs once each).
  listen('update-state-changed', () => { void refreshUpdateReady(); }).then(keep).catch(() => {});
  onScopeDispose(() => {
    disposed = true;
    offs.splice(0).forEach((f) => f());
    clearInterval(periodicTimer);
  });
}
