<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { open as openFile } from '@tauri-apps/plugin-dialog';
import { useTranslation } from 'i18next-vue';
import { api, errorKind, errorMessage } from '../api/client';
import { askApproveSecretSource } from '../composables/secretSourceApproval';
import { askMasterPassword, unlockOf } from '../composables/secretSourceUnlock';
import {
  BITWARDEN_CLOUDS, ENTRY_FIELDS, ENTRY_KINDS, fieldsQuery, isExternal, missingForBrowse, missingInputs, orderedKinds, passwordField, setPasswordField, sourceFrom,
  typedTokens, userFromSource, VAULT_AUTHS, ZOHO_DATA_CENTERS, applyBrowse, type DraftKind, type SourceDraft,
} from '../composables/secretSources';
import type { SecretBrowseEntry } from '../api/types';
import SecretBrowseDialog from './SecretBrowseDialog.vue';
import SecretMasterInputs from './SecretMasterInputs.vue';

// Where one secret field takes its value (docs/secret-sources.md): a
// selector first, then that source's inputs in the form's own grid (same
// label column and widths), "Examinar" next to the item where the source
// can list, and "Probar", which resolves it and shows only how many
// characters came back.
//
// - `credentials`: the database user and password. With a local option
//   (kept, asked every time) the parent's user and password inputs show
//   (slot `local`); with a manager they go, and the user input comes back
//   (slot `user`) only when the manager doesn't give the user too.
// - `field`: any other secret (a token, an API key, the SSH password): the
//   selector next to the field's own input (slot `local`), which the
//   source's inputs replace.
// The root is a two-column grid that spans the parent's grid.

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
  mode?: 'credentials' | 'field';
  /** `field` mode: the field's label, and whether its local input is required. */
  label?: string;
  required?: boolean;
}>();
const { t, i18next } = useTranslation();

/** Local options first, then the managers by name in the UI's language. */
const kinds = computed(() => orderedKinds((k) => t(`connection:source.kind.${k}`), i18next.language || 'es', props.allowAsk));
const credentials = computed(() => props.mode === 'credentials');
const testing = ref(false);
const result = ref<{ ok: boolean; text: string } | null>(null);
watch(draft, () => { result.value = null; }, { deep: true });

const external = computed(() => isExternal(draft.value));
const missing = computed(() => missingInputs(draft.value));

function setKind(k: DraftKind) {
  draft.value.kind = k;
  // Zoho Vault and Bitwarden always need their passphrase, Delinea its password.
  if ((k === 'zoho' || k === 'delinea' || k === 'bitwarden') && draft.value.master === 'none') draft.value.master = 'keychain';
  // Bitwarden's server starts as the US cloud.
  if ((k === 'bitwarden' || k === 'bitwarden_sm') && !draft.value.server) draft.value.server = BITWARDEN_CLOUDS[0];
  // An entry gives the database user and password both.
  if (credentials.value && ENTRY_KINDS.includes(k)) draft.value.fill_username = true;
}

async function browse(input: 'ca_file' | 'file' | 'key_file' | 'client_cert' | 'client_key') {
  try {
    const filters = input === 'file' ? [{ name: 'KeePass', extensions: ['kdbx'] }] : undefined;
    const p = await openFile({ multiple: false, directory: false, filters });
    if (typeof p === 'string') draft.value[input] = p;
  } catch { /* no dialog outside Tauri */ }
}

const hasMaster = computed(() => ['keepass', 'zoho', 'delinea', 'bitwarden'].includes(draft.value.kind));
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

/** A single value whose JSON may carry the user ("Usar también el usuario de la entrada"). */
const canFillUser = computed(() => props.field === 'password' && (draft.value.kind === 'aws' || draft.value.kind === 'ssm'));
/** Single values: their JSON key. */
const JSON_KEY_KINDS: DraftKind[] = ['command', 'onepassword', 'aws', 'ssm', 'azure', 'gcp', 'conjur', 'bitwarden_sm'];
/** An entry as the database credentials whose (older) definition doesn't give the user. */
const legacyUser = computed(() => credentials.value && ENTRY_KINDS.includes(draft.value.kind) && !draft.value.fill_username);
/** What the option says about where the user comes from, per source. */
const fillUserHint = computed(() => {
  const k = draft.value.kind;
  return t(k === 'aws' || k === 'ssm' ? 'connection:source.input.fill_username' : `connection:source.input.fill_username_${k}`);
});
const userShown = computed(() => !userFromSource(draft.value));

