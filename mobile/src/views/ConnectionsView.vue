<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { useTranslation } from 'i18next-vue';
import { api, errorText, type ConnectionItem } from '../api';

// The computer's connections: name and engine only.

defineProps<{ desktop: string; fingerprint: string }>();
const emit = defineEmits<{ open: [conn: ConnectionItem, canKill: boolean]; unpair: [] }>();
const { t } = useTranslation();
const list = ref<ConnectionItem[] | null>(null);
const canKill = ref(false);
const error = ref('');
const loading = ref(false);
const filter = ref('');
const via = ref<'local' | 'internet' | null>(null);

async function load() {
  loading.value = true;
  error.value = '';
  try {
    const hello = await api.hello();
    canKill.value = hello.can_kill;
    list.value = await api.connections();
    via.value = (await api.status()).via;
  } catch (e) {
    error.value = errorText(e);
  } finally {
    loading.value = false;
  }
}

function confirmUnpair() {
  if (confirm(t('connections.unpairConfirm'))) emit('unpair');
}

onMounted(load);
</script>

<template>
  <div>
    <div class="bar">
      <h1>{{ desktop }}</h1>
      <span v-if="via" class="via" :class="via">{{ $t(`connections.via.${via}`) }}</span>
      <button :disabled="loading" @click="load">↻</button>
    </div>
    <div class="page">
      <p v-if="error" class="error">{{ error }}</p>
      <input v-if="list && list.length > 6" v-model="filter" :placeholder="$t('connections.filter')" />
      <p v-if="list && !list.length" class="muted">{{ $t('connections.none') }}</p>
      <button
        v-for="c in (list ?? []).filter((x) => !filter || x.name.toLowerCase().includes(filter.toLowerCase()))"
        :key="c.id"
        class="conn"
        @click="emit('open', c, canKill)"
      >
        <span class="dot" :style="{ background: c.color || 'var(--text-dim)' }" />
        <span class="name">{{ c.name }}</span>
        <span class="muted">{{ c.engine }}</span>
      </button>
      <p v-if="loading && !list" class="muted">{{ $t('connections.loading') }}</p>
      <div class="foot muted">
        <p>{{ $t('connections.thisPhone', { code: fingerprint }) }}</p>
        <button @click="confirmUnpair">{{ $t('connections.unpair') }}</button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.conn { display: flex; align-items: center; gap: 10px; text-align: left; padding: 14px; border: 1px solid var(--border); }
.conn .name { flex: 1; color: var(--text-strong); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.dot { width: 10px; height: 10px; border-radius: 50%; flex: none; }
.via { font-size: 12px; padding: 3px 8px; border-radius: 10px; background: var(--bg-elev); color: var(--text-dim); white-space: nowrap; }
.via.internet { color: var(--accent); }
.foot { margin-top: 24px; display: flex; flex-direction: column; gap: 8px; }
</style>
