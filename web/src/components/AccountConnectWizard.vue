<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import { open as openFile } from '@tauri-apps/plugin-dialog';
import { ElMessage } from 'element-plus';
import { accountsApi } from '../api/secretAccounts';
import { errorMessage } from '../api/client';
import type { SecretAccount } from '../api/types';
import { tb } from '../i18n/backend';
import { loadSecretAccounts, PROVIDER } from '../composables/secretAccounts';
import {
  azureTenants, back, configOf, destinations, emptyForm, formFromAccount, gcloudAccounts, gcloudPreselect, gcloudSignIn, isLast, keepsSecrets, SECRET_INPUTS, secretsOf, stepForError, stepProblems, STEPS,
  suggestedLabel, tenantChoices, tenantLabel, tenantSignIn, type WizardForm, type WizardKind,
} from '../composables/accountWizards';

// "Conectar <gestor>" (docs/secret-sources.md, "Accounts") for every secret
// manager but Zoho Vault: a few steps, each checked before the next, with
// where to find each value in the manager's own words; the last step checks
// the account live against the manager (`connect_secret_account`) and only
// then saves it. Closing before the end leaves nothing behind: nothing is
// sent before the last step, and what was typed is wiped.
//
// `account`: reconnect that account (same ID: its connections go on; a
// secret left empty keeps the one saved, while it goes to the same place).
// `initialStep` / `preview`: the component preview only.

type Preview = Partial<WizardForm> & { result?: 'ok'; error?: string; identity?: string; detail?: string; detect?: Record<string, unknown> };
const props = defineProps<{ kind: WizardKind; account?: SecretAccount | null; initialStep?: number; preview?: Preview }>();
const emit = defineEmits<{ close: []; connected: [account: SecretAccount] }>();
const { t } = useTranslation();

const s = reactive(emptyForm());
if (props.account) Object.assign(s, formFromAccount(props.account));
if (props.preview) {
  const { result: _r, error: _e, identity: _i, detail: _d, detect: _x, ...form } = props.preview;
  Object.assign(s, form);
}
if (props.initialStep) s.step = props.initialStep;

const visible = ref(true);
const busy = ref(false);
const error = ref(props.preview?.error ?? '');
const done = ref<{ account: SecretAccount; detail: string } | null>(
  props.preview?.result === 'ok'
    ? { account: { id: 'p', kind: props.kind, label: s.label || suggestedLabel(props.kind, s, props.preview.identity ?? ''), identity: props.preview.identity ?? '', created_at: '', updated_at: '' } as unknown as SecretAccount, detail: props.preview.detail ?? '' }
    : null,
);
const showProblems = ref(false);
/** The backend's message as written (Spanish), to know where it sends the user. */
let rawError = '';

const provider = computed(() => PROVIDER[props.kind]);
const steps = computed(() => STEPS[props.kind]);
const stepKey = computed(() => steps.value[s.step - 1]);
const title = computed(() => (props.account ? t('secretAccounts:wizard.reconnectTitle', { label: props.account.label }) : t('secretAccounts:acct.title', { provider: provider.value })));

/** Reconnecting to the same place: a secret left empty keeps the saved one. */
const kept = computed(() => keepsSecrets(props.kind, props.account, s));
const problems = computed(() => stepProblems(props.kind, s).filter((p) => !(kept.value && (SECRET_INPUTS as string[]).includes(p))));
const problemText = computed(() => {
  const p = problems.value;
  if (!p.length) return '';
  const formats = p.filter((k) => k.endsWith('_format'));
  if (formats.length) return formats.map((k) => t(`secretAccounts:acct.format.${k.replace(/_format$/, '')}`)).join(' · ');
  return t('secretAccounts:wizard.missing', { fields: p.map((k) => inputLabel(k)).join(', ') });
});
/** An input's label (some depend on the kind or the mode). */
function inputLabel(k: string): string {
  if (k === 'passphrase' || k === 'password') return t(`secretAccounts:acct.masterInput.${props.kind}`);
  if (k === 'username' && props.kind === 'delinea' && s.delinea_mode === 'platform') return t('secretAccounts:acct.input.client_id_platform');
  if (k === 'master') return t('secretAccounts:wizard.mode');
  return t(`secretAccounts:acct.input.${k}`);
}

/** `«label»` segments of an instruction: odd indexes are the manager's own labels. */
function parts(text: string): string[] {
  return text.split(/«([^»]+)»/);
}