// ---- "Examinar" ----------------------------------------------------------------
// Disabled, with what's missing in its tooltip, until what signs in is filled.
const browseMissing = computed(() => missingForBrowse(draft.value, props.editing));
/** An input's name, for "Completá primero". */
function inputLabel(k: keyof SourceDraft): string {
  const kind = draft.value.kind;
  if (k === 'token') {
    if (hasMaster.value) return t(`connection:source.input.master_value_${kind}`);
    if (kind === 'conjur') return t('connection:source.input.api_key');
    if (kind === 'bitwarden_sm') return t('connection:source.input.access_token');
  }
  return t(`connection:source.input.${k}`);
}
const browseTitle = computed(() => browseMissing.value.length
  ? t('connection:source.browse.fillFirst', { fields: browseMissing.value.map(inputLabel).join(', ') })
  : t('connection:source.browse.button'));

/** The entry's own user and password fields (the defaults). */
const entryDefaults = computed(() => ENTRY_FIELDS[draft.value.kind]);
/** The user is the entry's: the browse's field step assigns user and password. */
const entryUser = computed(() => credentials.value && !!entryDefaults.value && draft.value.fill_username);

/** The form's tokens, with a master password asked here. */
function formTokens(): Record<string, string> {
  const tokens = typedTokens(draft.value);
  if (hasMaster.value && draft.value.master === 'ask' && asked.value) tokens[''] = asked.value;
  return tokens;
}

const browsing = ref<{ source: Record<string, unknown>; tokens: Record<string, string>; roles?: { user: string; password: string } } | null>(null);
function openBrowse() {
  if (browseMissing.value.length) return;
  const source = sourceFrom(draft.value, props.field);
  if (!source) return;
  const d = entryDefaults.value;
  const roles = entryUser.value && d ? { user: draft.value.user_field || d.user, password: passwordField(draft.value) || d.password } : undefined;
  browsing.value = { source: source as unknown as Record<string, unknown>, tokens: formTokens(), roles };
}
function picked(entry: SecretBrowseEntry, field: string | null, userField?: string, fields?: string[]) {
  applyBrowse(draft.value, entry, field, userField);
  // The entry's field names came with it: the advanced selectors have them.
  if (fields) {
    fieldOptions.value = fields;
    fieldsState.value = 'ready';
    loadedFor = fieldsKey.value;
  }
}

// ---- "Configuración avanzada": the entry's user and password fields ---------------
// Collapsed, always at the bottom. The entry's field names load in the
// background (names only) so they're there when it's opened.
const hasAdvanced = computed(() => !!entryDefaults.value);
const advancedOpen = ref(false);
const userFieldShown = computed(() => credentials.value && draft.value.fill_username);
const userFieldModel = computed({
  get: () => draft.value.user_field,
  set: (v: string) => { draft.value.user_field = v === entryDefaults.value?.user ? '' : (v ?? '').trim(); },
});
const passwordFieldModel = computed({
  get: () => passwordField(draft.value),
  set: (v: string) => setPasswordField(draft.value, (v ?? '').trim()),
});
const advancedSummary = computed(() => {
  const d = entryDefaults.value;
  if (!d) return '';
  const user = draft.value.user_field;
  const pass = passwordField(draft.value);
  const parts = [userFieldShown.value && user ? `${t('connection:source.advanced.user')}: ${user}` : '', pass && pass !== d.password ? `${t('connection:source.advanced.password')}: ${pass}` : ''];
  return parts.filter(Boolean).join(' · ');
});
const fieldOptions = ref<string[]>([]);
const fieldsState = ref<'idle' | 'loading' | 'ready' | 'locked' | 'error'>('idle');
const fieldsError = ref('');
/** What the field names depend on: the entry and how to sign in. */
const fieldsKey = computed(() => {
  if (!hasAdvanced.value) return '';
  const q = fieldsQuery(draft.value);
  if (!q || browseMissing.value.length) return '';
  const src = { ...(sourceFrom(draft.value, props.field) as unknown as Record<string, unknown>) };
  for (const k of ['attribute', 'user_field', 'fill_username', 'json_key', 'reference']) delete src[k];
  return JSON.stringify([q, src, typedTokens(draft.value)]);
});
let loadedFor = '';
let timer: ReturnType<typeof setTimeout> | undefined;
watch(fieldsKey, (k) => {
  clearTimeout(timer);
  if (!k) { fieldsState.value = 'idle'; fieldOptions.value = []; return; }
  if (k === loadedFor) return;
  timer = setTimeout(() => loadFields(false), 700);
}, { immediate: true });

