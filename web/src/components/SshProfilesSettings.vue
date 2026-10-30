<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { sshProfilesApi } from '../api/sshProfiles';
import type { ListedSshProfile } from '../api/types';
import { loadSshProfiles, profileSummary, sshProfiles } from '../composables/sshProfiles';
import { useConnectionsStore } from '../stores/connections';
import SshProfileDialog from './SshProfileDialog.vue';

// Settings › Perfiles SSH (docs/ssh-tunnels.md, "Profiles"): the SSH side of
// tunnels saved once and used by many connections; create, edit, delete. A
// profile in use is deleted only after the warning names its connections,
// and each of them gets a copy of it as its own settings (none is left
// without its tunnel).

const { t } = useTranslation();
const conns = useConnectionsStore();
/** The dialog open: `null` closed, `'new'`, or the profile edited. */
const editing = ref<null | 'new' | ListedSshProfile>(null);

onMounted(loadSshProfiles);

function escape(s: string): string {
  return s.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]!);
}

async function remove(p: ListedSshProfile) {
  const used = p.used_by.length;
  const list = p.used_by.map((c) => `<li>${escape(c.name)}</li>`).join('');
  const body = used
    ? `<p>${escape(t('tunnel:profiles.removeInUse', { name: p.name, count: used }))}</p><ul style="margin:6px 0 6px 18px">${list}</ul><p>${escape(t('tunnel:profiles.removeInUseAfter'))}</p>`
    : `<p>${escape(t('tunnel:profiles.removeConfirm', { name: p.name }))}</p>`;
  try {
    await ElMessageBox.confirm(body, t('tunnel:profiles.removeTitle'), {
      type: 'warning',
      dangerouslyUseHTMLString: true,
      confirmButtonText: t('tunnel:profiles.remove'),
      cancelButtonText: t('common:cancel'),
      autofocus: false,
    });
  } catch {
    return;
  }
  try {
    await sshProfilesApi.remove(p.id);
    ElMessage.success(t('tunnel:profiles.removed'));
    if (used) await conns.load();
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
  await loadSshProfiles();
}
</script>

<template>
  <div class="sp">
    <h3>{{ $t('tunnel:profiles.title') }}</h3>
    <p class="sp-muted">{{ $t('tunnel:profiles.intro') }}</p>
    <div class="sp-actions">
      <el-button type="primary" data-new-ssh-profile @click="editing = 'new'"><el-icon><ei-plus /></el-icon>{{ $t('tunnel:profiles.new') }}</el-button>
    </div>
    <p v-if="!sshProfiles.length" class="sp-muted">{{ $t('tunnel:profiles.none') }}</p>
    <div v-for="p in sshProfiles" :key="p.id" class="sp-item" :data-ssh-profile="p.id">
      <el-icon class="sp-icon"><ei-connection /></el-icon>
      <div class="sp-info">
        <strong :title="p.name">{{ p.name }}</strong>
        <span :title="profileSummary(p.options)">{{ profileSummary(p.options) }}</span>
        <span class="sp-used">{{ p.used_by.length ? $t('tunnel:profiles.usedByCount', { count: p.used_by.length }) : $t('tunnel:profiles.unused') }}</span>
      </div>
      <div class="sp-buttons">
        <el-button size="small" @click="editing = p">{{ $t('tunnel:profiles.edit') }}</el-button>
        <el-button size="small" type="danger" plain @click="remove(p)">{{ $t('tunnel:profiles.remove') }}</el-button>
      </div>
    </div>
    <SshProfileDialog
      v-if="editing"
      :profile="editing === 'new' ? null : editing"
      @saved="editing = null" @close="editing = null"
    />
  </div>
</template>

<style scoped>
.sp-muted { font-size: 12.5px; color: var(--nm-text-dim); line-height: 1.45; }
.sp-actions { margin: 4px 0 14px; }
.sp-actions .el-icon { margin-right: 4px; }
.sp-item { display: flex; align-items: center; gap: 12px; padding: 10px 0; border-top: 1px solid var(--nm-border-soft); }
.sp-icon { font-size: 18px; color: var(--nm-text-dim); flex: none; }
.sp-info { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 2px; font-size: 12px; color: var(--nm-text-dim); }
.sp-info strong { font-size: 13px; color: var(--nm-text-strong); font-weight: 600; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sp-info span { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-family: var(--nm-font-mono, ui-monospace, monospace); font-size: 11.5px; }
.sp-info .sp-used { font-family: inherit; font-size: 12px; }
.sp-buttons { display: flex; gap: 6px; flex: none; }
.sp-buttons .el-button + .el-button { margin-left: 0; }
</style>
