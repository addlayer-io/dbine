<script setup lang="ts">
import { ref, watch } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { useSettingsStore } from '../stores/settings';
import { updateOffer } from '../composables/updates';

// "Apoyá el proyecto": voluntary support through GitHub Sponsors, not a
// license (nothing in the app depends on it, and the app can't see who
// supports it). One reminder: a little after the app is first opened, then
// once a month, whatever the person answers.
// The date is a preference: it travels with the cloud backup, so it doesn't
// repeat on every machine of the same person.

const NEXT = 'support.next';
const DAY = 86_400_000;
const EVERY_DAYS = 30;

const settings = useSettingsStore();
const open = ref(false);

const later = (days: number) => new Date(Date.now() + days * DAY).toISOString();

function evaluate() {
  const next = Date.parse(settings.get<string | null>(NEXT, null) ?? '');
  // First time (no date yet) or due: a little after opening the app, not
  // over what the user is starting to do.
  if (!Number.isFinite(next) || Date.now() >= next) setTimeout(show, 8000);
}

/** Not over the update notice: wait until it's closed. */
function show() {
  if (!updateOffer.value) { open.value = true; return; }
  const stop = watch(updateOffer, (u) => { if (!u) { stop(); open.value = true; } });
}

/** Close, and come back in a month. */
function close() {
  open.value = false;
  settings.set(NEXT, later(EVERY_DAYS));
}

function support() {
  invoke('open_support_page').catch(() => {});
  close();
}

watch(() => settings.loaded, (l) => { if (l) evaluate(); }, { immediate: true });
</script>

<template>
  <el-dialog :model-value="open" width="460px" append-to-body :close-on-click-modal="false" :show-close="false">
    <div class="sr-body">
      <div class="sr-heart">♥</div>
      <h3>{{ $t('dialogs:support.title') }}</h3>
      <p>{{ $t('dialogs:support.text') }}</p>
      <p class="sr-muted">{{ $t('dialogs:support.notPayment') }}</p>
    </div>
    <template #footer>
      <div class="sr-foot">
        <div class="nm-spacer" />
        <el-button @click="close">{{ $t('dialogs:support.notNow') }}</el-button>
        <el-button type="primary" @click="support">{{ $t('dialogs:support.support') }}</el-button>
      </div>
    </template>
  </el-dialog>
</template>

<style scoped>
.sr-body { text-align: center; padding: 4px 8px 0; }
.sr-heart { font-size: 28px; color: #f14c4c; line-height: 1; }
.sr-body h3 { margin: 8px 0 10px; font-size: 16px; color: var(--nm-text-strong); }
.sr-body p { margin: 0 0 8px; line-height: 1.55; }
.sr-muted { color: var(--nm-text-muted); font-size: 12.5px; }
.sr-foot { display: flex; align-items: center; gap: 8px; }
</style>
