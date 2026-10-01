<script setup lang="ts">
import { computed, onBeforeUnmount, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { api, errorMessage } from '../api/client';
import { TELEMETRY_CONSENT, TELEMETRY_NOTICE } from '../composables/telemetry';
import { UPDATES_SKIPPED, checkForUpdateInBackground, updateOffer } from '../composables/updates';
import { useSettingsStore } from '../stores/settings';

// "Hay una nueva versión de DBine": the background check a few seconds after
// start, and the dialog for both it and the manual checks
// (composables/updates.ts). "Descargar" only opens the release page.

/** From launch to the background check. */
const DELAY_MS = 5000;

const settings = useSettingsStore();

// The telemetry notice (TelemetryConsent) goes first: while it's due or
// open, this one waits.
const telemetryNoticePending = computed(() =>
  settings.get<boolean | null>(TELEMETRY_CONSENT, null) === null && !settings.get<boolean>(TELEMETRY_NOTICE, false));
const open = computed(() => updateOffer.value !== null && !telemetryNoticePending.value);

let timer: ReturnType<typeof setTimeout> | undefined;
watch(() => settings.loaded, (l) => {
  if (!l || timer !== undefined || !('__TAURI_INTERNALS__' in window)) return;
  timer = setTimeout(checkForUpdateInBackground, Math.max(0, DELAY_MS - performance.now()));
}, { immediate: true });
onBeforeUnmount(() => clearTimeout(timer));

function close() {
  updateOffer.value = null;
}

function skip() {
  if (updateOffer.value) settings.set(UPDATES_SKIPPED, updateOffer.value.latest);
  close();
}

function download() {
  if (updateOffer.value) api.openReleasePage(updateOffer.value.url).catch((e) => ElMessage.error(errorMessage(e)));
  close();
}
</script>

<template>
  <el-dialog :model-value="open" width="520px" append-to-body :close-on-click-modal="false" :show-close="false" @close="close">
    <div v-if="updateOffer" class="un-body">
      <h3>{{ $t('updates:dialog.title') }}</h3>
      <p>{{ $t('updates:dialog.text', { latest: updateOffer.latest, current: updateOffer.current }) }}</p>
      <!-- Plain text on purpose: the notes come from the network. -->
      <pre v-if="updateOffer.notes" class="un-notes">{{ updateOffer.notes }}</pre>
    </div>
    <template #footer>
      <div class="un-foot">
        <el-button text @click="skip">{{ $t('updates:dialog.skip') }}</el-button>
        <div class="nm-spacer" />
        <el-button @click="close">{{ $t('updates:dialog.later') }}</el-button>
        <el-button type="primary" @click="download">{{ $t('updates:dialog.download') }}</el-button>
      </div>
    </template>
  </el-dialog>
</template>

<style scoped>
.un-body { padding: 4px 8px 0; }
.un-body h3 { margin: 0 0 10px; font-size: 16px; color: var(--nm-text-strong); }
.un-body p { margin: 0 0 10px; line-height: 1.55; }
.un-notes {
  margin: 0; max-height: 220px; overflow: auto; padding: 8px 10px;
  border: 1px solid var(--nm-border); border-radius: 4px;
  white-space: pre-wrap; word-break: break-word;
  font-family: inherit; font-size: 12.5px; line-height: 1.5; color: var(--nm-text-muted);
}
.un-foot { display: flex; align-items: center; gap: 8px; }
</style>
