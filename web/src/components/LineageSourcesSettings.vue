<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import type { SavedConnection } from '../api/types';
import { internalWhere } from '../composables/lineageSource';
import { useConnectionsStore } from '../stores/connections';
import EngineIcon from './EngineIcon.vue';

// Settings › Fuentes del linaje: the connections the lineage made by itself to follow a source into
// another database (a Power BI table that reads SQL Server). They are not in the explorer: here each one
// can be forgotten (the connection and its remembered password) or added to the explorer as a regular
// connection (still read-only).

const { t } = useTranslation();
const conns = useConnectionsStore();
const busy = ref('');

onMounted(() => void conns.loadHidden().catch((e) => ElMessage.error(errorMessage(e))));

async function forget(c: SavedConnection) {
  try {
    await ElMessageBox.confirm(t('dependencies:sources.forgetConfirm', { name: c.name }), t('dependencies:sources.forget'), {
      type: 'warning', confirmButtonText: t('dependencies:sources.forget'), cancelButtonText: t('common:cancel'), autofocus: false,
    });
  } catch {
    return;
  }
  busy.value = c.id;
  try {
    await conns.forgetHidden(c.id);
    ElMessage.success(t('dependencies:sources.forgotten'));
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    busy.value = '';
  }
}

async function addToExplorer(c: SavedConnection) {
  busy.value = c.id;
  try {
    await conns.promoteHidden(c.id);
    ElMessage.success(t('dependencies:sources.added', { name: c.name }));
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    busy.value = '';
  }
}
</script>

<template>
  <div class="ls-set">
    <h3>{{ $t('dependencies:sources.title') }}</h3>
    <p class="ls-muted">{{ $t('dependencies:sources.intro') }}</p>
    <p v-if="!conns.hidden.length" class="ls-muted">{{ $t('dependencies:sources.none') }}</p>
    <div v-for="c in conns.hidden" :key="c.id" class="ls-item" :data-lineage-source="c.id">
      <EngineIcon :id="c.config.driver" :name="c.config.driver" :size="18" />
      <div class="ls-info">
        <strong :title="c.name">{{ c.name }}</strong>
        <span :title="internalWhere(c)">{{ internalWhere(c) }}</span>
        <span class="ls-pass">{{ c.save_password ? $t('dependencies:sources.remembered') : $t('dependencies:sources.notRemembered') }}</span>
      </div>
      <div class="ls-buttons">
        <el-button size="small" :disabled="busy === c.id" @click="addToExplorer(c)">{{ $t('dependencies:sources.addToExplorer') }}</el-button>
        <el-button size="small" type="danger" plain :disabled="busy === c.id" @click="forget(c)">{{ $t('dependencies:sources.forget') }}</el-button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.ls-muted { font-size: 12.5px; color: var(--nm-text-dim); line-height: 1.45; }
.ls-item { display: flex; align-items: center; gap: 12px; padding: 10px 0; border-top: 1px solid var(--nm-border-soft); }
.ls-info { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 2px; font-size: 12px; color: var(--nm-text-dim); }
.ls-info strong { font-size: 13px; color: var(--nm-text-strong); font-weight: 600; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.ls-info span { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-family: var(--nm-font-mono, ui-monospace, monospace); font-size: 11.5px; }
.ls-info .ls-pass { font-family: inherit; font-size: 12px; }
.ls-buttons { display: flex; gap: 6px; flex: none; }
.ls-buttons .el-button + .el-button { margin-left: 0; }
</style>