// ---- what this computer has (1Password, AWS, Azure, Google Cloud) --------------------
const detected = ref<Record<string, unknown> | null>(props.preview?.detect ?? null);
const detecting = ref(false);
const DETECTS = ['onepassword', 'aws', 'azure', 'gcp'] as const;
type Detectable = (typeof DETECTS)[number];
async function detect() {
  if (!(DETECTS as readonly string[]).includes(props.kind) || props.preview) return;
  detecting.value = true;
  try {
    detected.value = await accountsApi.detect(props.kind as Detectable);
  } catch (e) {
    detected.value = { error: errorMessage(e) };
  } finally {
    detecting.value = false;
  }
  prefill();
}
/** What was found fills what's still empty. */
function prefill() {
  const d = detected.value;
  if (!d) return;
  const str = (v: unknown) => (typeof v === 'string' ? v : '');
  if (props.kind === 'onepassword' && !s.op_account && Array.isArray(d.accounts) && d.accounts.length === 1) s.op_account = str((d.accounts[0] as Record<string, unknown>).url);
  if (props.kind === 'azure' && !s.tenant && !props.account) s.tenant = str(d.tenant);
  if (props.kind === 'gcp' && !s.project) s.project = str(d.project);
  // The account the tokens are asked for: the one kept, else gcloud's active one.
  if (props.kind === 'gcp') s.gcp_account = gcloudPreselect(gcloudAccounts(d), s.gcp_account);
  if (props.kind === 'aws') {
    if (!s.profile && !props.account && str(d.env_profile)) s.profile = str(d.env_profile);
    if (!s.region) s.region = profileRegion(s.profile) || str(d.region);
  }
}
const opAccounts = computed(() => (Array.isArray(detected.value?.accounts) ? (detected.value!.accounts as Record<string, string>[]) : []));
const awsProfiles = computed(() => (Array.isArray(detected.value?.profiles) ? (detected.value!.profiles as { name: string; region: string }[]) : []));
function profileRegion(name: string): string {
  return awsProfiles.value.find((p) => p.name === (name || 'default'))?.region ?? '';
}
/** Azure: the tenants the CLI knows (the current one among them), filtered by what's typed. */
const tenants = computed(() => azureTenants(detected.value));
const currentTenant = computed(() => (typeof detected.value?.tenant === 'string' ? detected.value.tenant : '').toLowerCase());
const tenantQuery = ref('');
const tenantOptions = computed(() => tenantChoices(tenants.value, tenantQuery.value, s.tenant));
/** Google: the accounts gcloud has signed in (the active one first). */
const gAccounts = computed(() => gcloudAccounts(detected.value));
/** Reconnecting a Google account saved before the gcloud account was kept. */
const unpinned = computed(() => props.kind === 'gcp' && props.account?.kind === 'gcp' && !props.account.account);
watch(() => s.profile, (p, old) => {
  const r = profileRegion(p);
  if (r && (!s.region || s.region === profileRegion(old))) s.region = r;
});
onMounted(detect);

// ---- moving -------------------------------------------------------------------------
function goNext() {
  showProblems.value = true;
  if (problems.value.length) return;
  if (!isLast(props.kind, s)) s.step += 1;
  showProblems.value = false;
  error.value = '';
}
function goBack() {
  back(s);
  error.value = '';
  showProblems.value = false;
}
function goTo(step: number) {
  s.step = step;
  error.value = '';
}
const errorText = computed(() => error.value.charAt(0).toUpperCase() + error.value.slice(1));
const errorStep = computed(() => (error.value ? stepForError(props.kind, rawError || error.value) : null));
/** Azure without a sign-in in the chosen tenant: the command that signs in there (DBine never runs it). */
const signInTenant = computed(() => (props.kind === 'azure' && error.value ? tenantSignIn(rawError || error.value) : null));
/** Google without a valid sign-in for the chosen account: the same, with gcloud. */
const signInGcloud = computed(() => (props.kind === 'gcp' && error.value ? gcloudSignIn(rawError || error.value) : null));
const signInCommand = computed(() => (signInTenant.value ? `az login --tenant ${signInTenant.value}` : signInGcloud.value ? `gcloud auth login ${signInGcloud.value}` : ''));
const signInText = computed(() => t(signInGcloud.value ? 'secretAccounts:acct.gcp.signIn' : 'secretAccounts:acct.azure.signIn', { command: `«${signInCommand.value}»`, interpolation: { escapeValue: false } }));
const copied = ref(false);
async function copyCommand() {
  try {
    await navigator.clipboard.writeText(signInCommand.value);
    copied.value = true;
    ElMessage.success(t('common:copied'));
    setTimeout(() => { copied.value = false; }, 2000);
  } catch { /* no clipboard: the command can still be selected */ }
}

/** The last step: check live and, if it passes, save. */
async function finish() {
  showProblems.value = true;
  if (problems.value.length || busy.value || props.preview) return;
  busy.value = true;
  error.value = '';
  try {
    const r = await accountsApi.connect({ accountId: props.account?.id ?? null, label: s.label.trim(), config: configOf(props.kind, s), secrets: secretsOf(props.kind, s) });
    done.value = r;
    wipe();
    await loadSecretAccounts();
  } catch (e) {
    rawError = e && typeof e === 'object' && 'message' in e ? String((e as { message: unknown }).message) : String(e);
    error.value = errorMessage(e);
  } finally {
    busy.value = false;
  }
}

function wipe() {
  for (const k of SECRET_INPUTS) (s as unknown as Record<string, string>)[k] = '';
}
function close() {
  visible.value = false;
}
function closed() {
  wipe();
  if (done.value && !props.preview) emit('connected', done.value.account);
  emit('close');
}
onBeforeUnmount(wipe);

async function browse(input: 'ca_file' | 'client_cert' | 'client_key' | 'file' | 'key_file') {
  try {
    const filters = input === 'file' ? [{ name: 'KeePass', extensions: ['kdbx'] }] : undefined;
    const p = await openFile({ multiple: false, directory: false, filters });
    if (typeof p === 'string') s[input] = p;
  } catch { /* no dialog outside Tauri */ }
}
function openPage(page: string) {
  accountsApi.openPage(page).catch(() => {});
}

const bwCloud = computed(() => (s.server === 'self' ? '' : s.server));
const dests = computed(() => destinations(props.kind, s));
const labelHint = computed(() => suggestedLabel(props.kind, s, ''));
const doneLabel = computed(() => done.value?.account.label ?? s.label);
const keptPlaceholder = computed(() => (kept.value ? t('secretAccounts:acct.keptPlaceholder') : ''));
const ambient = computed(() => ['onepassword', 'aws', 'azure', 'gcp'].includes(props.kind));
</script>

