<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from 'vue';
import { api, type EventRow } from '../api';
import { tb } from '../backend';

// What DBine told this phone while the app was running: queries that took
// long and ended, tasks that failed or alerted. The same notices arrive as
// Android notifications.

const list = ref<EventRow[]>([]);
let timer: ReturnType<typeof setInterval> | null = null;

async function load() {
  try { list.value = await api.news(); } catch { /* not yet */ }
}

onMounted(() => {
  load();
  timer = setInterval(load, 5000);
});
onBeforeUnmount(() => timer && clearInterval(timer));
</script>

<template>
  <div>
    <div class="bar"><h1>{{ $t('news.title') }}</h1></div>
    <div class="page">
      <p class="muted">{{ $t('news.intro') }}</p>
      <p v-if="!list.length" class="muted">{{ $t('news.none') }}</p>
      <div v-for="e in list" :key="e.seq + e.at" class="card ev" :class="{ bad: !e.ok }">
        <div class="head">
          <span class="kind">{{ $t(`news.kind.${e.kind}`) }}</span>
          <span class="muted">{{ e.at }}</span>
        </div>
        <strong>{{ e.title }}</strong>
        <p>{{ tb(e.body) }}</p>
      </div>
    </div>
  </div>
</template>

<style scoped>
.ev { display: flex; flex-direction: column; gap: 4px; border-left: 3px solid var(--success); }
.ev.bad { border-left-color: var(--danger); }
.ev p { margin: 0; white-space: pre-line; font-size: 14px; }
.head { display: flex; justify-content: space-between; }
.kind { font-size: 12px; text-transform: uppercase; letter-spacing: .5px; color: var(--text-dim); }
</style>
