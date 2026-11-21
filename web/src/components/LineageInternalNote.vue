<script setup lang="ts">
// Shown where a lineage was followed into one of its internal connections (hidden from the explorer):
// says so and offers "Agregar al explorador", an optional extra (the lineage works without it).
import { computed, ref } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { useConnectionsStore } from '../stores/connections';

const props = defineProps<{ connectionId: string }>();
const { t } = useTranslation();
const conns = useConnectionsStore();
const conn = computed(() => conns.hidden.find((c) => c.id === props.connectionId));
const busy = ref(false);

async function add() {
  busy.value = true;
  try {
    const saved = await conns.promoteHidden(props.connectionId);
    ElMessage.success(t('dependencies:sources.added', { name: saved.name }));
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    busy.value = false;
  }
}
</script>

<template>
  <div v-if="conn" class="lin" data-lineage-internal>
    <span class="lin-text">{{ $t('dependencies:sources.internalNote', { name: conn.name }) }}</span>
    <el-button size="small" :loading="busy" @click="add">{{ $t('dependencies:sources.addToExplorer') }}</el-button>
  </div>
</template>

<style scoped>
.lin { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; margin: 8px 0; font-size: 12px; color: var(--nm-text-dim); }
</style>
