<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { listen } from '@tauri-apps/api/event';
import { getVersion } from '@tauri-apps/api/app';
import { useTranslation } from 'i18next-vue';
import { api, newerRelease, type ConnectionItem, type Status } from './api';
import PairView from './views/PairView.vue';
import ConnectionsView from './views/ConnectionsView.vue';
import ConnectionView from './views/ConnectionView.vue';
import ActivityView from './views/ActivityView.vue';

// Pairing first; then two sections at the bottom (connections, activity),
// and a connection's own screen above them.

const status = ref<Status | null>(null);
const open = ref<ConnectionItem | null>(null);
const canKill = ref(false);
const section = ref<'connections' | 'activity'>('connections');
const { t } = useTranslation();
/** A pairing link that opened the app. */
const link = ref<string | null>(null);
const linkNotice = ref('');
const update = ref<{ version: string; url: string } | null>(null);

async function takeLink() {
  const l = await api.pendingLink();
  if (!l) return;
  await refresh();
  if (status.value?.paired) {
    // Never replaces a pairing silently: unpair first.
    linkNotice.value = t('pair.alreadyPaired', { desktop: status.value.desktop ?? '' });
    return;
  }
  link.value = l;
}

async function paired() {
  link.value = null;
  await refresh();
}

async function refresh() {
  status.value = await api.status();
}

async function unpaired() {
  open.value = null;
  await api.unpair();
  await refresh();
}

onMounted(async () => {
  await refresh();
  await takeLink();
  await listen('pair-link', () => takeLink());
  update.value = await newerRelease(await getVersion());
});
</script>

<template>
  <template v-if="status">
    <div v-if="update" class="upd">
      <span>{{ $t('update.available', { version: update.version }) }}</span>
      <button class="primary" @click="api.openRelease(update.url)">{{ $t('update.download') }}</button>
    </div>
    <p v-if="linkNotice" class="upd warn" @click="linkNotice = ''">{{ linkNotice }}</p>
    <PairView v-if="!status.paired" :code="link" @paired="paired" @cancel-link="link = null" />
    <template v-else>
      <main class="main">
        <ConnectionView v-if="open" :conn="open" :can-kill="canKill" @back="open = null" />
        <ConnectionsView
          v-else-if="section === 'connections'"
          :desktop="status.desktop ?? ''"
          :fingerprint="status.fingerprint ?? ''"
          @open="(c, k) => { open = c; canKill = k; }"
          @unpair="unpaired"
        />
        <ActivityView v-else />
      </main>
      <nav class="nav">
        <button :class="{ on: section === 'connections' && !open }" @click="section = 'connections'; open = null">{{ $t('nav.connections') }}</button>
        <button :class="{ on: section === 'activity' && !open }" @click="section = 'activity'; open = null">{{ $t('nav.activity') }}</button>
      </nav>
    </template>
  </template>
</template>

<style scoped>
.main { padding-bottom: 72px; }
.upd { display: flex; align-items: center; gap: 10px; margin: 0; padding: 10px 16px; background: var(--bg-elev); border-bottom: 1px solid var(--border); font-size: 14px; }
.upd span { flex: 1; }
.upd button { padding: 8px 12px; }
.upd.warn { color: var(--warning); }
.nav {
  position: fixed; left: 0; right: 0; bottom: 0; display: flex; gap: 6px; padding: 8px 10px calc(8px + env(safe-area-inset-bottom));
  background: var(--bg-elev); border-top: 1px solid var(--border);
}
.nav button { flex: 1; padding: 10px 4px; background: transparent; color: var(--text-dim); font-size: 14px; }
.nav button.on { color: var(--accent); background: var(--bg); }
</style>
