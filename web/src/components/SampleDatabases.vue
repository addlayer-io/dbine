<script setup lang="ts">
import { watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import { samplesApi } from '../api/samples';
import { SAMPLES_INSTALLED, sampleTexts } from '../composables/sampleDatabases';
import { useSettingsStore } from '../stores/settings';

// First start (and the first start after updating): the sample databases'
// folder, once (samples.rs keeps it from happening twice, also across
// windows). Quietly: the first launch only offers importing connections, and
// the tour opens from the folder's menu or the welcome screen.

const { t } = useTranslation();
const settings = useSettingsStore();

async function install() {
  if (settings.get<boolean>(SAMPLES_INSTALLED, false)) return;
  try {
    await samplesApi.install(sampleTexts(t));
  } catch (e) {
    console.warn('sample databases not added', e); // tried again next start
  }
}

watch(() => settings.loaded, (l) => { if (l) install(); }, { immediate: true });
</script>

<template>
  <span v-if="false" />
</template>
