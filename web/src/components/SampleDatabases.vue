<script setup lang="ts">
import { watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import { samplesApi } from '../api/samples';
import { SAMPLES_INSTALLED, sampleTexts, tourToOpen } from '../composables/sampleDatabases';
import { TELEMETRY_CONSENT, TELEMETRY_NOTICE } from '../composables/telemetry';
import { useSettingsStore } from '../stores/settings';
import { useTabsStore } from '../stores/tabs';

// First start (and the first start after updating): the sample databases'
// folder, once (samples.rs keeps it from happening twice, also across
// windows). On a new installation the tour opens too. Shows nothing.

const { t } = useTranslation();
const settings = useSettingsStore();
const tabs = useTabsStore();

async function install() {
  if (settings.get<boolean>(SAMPLES_INSTALLED, false)) return;
  // New installation: the telemetry notice not seen yet and no choice made.
  const fresh = settings.get<boolean | null>(TELEMETRY_CONSENT, null) === null && !settings.get<boolean>(TELEMETRY_NOTICE, false);
  try {
    const path = tourToOpen(fresh, await samplesApi.install(sampleTexts(t)));
    if (path) tabs.openNotebook({ path });
  } catch (e) {
    console.warn('sample databases not added', e); // tried again next start
  }
}

watch(() => settings.loaded, (l) => { if (l) install(); }, { immediate: true });
</script>

<template>
  <span v-if="false" />
</template>
