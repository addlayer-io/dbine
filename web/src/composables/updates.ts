import { ref } from 'vue';
import { ElMessage } from 'element-plus';
import { api, errorMessage } from '../api/client';
import type { UpdateInfo } from '../api/types';
import { t } from '../i18n';
import { useSettingsStore } from '../stores/settings';
import { startupClaim } from './windowRole';

// Update check (GitHub's latest release). Nothing is installed: the dialog
// (UpdateNotice.vue) offers the release page. Two ways in:
// - in the background, once per run, a few seconds after start: silent on
//   errors and quiet about a version the user chose to skip; with several
//   windows, only the first one to start does it;
// - by hand (Ayuda › Buscar actualizaciones…, Configuración › General):
//   always answers, and ignores the skipped version.

/** Preference: the version "Omitir esta versión" was pressed for. */
export const UPDATES_SKIPPED = 'updates.skipped';

/** The release the dialog offers; null while there's nothing to show. */
export const updateOffer = ref<UpdateInfo | null>(null);
/** A manual check is running (its button shows loading). */
export const checkingForUpdate = ref(false);

let backgroundDone = false;

/** The startup check: at most once per run, errors are dropped. */
export async function checkForUpdateInBackground() {
  if (backgroundDone) return;
  backgroundDone = true;
  if (!(await startupClaim())) return;
  try {
    const info = await api.checkForUpdate();
    const skipped = useSettingsStore().get<string | null>(UPDATES_SKIPPED, null);
    if (info.available && info.latest !== skipped && !updateOffer.value) updateOffer.value = info;
  } catch {
    // Offline, rate-limited, blocked: not worth bothering anyone.
  }
}

/** The manual check: the dialog, "you're up to date" or the error. */
export async function checkForUpdateNow() {
  if (checkingForUpdate.value) return;
  checkingForUpdate.value = true;
  try {
    const info = await api.checkForUpdate();
    if (info.available) updateOffer.value = info;
    else ElMessage.success(t('updates:upToDate', { version: info.current }));
  } catch (e) {
    ElMessage.error(t('updates:failed', { error: errorMessage(e) }));
  } finally {
    checkingForUpdate.value = false;
  }
}