<template>
  <el-dialog v-model="visible" :title="title" width="640px" class="aw-dialog" append-to-body :close-on-click-modal="false" @closed="closed">
    <ol class="aw-steps" :aria-label="title">
      <li v-for="(st, i) in steps" :key="st" :class="{ on: s.step === i + 1 && !done, past: s.step > i + 1 || !!done }">
        <span class="aw-num">{{ s.step > i + 1 || done ? '✓' : i + 1 }}</span>{{ $t(`secretAccounts:acct.steps.${st}`) }}
      </li>
    </ol>

    <section class="aw-body" :data-step="stepKey" :data-kind="kind">
      <!-- The result. -->
      <template v-if="done">
        <div class="aw-done">
          <el-icon class="aw-done-icon"><ei-circle-check-filled /></el-icon>
          <h4>{{ $t('secretAccounts:wizard.s4.done') }}</h4>
          <p v-if="done.account.identity" class="aw-who" data-identity>{{ done.account.identity }}</p>
          <p v-if="done.detail" class="aw-muted">{{ tb(done.detail) }}</p>
          <p class="aw-muted">{{ $t('secretAccounts:acct.doneHelp', { label: doneLabel, provider }) }}</p>
        </div>
      </template>

      <!-- Bitwarden (and Secrets Manager): the server. -->
      <template v-else-if="stepKey === 'server' && (kind === 'bitwarden' || kind === 'bitwarden_sm')">
        <h4>{{ $t('secretAccounts:acct.bitwarden.server.title') }}</h4>
        <p class="aw-muted">{{ $t('secretAccounts:acct.bitwarden.server.intro') }}</p>
        <el-radio-group v-model="s.server" class="aw-modes">
          <el-radio value="bitwarden.com">{{ $t('secretAccounts:acct.bitwarden.server.us') }}</el-radio>
          <el-radio value="bitwarden.eu">{{ $t('secretAccounts:acct.bitwarden.server.eu') }}</el-radio>
          <el-radio value="self">{{ $t('secretAccounts:acct.bitwarden.server.self') }}</el-radio>
        </el-radio-group>
        <el-form v-if="s.server === 'self'" label-position="top" class="aw-form" @submit.prevent="goNext">
          <el-form-item :label="$t('secretAccounts:acct.input.server_url')" required>
            <el-input v-model="s.server_url" placeholder="https://bitwarden.example.com" spellcheck="false" />
          </el-form-item>
        </el-form>
      </template>

      <!-- Bitwarden: the personal API key. -->
      <template v-else-if="stepKey === 'apiKey'">
        <h4>{{ $t('secretAccounts:acct.bitwarden.apiKey.title') }}</h4>
        <p class="aw-muted">{{ $t('secretAccounts:acct.bitwarden.apiKey.intro') }}</p>
        <el-button v-if="bwCloud" type="primary" plain class="aw-open" @click="openPage(bwCloud)"><el-icon><ei-link /></el-icon>{{ $t('secretAccounts:acct.bitwarden.apiKey.open') }}</el-button>
        <ol class="aw-list">
          <li v-for="k in ['a', 'b', 'c']" :key="k">
            <template v-for="(p, i) in parts($t(`secretAccounts:acct.bitwarden.apiKey.${k}`))" :key="i"><strong v-if="i % 2" class="aw-z">{{ p }}</strong><span v-else>{{ p }}</span></template>
          </li>
        </ol>
        <el-form label-position="top" class="aw-form" @submit.prevent="goNext">
          <el-form-item :label="$t('secretAccounts:acct.input.client_id')" required>
            <el-input v-model="s.client_id" class="aw-mono" placeholder="user.xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx" spellcheck="false" autocomplete="off" />
          </el-form-item>
          <el-form-item :label="$t('secretAccounts:acct.input.client_secret')" :required="!kept">
            <el-input v-model="s.client_secret" type="password" show-password autocomplete="new-password" class="aw-mono" :placeholder="keptPlaceholder" />
          </el-form-item>
        </el-form>
        <p class="aw-muted aw-note">{{ $t('secretAccounts:acct.bitwarden.apiKey.note') }}</p>
      </template>

      <!-- A master password (Bitwarden, KeePass) or Delinea's password. -->
      <template v-else-if="stepKey === 'master' || stepKey === 'user'">
        <h4>{{ $t(`secretAccounts:acct.${kind}.${stepKey}.title`) }}</h4>
        <p class="aw-muted">{{ $t(`secretAccounts:acct.${kind}.${stepKey}.intro`) }}</p>
        <template v-if="kind === 'delinea'">
          <ol v-if="s.delinea_mode === 'platform'" class="aw-list">
            <li><template v-for="(p, i) in parts($t('secretAccounts:acct.delinea.user.platform'))" :key="i"><strong v-if="i % 2" class="aw-z">{{ p }}</strong><span v-else>{{ p }}</span></template></li>
          </ol>
          <el-form label-position="top" class="aw-form aw-cols" @submit.prevent="goNext">
            <el-form-item :label="inputLabel('username')" required>
              <el-input v-model="s.username" class="aw-mono" :placeholder="s.delinea_mode === 'platform' ? 'dbine@acme' : 'svc_dbine'" spellcheck="false" />
            </el-form-item>
            <el-form-item v-if="s.delinea_mode === 'server'" :label="$t('secretAccounts:acct.input.domain')">
              <el-input v-model="s.domain" class="aw-mono" spellcheck="false" />
            </el-form-item>
          </el-form>
        </template>
        <el-form label-position="top" class="aw-form" @submit.prevent="isLast(kind, s) ? finish() : goNext()">
          <el-form-item :label="$t('secretAccounts:wizard.mode')">
            <el-radio-group v-model="s.master" class="aw-modes">
              <el-radio value="keychain">{{ $t('connection:source.master.keychain') }}</el-radio>
              <el-radio value="ask">{{ $t('connection:source.master.ask') }}</el-radio>
              <el-radio value="command">{{ $t('connection:source.master.command') }}</el-radio>
              <el-radio v-if="kind === 'keepass'" value="none">{{ $t('connection:source.master.none') }}</el-radio>
            </el-radio-group>
          </el-form-item>
          <el-form-item v-if="s.master === 'command'" :label="$t('secretAccounts:wizard.commandInput')" required>
            <el-input v-model="s.master_command" class="aw-mono" :placeholder="$t('connection:source.placeholder.command')" spellcheck="false" />
          </el-form-item>
          <el-form-item v-else-if="s.master !== 'none'" :label="inputLabel(kind === 'delinea' ? 'password' : 'passphrase')" :required="!(kept && s.master === 'keychain')">
            <el-input v-if="kind === 'delinea'" v-model="s.password" type="password" show-password autocomplete="new-password" :placeholder="s.master === 'keychain' ? keptPlaceholder : ''" />
            <el-input v-else v-model="s.passphrase" type="password" show-password autocomplete="new-password" :placeholder="s.master === 'keychain' ? keptPlaceholder : ''" />
          </el-form-item>
          <p class="aw-muted aw-help">{{ $t(`secretAccounts:acct.masterHelp.${s.master}`) }}</p>
        </el-form>
      </template>

      <!-- Bitwarden Secrets Manager: the machine account's access token. -->
      <template v-else-if="stepKey === 'token'">
        <h4>{{ $t('secretAccounts:acct.bitwarden_sm.token.title') }}</h4>
        <p class="aw-muted">{{ $t('secretAccounts:acct.bitwarden_sm.token.intro') }}</p>
        <el-button v-if="bwCloud" type="primary" plain class="aw-open" @click="openPage(bwCloud)"><el-icon><ei-link /></el-icon>{{ $t('secretAccounts:acct.bitwarden.apiKey.open') }}</el-button>
        <ol class="aw-list">
          <li v-for="k in ['a', 'b', 'c', 'd']" :key="k">
            <template v-for="(p, i) in parts($t(`secretAccounts:acct.bitwarden_sm.token.${k}`))" :key="i"><strong v-if="i % 2" class="aw-z">{{ p }}</strong><span v-else>{{ p }}</span></template>
          </li>
        </ol>
        <el-form label-position="top" class="aw-form" @submit.prevent="goNext">
          <el-form-item :label="$t('secretAccounts:acct.input.token')" :required="!kept">
            <el-input v-model="s.token" type="password" show-password autocomplete="new-password" class="aw-mono" :placeholder="keptPlaceholder || '0.xxxxxxxx-….…:…'" />
          </el-form-item>
        </el-form>
      </template>

      <!-- HashiCorp Vault, Delinea, Conjur, the CCP: where the server is. -->
      <template v-else-if="stepKey === 'server'">
        <h4>{{ $t(`secretAccounts:acct.${kind}.server.title`) }}</h4>
        <p class="aw-muted">{{ $t(`secretAccounts:acct.${kind}.server.intro`) }}</p>
        <el-radio-group v-if="kind === 'delinea'" v-model="s.delinea_mode" class="aw-modes">
          <el-radio value="server">{{ $t('secretAccounts:acct.delinea.server.modeServer') }}</el-radio>
          <el-radio value="platform">{{ $t('secretAccounts:acct.delinea.server.modePlatform') }}</el-radio>
        </el-radio-group>
        <el-form label-position="top" class="aw-form aw-cols" @submit.prevent="goNext">
          <el-form-item v-if="kind === 'vault'" :label="$t('secretAccounts:acct.input.address')" required class="aw-wide">
            <el-input v-model="s.address" placeholder="https://vault.example.com:8200" spellcheck="false" />
          </el-form-item>
          <el-form-item v-else :label="$t(`secretAccounts:acct.input.url_${kind}`)" required class="aw-wide">
            <el-input v-model="s.url" :placeholder="$t(`secretAccounts:acct.placeholder.url_${kind}`)" spellcheck="false" />
          </el-form-item>
          <el-form-item v-if="kind === 'delinea' && s.delinea_mode === 'platform'" :label="$t('secretAccounts:acct.input.platform_url')" required class="aw-wide">
            <el-input v-model="s.platform_url" placeholder="https://acme.delinea.app" spellcheck="false" />
          </el-form-item>
          <el-form-item v-if="kind === 'vault'" :label="$t('secretAccounts:acct.input.namespace')">
            <el-input v-model="s.namespace" placeholder="admin/team" spellcheck="false" />
          </el-form-item>
          <el-form-item v-if="kind === 'conjur'" :label="$t('secretAccounts:acct.input.conjur_account')" required>
            <el-input v-model="s.conjur_account" class="aw-mono" placeholder="myorg" spellcheck="false" />
          </el-form-item>
          <el-form-item v-if="kind === 'ccp'" :label="$t('secretAccounts:acct.input.app_id')" required>
            <el-input v-model="s.app_id" class="aw-mono" placeholder="DBine" spellcheck="false" />
          </el-form-item>
          <el-form-item v-if="kind !== 'delinea'" :label="$t('secretAccounts:acct.input.ca_file')">
            <el-input v-model="s.ca_file" placeholder="~/ca.pem" spellcheck="false">
              <template #append><el-button @click="browse('ca_file')"><el-icon><ei-folder-opened /></el-icon></el-button></template>
            </el-input>
          </el-form-item>
        </el-form>
        <p class="aw-muted aw-note">{{ $t(`secretAccounts:acct.${kind}.server.note`) }}</p>
      </template>

      <!-- HashiCorp Vault: a token or AppRole. -->
      <template v-else-if="stepKey === 'auth'">
        <h4>{{ $t('secretAccounts:acct.vault.auth.title') }}</h4>
        <el-radio-group v-model="s.auth" class="aw-modes">
          <el-radio value="token">{{ $t('connection:source.vaultAuth.token') }}</el-radio>
          <el-radio value="approle">{{ $t('connection:source.vaultAuth.approle') }}</el-radio>
        </el-radio-group>
        <el-form label-position="top" class="aw-form aw-cols" @submit.prevent="goNext">
          <template v-if="s.auth === 'token'">
            <el-form-item :label="$t('secretAccounts:acct.input.token_vault')" class="aw-wide">
              <el-input v-model="s.token" type="password" show-password autocomplete="new-password" class="aw-mono" :placeholder="keptPlaceholder || 'hvs.…'" />
            </el-form-item>
          </template>
          <template v-else>
            <el-form-item :label="$t('secretAccounts:acct.input.role_id')" required>
              <el-input v-model="s.role_id" class="aw-mono" spellcheck="false" />
            </el-form-item>
            <el-form-item :label="$t('secretAccounts:acct.input.secret_id')">
              <el-input v-model="s.secret_id" type="password" show-password autocomplete="new-password" class="aw-mono" :placeholder="keptPlaceholder" />
            </el-form-item>
            <el-form-item :label="$t('secretAccounts:acct.input.approle_mount')">
              <el-input v-model="s.approle_mount" class="aw-mono" placeholder="approle" spellcheck="false" />
            </el-form-item>
          </template>
        </el-form>
        <ol class="aw-list">
          <li v-for="k in (s.auth === 'token' ? ['token', 'login'] : ['roleId', 'secretId'])" :key="k">
            <template v-for="(p, i) in parts($t(`secretAccounts:acct.vault.auth.${k}`))" :key="i"><code v-if="i % 2" class="aw-code">{{ p }}</code><span v-else>{{ p }}</span></template>
          </li>
        </ol>
      </template>

      <!-- Conjur: the host or user and its API key. -->
      <template v-else-if="stepKey === 'identity'">
        <h4>{{ $t('secretAccounts:acct.conjur.identity.title') }}</h4>
        <p class="aw-muted">{{ $t('secretAccounts:acct.conjur.identity.intro') }}</p>
        <el-form label-position="top" class="aw-form" @submit.prevent="goNext">
          <el-form-item :label="$t('secretAccounts:acct.input.login')" required>
            <el-input v-model="s.login" class="aw-mono" placeholder="host/apps/dbine" spellcheck="false" />
          </el-form-item>
          <el-form-item :label="$t('secretAccounts:acct.input.api_key')" :required="!kept">
            <el-input v-model="s.api_key" type="password" show-password autocomplete="new-password" class="aw-mono" :placeholder="keptPlaceholder" />
          </el-form-item>
        </el-form>
      </template>

      <!-- The CCP: the client certificate. -->
      <template v-else-if="stepKey === 'certificate'">
        <h4>{{ $t('secretAccounts:acct.ccp.certificate.title') }}</h4>
        <p class="aw-muted">{{ $t('secretAccounts:acct.ccp.certificate.intro') }}</p>
        <el-form label-position="top" class="aw-form" @submit.prevent="goNext">
          <el-form-item :label="$t('secretAccounts:acct.input.client_cert')">
            <el-input v-model="s.client_cert" placeholder="~/dbine.p12" spellcheck="false">
              <template #append><el-button @click="browse('client_cert')"><el-icon><ei-folder-opened /></el-icon></el-button></template>
            </el-input>
          </el-form-item>
          <el-form-item :label="$t('secretAccounts:acct.input.client_key')">
            <el-input v-model="s.client_key" placeholder="~/dbine.key" spellcheck="false" :disabled="!s.client_cert.trim()">
              <template #append><el-button :disabled="!s.client_cert.trim()" @click="browse('client_key')"><el-icon><ei-folder-opened /></el-icon></el-button></template>
            </el-input>
          </el-form-item>
          <el-form-item :label="$t('secretAccounts:acct.input.cert_passphrase')">
            <el-input v-model="s.passphrase" type="password" show-password autocomplete="new-password" :disabled="!s.client_cert.trim()" :placeholder="keptPlaceholder" />
          </el-form-item>
        </el-form>
        <p class="aw-muted aw-note">{{ $t('secretAccounts:acct.ccp.certificate.note') }}</p>
      </template>

      <!-- 1Password: the CLI and its account. -->
      <template v-else-if="stepKey === 'cli'">
        <h4>{{ $t('secretAccounts:acct.onepassword.cli.title') }}</h4>
        <p class="aw-muted">{{ $t('secretAccounts:acct.onepassword.cli.intro') }}</p>
        <div v-if="detecting" class="aw-status"><el-icon class="is-loading"><ei-loading /></el-icon>{{ $t('secretAccounts:acct.detecting') }}</div>
        <template v-else-if="detected && detected.installed === false">
          <div class="aw-status bad" role="alert"><el-icon><ei-warning-filled /></el-icon>{{ $t('secretAccounts:acct.onepassword.cli.missing') }}</div>
          <el-button type="primary" plain class="aw-open" @click="openPage('onepassword-cli')"><el-icon><ei-link /></el-icon>{{ $t('secretAccounts:acct.onepassword.cli.install') }}</el-button>
          <el-button @click="detect">{{ $t('secretAccounts:acct.retryDetect') }}</el-button>
        </template>
        <template v-else>
          <el-radio-group v-if="opAccounts.length" v-model="s.op_account" class="aw-modes aw-accounts">
            <el-radio v-for="a in opAccounts" :key="a.url || a.account_uuid" :value="a.url || a.account_uuid">
              <span class="aw-mono-text">{{ a.url || a.account_uuid }}</span><span v-if="a.email" class="aw-dim"> · {{ a.email }}</span>
            </el-radio>
          </el-radio-group>
          <template v-else>
            <div class="aw-status bad"><el-icon><ei-warning-filled /></el-icon>{{ $t('secretAccounts:acct.onepassword.cli.none') }}</div>
            <ol class="aw-list">
              <li v-for="k in ['a', 'b', 'c']" :key="k">
                <template v-for="(p, i) in parts($t(`secretAccounts:acct.onepassword.cli.${k}`))" :key="i"><strong v-if="i % 2" class="aw-z">{{ p }}</strong><span v-else>{{ p }}</span></template>
              </li>
            </ol>
            <el-button type="primary" plain class="aw-open" @click="openPage('onepassword-app')"><el-icon><ei-link /></el-icon>{{ $t('secretAccounts:acct.onepassword.cli.guide') }}</el-button>
            <el-button @click="detect">{{ $t('secretAccounts:acct.retryDetect') }}</el-button>
          </template>
          <el-form label-position="top" class="aw-form" @submit.prevent="goNext">
            <el-form-item :label="$t('secretAccounts:acct.input.op_account')" required>
              <el-input v-model="s.op_account" class="aw-mono" placeholder="my.1password.com" spellcheck="false" />
            </el-form-item>
          </el-form>
        </template>
      </template>

      <!-- AWS: a profile and a region. -->
      <template v-else-if="stepKey === 'profile'">
        <h4>{{ $t('secretAccounts:acct.aws.profile.title') }}</h4>
        <p class="aw-muted">{{ $t('secretAccounts:acct.aws.profile.intro') }}</p>
        <div v-if="detecting" class="aw-status"><el-icon class="is-loading"><ei-loading /></el-icon>{{ $t('secretAccounts:acct.detecting') }}</div>
        <el-form label-position="top" class="aw-form aw-cols" @submit.prevent="goNext">
          <el-form-item :label="$t('secretAccounts:acct.input.profile')">
            <el-select v-model="s.profile" filterable allow-create default-first-option style="width: 100%" :placeholder="$t('secretAccounts:acct.aws.profile.default')">
              <el-option value="" :label="$t('secretAccounts:acct.aws.profile.default')" />
              <el-option v-for="p in awsProfiles" :key="p.name" :value="p.name" :label="p.name">
                <span>{{ p.name }}</span><span v-if="p.region" class="aw-opt-detail">{{ p.region }}</span>
              </el-option>
            </el-select>
          </el-form-item>
          <el-form-item :label="$t('secretAccounts:acct.input.region')" required>
            <el-input v-model="s.region" placeholder="us-east-1" spellcheck="false" />
          </el-form-item>
        </el-form>
        <p v-if="detected && !awsProfiles.length && !detecting" class="aw-muted">{{ $t('secretAccounts:acct.aws.profile.none') }}</p>
        <p class="aw-muted aw-note">{{ $t('secretAccounts:acct.aws.profile.note') }}</p>
      </template>

      <!-- Azure Key Vault: the vault and the tenant. -->
      <template v-else-if="stepKey === 'vault'">
        <h4>{{ $t('secretAccounts:acct.azure.vault.title') }}</h4>
        <p class="aw-muted">{{ $t('secretAccounts:acct.azure.vault.intro') }}</p>
        <div v-if="detecting" class="aw-status"><el-icon class="is-loading"><ei-loading /></el-icon>{{ $t('secretAccounts:acct.detecting') }}</div>
        <div v-else-if="detected && !detected.error" class="aw-status ok" data-detected>
          <el-icon><ei-circle-check-filled /></el-icon>{{ $t('secretAccounts:acct.azure.vault.found', { user: detected.user, tenant: detected.tenant, subscription: detected.subscription }) }}
        </div>
        <div v-else-if="detected" class="aw-status bad"><el-icon><ei-warning-filled /></el-icon>{{ $t('secretAccounts:acct.azure.vault.notFound') }}</div>
        <ol class="aw-list">
          <li><template v-for="(p, i) in parts($t('secretAccounts:acct.azure.vault.a'))" :key="i"><strong v-if="i % 2" class="aw-z">{{ p }}</strong><span v-else>{{ p }}</span></template></li>
        </ol>
        <el-form label-position="top" class="aw-form" @submit.prevent="goNext">
          <el-form-item :label="$t('secretAccounts:acct.input.vault_url')" required>
            <el-input v-model="s.vault_url" placeholder="https://my-vault.vault.azure.net" spellcheck="false" />
          </el-form-item>
          <el-form-item :label="$t('secretAccounts:acct.input.tenant')">
            <el-select
              v-model="s.tenant" filterable default-first-option clearable style="width: 100%" data-tenant
              :filter-method="(q: string) => (tenantQuery = q)" popper-class="aw-tenant-popper"
              :placeholder="tenants.length ? $t('secretAccounts:acct.azure.vault.tenantPlaceholder') : 'contoso.onmicrosoft.com'"
              @visible-change="(open: boolean) => { if (!open) tenantQuery = ''; }"
            >
              <el-option v-for="tn in tenantOptions" :key="tn.id" :value="tn.id" :label="tenantLabel(tn)">
                <div class="aw-tenant-opt">
                  <span class="aw-tenant-name">
                    <span :class="{ 'aw-tenant-typed': tn.typed }">{{ tn.display_name || tn.default_domain || tn.id }}</span>
                    <span v-if="tn.display_name && tn.default_domain" class="aw-tenant-domain">{{ tn.default_domain }}</span>
                    <span v-if="tn.id.toLowerCase() === currentTenant" class="aw-tenant-current">{{ $t('secretAccounts:acct.azure.vault.current') }}</span>
                  </span>
                  <span v-if="!tn.typed" class="aw-tenant-id">{{ tn.id }}</span>
                </div>
              </el-option>
            </el-select>
          </el-form-item>
        </el-form>
        <p class="aw-muted aw-note">{{ $t('secretAccounts:acct.azure.vault.perAccount') }}</p>
      </template>

      <!-- Google Secret Manager: the project. -->
      <template v-else-if="stepKey === 'project'">
        <h4>{{ $t('secretAccounts:acct.gcp.project.title') }}</h4>
        <p class="aw-muted">{{ $t('secretAccounts:acct.gcp.project.intro') }}</p>
        <div v-if="detecting" class="aw-status"><el-icon class="is-loading"><ei-loading /></el-icon>{{ $t('secretAccounts:acct.detecting') }}</div>
        <div v-else-if="detected && !detected.error && detected.account" class="aw-status ok" data-detected>
          <el-icon><ei-circle-check-filled /></el-icon>{{ $t('secretAccounts:acct.gcp.project.found', { account: detected.account, project: detected.project || '—' }) }}
        </div>
        <div v-else-if="detected" class="aw-status bad"><el-icon><ei-warning-filled /></el-icon>{{ $t('secretAccounts:acct.gcp.project.notFound') }}</div>
        <el-form label-position="top" class="aw-form" @submit.prevent="goNext">
          <el-form-item :label="$t('secretAccounts:acct.input.project')" required>
            <el-input v-model="s.project" class="aw-mono" placeholder="my-project" spellcheck="false" />
          </el-form-item>
          <el-form-item :label="$t('secretAccounts:acct.input.gcp_account')">
            <el-select
              v-model="s.gcp_account" filterable allow-create default-first-option clearable style="width: 100%" data-gcp-account
              :placeholder="gAccounts.length ? $t('secretAccounts:acct.gcp.project.accountPlaceholder') : 'ana@example.com'"
            >
              <el-option v-for="ga in gAccounts" :key="ga.account" :value="ga.account" :label="ga.account">
                <span class="aw-tenant-name">
                  <span>{{ ga.account }}</span>
                  <span v-if="ga.active" class="aw-tenant-current">{{ $t('secretAccounts:acct.azure.vault.current') }}</span>
                </span>
              </el-option>
            </el-select>
          </el-form-item>
        </el-form>
        <p v-if="unpinned" class="aw-muted aw-note" data-unpinned>{{ $t('secretAccounts:acct.gcp.project.unpinned') }}</p>
        <p class="aw-muted aw-note">{{ $t('secretAccounts:acct.gcp.project.accountHelp') }}</p>
      </template>

      <!-- KeePass: the database. -->
      <template v-else-if="stepKey === 'database'">
        <h4>{{ $t('secretAccounts:acct.keepass.database.title') }}</h4>
        <p class="aw-muted">{{ $t('secretAccounts:acct.keepass.database.intro') }}</p>
        <el-form label-position="top" class="aw-form" @submit.prevent="goNext">
          <el-form-item :label="$t('secretAccounts:acct.input.file')" required>
            <el-input v-model="s.file" placeholder="~/Passwords/databases.kdbx" spellcheck="false">
              <template #append><el-button @click="browse('file')"><el-icon><ei-folder-opened /></el-icon></el-button></template>
            </el-input>
          </el-form-item>
          <el-form-item :label="$t('secretAccounts:acct.input.key_file')">
            <el-input v-model="s.key_file" placeholder="~/Passwords/databases.keyx" spellcheck="false">
              <template #append><el-button @click="browse('key_file')"><el-icon><ei-folder-opened /></el-icon></el-button></template>
            </el-input>
          </el-form-item>
        </el-form>
      </template>

      <!-- The last step: check live, then save. -->
      <template v-else-if="stepKey === 'connect'">
        <h4>{{ $t('secretAccounts:acct.connect.title') }}</h4>
        <p class="aw-muted">{{ $t(`secretAccounts:acct.connect.${kind}`) }}</p>
        <div class="aw-dest" data-destinations>
          <span>{{ $t(ambient ? 'secretAccounts:acct.connect.destAmbient' : 'secretAccounts:acct.connect.dest') }}</span>
          <code v-for="d in dests" :key="d">{{ d }}</code>
        </div>
        <el-form label-position="top" class="aw-form" @submit.prevent="finish">
          <el-form-item :label="$t('secretAccounts:wizard.label')">
            <el-input v-model="s.label" maxlength="200" :placeholder="labelHint" />
          </el-form-item>
        </el-form>
        <p class="aw-muted aw-help">{{ $t('secretAccounts:acct.connect.labelHelp') }}</p>
        <div v-if="busy" class="aw-status"><el-icon class="is-loading"><ei-loading /></el-icon>{{ $t('secretAccounts:acct.connect.checking', { provider }) }}</div>
      </template>
    </section>

    <div v-if="error && !busy && !done" class="aw-error-actions">
      <div v-if="signInCommand" class="aw-signin" role="alert" data-signin>
        <div class="aw-error">{{ signInText }}</div>
        <div class="aw-cmd"><code>{{ signInCommand }}</code><el-button size="small" data-copy @click="copyCommand">{{ copied ? $t('common:copied') : $t('common:copy') }}</el-button></div>
      </div>
      <div v-else class="aw-error" role="alert">{{ errorText }}</div>
      <el-button v-if="errorStep && errorStep !== s.step" size="small" @click="goTo(errorStep)">{{ $t('secretAccounts:acct.toStep', { step: $t(`secretAccounts:acct.steps.${steps[errorStep - 1]}`) }) }}</el-button>
    </div>
    <div v-else-if="showProblems && problemText && !done" class="aw-error" role="alert">{{ problemText }}</div>

    <template #footer>
      <template v-if="done">
        <el-button type="primary" @click="close">{{ $t('secretAccounts:wizard.close') }}</el-button>
      </template>
      <template v-else>
        <el-button @click="close">{{ $t('common:cancel') }}</el-button>
        <el-button v-if="s.step > 1" :disabled="busy" @click="goBack">{{ $t('secretAccounts:wizard.back') }}</el-button>
        <el-button v-if="!isLast(kind, s)" type="primary" :disabled="busy" @click="goNext">{{ $t('secretAccounts:wizard.next') }}</el-button>
        <el-button v-else type="primary" :loading="busy" data-connect @click="finish">{{ $t('secretAccounts:wizard.s4.finish') }}</el-button>
      </template>
    </template>
  </el-dialog>
