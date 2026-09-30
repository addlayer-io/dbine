<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { ElMessage } from 'element-plus';
import { errorMessage } from '../api/client';
import { driversApi, type DriverPackage } from '../api/drivers';
import { useConnectionsStore } from '../stores/connections';
import { locale } from '../i18n';

// Configuración → Drivers: the downloadable drivers (docs/drivers-bajo-demanda.md).
// Each one downloads by itself on the first connection; here they can be
// downloaded ahead (a machine that will go offline) or removed.

const props = defineProps<{ packages: DriverPackage[] }>();
const emit = defineEmits<{ changed: [] }>();
const conns = useConnectionsStore();

const busy = ref<Record<string, 'install' | 'remove'>>({});
const all = ref(false);
const filter = ref('');

const mb = (n: number) => (n / 1e6).toLocaleString(locale(), { maximumFractionDigits: 1 });
const shown = computed(() => {
  const q = filter.value.trim().toLowerCase();
  return props.packages
    .filter((p) => !q || p.label.toLowerCase().includes(q) || p.drivers.some((d) => d.toLowerCase().includes(q)))
    .sort((a, b) => a.label.localeCompare(b.label, locale()));
});
const installed = computed(() => props.packages.filter((p) => p.installed != null));
const missing = computed(() => props.packages.filter((p) => p.installed == null));
const onDisk = computed(() => installed.value.reduce((n, p) => n + (p.installed ?? 0), 0));
const toDownload = computed(() => missing.value.reduce((n, p) => n + p.size, 0));

/** "45 %" while this driver downloads (the same progress the explorer shows). */
function progress(p: DriverPackage): string | null {
  const d = conns.downloads[`el driver de ${p.label}`];
  return d && d.total ? `${Math.floor((d.done / d.total) * 100)} %` : null;
}

async function install(p: DriverPackage) {
  busy.value[p.package] = 'install';
  try {
    await driversApi.install(p.package);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    delete busy.value[p.package];
    emit('changed');
  }
}

async function remove(p: DriverPackage) {
  busy.value[p.package] = 'remove';
  try {
    await driversApi.remove(p.package);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    delete busy.value[p.package];
    emit('changed');
  }
}

async function installAll() {
  all.value = true;
  try {
    // One at a time: the progress stays readable and a slow link isn't split.
    for (const p of missing.value) await install(p);
  } finally {
    all.value = false;
  }
}

onMounted(() => conns.listenDownloads());
</script>

<template>
  <div class="dv">
    <p class="st-muted">
      {{ $t('dialogs:drivers.intro') }}
    </p>
    <div class="dv-bar">
      <el-input v-model="filter" :placeholder="$t('dialogs:drivers.search')" clearable size="small" style="width: 220px" />
      <span class="dv-sum" :title="$t('dialogs:drivers.diskTitle')">{{ $t('dialogs:drivers.summary', { installed: installed.length, total: packages.length, size: mb(onDisk) }) }}</span>
      <el-button size="small" :disabled="!missing.length" :loading="all" @click="installAll">
        {{ $t('dialogs:drivers.downloadAll', { size: mb(toDownload) }) }}
      </el-button>
    </div>
    <div class="dv-list">
      <div v-for="p in shown" :key="p.package" class="dv-row">
        <div class="dv-name">
          <strong>{{ p.label }} <span class="dv-ver">{{ p.version }}</span></strong>
          <span v-if="p.drivers.length > 1" :title="p.drivers.join(', ')">{{ p.drivers.join(', ') }}</span>
        </div>
        <span class="dv-size">
          <template v-if="p.installed != null"><el-icon class="dv-ok"><ei-circle-check /></el-icon>{{ mb(p.installed) }} MB</template>
          <template v-else>{{ mb(p.size) }} MB</template>
        </span>
        <el-button v-if="p.installed == null" size="small" :loading="busy[p.package] === 'install'" :disabled="all && !busy[p.package]" @click="install(p)">
          {{ busy[p.package] === 'install' ? (progress(p) ?? $t('dialogs:drivers.downloading')) : $t('dialogs:drivers.download') }}
        </el-button>
        <el-button v-else size="small" text :loading="busy[p.package] === 'remove'" @click="remove(p)">{{ $t('dialogs:drivers.remove') }}</el-button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.dv { min-width: 0; }
.dv-bar { display: flex; align-items: center; gap: 12px; margin: 8px 0 6px; }
.dv-sum { flex: 1; min-width: 0; font-size: 12px; color: var(--nm-text-dim); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.dv-list { border-top: 1px solid var(--nm-border); }
.dv-row { display: flex; align-items: center; gap: 12px; padding: 7px 0; border-bottom: 1px solid var(--nm-border); }
.dv-name { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 2px; }
.dv-name strong { font-size: 13px; font-weight: 500; color: var(--nm-text-strong); }
.dv-name span { font-size: 11.5px; color: var(--nm-text-dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.dv-size { display: flex; align-items: center; gap: 4px; font-size: 12px; color: var(--nm-text-dim); width: 90px; justify-content: flex-end; }
.dv-ok { color: var(--nm-success); }
.dv-name .dv-ver { font-size: 11px; font-weight: 400; color: var(--nm-text-dim); margin-left: 4px; }
.dv-row :deep(.el-button) { width: 104px; }
</style>
