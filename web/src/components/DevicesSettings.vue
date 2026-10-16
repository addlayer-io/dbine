<script setup lang="ts">
import { h, onBeforeUnmount, onMounted, ref } from 'vue';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { LINK_CHANGED_EVENT, LINK_PAIR_EVENT, devicesApi, type Device, type LinkInvite, type LinkStatus, type PairAsk, type RemoteSetting } from '../api/devices';
import { tb } from '../i18n/backend';

// Configuración › Dispositivos (docs/mobile-companion.md): the phone app's
// server, the pairing QR code and the paired phones.

const { t } = useTranslation();
const status = ref<LinkStatus | null>(null);
const port = ref(27518);
const saving = ref(false);

async function load() {
  try {
    status.value = await devicesApi.status();
    port.value = status.value.port;
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

async function configure(patch: { enabled?: boolean; port?: number; remote?: RemoteSetting }) {
  if (!status.value) return;
  saving.value = true;
  try {
    status.value = await devicesApi.configure(patch.enabled ?? status.value.enabled, patch.port ?? status.value.port, patch.remote);
    port.value = status.value.port;
    syncRemote();
  } catch (e) {
    ElMessage.error(errorMessage(e));
    port.value = status.value.port;
  } finally {
    saving.value = false;
  }
}

// -- outside the local network ----------------------------------------------------
const remoteMode = ref<'public' | 'own' | 'off'>('off');
const relaysText = ref('');

function syncRemote() {
  const r = status.value?.remote;
  if (!r) return;
  remoteMode.value = r.mode;
  relaysText.value = r.mode === 'own' ? r.relays.join('\n') : relaysText.value;
}

/** Leaving the local network through a third party is an explicit choice. */
async function pickRemote(v: 'public' | 'own' | 'off') {
  const before = status.value?.remote.mode ?? 'off';
  if (v === 'public' && before !== 'public') {
    try {
      const text = h('div', { style: 'white-space: pre-line; line-height: 1.5' }, t('devices:publicConfirm'));
      await ElMessageBox.confirm(text, t('devices:publicConfirmTitle'), {
        type: 'warning',
        confirmButtonText: t('devices:publicConfirmOk'),
        cancelButtonText: t('common:cancel'),
      });
    } catch {
      remoteMode.value = before;
      return;
    }
  }
  remoteMode.value = v;
  if (v !== 'own') applyRemote();
}

function applyRemote() {
  const relays = relaysText.value.split(/[\s,]+/).map((x) => x.trim()).filter(Boolean);
  if (remoteMode.value === 'own' && !relays.length) return;
  const remote: RemoteSetting = remoteMode.value === 'own' ? { mode: 'own', relays } : { mode: remoteMode.value };
  configure({ remote });
}

// The relay connection comes up a few seconds after turning it on.
let remotePoll: ReturnType<typeof setInterval> | null = null;

function applyPort() {
  if (status.value && port.value && port.value !== status.value.port) configure({ port: port.value });
}

// -- pairing ------------------------------------------------------------------
const invite = ref<LinkInvite | null>(null);
const left = ref(0);
let timer: ReturnType<typeof setInterval> | null = null;
/** A phone read the code: waiting for the user's yes. */
const asking = ref<PairAsk | null>(null);

function stopTimer() {
  if (timer) clearInterval(timer);
  timer = null;
}

async function startPairing() {
  try {
    invite.value = await devicesApi.invite();
    left.value = invite.value.seconds;
    stopTimer();
    timer = setInterval(() => {
      left.value -= 1;
      if (left.value <= 0) closePairing();
    }, 1000);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

function closePairing() {
  stopTimer();
  if (invite.value || asking.value) devicesApi.cancelInvite().catch(() => {});
  invite.value = null;
  asking.value = null;
}

async function answer(accept: boolean) {
  asking.value = null;
  stopTimer();
  invite.value = null;
  await devicesApi.answer(accept).catch(() => {});
  if (accept) ElMessage.success(t('devices:paired'));
  load();
}

// -- phones -------------------------------------------------------------------
async function remove(d: Device) {
  try {
    await ElMessageBox.confirm(t('devices:removeConfirm', { name: d.name }), t('devices:remove'), { type: 'warning', confirmButtonText: t('devices:remove'), cancelButtonText: t('common:cancel') });
  } catch {
    return;
  }
  try {
    status.value = await devicesApi.remove(d.id);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

async function setCanRunTasks(d: Device, v: boolean) {
  try {
    status.value = await devicesApi.setCanRunTasks(d.id, v);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

async function setCanKill(d: Device, v: boolean) {
  try {
    status.value = await devicesApi.setCanKill(d.id, v);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

const unlisten: UnlistenFn[] = [];
onMounted(async () => {
  await load();
  syncRemote();
  remotePoll = setInterval(() => { if (status.value?.enabled && status.value.remote.mode !== 'off' && !status.value.remote_relay) load(); }, 3000);
  unlisten.push(await listen<PairAsk>(LINK_PAIR_EVENT, (e) => { asking.value = e.payload; stopTimer(); }));
  unlisten.push(await listen(LINK_CHANGED_EVENT, () => load()));
});
onBeforeUnmount(() => {
  if (remotePoll) clearInterval(remotePoll);
  closePairing();
  unlisten.forEach((u) => u());
});
</script>

<template>
  <div class="dv">
    <h3>{{ $t('devices:title') }}</h3>
    <p class="dv-muted">{{ $t('devices:intro') }}</p>

    <template v-if="status">
      <div class="dv-row">
        <div>
          <strong>{{ $t('devices:allow') }}</strong>
          <span>{{ $t('devices:allowHelp') }}</span>
          <span v-if="status.enabled && status.running" class="dv-ok">{{ $t('devices:running', { port: status.port }) }}</span>
          <span v-else-if="status.enabled && status.error" class="dv-err">{{ tb(status.error) }}</span>
        </div>
        <el-switch :model-value="status.enabled" :loading="saving" @update:model-value="(v: string | number | boolean) => configure({ enabled: !!v })" />
      </div>
      <div class="dv-row">
        <div><strong>{{ $t('devices:port') }}</strong><span>{{ $t('devices:portHelp') }}</span></div>
        <el-input-number v-model="port" :min="1024" :max="65535" :controls="false" style="width: 120px" @blur="applyPort" @keyup.enter="applyPort" />
      </div>

      <div class="dv-row dv-remote">
        <div>
          <strong>{{ $t('devices:remote') }}</strong>
          <span>{{ $t('devices:remoteHelp') }}</span>
          <span v-if="status.enabled && status.remote.mode !== 'off' && status.remote_relay" class="dv-ok">{{ $t('devices:remoteReady', { relay: status.remote_relay }) }}</span>
          <span v-else-if="status.enabled && status.remote_error" class="dv-err">{{ tb(status.remote_error) }}</span>
          <span v-else-if="status.enabled && status.remote.mode !== 'off'">{{ $t('devices:remoteConnecting') }}</span>
        </div>
        <el-radio-group :model-value="remoteMode" class="dv-modes" @update:model-value="(v: any) => pickRemote(v)">
          <el-radio value="off">{{ $t('devices:remoteModes.off') }}</el-radio>
          <el-radio value="own">{{ $t('devices:remoteModes.own') }}</el-radio>
          <el-radio value="public">{{ $t('devices:remoteModes.public') }}</el-radio>
        </el-radio-group>
      </div>
      <div v-if="remoteMode === 'own'" class="dv-own">
        <el-input v-model="relaysText" type="textarea" :autosize="{ minRows: 1, maxRows: 4 }" placeholder="https://relay.ejemplo.com" />
        <el-button @click="applyRemote">{{ $t('devices:save') }}</el-button>
        <p class="dv-muted">{{ $t('devices:ownHelp') }}</p>
      </div>
      <p v-else-if="remoteMode === 'public'" class="dv-muted">{{ $t('devices:publicHelp') }}</p>

      <h4>{{ $t('devices:phones') }}</h4>
      <el-button type="primary" :disabled="!status.enabled || !status.running" @click="startPairing">
        <el-icon><ei-iphone /></el-icon>&nbsp;{{ $t('devices:pair') }}
      </el-button>
      <p v-if="!status.devices.length" class="dv-muted dv-empty">{{ $t('devices:none') }}</p>
      <div v-for="d in status.devices" :key="d.id" class="dv-device">
        <el-icon class="dv-phone"><ei-iphone /></el-icon>
        <div class="dv-info">
          <strong>{{ d.name }}</strong>
          <span>{{ $t('devices:pairedAt', { at: d.paired_at }) }}<template v-if="d.last_seen"> · {{ $t('devices:lastSeen', { at: d.last_seen }) }}</template></span>
          <span class="dv-fp">{{ d.id }}</span>
        </div>
        <div class="dv-perms">
          <el-checkbox :model-value="d.can_kill" @update:model-value="(v: any) => setCanKill(d, !!v)">{{ $t('devices:canKill') }}</el-checkbox>
          <el-checkbox :model-value="d.can_run_tasks" @update:model-value="(v: any) => setCanRunTasks(d, !!v)">{{ $t('devices:canRunTasks') }}</el-checkbox>
        </div>
        <el-button size="small" text type="danger" @click="remove(d)">{{ $t('devices:remove') }}</el-button>
      </div>
      <p class="dv-muted">{{ $t('devices:canKillHelp') }}</p>
    </template>

    <!-- The QR code, then the yes. -->
    <el-dialog :model-value="!!invite || !!asking" :title="$t('devices:pairTitle')" width="420px" append-to-body @close="closePairing">
      <div v-if="asking" class="dv-ask">
        <el-icon class="dv-ask-icon"><ei-iphone /></el-icon>
        <p>{{ $t('devices:askText', { name: asking.name }) }}</p>
        <p class="dv-muted">{{ $t('devices:askCompare') }}</p>
        <code class="dv-code">{{ asking.fingerprint }}</code>
      </div>
      <div v-else-if="invite" class="dv-qr-wrap">
        <!-- eslint-disable-next-line vue/no-v-html -- SVG rendered by the backend from our own text -->
        <div class="dv-qr" v-html="invite.svg" />
        <p>{{ $t('devices:scan') }}</p>
        <p class="dv-muted">{{ $t('devices:expires', { seconds: left }) }}</p>
        <p class="dv-muted">{{ $t('devices:sameNetwork') }}</p>
      </div>
      <template #footer>
        <template v-if="asking">
          <el-button @click="answer(false)">{{ $t('devices:reject') }}</el-button>
          <el-button type="primary" @click="answer(true)">{{ $t('devices:accept') }}</el-button>
        </template>
        <el-button v-else @click="closePairing">{{ $t('common:cancel') }}</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.dv h4 { margin: 18px 0 8px; }
.dv-muted { color: var(--nm-text-dim); font-size: 12px; line-height: 1.5; }
.dv-empty { margin-top: 10px; }
.dv-row { display: flex; justify-content: space-between; align-items: center; gap: 16px; padding: 10px 0; border-bottom: 1px solid var(--nm-border-soft); }
.dv-row > div { display: flex; flex-direction: column; gap: 2px; }
.dv-row span { font-size: 12px; color: var(--nm-text-dim); }
.dv-ok { color: var(--nm-success) !important; }
.dv-err { color: var(--nm-danger) !important; }
.dv-device { display: flex; align-items: center; gap: 12px; padding: 10px 0; border-bottom: 1px solid var(--nm-border-soft); }
.dv-device:first-of-type { margin-top: 10px; }
.dv-phone { font-size: 20px; color: var(--nm-text-dim); }
.dv-perms { display: flex; flex-direction: column; }
.dv-modes { display: flex; flex-direction: column; align-items: flex-start; flex: none; }
.dv-modes .el-radio { height: 24px; margin: 0; }
.dv-own { display: flex; gap: 8px; align-items: flex-start; flex-wrap: wrap; padding: 8px 0; }
.dv-own .el-textarea { flex: 1; min-width: 260px; }
.dv-own p { flex-basis: 100%; margin: 0; }
.dv-perms .el-checkbox { height: 24px; }
.dv-info { flex: 1; display: flex; flex-direction: column; gap: 2px; min-width: 0; }
.dv-info span { font-size: 12px; color: var(--nm-text-dim); }
.dv-fp { font-family: var(--nm-mono); font-size: 11px !important; }
.dv-qr-wrap, .dv-ask { display: flex; flex-direction: column; align-items: center; text-align: center; }
.dv-qr { background: #fff; padding: 8px; border-radius: 6px; line-height: 0; }
.dv-qr :deep(svg) { width: 240px; height: 240px; }
.dv-ask-icon { font-size: 40px; color: var(--nm-primary); margin-bottom: 6px; }
.dv-code { font-family: var(--nm-mono); font-size: 18px; letter-spacing: 1px; padding: 6px 12px; border-radius: 4px; background: var(--ide-hover); }
</style>
