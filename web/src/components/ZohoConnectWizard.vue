<script setup lang="ts">
import { computed, onBeforeUnmount, reactive, ref, watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import { accountsApi } from '../api/secretAccounts';
import { errorMessage } from '../api/client';
import type { SecretAccount } from '../api/types';
import { loadSecretAccounts, zohoDomain } from '../composables/secretAccounts';
import {
  back, canAdvance, emptyWizard, next, stepForError, stepProblems, suggestedLabel, ZOHO_API_CONSOLE, ZOHO_SCOPES, type WizardStep,
} from '../composables/zohoWizard';

// "Conectar Zoho Vault" (docs/secret-sources.md, "Accounts"): the user's own
// Self Client, set up once. Four steps, each checked before the next; the
// code is exchanged by DBine (which finds the data centre) and nothing is
// kept until the passphrase opens the account. Closing it before the end
// leaves nothing behind (the exchange is dropped and its token revoked).
//
// `account`: reconnect that account (new code, same ID: its connections go on).
// `initialStep` / `preview`: the component preview only.

const props = defineProps<{ account?: SecretAccount | null; initialStep?: WizardStep; preview?: Partial<ReturnType<typeof emptyWizard>> & { result?: 'ok' | 'error'; error?: string } }>();
const emit = defineEmits<{ close: []; connected: [account: SecretAccount] }>();
const { t } = useTranslation();

const s = reactive(emptyWizard());
if (props.account) {
  const zoho = props.account as Extract<SecretAccount, { kind: 'zoho' }>;
  s.label = zoho.label;
  s.master = zoho.master ?? 'keychain';
  s.master_command = zoho.master_command ?? '';
}
if (props.preview) Object.assign(s, props.preview);
if (props.initialStep) s.step = props.initialStep;

const visible = ref(true);
const busy = ref(false);
const error = ref(props.preview?.error ?? '');
const done = ref<SecretAccount | null>(null);
const showProblems = ref(false);
const copied = ref(false);

const STEPS: { n: WizardStep; key: string }[] = [
  { n: 1, key: 'client' }, { n: 2, key: 'code' }, { n: 3, key: 'connect' }, { n: 4, key: 'passphrase' },
];

/** `«label»` segments of an instruction: odd indexes are Zoho's own labels. */
function parts(text: string): string[] {
  return text.split(/«([^»]+)»/);
}

const problems = computed(() => stepProblems(s));
const problemText = computed(() => {
  const p = problems.value.filter((k) => k !== 'connect');
  if (!p.length) return '';
  const formats = p.filter((k) => k.endsWith('_format'));
  if (formats.length) return formats.map((k) => t(`secretAccounts:wizard.${k}`)).join(' · ');
  return t('secretAccounts:wizard.missing', { fields: p.map((k) => t(`secretAccounts:wizard.${k === 'passphrase' ? 'passphraseInput' : k === 'master_command' ? 'commandInput' : k}`)).join(', ') });
});

function openConsole() {
  accountsApi.openZohoConsole().catch(() => window.open(ZOHO_API_CONSOLE, '_blank'));
}

async function copyScopes() {
  try {
    await navigator.clipboard.writeText(ZOHO_SCOPES);
    copied.value = true;
    setTimeout(() => { copied.value = false; }, 1500);
  } catch { /* no clipboard */ }
}

/** Step 3: exchange the code (the data centre is found by DBine). */
/** The backend's message as written (Spanish), to know where it sends the user. */
let rawError = '';
async function connect() {
  if (busy.value || s.pending) return;
  busy.value = true;
  error.value = '';
  try {
    const r = await accountsApi.zohoExchange(s.client_id.trim(), s.client_secret.trim(), s.code.trim());
    s.pending = r.pending;
    s.data_center = r.data_center;
    s.identity = r.identity;
    s.label = suggestedLabel(r.identity, s.label);
  } catch (e) {
    rawError = e && typeof e === 'object' && 'message' in e ? String((e as { message: unknown }).message) : String(e);
    error.value = errorMessage(e);
  } finally {
    busy.value = false;
  }
}
/** Backend messages start in lower case (they're also embedded in others). */
const errorText = computed(() => error.value.charAt(0).toUpperCase() + error.value.slice(1));
const errorStep = computed(() => (error.value ? stepForError(rawError || error.value) : null));

function goNext() {
  showProblems.value = true;
  if (!canAdvance(s)) return;
  next(s);
  showProblems.value = false;
  error.value = '';
  if (s.step === 3 && !s.pending && !props.preview) connect();
}

function goBack() {
  const { dropPending } = back(s);
  if (dropPending) accountsApi.zohoCancel(dropPending).catch(() => {});
  error.value = '';
  showProblems.value = false;
}

/** From an error: back to the step that fixes it. */
function goTo(step: WizardStep) {
  while (s.step > step) goBack();
}

/** Step 4: check the passphrase against the account, then save it. */
async function finish() {
  showProblems.value = true;
  if (!canAdvance(s) || !s.pending || busy.value) return;
  busy.value = true;
  error.value = '';
  try {
    const a = await accountsApi.zohoFinish({
      pending: s.pending, accountId: props.account?.id ?? null, label: s.label.trim(), master: s.master,
      masterCommand: s.master_command, passphrase: s.master === 'command' ? '' : s.passphrase,
    });
    s.pending = null;
    s.passphrase = '';
    done.value = a;
    await loadSecretAccounts();
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    busy.value = false;
  }
}

function close() {
  visible.value = false;
}
/** Leaving before the end: the exchange is dropped (nothing was saved). */
function closed() {
  if (s.pending && !props.preview) accountsApi.zohoCancel(s.pending).catch(() => {});
  s.pending = null;
  s.client_secret = '';
  s.passphrase = '';
  if (done.value) emit('connected', done.value);
  emit('close');
}
onBeforeUnmount(() => {
  if (s.pending && !props.preview) accountsApi.zohoCancel(s.pending).catch(() => {});
});
watch(() => s.master, () => { error.value = ''; });

const title = computed(() => (props.account ? t('secretAccounts:wizard.reconnectTitle', { label: props.account.label }) : t('secretAccounts:wizard.title')));
const showResult = computed(() => done.value || props.preview?.result === 'ok');
const doneLabel = computed(() => done.value?.label ?? s.label);
</script>

<template>
  <el-dialog v-model="visible" :title="title" width="620px" class="zw-dialog" append-to-body :close-on-click-modal="false" @closed="closed">
    <ol class="zw-steps" :aria-label="title">
      <li v-for="st in STEPS" :key="st.n" :class="{ on: s.step === st.n, past: s.step > st.n || showResult }">
        <span class="zw-num">{{ s.step > st.n || showResult ? '✓' : st.n }}</span>{{ $t(`secretAccounts:wizard.steps.${st.key}`) }}
      </li>
    </ol>

    <!-- 1. The Self Client -->
    <section v-if="s.step === 1" class="zw-body" data-step="1">
      <h4>{{ $t('secretAccounts:wizard.s1.title') }}</h4>
      <p class="zw-muted">{{ $t('secretAccounts:wizard.s1.intro') }}</p>
      <el-button type="primary" plain class="zw-open" @click="openConsole"><el-icon><ei-link /></el-icon>{{ $t('secretAccounts:wizard.s1.open') }}</el-button>
      <span class="zw-url">{{ ZOHO_API_CONSOLE }}</span>
      <ol class="zw-list">
        <li v-for="k in ['a', 'b', 'c']" :key="k">
          <template v-for="(p, i) in parts($t(`secretAccounts:wizard.s1.${k}`))" :key="i"><strong v-if="i % 2" class="zw-z">{{ p }}</strong><span v-else>{{ p }}</span></template>
        </li>
      </ol>
      <el-form label-position="top" class="zw-form" @submit.prevent="goNext">
        <el-form-item :label="$t('secretAccounts:wizard.client_id')" required>
          <el-input v-model="s.client_id" class="zw-mono" placeholder="1000.XXXXXXXXXXXXXXXXXXXXXXXXXXXXXX" spellcheck="false" autocomplete="off" />
        </el-form-item>
        <el-form-item :label="$t('secretAccounts:wizard.client_secret')" required>
          <el-input v-model="s.client_secret" type="password" show-password autocomplete="new-password" class="zw-mono" />
        </el-form-item>
      </el-form>
    </section>

    <!-- 2. The code -->
    <section v-else-if="s.step === 2" class="zw-body" data-step="2">
      <h4>{{ $t('secretAccounts:wizard.s2.title') }}</h4>
      <ol class="zw-list">
        <li><template v-for="(p, i) in parts($t('secretAccounts:wizard.s2.a'))" :key="i"><strong v-if="i % 2" class="zw-z">{{ p }}</strong><span v-else>{{ p }}</span></template></li>
        <li>
          <template v-for="(p, i) in parts($t('secretAccounts:wizard.s2.b'))" :key="i"><strong v-if="i % 2" class="zw-z">{{ p }}</strong><span v-else>{{ p }}</span></template>
          <div class="zw-scope">
            <code>{{ ZOHO_SCOPES }}</code>
            <el-button size="small" @click="copyScopes">{{ copied ? $t('secretAccounts:wizard.s2.copied') : $t('secretAccounts:wizard.s2.copy') }}</el-button>
          </div>
        </li>
        <li v-for="k in ['c', 'd', 'e']" :key="k">
          <template v-for="(p, i) in parts($t(`secretAccounts:wizard.s2.${k}`))" :key="i"><strong v-if="i % 2" class="zw-z">{{ p }}</strong><span v-else>{{ p }}</span></template>
        </li>
      </ol>
      <el-form label-position="top" class="zw-form" @submit.prevent="goNext">
        <el-form-item :label="$t('secretAccounts:wizard.code')" required>
          <el-input v-model="s.code" class="zw-mono" placeholder="1000.xxxxxxxx.xxxxxxxx" spellcheck="false" autocomplete="off" />
        </el-form-item>
      </el-form>
    </section>

    <!-- 3. The exchange -->
    <section v-else-if="s.step === 3" class="zw-body" data-step="3">
      <h4>{{ $t('secretAccounts:wizard.s3.title') }}</h4>
      <p class="zw-muted">{{ $t('secretAccounts:wizard.s3.intro') }}</p>
      <div v-if="busy" class="zw-status"><el-icon class="is-loading"><ei-loading /></el-icon>{{ $t('secretAccounts:wizard.s3.connecting') }}</div>
      <template v-else-if="s.pending">
        <div class="zw-status ok"><el-icon><ei-circle-check-filled /></el-icon>{{ $t('secretAccounts:wizard.s3.connected', { domain: zohoDomain(s.data_center) }) }}</div>
        <p v-if="s.identity" class="zw-muted">{{ $t('secretAccounts:wizard.s3.user', { identity: s.identity }) }}</p>
        <p v-else class="zw-muted">{{ $t('secretAccounts:wizard.s3.noUser') }}</p>
        <el-form label-position="top" class="zw-form" @submit.prevent="goNext">
          <el-form-item :label="$t('secretAccounts:wizard.label')" required>
            <el-input v-model="s.label" maxlength="200" />
          </el-form-item>
        </el-form>
      </template>
      <div v-else-if="error" class="zw-error-actions">
        <div class="zw-error zw-error-top" role="alert">{{ errorText }}</div>
        <el-button v-if="errorStep === 2" type="primary" @click="goTo(2)">{{ $t('secretAccounts:wizard.s3.newCode') }}</el-button>
        <el-button v-else-if="errorStep === 1" @click="goTo(1)">{{ $t('secretAccounts:wizard.s3.toClient') }}</el-button>
        <el-button v-else @click="connect">{{ $t('secretAccounts:wizard.s3.retry') }}</el-button>
      </div>
    </section>

    <!-- 4. The passphrase -->
    <section v-else class="zw-body" data-step="4">
      <template v-if="showResult">
        <div class="zw-done">
          <el-icon class="zw-done-icon"><ei-circle-check-filled /></el-icon>
          <h4>{{ $t('secretAccounts:wizard.s4.done') }}</h4>
          <p class="zw-muted">{{ $t('secretAccounts:wizard.s4.doneHelp', { label: doneLabel }) }}</p>
        </div>
      </template>
      <template v-else>
        <h4>{{ $t('secretAccounts:wizard.s4.title') }}</h4>
        <p class="zw-muted">{{ $t('secretAccounts:wizard.s4.intro') }}</p>
        <el-form label-position="top" class="zw-form" @submit.prevent="finish">
          <el-form-item :label="$t('secretAccounts:wizard.mode')">
            <el-radio-group v-model="s.master" class="zw-modes">
              <el-radio value="keychain">{{ $t('connection:source.master.keychain') }}</el-radio>
              <el-radio value="ask">{{ $t('connection:source.master.ask') }}</el-radio>
              <el-radio value="command">{{ $t('connection:source.master.command') }}</el-radio>
            </el-radio-group>
          </el-form-item>
          <el-form-item v-if="s.master !== 'command'" :label="$t('secretAccounts:wizard.passphraseInput')" required>
            <el-input v-model="s.passphrase" type="password" show-password autocomplete="new-password" />
          </el-form-item>
          <el-form-item v-else :label="$t('secretAccounts:wizard.commandInput')" required>
            <el-input v-model="s.master_command" class="zw-mono" :placeholder="$t('connection:source.placeholder.command')" spellcheck="false" />
          </el-form-item>
          <p class="zw-muted zw-help">{{ $t(`secretAccounts:wizard.s4.${s.master}Help`) }}</p>
        </el-form>
        <div v-if="busy" class="zw-status"><el-icon class="is-loading"><ei-loading /></el-icon>{{ $t('secretAccounts:wizard.s4.checking') }}</div>
      </template>
    </section>

    <div v-if="error && !busy && s.step !== 3" class="zw-error" role="alert">{{ errorText }}</div>
    <div v-else-if="showProblems && problemText" class="zw-error" role="alert">{{ problemText }}</div>

    <template #footer>
      <template v-if="showResult">
        <el-button type="primary" @click="close">{{ $t('secretAccounts:wizard.close') }}</el-button>
      </template>
      <template v-else>
        <el-button @click="close">{{ $t('common:cancel') }}</el-button>
        <el-button v-if="s.step > 1" :disabled="busy" @click="goBack">{{ $t('secretAccounts:wizard.back') }}</el-button>
        <el-button v-if="s.step < 4" type="primary" :disabled="busy || (s.step === 3 && !s.pending)" @click="goNext">{{ $t('secretAccounts:wizard.next') }}</el-button>
        <el-button v-else type="primary" :loading="busy" @click="finish">{{ $t('secretAccounts:wizard.s4.finish') }}</el-button>
      </template>
    </template>
  </el-dialog>
</template>

<style scoped lang="scss">
.zw-steps {
  display: flex; gap: 6px; list-style: none; margin: -4px 0 14px; padding: 0; counter-reset: none;
  li {
    flex: 1; display: flex; align-items: center; gap: 6px; font-size: 12px; color: var(--nm-text-dim);
    padding-bottom: 6px; border-bottom: 2px solid var(--nm-border-soft);
    &.on { color: var(--nm-text-strong); border-color: var(--el-color-primary); }
    &.past { color: var(--nm-text); border-color: var(--el-color-primary-light-5); }
  }
}
.zw-num {
  display: inline-flex; align-items: center; justify-content: center; width: 18px; height: 18px; border-radius: 50%;
  font-size: 11px; background: var(--nm-border-soft); color: var(--nm-text);
  .on & { background: var(--el-color-primary); color: #fff; }
}
.zw-body { h4 { margin: 0 0 6px; font-size: 14px; color: var(--nm-text-strong); } }
.zw-muted { font-size: 12.5px; color: var(--nm-text-dim); line-height: 1.45; margin: 0 0 10px; }
.zw-help { margin-top: -6px; }
.zw-open { margin-bottom: 4px; }
.zw-url { margin-left: 8px; font-size: 11.5px; color: var(--nm-text-dim); font-family: var(--nm-font-mono, ui-monospace, monospace); }
.zw-list {
  margin: 10px 0 12px; padding-left: 20px; font-size: 12.5px; line-height: 1.5; color: var(--nm-text);
  li { margin-bottom: 6px; }
}
.zw-z { font-weight: 600; color: var(--nm-text-strong); }
.zw-scope {
  display: flex; align-items: center; gap: 8px; margin-top: 4px;
  code {
    flex: 1; min-width: 0; padding: 5px 8px; border-radius: 4px; background: var(--nm-bg-soft, var(--el-fill-color-light));
    border: 1px solid var(--nm-border-soft); font-size: 12px; user-select: all; word-break: break-all;
  }
}
.zw-form { margin-top: 6px; }
.zw-mono :deep(input) { font-family: var(--nm-font-mono, ui-monospace, monospace); font-size: 12px; }
.zw-modes { display: flex; flex-direction: column; align-items: flex-start; gap: 2px; }
.zw-status {
  display: flex; align-items: center; gap: 8px; font-size: 13px; margin: 8px 0;
  &.ok { color: var(--nm-success); }
}
.zw-error { margin-top: 10px; font-size: 12.5px; color: var(--nm-danger); white-space: pre-wrap; word-break: break-word; }
.zw-error-actions { margin-top: 10px; }
.zw-error-top { margin: 0 0 10px; }
.zw-done { text-align: center; padding: 18px 0 6px; h4 { font-size: 15px; } }
.zw-done-icon { font-size: 34px; color: var(--nm-success); margin-bottom: 6px; }
</style>
