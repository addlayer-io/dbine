<script setup lang="ts">
import { computed, onMounted, ref, watchEffect } from 'vue';
import { useTranslation } from 'i18next-vue';
import { locale } from '../i18n';
import { setWindowTitle } from '../native';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { useOutputStore } from '../stores/output';
import { useSyncStore } from '../stores/sync';
import { useUiStore } from '../stores/ui';
import { useTabsStore } from '../stores/tabs';

// Status bar (VS Code's blue strip): live connections and problems on the
// left; the active tab's connection, database and server on the right.

defineProps<{ panelOpen: boolean }>();
defineEmits<{ 'toggle-panel': [] }>();

const conns = useConnectionsStore();
const output = useOutputStore();
const tabs = useTabsStore();
const sync = useSyncStore();
const ui = useUiStore();
const { t } = useTranslation();

/** Cloud backup state, when it's on. */
const syncBadge = computed(() => {
  const i = sync.info;
  if (!i?.config.enabled) return null;
  const r = i.status;
  if (r.running) return { icon: 'busy', text: t('workbench:status.syncing'), title: t('workbench:status.syncingTitle') };
  if (r.last_error) return { icon: 'error', text: t('workbench:status.sync'), title: t('workbench:status.syncError', { error: tb(r.last_error) }) };
  if (i.dirty) return { icon: 'dirty', text: '', title: t('workbench:status.syncDirty') };
  const at = i.last_sync_at ? new Date(i.last_sync_at).toLocaleString(locale(), { dateStyle: 'short', timeStyle: 'short' }) : t('workbench:status.never');
  return { icon: 'ok', text: '', title: t('workbench:status.synced', { at }) };
});

const liveCount = computed(() => Object.values(conns.live).filter((l) => l.status === 'connected').length);
const active = computed(() => tabs.active);
const conn = computed(() => (active.value ? conns.byId(active.value.connectionId) : null));
const server = computed(() => (active.value ? conns.live[active.value.connectionId]?.serverVersion ?? '' : ''));
const driver = computed(() => (active.value ? conns.driverOf(active.value.connectionId) : null));

// The window's title says where the active tab is: "DBine — connection —
// database" (a long database name gets lost in the editor's combo).
watchEffect(() => {
  const parts = ['DBine'];
  if (conn.value) parts.push(conn.value.name);
  if (active.value?.database) parts.push(active.value.database);
  setWindowTitle(parts.join(' — '));
});

const version = ref('');
onMounted(async () => {
  try {
    const { getVersion } = await import('@tauri-apps/api/app');
    version.value = await getVersion();
  } catch { /* running outside Tauri */ }
});
</script>

<template>
  <footer class="sb">
    <div class="sb-left">
      <span class="sb-item" :title="$t('workbench:status.openConnections', { count: liveCount })">
        <el-icon><ei-connection /></el-icon>{{ liveCount }}
      </span>
      <button class="sb-item sb-btn" :title="panelOpen ? $t('workbench:status.hideOutput') : $t('workbench:status.showOutput')" @click="$emit('toggle-panel')">
        <el-icon><ei-circle-close /></el-icon>{{ output.errors }}
        <el-icon style="margin-left: 6px;"><ei-warning /></el-icon>{{ output.warnings }}
      </button>
    </div>
    <div class="sb-right">
      <button v-if="syncBadge" class="sb-item sb-btn" :class="{ 'sb-sync-error': syncBadge.icon === 'error' }" :title="syncBadge.title" @click="ui.openSettings('sync')">
        <el-icon v-if="syncBadge.icon === 'busy'" class="is-loading"><ei-refresh /></el-icon>
        <el-icon v-else-if="syncBadge.icon === 'error'"><ei-warning-filled /></el-icon>
        <el-icon v-else-if="syncBadge.icon === 'dirty'"><ei-upload /></el-icon>
        <el-icon v-else><ei-upload-filled /></el-icon>
        <template v-if="syncBadge.text">{{ syncBadge.text }}</template>
      </button>
      <template v-if="conn">
        <span class="sb-item">{{ driver?.name }}</span>
        <span class="sb-item">{{ conn.name }}{{ active?.database ? ` · ${active.database}` : '' }}</span>
        <span v-if="conn.config.read_only" class="sb-item sb-ro">{{ $t('workbench:status.readOnly') }}</span>
        <span v-if="server" class="sb-item sb-server" :title="server">{{ server }}</span>
      </template>
      <span class="sb-item">DBine{{ version ? ` ${version}` : '' }}</span>
    </div>
  </footer>
</template>

<style scoped>
.sb {
  display: flex; justify-content: space-between; align-items: center; height: 22px; padding: 0 4px;
  background: var(--ide-status); color: #ffffff; font-size: 12px; overflow: hidden; white-space: nowrap;
}
.sb-left, .sb-right { display: flex; align-items: center; height: 100%; min-width: 0; }
.sb-item { display: inline-flex; align-items: center; gap: 4px; height: 100%; padding: 0 7px; font-variant-numeric: tabular-nums; }
.sb-server { max-width: 360px; overflow: hidden; text-overflow: ellipsis; display: inline-block; line-height: 22px; }
.sb-ro { background: #c27d0e; }
.sb-btn { font: inherit; color: inherit; background: transparent; border: none; cursor: pointer; }
.sb-btn:hover { background: rgba(255, 255, 255, 0.12); }
.sb-sync-error { background: rgba(255, 90, 90, 0.35); }
</style>
