<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { api, type ConnectionItem, type Status } from './api';
import PairView from './views/PairView.vue';
import ConnectionsView from './views/ConnectionsView.vue';
import ConnectionView from './views/ConnectionView.vue';

// Three screens: pair with a computer, its connections, one connection's
// locks and running queries.

const status = ref<Status | null>(null);
const open = ref<ConnectionItem | null>(null);
const canKill = ref(false);

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
    <ConnectionView v-else-if="open" :conn="open" :can-kill="canKill" @back="open = null" />
    <ConnectionsView v-else :desktop="status.desktop ?? ''" :fingerprint="status.fingerprint ?? ''" @open="(c, k) => { open = c; canKill = k; }" @unpair="unpaired" />
  </template>
</template>
