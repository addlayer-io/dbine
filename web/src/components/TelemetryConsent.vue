<script setup lang="ts">
import { ref, watch } from 'vue';
import { useSettingsStore } from '../stores/settings';
import { useTabsStore } from '../stores/tabs';
import { TELEMETRY_CONSENT, TELEMETRY_NOTICE, telemetryAllowed, trackAppStarted, trackModuleOpened } from '../composables/telemetry';

// Telemetry is on by default. Once, this notice tells what's sent and that
// Configuración › General turns it off; it doesn't ask. Both the choice and
// the notice being seen are preferences (they travel with the cloud backup).

const settings = useSettingsStore();
const open = ref(false);

watch(() => settings.loaded, (l) => {
  if (!l) return;
  trackAppStarted();
  // Not for whoever already chose (an earlier version asked).
  if (settings.get<boolean | null>(TELEMETRY_CONSENT, null) === null && !settings.get<boolean>(TELEMETRY_NOTICE, false)) {
    // A moment after opening, not over the first thing the user does.
    setTimeout(() => { open.value = true; }, 2500);
  }
}, { immediate: true });

// A module counts as used when its tab is shown, not when it's restored in
// the background at startup. Also re-checked when the setting changes.
const tabs = useTabsStore();
watch(() => [tabs.tabs.find((t) => t.id === tabs.activeId)?.kind, telemetryAllowed()] as const, ([kind, allowed]) => {
  if (kind && allowed) trackModuleOpened(kind);
}, { immediate: true });

function close() {
  open.value = false;
  settings.set(TELEMETRY_NOTICE, true);
}
</script>

<template>
  <el-dialog :model-value="open" width="480px" append-to-body :close-on-click-modal="false" :close-on-press-escape="false" :show-close="false">
    <div class="tc-body">
      <h3>{{ $t('telemetry:consent.title') }}</h3>
      <p>{{ $t('telemetry:consent.text') }}</p>
      <ul>
        <li>{{ $t('telemetry:consent.sent') }}</li>
        <li>{{ $t('telemetry:consent.notSent') }}</li>
      </ul>
      <p class="tc-muted">{{ $t('telemetry:consent.change') }}</p>
    </div>
    <template #footer>
      <el-button type="primary" @click="close">{{ $t('telemetry:consent.ok') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.tc-body { padding: 4px 8px 0; }
.tc-body h3 { margin: 0 0 10px; font-size: 16px; color: var(--nm-text-strong); }
.tc-body p { margin: 0 0 8px; line-height: 1.55; }
.tc-body ul { margin: 0 0 10px; padding-left: 18px; line-height: 1.55; }
.tc-body li { margin-bottom: 4px; }
.tc-muted { color: var(--nm-text-muted); font-size: 12.5px; }
</style>
