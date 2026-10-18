<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { open as openFile } from '@tauri-apps/plugin-dialog';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import { askApproveSecretSource } from '../composables/secretSourceApproval';
import { askMasterPassword, unlockOf } from '../composables/secretSourceUnlock';
import {
  isExternal, MASTER_SOURCES, missingInputs, SOURCE_KINDS, sourceFrom, typedTokens, VAULT_AUTHS, ZOHO_DATA_CENTERS, type DraftKind, type SourceDraft,
} from '../composables/secretSources';

// Where one secret field takes its value (docs/secret-sources.md): the
// selector, the chosen provider's inputs and "Probar", which resolves it and
// shows only how many characters came back. With "saved" (or "ask") the
// field's own input, which the parent renders, is used as before.

const draft = defineModel<SourceDraft>({ required: true });
const props = defineProps<{
  /** The field's key (`password`, `api_key`, `ssh.password`…). */
  field: string;
  /** Offer "ask every time" (the database password). */
  allowAsk: boolean;
  /** The connection being edited, if it's saved. */
  connectionId: string | null;
  /** The form's engine (for the field's name in errors). */
  driver: string;
  /** The connection is saved: a token typed before is kept for this field. */
  editing: boolean;
}>();
const { t } = useTranslation();

const kinds = computed(() => SOURCE_KINDS.filter((k) => k !== 'ask' || props.allowAsk));
const testing = ref(false);
const result = ref<{ ok: boolean; text: string } | null>(null);
watch(draft, () => { result.value = null; }, { deep: true });

const external = computed(() => isExternal(draft.value));
const missing = computed(() => missingInputs(draft.value));

function setKind(k: DraftKind) {
  draft.value.kind = k;
  // Zoho Vault always needs its passphrase, Delinea its password.
  if ((k === 'zoho' || k === 'delinea') && draft.value.master === 'none') draft.value.master = 'keychain';
}

async function browse(input: 'ca_file' | 'file' | 'key_file' | 'client_cert' | 'client_key') {
  try {
    const filters = input === 'file' ? [{ name: 'KeePass', extensions: ['kdbx'] }] : undefined;
    const p = await openFile({ multiple: false, directory: false, filters });
    if (typeof p === 'string') draft.value[input] = p;
  } catch { /* no dialog outside Tauri */ }
}

/** KeePass / Zoho Vault: where the master password comes from. */
const masters = computed(() => MASTER_SOURCES.filter((m) => m !== 'none' || draft.value.kind === 'keepass'));
const hasMaster = computed(() => ['keepass', 'zoho', 'delinea'].includes(draft.value.kind));
/** HashiCorp Vault (KV or database credentials): its sign-in. */
const isVault = computed(() => draft.value.kind === 'vault' || draft.value.kind === 'vault_db');
/** A master password asked while testing ("ask"): this picker's tests only. */
const asked = ref('');

async function test() {
  const source = sourceFrom(draft.value, props.field);
  if (!source) return;
  if (missing.value.length) {
    result.value = { ok: false, text: t('connection:form.missingFields', { fields: missing.value.map((k) => t(`connection:source.input.${k}`)).join(', ') }) };
    return;
  }
  testing.value = true;
  result.value = null;
  try {
    const tokens = typedTokens(draft.value);
    if (hasMaster.value && draft.value.master === 'ask' && asked.value) tokens[''] = asked.value;
    const r = await api.testSecretSource(props.connectionId, props.driver, props.field, source, tokens);
    result.value = { ok: true, text: t(r.username ? 'connection:source.testOkUser' : 'connection:source.testOk', { n: r.chars }) };
  } catch (e) {
    // Came from outside and not confirmed yet: show it, and test again if confirmed.
    if (await askApproveSecretSource(e)) {
      testing.value = false;
      return test();
    }
    // The master password is asked: ask it, and test again with it.
    const unlock = unlockOf(e);
    if (unlock) {
      const typed = await askMasterPassword(unlock);
      if (typed !== null) {
        asked.value = typed;
        testing.value = false;
        return test();
      }
    }
    result.value = { ok: false, text: errorMessage(e) };
  } finally {
    testing.value = false;
  }
}

/** The chosen provider can't do without this input (as it's set now). */
const req = (k: keyof SourceDraft) => external.value && missingInputs({ ...draft.value, [k]: '' } as SourceDraft).includes(k);
</script>

