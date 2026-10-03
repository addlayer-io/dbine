<script setup lang="ts">
import { computed, onBeforeUnmount, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { api, errorMessage } from '../api/client';
import { TELEMETRY_CONSENT, TELEMETRY_NOTICE } from '../composables/telemetry';
import {
  UPDATES_SKIPPED, cancelUpdateDownload, checkForUpdateInBackground, formatBytes, restartToUpdate,
  startUpdateDownload, updateError, updateOffer, updateProgress, updateStage, useUpdateEvents,
} from '../composables/updates';
import { noteBlocks } from '../composables/releaseNotes';
import { useSettingsStore } from '../stores/settings';

// "Hay una nueva versión de DBine": the background check a few seconds after
// start, and the dialog for both it and the manual checks
// (composables/updates.ts). Stages: the offer, the download with its
// progress, the verification, "ready" (restart to finish) and an error with
// the release page as the way out. Without self-update (a deb/rpm install,
// no manifest…) the offer's "Descargar" opens the release page.

/** From launch to the background check. */
const DELAY_MS = 5000;

const settings = useSettingsStore();
useUpdateEvents();

// The telemetry notice (TelemetryConsent) goes first: while it's due or
// open, this one waits.
const telemetryNoticePending = computed(() =>
  settings.get<boolean | null>(TELEMETRY_CONSENT, null) === null && !settings.get<boolean>(TELEMETRY_NOTICE, false));
const open = computed(() => updateOffer.value !== null && !telemetryNoticePending.value);
/** The notes' Markdown as plain-text blocks (composables/releaseNotes.ts). */
const notes = computed(() => noteBlocks(updateOffer.value?.notes ?? ''));
const busy = computed(() => updateStage.value === 'downloading' || updateStage.value === 'verifying');

const percent = computed(() => {
  const { downloaded, total } = updateProgress.value;
  return total ? Math.min(100, Math.floor((downloaded / total) * 100)) : null;
});
const progressText = computed(() => {
  const { downloaded, total } = updateProgress.value;
  return total
    ? { key: 'updates:dialog.progress', args: { done: formatBytes(downloaded), total: formatBytes(total) } }
    : { key: 'updates:dialog.progressUnknown', args: { done: formatBytes(downloaded) } };
});
/** The backend's message reads as a sentence. */
const errorText = computed(() => {
  const e = updateError.value.trim();
  return e ? `${e[0].toLocaleUpperCase()}${e.slice(1)}${/[.!?…]$/.test(e) ? '' : '.'}` : '';
});
/** Why "Actualizar ahora" isn't there, when it's worth saying. */
const reasonKey = computed(() => {
  const r = updateOffer.value?.reason;
  return r === 'package' ? 'updates:reason.package' : r === 'location' ? 'updates:reason.location' : null;
});

let timer: ReturnType<typeof setTimeout> | undefined;
watch(() => settings.loaded, (l) => {
  if (!l || timer !== undefined || !('__TAURI_INTERNALS__' in window)) return;
  timer = setTimeout(checkForUpdateInBackground, Math.max(0, DELAY_MS - performance.now()));
}, { immediate: true });
onBeforeUnmount(() => clearTimeout(timer));

function close() {
  if (busy.value) return;
  updateOffer.value = null;
}

function skip() {
  if (updateOffer.value) settings.set(UPDATES_SKIPPED, updateOffer.value.latest);
  close();
}

function openPage() {
  if (updateOffer.value) api.openReleasePage(updateOffer.value.url).catch((e) => ElMessage.error(errorMessage(e)));
  close();
}
</script>

<template>
  <el-dialog
    :model-value="open" width="520px" append-to-body :close-on-click-modal="false"
    :close-on-press-escape="!busy" :show-close="false" @close="close"
  >
    <div v-if="updateOffer" class="un-body">
      <template v-if="updateStage === 'offer'">
        <h3>{{ $t('updates:dialog.title') }}</h3>
        <p>{{ $t('updates:dialog.text', { latest: updateOffer.latest, current: updateOffer.current }) }}</p>
        <!-- Text only, never v-html: the notes come from the network. -->
        <div v-if="notes.length" class="un-notes">
          <template v-for="(b, i) in notes" :key="i">
            <h4 v-if="b.kind === 'heading'">{{ b.text }}</h4>
            <div v-else-if="b.kind === 'item'" class="un-item" :class="{ nested: b.nested }">{{ b.text }}</div>
            <p v-else>{{ b.text }}</p>
          </template>
        </div>
        <p v-if="!updateOffer.installable && reasonKey" class="un-reason">{{ $t(reasonKey) }}</p>
      </template>
      <template v-else-if="updateStage === 'downloading'">
        <h3>{{ $t('updates:dialog.downloading', { latest: updateOffer.latest }) }}</h3>
        <el-progress v-if="percent != null" :percentage="percent" :show-text="false" :stroke-width="6" class="un-bar" />
        <el-progress v-else :percentage="100" :indeterminate="true" :duration="2" :show-text="false" :stroke-width="6" class="un-bar" />
        <p class="un-muted">{{ $t(progressText.key, progressText.args) }}</p>
      </template>
      <template v-else-if="updateStage === 'verifying'">
        <h3>{{ $t('updates:dialog.downloading', { latest: updateOffer.latest }) }}</h3>
        <el-progress :percentage="100" :indeterminate="true" :duration="2" :show-text="false" :stroke-width="6" class="un-bar" />
        <p class="un-muted">{{ $t('updates:dialog.verifying') }}</p>
      </template>
      <template v-else-if="updateStage === 'ready'">
        <h3>{{ $t('updates:dialog.readyTitle', { latest: updateOffer.latest }) }}</h3>
        <p>{{ $t('updates:dialog.ready') }}</p>
      </template>
      <template v-else>
        <h3>{{ $t('updates:dialog.failedTitle') }}</h3>
        <p class="un-error">{{ errorText }}</p>
        <p>{{ $t('updates:dialog.failed') }}</p>
      </template>
    </div>
    <template #footer>
      <div class="un-foot">
        <template v-if="updateStage === 'offer'">
          <el-button text @click="skip">{{ $t('updates:dialog.skip') }}</el-button>
          <div class="nm-spacer" />
          <el-button @click="close">{{ $t('updates:dialog.later') }}</el-button>
          <el-button v-if="updateOffer?.installable" type="primary" @click="startUpdateDownload">{{ $t('updates:dialog.updateNow') }}</el-button>
          <el-button v-else type="primary" @click="openPage">{{ $t('updates:dialog.download') }}</el-button>
        </template>
        <template v-else-if="updateStage === 'downloading'">
          <div class="nm-spacer" />
          <el-button @click="cancelUpdateDownload">{{ $t('updates:dialog.cancel') }}</el-button>
        </template>
        <template v-else-if="updateStage === 'ready'">
          <div class="nm-spacer" />
          <el-button @click="close">{{ $t('updates:dialog.later') }}</el-button>
          <el-button type="primary" @click="restartToUpdate">{{ $t('updates:dialog.restart') }}</el-button>
        </template>
        <template v-else-if="updateStage === 'error'">
          <div class="nm-spacer" />
          <el-button @click="close">{{ $t('updates:dialog.close') }}</el-button>
          <el-button type="primary" @click="openPage">{{ $t('updates:dialog.openPage') }}</el-button>
        </template>
      </div>
    </template>
  </el-dialog>
</template>

<style scoped>
.un-body { padding: 4px 8px 0; }
.un-body h3 { margin: 0 0 10px; font-size: 16px; color: var(--nm-text-strong); }
.un-body p { margin: 0 0 10px; line-height: 1.55; }
.un-notes {
  margin: 0 0 10px; max-height: 220px; overflow: auto; padding: 8px 10px;
  border: 1px solid var(--nm-border); border-radius: 4px;
  word-break: break-word; font-size: 12.5px; line-height: 1.5; color: var(--nm-text-muted);
}
.un-notes h4 { margin: 8px 0 4px; font-size: 12.5px; font-weight: 600; color: var(--nm-text-strong); }
.un-notes h4:first-child { margin-top: 0; }
.un-notes p { margin: 0 0 6px; line-height: 1.5; }
.un-item { position: relative; padding-left: 14px; margin-bottom: 2px; }
.un-item::before { content: '•'; position: absolute; left: 3px; }
.un-item.nested { margin-left: 14px; }
.un-error { word-break: break-word; }
.un-reason, .un-muted { font-size: 12.5px; color: var(--nm-text-muted); }
.un-bar { margin: 4px 0 8px; }
.un-foot { display: flex; align-items: center; gap: 8px; min-height: 32px; }
</style>
