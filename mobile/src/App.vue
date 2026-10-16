<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { api, type ConnectionItem, type Status } from './api';
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

async function refresh() {
  status.value = await api.status();
}

async function unpaired() {
  open.value = null;
  await api.unpair();
  await refresh();
}

onMounted(refresh);
</script>

<template>
  <template v-if="status">
    <PairView v-if="!status.paired" @paired="refresh" />
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
.nav {
  position: fixed; left: 0; right: 0; bottom: 0; display: flex; gap: 6px; padding: 8px 10px calc(8px + env(safe-area-inset-bottom));
  background: var(--bg-elev); border-top: 1px solid var(--border);
}
.nav button { flex: 1; padding: 10px 4px; background: transparent; color: var(--text-dim); font-size: 14px; }
.nav button.on { color: var(--accent); background: var(--bg); }
</style>