<template>
  <div class="ss" :class="{ external }">
    <div class="ss-pick">
      <span class="ss-label">{{ $t('connection:source.label') }}</span>
      <el-select :model-value="draft.kind" size="small" class="ss-select" :data-field="field" @update:model-value="setKind">
        <el-option v-for="k in kinds" :key="k" :value="k" :label="$t(`connection:source.kind.${k}`)" />
      </el-select>
      <span v-if="draft.kind === 'ask'" class="ss-note">{{ $t('connection:source.askHelp') }}</span>
    </div>
    <div v-if="external" class="ss-body">
      <template v-if="draft.kind === 'command'">
        <el-form-item :label="$t('connection:source.input.command')" :required="req('command')" class="ss-wide">
          <el-input v-model="draft.command" class="ss-mono" :placeholder="$t('connection:source.placeholder.command')" spellcheck="false" />
          <div class="ss-help">{{ $t('connection:source.help.command') }}</div>
        </el-form-item>
      </template>
      <template v-else-if="draft.kind === 'onepassword'">
        <el-form-item :label="$t('connection:source.input.reference')" :required="req('reference')" class="ss-wide">
          <el-input v-model="draft.reference" class="ss-mono" placeholder="op://Private/Database/password" spellcheck="false" />
          <div class="ss-help">{{ $t('connection:source.help.onepassword') }}</div>
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.account')">
          <el-input v-model="draft.account" placeholder="my-team.1password.com" />
        </el-form-item>
      </template>
      <template v-else-if="draft.kind === 'vault'">
        <el-form-item :label="$t('connection:source.input.address')" :required="req('address')" class="ss-wide">
          <el-input v-model="draft.address" placeholder="https://vault.example.com:8200" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.path')" :required="req('path')">
          <el-input v-model="draft.path" class="ss-mono" placeholder="secret/myapp/db" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.key')" :required="req('key')">
          <el-input v-model="draft.key" class="ss-mono" placeholder="password" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.kv_version')">
          <el-select v-model="draft.kv_version" style="width: 100%">
            <el-option value="auto" :label="$t('connection:source.kvAuto')" />
            <el-option value="2" label="KV v2" />
            <el-option value="1" label="KV v1" />
          </el-select>
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.namespace')">
          <el-input v-model="draft.namespace" placeholder="admin/team" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.ca_file')">
          <el-input v-model="draft.ca_file" placeholder="~/vault-ca.pem">
            <template #append>
              <el-button @click="browse('ca_file')"><el-icon><ei-folder-opened /></el-icon></el-button>
            </template>
          </el-input>
        </el-form-item>
        <div class="ss-help ss-wide">{{ $t('connection:source.help.vault') }}</div>
      </template>
      <template v-else-if="draft.kind === 'aws'">
        <el-form-item :label="$t('connection:source.input.secret_id')" :required="req('secret_id')" class="ss-wide">
          <el-input v-model="draft.secret_id" class="ss-mono" placeholder="prod/myapp/db" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.region')">
          <el-input v-model="draft.region" placeholder="us-east-1" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.profile')">
          <el-input v-model="draft.profile" placeholder="default" />
        </el-form-item>
        <el-checkbox v-if="field === 'password'" v-model="draft.fill_username" class="ss-wide">{{ $t('connection:source.input.fill_username') }}</el-checkbox>
        <div class="ss-help ss-wide">{{ $t('connection:source.help.aws') }}</div>
      </template>
      <template v-else-if="draft.kind === 'azure'">
        <el-form-item :label="$t('connection:source.input.vault_url')" :required="req('vault_url')" class="ss-wide">
          <el-input v-model="draft.vault_url" placeholder="https://my-vault.vault.azure.net" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.name')" :required="req('name')">
          <el-input v-model="draft.name" class="ss-mono" placeholder="db-password" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.version')">
          <el-input v-model="draft.version" :placeholder="$t('connection:source.placeholder.latest')" />
        </el-form-item>
        <div class="ss-help ss-wide">{{ $t('connection:source.help.azure') }}</div>
      </template>
      <template v-else-if="draft.kind === 'gcp'">
        <el-form-item :label="$t('connection:source.input.project')" :required="req('project')">
          <el-input v-model="draft.project" placeholder="my-project" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.secret')" :required="req('secret')">
          <el-input v-model="draft.secret" class="ss-mono" placeholder="db-password" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.version')">
          <el-input v-model="draft.version" placeholder="latest" />
        </el-form-item>
        <div class="ss-help ss-wide">{{ $t('connection:source.help.gcp') }}</div>
      </template>
      <template v-else-if="draft.kind === 'keepass'">
        <el-form-item :label="$t('connection:source.input.file')" :required="req('file')" class="ss-wide">
          <el-input v-model="draft.file" placeholder="~/Passwords/databases.kdbx" spellcheck="false">
            <template #append>
              <el-button @click="browse('file')"><el-icon><ei-folder-opened /></el-icon></el-button>
            </template>
          </el-input>
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.entry')" :required="req('entry')" class="ss-wide">
          <el-input v-model="draft.entry" class="ss-mono" placeholder="Databases/Prod/pg-main" spellcheck="false" />
          <div class="ss-help">{{ $t('connection:source.help.entry') }}</div>
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.attribute')">
          <el-input v-model="draft.attribute" class="ss-mono" placeholder="Password" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.key_file')" :required="req('key_file')">
          <el-input v-model="draft.key_file" placeholder="~/Passwords/databases.keyx" spellcheck="false">
            <template #append>
              <el-button @click="browse('key_file')"><el-icon><ei-folder-opened /></el-icon></el-button>
            </template>
          </el-input>
        </el-form-item>
      </template>
      <template v-else-if="draft.kind === 'zoho'">
        <el-form-item :label="$t('connection:source.input.data_center')" :required="req('data_center')">
          <el-select v-model="draft.data_center" style="width: 100%" :placeholder="$t('connection:source.placeholder.data_center')">
            <el-option v-for="[id, domain] in ZOHO_DATA_CENTERS" :key="id" :value="id" :label="$t(`connection:source.dataCenter.${id.replace('.', '_')}`, { domain })" />
          </el-select>
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.secret_name')" :required="req('secret_name')">
          <el-input v-model="draft.secret_name" class="ss-mono" placeholder="pg-main" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.attribute')">
          <el-input v-model="draft.attribute" class="ss-mono" placeholder="password" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.client_id')" :required="req('client_id')">
          <el-input v-model="draft.client_id" class="ss-mono" placeholder="1000.XXXXXXXXXXXXXXXX" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.client_secret')">
          <el-input
            v-model="draft.client_secret" type="password" show-password autocomplete="new-password"
            :placeholder="editing ? $t('connection:form.secretKeptMasc') : ''"
          />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.refresh_token')">
          <el-input
            v-model="draft.refresh_token" type="password" show-password autocomplete="new-password"
            :placeholder="editing ? $t('connection:form.secretKeptMasc') : '1000.…'"
          />
        </el-form-item>
      </template>
      <template v-else-if="draft.kind === 'vault_db'">
        <el-form-item :label="$t('connection:source.input.address')" :required="req('address')" class="ss-wide">
          <el-input v-model="draft.address" placeholder="https://vault.example.com:8200" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.mount')">
          <el-input v-model="draft.mount" class="ss-mono" placeholder="database" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.role')" :required="req('role')">
          <el-input v-model="draft.role" class="ss-mono" placeholder="readonly" spellcheck="false" />
        </el-form-item>
        <el-checkbox v-model="draft.static_role" class="ss-wide">{{ $t('connection:source.input.static_role') }}</el-checkbox>
        <el-form-item :label="$t('connection:source.input.namespace')">
          <el-input v-model="draft.namespace" placeholder="admin/team" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.ca_file')">
          <el-input v-model="draft.ca_file" placeholder="~/vault-ca.pem">
            <template #append>
              <el-button @click="browse('ca_file')"><el-icon><ei-folder-opened /></el-icon></el-button>
            </template>
          </el-input>
        </el-form-item>
        <div class="ss-help ss-wide">{{ $t('connection:source.help.vault_db') }}</div>
      </template>
      <template v-else-if="draft.kind === 'conjur'">
        <el-form-item :label="$t('connection:source.input.url')" :required="req('url')" class="ss-wide">
          <el-input v-model="draft.url" placeholder="https://conjur.example.com" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.conjur_account')" :required="req('conjur_account')">
          <el-input v-model="draft.conjur_account" class="ss-mono" placeholder="myorg" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.login')" :required="req('login')">
          <el-input v-model="draft.login" class="ss-mono" placeholder="host/apps/dbine" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.variable')" :required="req('variable')">
          <el-input v-model="draft.variable" class="ss-mono" placeholder="db/prod/password" spellcheck="false" />
        </el-form-item>
        <el-form-item v-if="field === 'password'" :label="$t('connection:source.input.username_variable')">
          <el-input v-model="draft.username_variable" class="ss-mono" placeholder="db/prod/username" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.api_key')">
          <el-input
            v-model="draft.token" type="password" show-password autocomplete="new-password"
            :placeholder="editing ? $t('connection:form.secretKeptFem') : ''"
          />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.ca_file')">
          <el-input v-model="draft.ca_file" placeholder="~/conjur-ca.pem">
            <template #append>
              <el-button @click="browse('ca_file')"><el-icon><ei-folder-opened /></el-icon></el-button>
            </template>
          </el-input>
        </el-form-item>
        <div class="ss-help ss-wide">{{ $t('connection:source.help.conjur') }}</div>
      </template>
      <template v-else-if="draft.kind === 'ccp'">
        <el-form-item :label="$t('connection:source.input.url')" :required="req('url')" class="ss-wide">
          <el-input v-model="draft.url" placeholder="https://ccp.example.com" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.app_id')" :required="req('app_id')">
          <el-input v-model="draft.app_id" class="ss-mono" placeholder="DBine" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.safe')">
          <el-input v-model="draft.safe" class="ss-mono" placeholder="Databases" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.object')" :required="req('object')">
          <el-input v-model="draft.object" class="ss-mono" placeholder="pg-prod" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.folder')">
          <el-input v-model="draft.folder" class="ss-mono" placeholder="Root" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.account_user')">
          <el-input v-model="draft.account_user" class="ss-mono" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.account_address')">
          <el-input v-model="draft.account_address" class="ss-mono" placeholder="db.example.com" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.client_cert')">
          <el-input v-model="draft.client_cert" placeholder="~/dbine.p12" spellcheck="false">
            <template #append>
              <el-button @click="browse('client_cert')"><el-icon><ei-folder-opened /></el-icon></el-button>
            </template>
          </el-input>
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.client_key')">
          <el-input v-model="draft.client_key" placeholder="~/dbine.key" spellcheck="false" :disabled="!draft.client_cert.trim()">
            <template #append>
              <el-button :disabled="!draft.client_cert.trim()" @click="browse('client_key')"><el-icon><ei-folder-opened /></el-icon></el-button>
            </template>
          </el-input>
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.cert_passphrase')">
          <el-input
            v-model="draft.token" type="password" show-password autocomplete="new-password"
            :placeholder="editing ? $t('connection:form.secretKeptFem') : ''"
          />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.ca_file')">
          <el-input v-model="draft.ca_file" placeholder="~/ccp-ca.pem">
            <template #append>
              <el-button @click="browse('ca_file')"><el-icon><ei-folder-opened /></el-icon></el-button>
            </template>
          </el-input>
        </el-form-item>
        <el-checkbox v-if="field === 'password'" v-model="draft.fill_username" class="ss-wide">{{ $t('connection:source.input.fill_username_ccp') }}</el-checkbox>
        <div class="ss-help ss-wide">{{ $t('connection:source.help.ccp') }}</div>
      </template>
      <template v-else-if="draft.kind === 'delinea'">
        <el-form-item :label="$t('connection:source.input.url')" :required="req('url')" class="ss-wide">
          <el-input v-model="draft.url" placeholder="https://mytenant.secretservercloud.com" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.platform_url')" class="ss-wide">
          <el-input v-model="draft.platform_url" placeholder="https://mytenant.delinea.app" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.username')" :required="req('username')">
          <el-input v-model="draft.username" class="ss-mono" placeholder="svc_dbine" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.domain')">
          <el-input v-model="draft.domain" class="ss-mono" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.secret_name')" :required="req('secret_name')">
          <el-input v-model="draft.secret_name" class="ss-mono" placeholder="pg-prod" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.attribute')">
          <el-input v-model="draft.attribute" class="ss-mono" placeholder="password" spellcheck="false" />
        </el-form-item>
      </template>
      <template v-if="isVault">
        <el-form-item :label="$t('connection:source.input.auth')">
          <el-select v-model="draft.auth" style="width: 100%">
            <el-option v-for="a in VAULT_AUTHS" :key="a" :value="a" :label="$t(`connection:source.vaultAuth.${a}`)" />
          </el-select>
        </el-form-item>
        <el-form-item v-if="draft.auth !== 'approle'" :label="$t('connection:source.input.token')">
          <el-input
            v-model="draft.token"
            type="password"
            show-password
            autocomplete="new-password"
            :placeholder="editing ? $t('connection:form.secretKeptMasc') : $t('connection:source.placeholder.token')"
          />
        </el-form-item>
        <template v-else>
          <el-form-item :label="$t('connection:source.input.role_id')" :required="req('role_id')">
            <el-input v-model="draft.role_id" class="ss-mono" spellcheck="false" />
          </el-form-item>
          <el-form-item :label="$t('connection:source.input.approle_secret_id')">
            <el-input
              v-model="draft.approle_secret_id" type="password" show-password autocomplete="new-password"
              :placeholder="editing ? $t('connection:form.secretKeptMasc') : ''"
            />
          </el-form-item>
          <el-form-item :label="$t('connection:source.input.approle_mount')">
            <el-input v-model="draft.approle_mount" class="ss-mono" placeholder="approle" spellcheck="false" />
          </el-form-item>
          <div class="ss-help ss-wide">{{ $t('connection:source.help.approle') }}</div>
        </template>
      </template>
      <template v-if="hasMaster">
        <el-form-item :label="$t(`connection:source.input.master_${draft.kind}`)">
          <el-select v-model="draft.master" style="width: 100%">
            <el-option v-for="m in masters" :key="m" :value="m" :label="$t(`connection:source.master.${m}`)" />
          </el-select>
        </el-form-item>
        <el-form-item v-if="draft.master === 'keychain'" :label="$t(`connection:source.input.master_value_${draft.kind}`)">
          <el-input
            v-model="draft.token" type="password" show-password autocomplete="new-password"
            :placeholder="editing ? $t('connection:form.secretKeptFem') : ''"
          />
        </el-form-item>
        <el-form-item v-else-if="draft.master === 'command'" :label="$t('connection:source.input.master_command')" :required="req('master_command')">
          <el-input v-model="draft.master_command" class="ss-mono" :placeholder="$t('connection:source.placeholder.command')" spellcheck="false" />
        </el-form-item>
        <el-checkbox v-if="field === 'password'" v-model="draft.fill_username" class="ss-wide">
          {{ $t(`connection:source.input.fill_username_${draft.kind}`) }}
        </el-checkbox>
        <div class="ss-help ss-wide">{{ $t(`connection:source.help.${draft.kind}`) }}</div>
      </template>
      <el-form-item v-if="!['vault', 'keepass', 'zoho', 'vault_db', 'ccp', 'delinea'].includes(draft.kind)" :label="$t('connection:source.input.json_key')">
        <el-input v-model="draft.json_key" class="ss-mono" placeholder="password" spellcheck="false" />
        <div class="ss-help">{{ $t('connection:source.help.json_key') }}</div>
      </el-form-item>
      <div class="ss-test ss-wide">
        <el-button size="small" :loading="testing" @click="test">{{ $t('connection:source.test') }}</el-button>
        <span v-if="result" class="ss-result" :class="result.ok ? 'ok' : 'bad'" role="status">{{ result.text }}</span>
      </div>
    </div>
  </div>
