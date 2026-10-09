<script lang="ts">
import { ref as moduleRef } from 'vue';

// Kept outside the component: Configuración is destroy-on-close and a
// download (a task) outlives it. Reopening shows it still going.
const busy = moduleRef<Record<string, 'install' | 'remove' | 'rollback'>>({});
const all = moduleRef(false);
</script>

<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { driversApi, type DriverPackage } from '../api/drivers';
import { useConnectionsStore } from '../stores/connections';
import { locale } from '../i18n';
import { runTask, type TaskHandle } from '../stores/tasks';

// Configuración → Drivers: the downloadable drivers (docs/on-demand-drivers.md).
// Each one downloads by itself on the first connection; here they can be
// downloaded ahead (a machine that will go offline) or removed. A download
// is a task (stores/tasks.ts): it shows in Tareas with its progress, goes on
// if Configuración closes, and then notifies when it ends. The download has
// no cancel path, so the task offers no Cancelar.
//
// Drivers have versions of their own: a downloaded driver updates by itself
// in the background (the next connection uses the new version) and each row
// says how that goes. "Volver a la anterior" drops the version in use.

const props = defineProps<{ packages: DriverPackage[] }>();
const emit = defineEmits<{ changed: [] }>();
const conns = useConnectionsStore();
const { t } = useTranslation();

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

/** The download progress the explorer shows, keyed by the component's name. */
const downloadOf = (p: DriverPackage) => conns.downloads[`el driver de ${p.label}`];
/** "45 %" while this driver downloads. */
function progress(p: DriverPackage): string | null {
  const d = downloadOf(p);
  return d && d.total ? `${Math.floor((d.done / d.total) * 100)} %` : null;
}

/** The row's version note, if there's anything to say. */
function statusText(p: DriverPackage): string | null {
  const s = p.status;
  switch (s.kind) {
    case 'downloading':
      return t('dialogs:drivers.statusDownloading', { available: p.available });
    case 'ready_next_connection':
      return t('dialogs:drivers.statusReady', { version: p.version });
    case 'needs_app':
      return t('dialogs:drivers.statusNeedsApp', { minApp: s.min_app });
    case 'rolled_back':
      return s.reason === 'user'
        ? t('dialogs:drivers.statusRolledBackUser', { version: p.version, from: s.from })
        : t('dialogs:drivers.statusRolledBack', { version: p.version, from: s.from });
    case 'restart_for_new_options':
      return t('dialogs:drivers.statusRestart');
    default:
      return null;
  }
}

const checking = ref(false);
async function checkUpdates() {
  checking.value = true;
  try {
    await driversApi.checkUpdates();
    emit('changed');
    const now = await driversApi.packages();
    if (!now.packages.some((p) => p.installed != null && p.available !== p.version)) {
      ElMessage.success(t('dialogs:drivers.upToDateAll'));
    }
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    checking.value = false;
  }
}

async function rollback(p: DriverPackage) {
  try {
    await ElMessageBox.confirm(
      t('dialogs:drivers.rollbackConfirm', { label: p.label, version: p.version, previous: p.previous }),
      t('dialogs:drivers.rollback'),
      { confirmButtonText: t('dialogs:drivers.rollback'), cancelButtonText: t('common:cancel'), type: 'warning' },
    );
  } catch {
    return;
  }
  busy.value[p.package] = 'rollback';
  try {
    await driversApi.rollback(p.package);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    delete busy.value[p.package];
    emit('changed');
  }
}

let alive = true;
let unlisten: UnlistenFn | null = null;
const live = new Set<TaskHandle>();
onBeforeUnmount(() => {
  alive = false;
  unlisten?.();
  // Configuración closed mid-download: it goes on and notifies at the end.
  live.forEach((x) => x.background());
});

async function install(p: DriverPackage) {
  busy.value[p.package] = 'install';
  const { task, promise } = runTask<void>({
    kind: 'driver-install',
    title: t('tasks:settingsGit.drivers.install', { label: p.label }),
    // "Descargar todos" keeps going after Configuración closes: the next
    // downloads start in the background so they notify too.
    background: !alive,
    run: async (task) => {
      // Mirror the download's bytes into the task while it runs (the store
      // subscription is detached: it outlives this component).
      const stop = conns.$subscribe(() => {
        const d = downloadOf(p);
        if (d) task.progress({ done: d.done, total: d.total, unit: 'bytes' });
      }, { detached: true });
      try {
        await driversApi.install(p.package);
      } finally {
        stop();
      }
    },
  });
  live.add(task);
  try {
    await promise;
  } catch (e) {
    if (alive) ElMessage.error(errorMessage(e));
  } finally {
    live.delete(task);
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

onMounted(async () => {
  conns.listenDownloads();
  // Background updates, rollbacks: the list is read again.
  const off = await listen('drivers-changed', () => emit('changed'));
  if (alive) unlisten = off;
  else off();
});
</script>

<template>
  <div class="dv">
    <p class="st-muted">
      {{ $t('dialogs:drivers.intro') }}
    </p>
    <div class="dv-bar">
      <el-input v-model="filter" :placeholder="$t('dialogs:drivers.search')" clearable size="small" style="width: 220px" />
      <span class="dv-sum" :title="$t('dialogs:drivers.diskTitle')">{{ $t('dialogs:drivers.summary', { installed: installed.length, total: packages.length, size: mb(onDisk) }) }}</span>
      <el-button size="small" :loading="checking" @click="checkUpdates">
        {{ checking ? $t('dialogs:drivers.checking') : $t('dialogs:drivers.checkUpdates') }}
      </el-button>
      <el-button size="small" :disabled="!missing.length" :loading="all" @click="installAll">
        {{ $t('dialogs:drivers.downloadAll', { size: mb(toDownload) }) }}
      </el-button>
    </div>
    <div class="dv-list">
      <div v-for="p in shown" :key="p.package" class="dv-row">
        <div class="dv-name">
          <strong>{{ p.label }} <span class="dv-ver">{{ p.version }}</span></strong>
          <span v-if="p.drivers.length > 1" :title="p.drivers.join(', ')">{{ p.drivers.join(', ') }}</span>
          <span v-if="statusText(p)" class="dv-status" :class="'dv-' + p.status.kind" :title="statusText(p) ?? ''">{{ statusText(p) }}</span>
        </div>
        <el-button
          v-if="p.installed != null && p.previous"
          size="small"
          text
          class="dv-back"
          :title="$t('dialogs:drivers.rollbackTitle', { version: p.version, previous: p.previous })"
          :loading="busy[p.package] === 'rollback'"
          @click="rollback(p)"
        >{{ $t('dialogs:drivers.rollback') }}</el-button>
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
.dv-row :deep(.el-button.dv-back) { width: auto; }
.dv-name .dv-status { color: var(--nm-text-dim); }
.dv-name .dv-needs_app, .dv-name .dv-rolled_back, .dv-name .dv-restart_for_new_options { color: var(--nm-warning); }
</style>
