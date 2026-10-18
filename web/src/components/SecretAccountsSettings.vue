<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { accountsApi } from '../api/secretAccounts';
import { errorMessage } from '../api/client';
import type { SecretAccount } from '../api/types';
import { loadSecretAccounts, secretAccounts, zohoDomain } from '../composables/secretAccounts';
import ZohoConnectWizard from './ZohoConnectWizard.vue';

// Settings › Cuentas de gestores de secretos (docs/secret-sources.md,
// "Accounts"): this computer's accounts; rename, reconnect (the wizard
// again, same ID) and remove (its keychain items too).

const { t } = useTranslation();
/** The wizard: `null` closed, `'new'` a new account, else the account reconnected. */
const wizard = ref<SecretAccount | 'new' | null>(null);

onMounted(loadSecretAccounts);

async function rename(a: SecretAccount) {
  try {
    const { value } = await ElMessageBox.prompt(t('secretAccounts:renamePrompt'), t('secretAccounts:renameTitle'), {
      inputValue: a.label,
      inputValidator: (v: string) => !!v.trim() && v.trim().length <= 200,
      confirmButtonText: t('common:save'),
      cancelButtonText: t('common:cancel'),
    });
    await accountsApi.rename(a.id, value.trim());
    await loadSecretAccounts();
  } catch (e) {
    if (e !== 'cancel' && e !== 'close') ElMessage.error(errorMessage(e));
  }
}

async function remove(a: SecretAccount) {
  try {
    await ElMessageBox.confirm(t('secretAccounts:removeConfirm', { label: a.label }), t('secretAccounts:removeTitle'), {
      type: 'warning',
      confirmButtonText: t('secretAccounts:remove'),
      cancelButtonText: t('common:cancel'),
      autofocus: false,
    });
  } catch {
    return;
  }
  try {
    await accountsApi.remove(a.id);
    ElMessage.success(t('secretAccounts:removed'));
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
  await loadSecretAccounts();
}
</script>

<template>
  <div class="sa">
    <h3>{{ $t('secretAccounts:title') }}</h3>
    <p class="sa-muted">{{ $t('secretAccounts:intro') }}</p>
    <div class="sa-actions">
      <el-button type="primary" @click="wizard = 'new'"><el-icon><ei-plus /></el-icon>{{ $t('secretAccounts:connectZoho') }}</el-button>
    </div>
    <p v-if="!secretAccounts.length" class="sa-muted sa-empty">{{ $t('secretAccounts:none') }}</p>
    <div v-for="a in secretAccounts" :key="a.id" class="sa-item" :data-account="a.id">
      <el-icon class="sa-icon"><ei-key /></el-icon>
      <div class="sa-info">
        <strong>{{ a.label }}</strong>
        <span>{{ $t(`secretAccounts:kind.${a.kind}`) }}<template v-if="a.identity"> · {{ a.identity }}</template></span>
        <span>{{ $t('secretAccounts:dataCenter', { domain: zohoDomain(a.data_center) }) }} · {{ $t(`secretAccounts:passphrase.${a.master ?? 'keychain'}`) }}</span>
      </div>
      <div class="sa-buttons">
        <el-button size="small" @click="rename(a)">{{ $t('secretAccounts:rename') }}</el-button>
        <el-button size="small" :title="$t('secretAccounts:reconnectHint')" @click="wizard = a">{{ $t('secretAccounts:reconnect') }}</el-button>
        <el-button size="small" type="danger" plain @click="remove(a)">{{ $t('secretAccounts:remove') }}</el-button>
      </div>
    </div>
    <ZohoConnectWizard v-if="wizard" :account="wizard === 'new' ? null : wizard" @close="wizard = null" />
  </div>
</template>

<style scoped>
.sa-muted { font-size: 12.5px; color: var(--nm-text-dim); line-height: 1.45; }
.sa-actions { margin: 4px 0 14px; }
.sa-actions .el-icon { margin-right: 4px; }
.sa-empty { margin-top: 6px; }
.sa-item {
  display: flex; align-items: center; gap: 12px; padding: 10px 0; border-top: 1px solid var(--nm-border-soft);
}
.sa-icon { font-size: 18px; color: var(--nm-text-dim); flex: none; }
.sa-info { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 2px; font-size: 12px; color: var(--nm-text-dim); }
.sa-info strong { font-size: 13px; color: var(--nm-text-strong); font-weight: 600; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sa-info span { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sa-buttons { display: flex; gap: 6px; flex: none; }
.sa-buttons .el-button + .el-button { margin-left: 0; }
</style>