/** The entry's field names. `ask`: the user clicked "Cargar campos" (ask
 *  for a master password or a confirmation); in the background, never. */
async function loadFields(ask: boolean): Promise<void> {
  const q = fieldsQuery(draft.value);
  const source = sourceFrom(draft.value, props.field);
  if (!q || !source) return;
  const key = fieldsKey.value;
  fieldsState.value = 'loading';
  try {
    const l = await api.browseSecretSource(props.connectionId, props.driver, props.field, source as unknown as Record<string, unknown>, formTokens(), q);
    if (key !== fieldsKey.value) return;
    fieldOptions.value = l.fields ?? [];
    fieldsState.value = 'ready';
    loadedFor = key;
  } catch (e) {
    if (key !== fieldsKey.value) return;
    const unlock = unlockOf(e);
    if (ask && (await askApproveSecretSource(e))) return loadFields(true);
    if (ask && unlock) {
      const typed = await askMasterPassword(unlock);
      if (typed !== null) {
        asked.value = typed;
        return loadFields(true);
      }
    }
    // A master password to ask, a definition to confirm: the button does it.
    if (unlock || errorKind(e) === 'secret_source_approval') fieldsState.value = 'locked';
    else {
      fieldsState.value = 'error';
      fieldsError.value = errorMessage(e);
    }
  }
}

/** The chosen provider can't do without this input (as it's set now). */
const req = (k: keyof SourceDraft) => external.value && missingInputs({ ...draft.value, [k]: '' } as SourceDraft).includes(k);
</script>

