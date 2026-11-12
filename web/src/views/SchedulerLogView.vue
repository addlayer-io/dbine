<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { errorMessage } from '../api/client';
import { schedulerApi } from '../api/scheduler';
import type { MonitorTable } from '../api/types';
import ConnectionState from '../components/ConnectionState.vue';
import EngineIcon from '../components/EngineIcon.vue';
import { useConnectionGate } from '../composables/connectionGate';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import type { SchedulerLogTab } from '../stores/tabs';

// The server's scheduler log ("Registro del planificador": SQL Server
// Agent's error logs, the current one and its archives), one table per log,
// read when the tab is shown and on "Actualizar". Titles and columns come in
// Spanish from the driver (`tb`); what the server wrote stays as it came.

const props = defineProps<{ tab: SchedulerLogTab; active: boolean }>();
const conns = useConnectionsStore();
const { blocked: connBlocked, ensure: ensureConnection } = useConnectionGate(() => props.tab.connectionId);

const tables = ref<MonitorTable[]>([]);
const read = ref(false);
const loading = ref(false);
const error = ref<string | null>(null);
const key = ref('');
const filter = ref('');

const conn = computed(() => conns.byId(props.tab.connectionId));
const driver = computed(() => conns.driverOf(props.tab.connectionId));
const table = computed(() => tables.value.find((x) => x.key === key.value));

async function load() {
  if (loading.value) return;
  loading.value = true;
  try {
    if (!(await ensureConnection({ auto: read.value }))) return;
    tables.value = await schedulerApi.logs(props.tab.connectionId);
    read.value = true;
    error.value = null;
    if (!tables.value.some((x) => x.key === key.value)) key.value = tables.value[0]?.key ?? '';
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    loading.value = false;
  }
}
watch(() => props.active, (shown) => { if (shown && !read.value) void load(); }, { immediate: true });

function cell(v: unknown): string {
  if (v === null || v === undefined) return '';
  if (typeof v === 'number') return Number.isInteger(v) ? String(v) : v.toLocaleString(locale(), { maximumFractionDigits: 3 });
  if (typeof v === 'object') return JSON.stringify(v);
  return String(v);
}
const rows = computed(() => {
  const all = (table.value?.rows ?? []).map((r) => r.map(cell));
  const q = filter.value.trim().toLowerCase();
  return q ? all.filter((r) => r.some((c) => c.toLowerCase().includes(q))) : all;
});
</script>

<template>
  <div class="slv">
    <header class="slv-head">
      <EngineIcon v-if="driver" :id="driver.id" :name="driver.name" :size="20" />
      <div class="slv-title">
        <strong>{{ $t('scheduled:log.tab') }}</strong>
        <span class="nm-muted">{{ conn?.name }}</span>
      </div>
      <div class="slv-sp" />
      <el-input v-if="tables.length" v-model="filter" size="small" :placeholder="$t('monitor:filter')" clearable style="width: 220px">
        <template #prefix><el-icon><ei-search /></el-icon></template>
      </el-input>
      <el-button size="small" :loading="loading" :aria-label="$t('common:refresh')" :title="$t('common:refresh')" @click="load()">
        <el-icon><ei-refresh /></el-icon>
      </el-button>
    </header>
    <ConnectionState v-if="connBlocked" :connection-id="tab.connectionId" @retry="load" />
    <el-alert v-if="error" type="error" :title="error" :closable="false" show-icon class="slv-alert" />
    <div v-if="loading && !read && !error" class="slv-empty"><el-icon class="is-loading" :size="22"><ei-loading /></el-icon></div>
    <div v-else-if="read && !tables.length" class="slv-empty nm-muted">{{ $t('scheduled:log.empty') }}</div>

    <div v-if="tables.length" class="slv-body">
      <div class="slv-tabs" role="tablist">
        <button
          v-for="x in tables"
          :key="x.key"
          role="tab"
          :aria-selected="x.key === key"
          class="slv-tab"
          :class="{ active: x.key === key }"
          @click="key = x.key"
        >{{ tb(x.title) }} <span class="slv-count">{{ x.rows.length }}</span></button>
      </div>
      <div v-if="table" class="slv-wrap">
        <table class="slv-table">
          <thead>
            <tr><th v-for="c in table.columns" :key="c">{{ tb(c) }}</th></tr>
          </thead>
          <tbody>
            <tr v-for="(r, i) in rows" :key="i">
              <td v-for="(c, j) in r" :key="j" :title="c.length > 60 ? c : undefined">{{ c }}</td>
            </tr>
            <tr v-if="!rows.length">
              <td :colspan="table.columns.length" class="nm-muted">{{ filter ? $t('monitor:noMatch') : $t('monitor:noRows') }}</td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>
  </div>
</template>

<style scoped>
.slv { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--nm-bg); }
.slv-head { display: flex; align-items: center; gap: 10px; padding: 10px 16px; border-bottom: 1px solid var(--nm-border-soft); }
.slv-head .el-button { margin: 0; }
.slv-title { display: flex; flex-direction: column; min-width: 0; }
.slv-title strong { color: var(--nm-text-strong); }
.slv-title span { font-size: 11.5px; }
.slv-sp { flex: 1; }
.slv-alert { margin: 10px 16px 0; width: auto; }
.slv-empty { display: flex; justify-content: center; padding: 40px; color: var(--nm-text-dim); font-size: 12.5px; }
.slv-body { flex: 1; min-height: 0; display: flex; flex-direction: column; padding: 8px 16px 12px; }
.slv-tabs { display: flex; gap: 2px; flex-wrap: wrap; border-bottom: 1px solid var(--nm-border-soft); }
.slv-tab { padding: 5px 10px; border: 0; border-bottom: 2px solid transparent; background: none; font: inherit; font-size: 12.5px; color: var(--nm-text-dim); cursor: pointer; }
.slv-tab:hover { color: var(--nm-text-strong); }
.slv-tab.active { color: var(--nm-text-strong); border-bottom-color: var(--nm-link); }
.slv-count { font-size: 10.5px; padding: 0 5px; border-radius: 8px; background: var(--ide-hover); color: var(--nm-text-dim); }
.slv-wrap { flex: 1; min-height: 0; overflow: auto; }
.slv-table { width: 100%; border-collapse: collapse; font-size: 12px; }
.slv-table th { position: sticky; top: 0; text-align: left; font-weight: 600; padding: 5px 10px; color: var(--nm-text-dim); background: var(--nm-bg); border-bottom: 1px solid var(--nm-border); white-space: nowrap; }
.slv-table td { padding: 4px 10px; border-bottom: 1px solid var(--nm-border-soft); color: var(--nm-text); max-width: 640px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-variant-numeric: tabular-nums; }
</style>
