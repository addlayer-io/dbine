<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { accountsApi } from '../api/secretAccounts';
import { errorMessage } from '../api/client';
import type { SecretAccount, SecretAccountKind } from '../api/types';
import { ACCOUNT_KINDS, accountFacts, groupedAccounts, loadSecretAccounts, PROVIDER, secretAccounts } from '../composables/secretAccounts';
import type { WizardKind } from '../composables/accountWizards';
import ZohoConnectWizard from './ZohoConnectWizard.vue';
import AccountConnectWizard from './AccountConnectWizard.vue';

// Settings › Gestores de secretos (docs/secret-sources.md, "Accounts"): this
// computer's accounts of every manager, grouped by manager; connect one
// (each manager's wizard), rename, reconnect (the wizard again, same ID)
// and remove (its keychain items too).

const props = defineProps<{ preview?: { wizard?: WizardKind | 'zoho' } }>();
const { t } = useTranslation();
/** The wizard open: the kind, and the account reconnected (`null`: a new one). */
const wizard = ref<{ kind: SecretAccountKind; account: SecretAccount | null } | null>(props.preview?.wizard ? { kind: props.preview.wizard, account: null } : null);

onMounted(loadSecretAccounts);
const groups = computed(() => groupedAccounts(secretAccounts.value));

function connect(kind: SecretAccountKind) {
  wizard.value = { kind, account: null };
}

/** An account's facts, as text. */
function facts(a: SecretAccount): string {
  return accountFacts(a)
    .map(([k, v]) => (k.startsWith('master.') || k.startsWith('passphrase.') ? t(`secretAccounts:${k}`) : t(`secretAccounts:facts.${k}`, { value: v })))
    .join(' · ');
}

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
    await ElMessageBox.confirm(t('secretAccounts:removeConfirmAny', { label: a.label }), t('secretAccounts:removeTitle'), {
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
      <el-dropdown trigger="click" @command="connect">
        <el-button type="primary" data-connect-menu><el-icon><ei-plus /></el-icon>{{ $t('secretAccounts:connectAny') }}<el-icon class="el-icon--right"><ei-arrow-down /></el-icon></el-button>
        <template #dropdown>
          <el-dropdown-menu>
            <el-dropdown-item v-for="k in ACCOUNT_KINDS" :key="k" :command="k" :data-kind="k">{{ PROVIDER[k] }}</el-dropdown-item>
          </el-dropdown-menu>
        </template>
      </el-dropdown>
    </div>
    <p v-if="!secretAccounts.length" class="sa-muted sa-empty">{{ $t('secretAccounts:none') }}</p>
    <section v-for="g in groups" :key="g.kind" class="sa-group" :data-group="g.kind">
      <h4 class="sa-group-head">
        <span>{{ PROVIDER[g.kind] }}</span>
        <el-button link type="primary" size="small" @click="connect(g.kind)"><el-icon><ei-plus /></el-icon>{{ $t('secretAccounts:connectAnother') }}</el-button>
      </h4>
      <div v-for="a in g.accounts" :key="a.id" class="sa-item" :data-account="a.id">
        <el-icon class="sa-icon"><ei-key /></el-icon>
        <div class="sa-info">
          <strong :title="a.label">{{ a.label }}</strong>
          <span v-if="a.identity && !a.label.includes(a.identity)" class="sa-who" :title="a.identity">{{ a.identity }}</span>
          <span class="sa-facts">{{ facts(a) }}</span>
        </div>
        <div class="sa-buttons">
          <el-button size="small" @click="rename(a)">{{ $t('secretAccounts:rename') }}</el-button>
          <el-button size="small" :title="$t(a.kind === 'zoho' ? 'secretAccounts:reconnectHint' : 'secretAccounts:reconnectHintAny')" @click="wizard = { kind: a.kind, account: a }">{{ $t('secretAccounts:reconnect') }}</el-button>
          <el-button size="small" type="danger" plain @click="remove(a)">{{ $t('secretAccounts:remove') }}</el-button>
        </div>
      </div>
    </section>
    <template v-if="wizard">
      <ZohoConnectWizard v-if="wizard.kind === 'zoho'" :account="wizard.account" @close="wizard = null" />
      <AccountConnectWizard v-else :kind="wizard.kind" :account="wizard.account" @close="wizard = null" />
    </template>
  </div>
</template>

<style scoped>
.sa-muted { font-size: 12.5px; color: var(--nm-text-dim); line-height: 1.45; }
.sa-actions { margin: 4px 0 14px; }
.sa-actions .el-icon { margin-right: 4px; }
.sa-actions .el-icon--right { margin: 0 0 0 6px; }
.sa-empty { margin-top: 6px; }
.sa-group { margin-bottom: 10px; }
.sa-group-head {
  display: flex; align-items: center; justify-content: space-between; margin: 12px 0 2px; font-size: 11.5px; font-weight: 600;
  text-transform: uppercase; letter-spacing: 0.04em; color: var(--nm-text-dim);
}
.sa-group-head .el-icon { margin-right: 2px; }
.sa-item {
  display: flex; align-items: center; gap: 12px; padding: 10px 0; border-top: 1px solid var(--nm-border-soft);
}
.sa-icon { font-size: 18px; color: var(--nm-text-dim); flex: none; }
.sa-info { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 2px; font-size: 12px; color: var(--nm-text-dim); }
.sa-info strong { font-size: 13px; color: var(--nm-text-strong); font-weight: 600; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sa-info span { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
/* The facts wrap: longer languages (de, ru) would cut them off. */
.sa-info .sa-facts { white-space: normal; overflow-wrap: anywhere; line-height: 1.4; }
.sa-who { font-family: var(--nm-font-mono, ui-monospace, monospace); font-size: 11.5px; }
.sa-buttons { display: flex; gap: 6px; flex: none; }
.sa-buttons .el-button + .el-button { margin-left: 0; }
</style>