<template>
  <div class="ss" :class="[mode ?? 'field', { external }]">
    <el-form-item :label="credentials ? $t('connection:source.credentials') : label" class="ss-pick-item ss-wide" :required="!credentials && required && !external">
      <div class="ss-pick">
        <el-select :model-value="draft.kind" class="ss-select" :data-field="field" @update:model-value="setKind">
          <el-option-group :label="$t('connection:source.group.local')">
            <el-option v-for="k in kinds.local" :key="k" :value="k" :label="$t(`connection:source.kind.${k}`)" />
          </el-option-group>
          <el-option-group :label="$t('connection:source.group.managers')">
            <el-option v-for="k in kinds.managers" :key="k" :value="k" :label="$t(`connection:source.kind.${k}`)" />
          </el-option-group>
        </el-select>
        <div v-if="!credentials && !external" class="ss-inline"><slot name="local" /></div>
      </div>
      <div v-if="draft.kind === 'ask'" class="ss-help">{{ $t('connection:source.askHelp') }}</div>
    </el-form-item>
    <template v-if="credentials && !external"><slot name="local" /></template>
    <template v-if="external">
      <!-- Each source: what signs in first, then the item (with "Examinar"),
           so browsing has everything it needs; "Configuración avanzada" last. -->
      <template v-if="draft.kind === 'command'">
        <el-form-item :label="$t('connection:source.input.command')" :required="req('command')" class="ss-wide">
          <el-input v-model="draft.command" class="ss-mono" :placeholder="$t('connection:source.placeholder.command')" spellcheck="false" />
          <div class="ss-help">{{ $t('connection:source.help.command') }}</div>
        </el-form-item>
      </template>

      <template v-else-if="draft.kind === 'onepassword'">
        <el-form-item :label="$t('connection:source.input.account')">
          <el-input v-model="draft.account" placeholder="my-team.1password.com" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.reference')" :required="req('reference')" class="ss-wide">
          <el-input v-model="draft.reference" class="ss-mono" placeholder="op://Private/Database/password" spellcheck="false">
            <template #append><el-button class="ss-browse" :disabled="!!browseMissing.length" :title="browseTitle" :aria-label="$t('connection:source.browse.button')" @click="openBrowse"><el-icon><ei-search /></el-icon></el-button></template>
          </el-input>
          <div class="ss-help">{{ $t('connection:source.help.onepassword') }}</div>
        </el-form-item>
      </template>

      <template v-else-if="isVault">
        <el-form-item :label="$t('connection:source.input.address')" :required="req('address')" class="ss-wide">
          <el-input v-model="draft.address" placeholder="https://vault.example.com:8200" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.namespace')">
          <el-input v-model="draft.namespace" placeholder="admin/team" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.ca_file')">
          <el-input v-model="draft.ca_file" placeholder="~/vault-ca.pem">
            <template #append><el-button @click="browse('ca_file')"><el-icon><ei-folder-opened /></el-icon></el-button></template>
          </el-input>
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.auth')">
          <el-select v-model="draft.auth" style="width: 100%">
            <el-option v-for="a in VAULT_AUTHS" :key="a" :value="a" :label="$t(`connection:source.vaultAuth.${a}`)" />
          </el-select>
        </el-form-item>
        <el-form-item v-if="draft.auth !== 'approle'" :label="$t('connection:source.input.token')">
          <el-input
            v-model="draft.token" type="password" show-password autocomplete="new-password"
            :placeholder="editing ? $t('connection:form.secretKeptMasc') : $t('connection:source.placeholder.token')"
          />
        </el-form-item>
        <template v-else>
          <el-form-item :label="$t('connection:source.input.role_id')" :required="req('role_id')">
            <el-input v-model="draft.role_id" class="ss-mono" spellcheck="false" />
          </el-form-item>
          <el-form-item :label="$t('connection:source.input.approle_secret_id')">
            <el-input v-model="draft.approle_secret_id" type="password" show-password autocomplete="new-password" :placeholder="editing ? $t('connection:form.secretKeptMasc') : ''" />
          </el-form-item>
          <el-form-item :label="$t('connection:source.input.approle_mount')">
            <el-input v-model="draft.approle_mount" class="ss-mono" placeholder="approle" spellcheck="false" />
          </el-form-item>
          <div class="ss-help ss-wide">{{ $t('connection:source.help.approle') }}</div>
        </template>
        <template v-if="draft.kind === 'vault'">
          <el-form-item :label="$t('connection:source.input.path')" :required="req('path')" class="ss-wide">
            <el-input v-model="draft.path" class="ss-mono" placeholder="secret/myapp/db" spellcheck="false">
              <template #append><el-button class="ss-browse" :disabled="!!browseMissing.length" :title="browseTitle" :aria-label="$t('connection:source.browse.button')" @click="openBrowse"><el-icon><ei-search /></el-icon></el-button></template>
            </el-input>
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
          <div class="ss-help ss-wide">{{ $t('connection:source.help.vault') }}</div>
        </template>
        <template v-else>
          <el-form-item :label="$t('connection:source.input.mount')">
            <el-input v-model="draft.mount" class="ss-mono" placeholder="database" spellcheck="false" />
          </el-form-item>
          <el-form-item :label="$t('connection:source.input.role')" :required="req('role')">
            <el-input v-model="draft.role" class="ss-mono" placeholder="readonly" spellcheck="false">
              <template #append><el-button class="ss-browse" :disabled="!!browseMissing.length" :title="browseTitle" :aria-label="$t('connection:source.browse.button')" @click="openBrowse"><el-icon><ei-search /></el-icon></el-button></template>
            </el-input>
          </el-form-item>
          <el-checkbox v-model="draft.static_role" class="ss-wide">{{ $t('connection:source.input.static_role') }}</el-checkbox>
          <div class="ss-help ss-wide">{{ $t('connection:source.help.vault_db') }}</div>
        </template>
      </template>

      <template v-else-if="draft.kind === 'aws' || draft.kind === 'ssm'">
        <el-form-item :label="$t('connection:source.input.region')">
          <el-input v-model="draft.region" placeholder="us-east-1" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.profile')">
          <el-input v-model="draft.profile" placeholder="default" />
        </el-form-item>
        <el-form-item v-if="draft.kind === 'aws'" :label="$t('connection:source.input.secret_id')" :required="req('secret_id')" class="ss-wide">
          <el-input v-model="draft.secret_id" class="ss-mono" placeholder="prod/myapp/db" spellcheck="false">
            <template #append><el-button class="ss-browse" :disabled="!!browseMissing.length" :title="browseTitle" :aria-label="$t('connection:source.browse.button')" @click="openBrowse"><el-icon><ei-search /></el-icon></el-button></template>
          </el-input>
        </el-form-item>
        <template v-else>
          <el-form-item :label="$t('connection:source.input.parameter')" :required="req('parameter')" class="ss-wide">
            <el-input v-model="draft.parameter" class="ss-mono" placeholder="/prod/myapp/db/password" spellcheck="false">
              <template #append><el-button class="ss-browse" :disabled="!!browseMissing.length" :title="browseTitle" :aria-label="$t('connection:source.browse.button')" @click="openBrowse"><el-icon><ei-search /></el-icon></el-button></template>
            </el-input>
          </el-form-item>
          <el-form-item :label="$t('connection:source.input.version')">
            <el-input v-model="draft.version" :placeholder="$t('connection:source.placeholder.ssmVersion')" />
          </el-form-item>
          <el-form-item :label="$t('connection:source.input.list_index')">
            <el-input v-model="draft.list_index" placeholder="1" />
          </el-form-item>
        </template>
        <div class="ss-help ss-wide">{{ $t(`connection:source.help.${draft.kind}`) }}</div>
      </template>

      <template v-else-if="draft.kind === 'azure'">
        <el-form-item :label="$t('connection:source.input.vault_url')" :required="req('vault_url')" class="ss-wide">
          <el-input v-model="draft.vault_url" placeholder="https://my-vault.vault.azure.net" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.name')" :required="req('name')">
          <el-input v-model="draft.name" class="ss-mono" placeholder="db-password" spellcheck="false">
            <template #append><el-button class="ss-browse" :disabled="!!browseMissing.length" :title="browseTitle" :aria-label="$t('connection:source.browse.button')" @click="openBrowse"><el-icon><ei-search /></el-icon></el-button></template>
          </el-input>
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
        <div />
        <el-form-item :label="$t('connection:source.input.secret')" :required="req('secret')">
          <el-input v-model="draft.secret" class="ss-mono" placeholder="db-password" spellcheck="false">
            <template #append><el-button class="ss-browse" :disabled="!!browseMissing.length" :title="browseTitle" :aria-label="$t('connection:source.browse.button')" @click="openBrowse"><el-icon><ei-search /></el-icon></el-button></template>
          </el-input>
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.version')">
          <el-input v-model="draft.version" placeholder="latest" />
        </el-form-item>
        <div class="ss-help ss-wide">{{ $t('connection:source.help.gcp') }}</div>
      </template>

      <template v-else-if="draft.kind === 'keepass'">
        <el-form-item :label="$t('connection:source.input.file')" :required="req('file')" class="ss-wide">
          <el-input v-model="draft.file" placeholder="~/Passwords/databases.kdbx" spellcheck="false">
            <template #append><el-button @click="browse('file')"><el-icon><ei-folder-opened /></el-icon></el-button></template>
          </el-input>
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.key_file')" :required="req('key_file')" class="ss-wide">
          <el-input v-model="draft.key_file" placeholder="~/Passwords/databases.keyx" spellcheck="false">
            <template #append><el-button @click="browse('key_file')"><el-icon><ei-folder-opened /></el-icon></el-button></template>
          </el-input>
        </el-form-item>
        <SecretMasterInputs v-model="draft" :editing="editing" :required-command="req('master_command')" />
        <el-form-item :label="$t('connection:source.input.entry')" :required="req('entry')" class="ss-wide">
          <el-input v-model="draft.entry" class="ss-mono" placeholder="Databases/Prod/pg-main" spellcheck="false">
            <template #append><el-button class="ss-browse" :disabled="!!browseMissing.length" :title="browseTitle" :aria-label="$t('connection:source.browse.button')" @click="openBrowse"><el-icon><ei-search /></el-icon></el-button></template>
          </el-input>
          <div class="ss-help">{{ $t('connection:source.help.entry') }}</div>
        </el-form-item>
      </template>

      <template v-else-if="draft.kind === 'zoho'">
        <el-form-item :label="$t('connection:source.input.data_center')" :required="req('data_center')">
          <el-select v-model="draft.data_center" style="width: 100%" :placeholder="$t('connection:source.placeholder.data_center')">
            <el-option v-for="[id, domain] in ZOHO_DATA_CENTERS" :key="id" :value="id" :label="$t(`connection:source.dataCenter.${id.replace('.', '_')}`, { domain })" />
          </el-select>
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.client_id')" :required="req('client_id')">
          <el-input v-model="draft.client_id" class="ss-mono" placeholder="1000.XXXXXXXXXXXXXXXX" spellcheck="false" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.client_secret')">
          <el-input v-model="draft.client_secret" type="password" show-password autocomplete="new-password" :placeholder="editing ? $t('connection:form.secretKeptMasc') : ''" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.refresh_token')">
          <el-input v-model="draft.refresh_token" type="password" show-password autocomplete="new-password" :placeholder="editing ? $t('connection:form.secretKeptMasc') : '1000.…'" />
        </el-form-item>
        <SecretMasterInputs v-model="draft" :editing="editing" :required-command="req('master_command')" />
        <el-form-item :label="$t('connection:source.input.secret_name')" :required="req('secret_name')" class="ss-wide">
          <el-input v-model="draft.secret_name" class="ss-mono" placeholder="pg-main" spellcheck="false">
            <template #append><el-button class="ss-browse" :disabled="!!browseMissing.length" :title="browseTitle" :aria-label="$t('connection:source.browse.button')" @click="openBrowse"><el-icon><ei-search /></el-icon></el-button></template>
          </el-input>
        </el-form-item>
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
        <el-form-item :label="$t('connection:source.input.api_key')">
          <el-input v-model="draft.token" type="password" show-password autocomplete="new-password" :placeholder="editing ? $t('connection:form.secretKeptFem') : ''" />
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.ca_file')">
          <el-input v-model="draft.ca_file" placeholder="~/conjur-ca.pem">
            <template #append><el-button @click="browse('ca_file')"><el-icon><ei-folder-opened /></el-icon></el-button></template>
          </el-input>
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.variable')" :required="req('variable')">
          <el-input v-model="draft.variable" class="ss-mono" placeholder="db/prod/password" spellcheck="false">
            <template #append><el-button class="ss-browse" :disabled="!!browseMissing.length" :title="browseTitle" :aria-label="$t('connection:source.browse.button')" @click="openBrowse"><el-icon><ei-search /></el-icon></el-button></template>
          </el-input>
        </el-form-item>
        <el-form-item v-if="field === 'password'" :label="$t('connection:source.input.username_variable')">
          <el-input v-model="draft.username_variable" class="ss-mono" placeholder="db/prod/username" spellcheck="false" />
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
        <el-form-item :label="$t('connection:source.input.ca_file')">
          <el-input v-model="draft.ca_file" placeholder="~/ccp-ca.pem">
            <template #append><el-button @click="browse('ca_file')"><el-icon><ei-folder-opened /></el-icon></el-button></template>
          </el-input>
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.client_cert')">
          <el-input v-model="draft.client_cert" placeholder="~/dbine.p12" spellcheck="false">
            <template #append><el-button @click="browse('client_cert')"><el-icon><ei-folder-opened /></el-icon></el-button></template>
          </el-input>
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.client_key')">
          <el-input v-model="draft.client_key" placeholder="~/dbine.key" spellcheck="false" :disabled="!draft.client_cert.trim()">
            <template #append><el-button :disabled="!draft.client_cert.trim()" @click="browse('client_key')"><el-icon><ei-folder-opened /></el-icon></el-button></template>
          </el-input>
        </el-form-item>
        <el-form-item :label="$t('connection:source.input.cert_passphrase')" class="ss-wide">
          <el-input v-model="draft.token" type="password" show-password autocomplete="new-password" :placeholder="editing ? $t('connection:form.secretKeptFem') : ''" />
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
        <SecretMasterInputs v-model="draft" :editing="editing" :required-command="req('master_command')" />
        <el-form-item :label="$t('connection:source.input.secret_name')" :required="req('secret_name')" class="ss-wide">
          <el-input v-model="draft.secret_name" class="ss-mono" placeholder="pg-prod" spellcheck="false">
            <template #append><el-button class="ss-browse" :disabled="!!browseMissing.length" :title="browseTitle" :aria-label="$t('connection:source.browse.button')" @click="openBrowse"><el-icon><ei-search /></el-icon></el-button></template>
          </el-input>
        </el-form-item>
      </template>

      <template v-else-if="draft.kind === 'bitwarden' || draft.kind === 'bitwarden_sm'">
        <el-form-item :label="$t('connection:source.input.server')" class="ss-wide">
          <el-select v-model="draft.server" filterable allow-create default-first-option style="width: 100%" :placeholder="$t('connection:source.placeholder.server')">
            <el-option v-for="c in BITWARDEN_CLOUDS" :key="c" :value="c" :label="$t(`connection:source.bitwardenCloud.${c.replace('bitwarden.', '')}`, { domain: c })" />
          </el-select>
          <div class="ss-help">{{ $t('connection:source.help.server') }}</div>
        </el-form-item>
        <template v-if="draft.kind === 'bitwarden'">
          <el-form-item :label="$t('connection:source.input.client_id')" :required="req('client_id')">
            <el-input v-model="draft.client_id" class="ss-mono" placeholder="user.xxxxxxxx-xxxx-…" spellcheck="false" />
          </el-form-item>
          <el-form-item :label="$t('connection:source.input.client_secret')">
            <el-input v-model="draft.client_secret" type="password" show-password autocomplete="new-password" :placeholder="editing ? $t('connection:form.secretKeptMasc') : ''" />
          </el-form-item>
          <SecretMasterInputs v-model="draft" :editing="editing" :required-command="req('master_command')" />
          <el-form-item :label="$t('connection:source.input.item')" :required="req('item')" class="ss-wide">
            <el-input v-model="draft.item" class="ss-mono" placeholder="pg-main" spellcheck="false">
              <template #append><el-button class="ss-browse" :disabled="!!browseMissing.length" :title="browseTitle" :aria-label="$t('connection:source.browse.button')" @click="openBrowse"><el-icon><ei-search /></el-icon></el-button></template>
            </el-input>
          </el-form-item>
        </template>
        <template v-else>
          <el-form-item :label="$t('connection:source.input.access_token')" class="ss-wide">
            <el-input v-model="draft.token" type="password" show-password autocomplete="new-password" :placeholder="editing ? $t('connection:form.secretKeptMasc') : 'BWS_ACCESS_TOKEN'" />
          </el-form-item>
          <el-form-item :label="$t('connection:source.input.secret_uuid')" :required="req('secret')" class="ss-wide">
            <el-input v-model="draft.secret" class="ss-mono" placeholder="xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx" spellcheck="false">
              <template #append><el-button class="ss-browse" :disabled="!!browseMissing.length" :title="browseTitle" :aria-label="$t('connection:source.browse.button')" @click="openBrowse"><el-icon><ei-search /></el-icon></el-button></template>
            </el-input>
          </el-form-item>
          <div class="ss-help ss-wide">{{ $t('connection:source.help.bitwarden_sm') }}</div>
        </template>
      </template>

      <!-- A single value: its JSON key, and the user from the same secret. -->
      <el-form-item v-if="JSON_KEY_KINDS.includes(draft.kind)" :label="$t('connection:source.input.json_key')">
        <el-input v-model="draft.json_key" class="ss-mono" placeholder="password" spellcheck="false" />
        <div class="ss-help">{{ $t('connection:source.help.json_key') }}</div>
      </el-form-item>
      <el-checkbox v-if="canFillUser" v-model="draft.fill_username" class="ss-wide" :title="fillUserHint">{{ $t('connection:source.fillUser') }}</el-checkbox>
      <!-- An entry gives the user too; an older definition that doesn't keeps the user field. -->
      <div v-if="legacyUser" class="ss-legacy ss-wide">
        {{ $t('connection:source.legacyUser') }}
        <el-button link type="primary" size="small" @click="draft.fill_username = true">{{ $t('connection:source.useEntryUser') }}</el-button>
      </div>
      <template v-if="credentials && userShown"><slot name="user" /></template>
      <div class="ss-test ss-wide">
        <el-button size="small" :loading="testing" @click="test">{{ $t('connection:source.test') }}</el-button>
        <span v-if="result" class="ss-result" :class="result.ok ? 'ok' : 'bad'" role="status">{{ result.text }}</span>
      </div>

      <!-- Last, collapsed: which of the entry's fields are the user and the password. -->
      <div v-if="hasAdvanced" class="ss-adv ss-wide">
        <button type="button" class="ss-adv-head" :aria-expanded="advancedOpen" @click="advancedOpen = !advancedOpen">
          <el-icon class="ss-adv-arrow" :class="{ open: advancedOpen }"><ei-arrow-right /></el-icon>
          <span>{{ $t('connection:source.advanced.title') }}</span>
          <span v-if="advancedSummary" class="ss-adv-sum">{{ advancedSummary }}</span>
        </button>
        <div v-show="advancedOpen" class="ss-adv-body">
          <el-form-item v-if="userFieldShown" :label="$t('connection:source.advanced.userField')">
            <el-select
              v-model="userFieldModel" filterable allow-create default-first-option clearable class="ss-mono-select" style="width: 100%"
              :placeholder="entryDefaults?.user" :loading="fieldsState === 'loading'" data-adv="user"
            >
              <el-option v-for="f in fieldOptions" :key="f" :value="f" :label="f" />
            </el-select>
          </el-form-item>
          <el-form-item :label="credentials ? $t('connection:source.advanced.passwordField') : $t('connection:source.advanced.field')">
            <el-select
              v-model="passwordFieldModel" filterable allow-create default-first-option clearable class="ss-mono-select" style="width: 100%"
              :placeholder="entryDefaults?.password" :loading="fieldsState === 'loading'" data-adv="password"
            >
              <el-option v-for="f in fieldOptions" :key="f" :value="f" :label="f" />
            </el-select>
          </el-form-item>
          <el-form-item v-if="draft.kind === 'bitwarden'" :label="$t('connection:source.input.json_key')">
            <el-input v-model="draft.json_key" class="ss-mono" placeholder="password" spellcheck="false" />
            <div class="ss-help">{{ $t('connection:source.help.json_key') }}</div>
          </el-form-item>
          <div class="ss-adv-state ss-wide">
            <span v-if="fieldsState === 'loading'">{{ $t('connection:source.advanced.loading') }}</span>
            <template v-else-if="fieldsState === 'locked' || fieldsState === 'error'">
              <span v-if="fieldsState === 'error'" class="bad" :title="fieldsError">{{ $t('connection:source.advanced.failed') }}</span>
              <el-button size="small" @click="loadFields(true)">{{ $t('connection:source.advanced.load') }}</el-button>
            </template>
            <span v-else-if="fieldsState === 'ready'">{{ $t('connection:source.advanced.loaded', { n: fieldOptions.length }) }}</span>
            <span v-else>{{ $t('connection:source.advanced.hint') }}</span>
          </div>
        </div>
      </div>
    </template>
    <SecretBrowseDialog
      v-if="browsing" :kind="draft.kind" :source="browsing.source" :tokens="browsing.tokens" :field="field" :connection-id="connectionId" :driver="driver"
      :roles="browsing.roles" @pick="picked" @close="browsing = null"
    />
  </div>
