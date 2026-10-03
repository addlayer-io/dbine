<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import type { ProjectTarget } from '../api/types';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import EngineIcon from './EngineIcon.vue';

// Connection › database, for a project's base (direct or an environment's).
// With an engine known (the repo's .dbine.json), only that engine's
// connections show, unless "Mostrar todas" is on.

const props = defineProps<{ modelValue: ProjectTarget | null; engine?: string | null }>();
const emit = defineEmits<{ 'update:modelValue': [value: ProjectTarget | null] }>();

const conns = useConnectionsStore();
const showAll = ref(false);
const connectionId = ref(props.modelValue?.connection_id ?? '');
const database = ref(props.modelValue?.database ?? '');
const loading = ref(false);

const connections = computed(() => conns.list
  .filter((c) => showAll.value || !props.engine || c.config.driver === props.engine || c.id === connectionId.value)
  .sort((a, b) => a.name.localeCompare(b.name)));
const driver = computed(() => (connectionId.value ? conns.driverOf(connectionId.value) : undefined));
/** Engines with a single namespace have no database to pick. */
const hasDatabases = computed(() => driver.value?.databases_label !== '');
const databases = computed(() => {
  const live = conns.live[connectionId.value]?.databases ?? [];
  return live.length ? live : database.value ? [database.value] : [];
});

async function loadDatabases() {
  if (!connectionId.value || conns.live[connectionId.value]?.databases?.length) return;
  loading.value = true;
  try { await conns.ensureConnected(connectionId.value); } finally { loading.value = false; }
  if (!hasDatabases.value && !database.value) database.value = conns.live[connectionId.value]?.databases[0] ?? '';
}

watch(connectionId, (id, before) => {
  if (before !== undefined && id !== before) database.value = '';
  void loadDatabases();
}, { immediate: true });

watch([connectionId, database], () => {
  const ok = !!connectionId.value && (!hasDatabases.value || !!database.value);
  emit('update:modelValue', ok ? { connection_id: connectionId.value, database: database.value } : null);
});
</script>

<template>
  <div class="tp">
    <el-select v-model="connectionId" filterable :placeholder="$t('projects:picker.connection')" class="tp-conn">
      <el-option v-for="c in connections" :key="c.id" :value="c.id" :label="c.name">
        <span class="tp-opt"><EngineIcon :id="c.config.driver" :name="c.name" :size="14" />{{ c.name }}</span>
      </el-option>
    </el-select>
    <el-select
      v-if="hasDatabases"
      v-model="database"
      filterable
      allow-create
      :loading="loading"
      :disabled="!connectionId"
      :placeholder="driver?.databases_label ? tb(driver.databases_label) : $t('projects:picker.database')"
      class="tp-db"
      @visible-change="(v: boolean) => v && loadDatabases()"
    >
      <el-option v-for="d in databases" :key="d" :value="d" :label="d" />
    </el-select>
    <el-checkbox v-if="engine" v-model="showAll" class="tp-all">{{ $t('projects:picker.showAll') }}</el-checkbox>
    <p v-if="engine && !connections.length" class="tp-none">{{ $t('projects:picker.noneForEngine', { engine: conns.driver(engine)?.name ?? engine }) }}</p>
  </div>
</template>

<style scoped>
.tp { display: flex; flex-direction: column; gap: 8px; }
.tp-opt { display: inline-flex; align-items: center; gap: 6px; }
.tp-all { margin: 0; }
.tp-none { margin: 0; font-size: 12px; color: var(--nm-text-dim); }
</style>
