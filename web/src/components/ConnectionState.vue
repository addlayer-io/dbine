<script setup lang="ts">
import { computed, ref } from 'vue';
import { useTranslation } from 'i18next-vue';
import { useConnectionsStore } from '../stores/connections';
import ConnectionProblemDialog from './ConnectionProblemDialog.vue';

// What a tab shows instead of its content while its connection isn't there:
// "Conectando…" while it connects, then — if it couldn't — the error (what
// kind of failure, the server's message) with "Ver qué pasa…" and
// "Reintentar". Used with composables/connectionGate.ts.

const props = defineProps<{ connectionId: string }>();
const emit = defineEmits<{ retry: [] }>();
const { t } = useTranslation();
const conns = useConnectionsStore();

const live = computed(() => conns.live[props.connectionId]);
const connecting = computed(() => live.value?.status === 'connecting');
const title = computed(() => {
  const kind = live.value?.detail?.kind;
  return t(`connection:problem.kind.${kind && kind !== 'unknown' ? kind : 'unknown'}.title`);
});
const message = computed(() => live.value?.error ?? t('common:error'));
const explaining = ref(false);
</script>

<template>
  <div v-if="connecting" class="cs cs-connecting" role="status">
    <el-icon class="is-loading"><ei-loading /></el-icon>
    <span>{{ conns.downloadNote(conns.driverOf(connectionId)?.id ?? '') ?? $t('explorer:tree.connecting') }}</span>
  </div>
  <div v-else class="cs cs-error" role="alert">
    <el-icon class="cs-ic"><ei-warning-filled /></el-icon>
    <div class="cs-body">
      <div class="cs-title">{{ title }}</div>
      <div class="cs-msg nm-selectable">{{ message }}</div>
      <div class="cs-actions">
        <el-button size="small" @click="explaining = true">{{ $t('explorer:tree.whatsWrong') }}</el-button>
        <el-button size="small" type="primary" @click="emit('retry')">{{ $t('common:retry') }}</el-button>
      </div>
    </div>
    <ConnectionProblemDialog v-if="explaining" :connection-id="connectionId" @close="explaining = false" @retry="explaining = false; emit('retry')" />
  </div>
</template>

<style scoped>
.cs { display: flex; gap: 10px; padding: 16px 20px; font-size: 13px; }
.cs-connecting { align-items: center; color: var(--nm-text-dim); }
.cs-ic { color: var(--nm-danger); font-size: 18px; flex: none; margin-top: 1px; }
.cs-body { min-width: 0; display: flex; flex-direction: column; gap: 6px; }
.cs-title { font-weight: 600; color: var(--nm-text-strong); }
.cs-msg { white-space: pre-wrap; word-break: break-word; color: var(--nm-text); max-height: 160px; overflow: auto; }
.cs-actions { display: flex; gap: 8px; }
</style>