</template>

<style scoped>
/* The parent's grid, again: two columns, the same gap, no padding, so the
   source's inputs line up with the rest of the form. */
.ss { display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 1fr); column-gap: 16px; width: 100%; }
.ss-wide { grid-column: 1 / -1; }
/* The selector in the left column; a local field's own input in the right. */
.ss-pick { display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 1fr); column-gap: 16px; align-items: center; width: 100%; }
.ss-select { width: 100%; }
.ss-inline { flex: 1; min-width: 0; display: flex; }
.ss-inline > :deep(*) { flex: 1; min-width: 0; }
.ss-mono :deep(input) { font-family: var(--nm-font-mono, ui-monospace, monospace); font-size: 12px; }
.ss-help { font-size: 11.5px; color: var(--nm-text-dim); line-height: 1.4; margin: 2px 0 8px; }
.ss > .el-checkbox { margin: -2px 0 12px; height: auto; }
.ss-legacy { font-size: 12px; color: var(--nm-text-dim); margin: -2px 0 10px; display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
/* "Configuración avanzada": collapsed, at the bottom. */
.ss-adv { border-top: 1px solid var(--nm-border-soft); margin: 0 0 12px; padding-top: 6px; }
.ss-adv-head {
  display: flex; align-items: center; gap: 6px; width: 100%; border: 0; background: none; padding: 4px 0; cursor: pointer;
  color: var(--nm-text); font: inherit; font-size: 12.5px; text-align: left;
}
.ss-adv-head:hover { color: var(--nm-text-strong); }
.ss-adv-arrow { transition: transform 0.12s; color: var(--nm-text-dim); }
.ss-adv-arrow.open { transform: rotate(90deg); }
.ss-adv-sum { margin-left: 6px; font-size: 11.5px; color: var(--nm-text-dim); font-family: var(--nm-font-mono, ui-monospace, monospace); }
.ss-adv-body { display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 1fr); column-gap: 16px; padding-top: 8px; }
.ss-adv-state { display: flex; align-items: center; gap: 8px; font-size: 11.5px; color: var(--nm-text-dim); margin: -4px 0 4px; }
.ss-adv-state .bad { color: var(--nm-danger); }
.ss-mono-select :deep(input) { font-family: var(--nm-font-mono, ui-monospace, monospace); font-size: 12px; }
.ss-test { display: flex; align-items: center; gap: 10px; margin: 0 0 14px; min-width: 0; }
.ss-result { font-size: 12px; min-width: 0; white-space: pre-wrap; word-break: break-word; }
.ss-result.ok { color: var(--nm-success); }
.ss-result.bad { color: var(--nm-danger); }
</style>