</template>

<style scoped lang="scss">
.aw-steps {
  display: flex; gap: 6px; list-style: none; margin: -4px 0 14px; padding: 0;
  li {
    flex: 1; display: flex; align-items: center; gap: 6px; font-size: 12px; color: var(--nm-text-dim); min-width: 0;
    padding-bottom: 6px; border-bottom: 2px solid var(--nm-border-soft); white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
    &.on { color: var(--nm-text-strong); border-color: var(--el-color-primary); }
    &.past { color: var(--nm-text); border-color: var(--el-color-primary-light-5); }
  }
}
.aw-num {
  flex: none; display: inline-flex; align-items: center; justify-content: center; width: 18px; height: 18px; border-radius: 50%;
  font-size: 11px; background: var(--nm-border-soft); color: var(--nm-text);
  .on & { background: var(--el-color-primary); color: #fff; }
}
.aw-body { min-height: 120px; h4 { margin: 0 0 6px; font-size: 14px; color: var(--nm-text-strong); } }
.aw-muted { font-size: 12.5px; color: var(--nm-text-dim); line-height: 1.45; margin: 0 0 10px; }
.aw-dim { color: var(--nm-text-dim); }
.aw-help { margin-top: -6px; }
.aw-note { margin-top: 2px; }
.aw-open { margin: 0 8px 4px 0; }
.aw-list {
  margin: 10px 0 12px; padding-left: 20px; font-size: 12.5px; line-height: 1.5; color: var(--nm-text);
  li { margin-bottom: 6px; }
}
.aw-z { font-weight: 600; color: var(--nm-text-strong); }
.aw-code {
  padding: 1px 5px; border-radius: 4px; background: var(--nm-bg-soft, var(--el-fill-color-light)); border: 1px solid var(--nm-border-soft);
  font-size: 11.5px; font-family: var(--nm-font-mono, ui-monospace, monospace); user-select: all; word-break: break-all;
}
.aw-form { margin-top: 6px; }
.aw-cols { display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 1fr); column-gap: 14px; }
.aw-wide { grid-column: 1 / -1; }
.aw-mono :deep(input) { font-family: var(--nm-font-mono, ui-monospace, monospace); font-size: 12px; }
.aw-mono-text { font-family: var(--nm-font-mono, ui-monospace, monospace); font-size: 12px; }
.aw-modes { display: flex; flex-direction: column; align-items: flex-start; gap: 2px; margin-bottom: 6px; }
.aw-accounts { margin: 4px 0 8px; }
.aw-opt-detail { margin-left: 8px; font-size: 11.5px; color: var(--nm-text-dim); }
.aw-status {
  display: flex; align-items: center; gap: 8px; font-size: 12.5px; margin: 8px 0;
  &.ok { color: var(--nm-success); }
  &.bad { color: var(--nm-warning, #d29922); }
}
.aw-dest {
  display: flex; flex-wrap: wrap; align-items: center; gap: 6px; font-size: 12px; color: var(--nm-text-dim); margin: 0 0 10px;
  code {
    padding: 1px 6px; border-radius: 4px; background: var(--nm-bg-soft, var(--el-fill-color-light)); border: 1px solid var(--nm-border-soft);
    font-family: var(--nm-font-mono, ui-monospace, monospace); font-size: 11.5px; color: var(--nm-text);
  }
}
.aw-error { margin-top: 10px; font-size: 12.5px; color: var(--nm-danger); white-space: pre-wrap; word-break: break-word; }
.aw-error-actions { margin-top: 10px; .aw-error { margin: 0 0 8px; } }
.aw-cmd {
  display: flex; align-items: center; gap: 10px; margin: 0 0 8px; padding: 6px 6px 6px 10px; border-radius: 4px;
  background: var(--nm-bg-soft, var(--el-fill-color-light)); border: 1px solid var(--nm-border-soft);
  code { flex: 1; min-width: 0; font-family: var(--nm-font-mono, ui-monospace, monospace); font-size: 12px; color: var(--nm-text-strong); user-select: all; word-break: break-all; }
}
.aw-done { text-align: center; padding: 18px 0 6px; h4 { font-size: 15px; } }
.aw-done-icon { font-size: 34px; color: var(--nm-success); margin-bottom: 6px; }
.aw-who { font-family: var(--nm-font-mono, ui-monospace, monospace); font-size: 13px; color: var(--nm-text-strong); margin: 0 0 8px; word-break: break-all; }
</style>

<style lang="scss">
/* The tenant select's list (teleported, so not scoped): name and domain, the ID below. */
.aw-tenant-popper .el-select-dropdown__item:has(.aw-tenant-opt) { height: auto; padding-top: 5px; padding-bottom: 5px; line-height: 1.35; }
.aw-tenant-opt { display: flex; flex-direction: column; min-width: 0; }
.aw-tenant-name { display: flex; align-items: baseline; gap: 8px; min-width: 0; }
.aw-tenant-domain { font-size: 11.5px; color: var(--nm-text-dim); font-weight: normal; }
.aw-tenant-current {
  font-size: 10.5px; padding: 0 5px; border-radius: 3px; color: var(--el-color-primary); border: 1px solid var(--el-color-primary-light-5); font-weight: normal;
}
.aw-tenant-typed { font-family: var(--nm-font-mono, ui-monospace, monospace); font-size: 12px; }
.aw-tenant-id { font-family: var(--nm-font-mono, ui-monospace, monospace); font-size: 11px; color: var(--nm-text-dim); font-weight: normal; }
</style>
