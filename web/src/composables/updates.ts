import { onScopeDispose, ref } from 'vue';
import { ElMessage } from 'element-plus';
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';
import { api, errorKind, errorMessage } from '../api/client';
import type { UpdateInfo, UpdateProgress } from '../api/types';
import { language, locale, t } from '../i18n';
import { useSettingsStore } from '../stores/settings';
import { requestUpdateRestart } from './quitGuard';
import { initWindowRole, startupClaim } from './windowRole';

// Updates (docs/updates.md). The dialog is UpdateNotice.vue. Two
// ways in:
// - in the background, once per run, a few seconds after start: silent on
//   errors and quiet about a version the user chose to skip; with several
//   windows, only the first one to start does it;
// - by hand (Ayuda › Buscar actualizaciones…, Configuración › General):
//   always answers, and ignores the skipped version.
// When the install can update itself, "Actualizar ahora" downloads and
// verifies the package in the backend and "Reiniciar para terminar"
// installs it through the quit guard. Otherwise the dialog offers the
// release page. One window drives an update (the backend's owner); another
// window that checks meanwhile is told where it is.

/** Preference: the version "Omitir esta versión" was pressed for. */
export const UPDATES_SKIPPED = 'updates.skipped';

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

let backgroundDone = false;

/** Open the dialog on `info`, in the stage the backend reports. */
function show(info: UpdateInfo) {
  updateOffer.value = info;
  updateStage.value = info.phase === 'ready' ? 'ready' : info.phase === 'downloading' ? 'downloading' : 'offer';
  if (info.phase === 'idle') updateProgress.value = { downloaded: 0, total: null };
}

/** The startup check: at most once per run, errors are dropped. */
export async function checkForUpdateInBackground() {
  if (backgroundDone) return;
  backgroundDone = true;
  if (!(await startupClaim())) return;
  try {
    const info = await api.checkForUpdate(false, language.value);
    const skipped = useSettingsStore().get<string | null>(UPDATES_SKIPPED, null);
    if (info.available && info.latest !== skipped && !updateOffer.value) show(info);
  } catch {
    // Offline, rate-limited, blocked: not worth bothering anyone.
  }
}

/** "main" is window 1, "win-3" is window 3. */
function windowNumber(label: string): string {
  const m = /^win-(\d+)$/.exec(label);
  return m ? m[1] : '1';
}

/** The manual check: the dialog, "you're up to date" or the error. */
export async function checkForUpdateNow() {
  if (checkingForUpdate.value) return;
  checkingForUpdate.value = true;
  try {
    const info = await api.checkForUpdate(true, language.value);
    const { label } = await initWindowRole();
    if (!info.available) {
      ElMessage.success(t('updates:upToDate', { version: info.current }));
    } else if (info.owner && info.owner !== label && info.phase === 'downloading') {
      ElMessage.info(t('updates:busyInWindow', { n: windowNumber(info.owner) }));
    } else {
      show(info);
    }
  } catch (e) {
    ElMessage.error(t('updates:failed', { error: errorMessage(e) }));
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
      updateError.value = errorMessage(e);
      updateStage.value = 'error';
    }
  }
}

export function cancelUpdateDownload() {
  api.updateCancel().catch(() => {});
}

/** "Reiniciar para terminar": through the quit guard (asks about running
 *  tasks). Staying keeps the dialog on `ready`; a failed install shows the
 *  error with the release page. */
export async function restartToUpdate() {
  try {
    await requestUpdateRestart();
  } catch (e) {
    updateError.value = errorMessage(e);
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
 *  own listener: they're sent to one window only. Call once from the
 *  always-mounted dialog. */
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
  // of another one that was closed (else its dialog would stay "downloading").
  w.listen<{ ok: boolean; cancelled: boolean; error: unknown }>('update-finished', ({ payload }) => {
    if (!updateOffer.value || (updateStage.value !== 'downloading' && updateStage.value !== 'verifying')) return;
    if (payload.ok) {
      updateStage.value = 'ready';
    } else if (payload.cancelled) {
      updateStage.value = 'offer';
    } else {
      updateError.value = errorMessage(payload.error);
      updateStage.value = 'error';
    }
  }).then(keep).catch(() => {});
  w.listen('update-owner-changed', () => {
    if (updateStage.value !== 'downloading' && updateStage.value !== 'verifying') updateOffer.value = null;
  }).then(keep).catch(() => {});
  onScopeDispose(() => { disposed = true; offs.splice(0).forEach((f) => f()); });
}