</template>

<style scoped>
.ss { width: 100%; }
.ss-pick { display: flex; align-items: center; gap: 8px; margin-bottom: 4px; }
.ss-label { font-size: 11.5px; color: var(--nm-text-dim); }
.ss-select { width: 210px; }
.ss-note { font-size: 11.5px; color: var(--nm-text-dim); }
.ss-body {
  display: grid; grid-template-columns: 1fr 1fr; column-gap: 12px; margin-top: 4px; padding: 10px 12px 4px;
  border: 1px solid var(--nm-border-soft); border-radius: 4px; background: var(--nm-bg-elev);
}
.ss-wide { grid-column: 1 / -1; }
.ss-body :deep(.el-form-item) { margin-bottom: 10px; }
.ss-mono :deep(input) { font-family: var(--nm-font-mono, ui-monospace, monospace); font-size: 12px; }
.ss-help { font-size: 11.5px; color: var(--nm-text-dim); line-height: 1.4; margin: 2px 0 8px; }
.ss-test { display: flex; align-items: center; gap: 10px; margin: 2px 0 8px; min-width: 0; }
.ss-result { font-size: 12px; min-width: 0; white-space: pre-wrap; word-break: break-word; }
.ss-result.ok { color: var(--nm-success); }
.ss-result.bad { color: var(--nm-danger); }
</style>
