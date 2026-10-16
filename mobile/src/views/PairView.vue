<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { useTranslation } from 'i18next-vue';
import { api, errorText } from '../api';

// Pairing: scan DBine's QR code (Configuración › Dispositivos), then the
// user says yes on the computer, comparing the code both screens show.

const props = defineProps<{ code?: string | null }>();
const emit = defineEmits<{ paired: []; cancelLink: [] }>();
const { t } = useTranslation();
const name = ref(t('pair.defaultName'));
const busy = ref(false);
const error = ref('');
const waiting = ref<{ desktop: string; fingerprint: string } | null>(null);
let unlisten: UnlistenFn | null = null;

onMounted(async () => {
  unlisten = await listen<{ desktop: string; fingerprint: string }>('pair-waiting', (e) => { waiting.value = e.payload; });
});
onBeforeUnmount(() => unlisten?.());

async function scan(): Promise<string | null> {
  const { scan, Format, checkPermissions, requestPermissions } = await import('@tauri-apps/plugin-barcode-scanner');
  let perm = await checkPermissions();
  if (perm !== 'granted') perm = await requestPermissions();
  if (perm !== 'granted') throw t('pair.noCamera');
  const r = await scan({ formats: [Format.QRCode], windowed: false });
  return r?.content ?? null;
}

/** The computer a pairing link points to (opened from the camera or a browser). */
const linkDesktop = ref<string | null>(null);
watch(() => props.code, async (c) => {
  linkDesktop.value = null;
  if (c) {
    try { linkDesktop.value = await api.inviteName(c); } catch (e) { error.value = errorText(e); }
  }
}, { immediate: true });

async function start() {
  error.value = '';
  busy.value = true;
  waiting.value = null;
  try {
    const code = props.code || (await scan());
    if (!code) return;
    await api.pair(code, name.value || t('pair.defaultName'));
    emit('paired');
  } catch (e) {
    error.value = errorText(e);
  } finally {
    busy.value = false;
    waiting.value = null;
  }
}
</script>

<template>
  <div class="pair">
    <img src="/logo.png" alt="" class="logo" />
    <h1>DBine</h1>
    <p class="muted">{{ $t('pair.intro') }}</p>
    <div v-if="waiting" class="card wait">
      <p>{{ $t('pair.confirmOn', { desktop: waiting.desktop }) }}</p>
      <p class="muted">{{ $t('pair.compare') }}</p>
      <p class="mono code">{{ waiting.fingerprint }}</p>
    </div>
    <template v-else>
      <label class="muted">{{ $t('pair.name') }}</label>
      <input v-model="name" maxlength="60" />
      <template v-if="code">
        <p v-if="linkDesktop" class="card">{{ $t('pair.fromLink', { desktop: linkDesktop }) }}</p>
        <button class="primary" :disabled="busy || !linkDesktop" @click="start">{{ busy ? $t('pair.connecting') : $t('pair.pairLink') }}</button>
        <button :disabled="busy" @click="emit('cancelLink')">{{ $t('pair.scanInstead') }}</button>
      </template>
      <button v-else class="primary" :disabled="busy" @click="start">{{ busy ? $t('pair.connecting') : $t('pair.scan') }}</button>
      <ol v-if="!code" class="muted steps">
        <li>{{ $t('pair.step1') }}</li>
        <li>{{ $t('pair.step2') }}</li>
        <li>{{ $t('pair.step3') }}</li>
      </ol>
    </template>
    <p v-if="error" class="error">{{ error }}</p>
  </div>
</template>

<style scoped>
.pair { padding: 48px 24px; display: flex; flex-direction: column; gap: 12px; }
.logo { width: 72px; height: 72px; align-self: center; }
h1 { text-align: center; margin: 0; color: var(--text-strong); }
.steps { padding-left: 18px; margin: 4px 0 0; }
.wait { text-align: center; }
.code { font-size: 22px; letter-spacing: 1px; color: var(--text-strong); margin: 4px 0; }
</style>
