<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { tb } from '../i18n/backend';
import { aiApi, apiSettingKey, vendorOf, type AiProvider, type ApiSetting } from '../api/ai';
import { useSettingsStore } from '../stores/settings';
import { offeredModels, providerAfterRemove } from '../composables/apiKeyForm';

// The assistant with the user's own API key (docs/ai-assistant.md, "Your own
// API key"): the key goes to the encrypted vault on this machine and never
// travels with the cloud backup; the URL and the model typed are settings.
// The provider bills the user's account.

const props = defineProps<{ provider: AiProvider | null }>();
const emit = defineEmits<{ close: []; changed: [] }>();

const { t } = useTranslation();
const settings = useSettingsStore();

const key = ref('');
const url = ref('');
const model = ref('');
const models = ref<string[]>([]);
const testing = ref(false);
const saving = ref(false);
const result = ref<{ ok: boolean; text: string } | null>(null);

const kind = computed(() => props.provider?.kind ?? null);
const vendor = computed(() => (kind.value ? vendorOf(kind.value) : null));
const needsUrl = computed(() => kind.value === 'azure_openai' || kind.value === 'openai_compatible');
const keyOptional = computed(() => kind.value === 'openai_compatible');
/** Where the user gets a key. */
const keySite = computed(() => ({
  openai: 'platform.openai.com/api-keys', anthropic: 'console.anthropic.com', gemini: 'aistudio.google.com/apikey', xai: 'console.x.ai',
} as Record<string, string>)[kind.value ?? ''] ?? null);
const configured = computed(() => !!props.provider && (props.provider.key_saved || !!props.provider.base_url || !!props.provider.model));

watch(() => props.provider, (p) => {
  if (!p) return;
  const saved = settings.get<ApiSetting>(apiSettingKey(p.kind), {});
  key.value = '';
  url.value = saved.url ?? '';
  model.value = saved.model ?? '';
  models.value = p.models.map((m) => m.id);
  result.value = null;
}, { immediate: true });

const nul = (s: string) => s.trim() || null;

async function test() {
  if (!kind.value) return;
  testing.value = true;
  result.value = null;
  try {
    const r = await aiApi.apiTest(kind.value, nul(key.value), nul(url.value), nul(model.value));
    result.value = { ok: true, text: tb(r.message) };
    // Offered right away, before saving.
    models.value = offeredModels(model.value, r.models, models.value);
  } catch (e) {
    result.value = { ok: false, text: errorMessage(e) };
  } finally {
    testing.value = false;
  }
}

async function save() {
  if (!kind.value) return;
  if (needsUrl.value && !url.value.trim()) {
    ElMessage.warning(t('ai:api.urlRequired'));
    return;
  }
  if (!keyOptional.value && !key.value.trim() && !props.provider?.key_saved) {
    ElMessage.warning(t('ai:api.keyRequired'));
    return;
  }
  saving.value = true;
  try {
    await aiApi.apiSave(kind.value, nul(key.value), nul(url.value), nul(model.value));
    const s: ApiSetting = {};
    if (url.value.trim()) s.url = url.value.trim();
    if (model.value.trim()) s.model = model.value.trim();
    settings.values[apiSettingKey(kind.value)] = Object.keys(s).length ? s : undefined;
    emit('changed');
    emit('close');
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    saving.value = false;
  }
}

async function remove() {
  if (!kind.value || !props.provider) return;
  try {
    await ElMessageBox.confirm(t('ai:api.removeConfirm', { provider: tb(props.provider.label) }), t('ai:api.remove'), {
      type: 'warning', confirmButtonText: t('ai:api.remove'), cancelButtonText: t('common:cancel'),
    });
  } catch { return; }
  try {
    await aiApi.apiRemove(kind.value);
    delete settings.values[apiSettingKey(kind.value)];
    // The chat no longer points at it: it uses the first provider available.
    const chosen = settings.get<string | null>('ai.provider', null);
    if (providerAfterRemove(chosen, kind.value) !== chosen) await settings.set('ai.provider', null);
    emit('changed');
    emit('close');
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}
</script>

<template>
  <el-dialog
    :model-value="!!provider"
    :title="provider ? $t('ai:api.title', { provider: tb(provider.label) }) : ''"
    width="520px"
    append-to-body
    data-test="ai-api-dialog"
    @close="emit('close')"
  >
    <el-form v-if="provider" label-position="top" size="small" @submit.prevent>
      <el-form-item :label="keyOptional ? $t('ai:api.keyOptional') : $t('ai:api.key')">
        <el-input
          v-model="key"
          type="password"
          show-password
          autocomplete="off"
          :placeholder="provider.key_saved ? $t('ai:api.keySaved') : $t('ai:api.keyPlaceholder')"
          data-test="ai-api-key"
        />
        <div v-if="keySite" class="aik-hint">{{ $t('ai:api.keyWhere', { site: keySite }) }}</div>
      </el-form-item>
      <el-form-item v-if="needsUrl" :label="kind === 'azure_openai' ? $t('ai:api.azureUrl') : $t('ai:api.baseUrl')">
        <el-input
          v-model="url"
          :placeholder="kind === 'azure_openai' ? 'https://<recurso>.openai.azure.com' : 'http://servidor:8000/v1'"
          data-test="ai-api-url"
        />
      </el-form-item>
      <el-form-item :label="kind === 'azure_openai' ? $t('ai:api.deployment') : $t('ai:api.model')">
        <el-select
          v-model="model"
          filterable
          allow-create
          clearable
          default-first-option
          :placeholder="kind === 'azure_openai' ? $t('ai:api.deploymentPlaceholder') : $t('ai:api.modelPlaceholder')"
          style="width: 100%"
          data-test="ai-api-model"
        >
          <el-option v-for="m in models" :key="m" :value="m" :label="m" />
        </el-select>
      </el-form-item>
      <p class="aik-note">
        <el-icon><ei-lock /></el-icon>
        <span>{{ vendor ? $t('ai:api.privacy', { vendor }) : $t('ai:api.privacyOwn') }}</span>
      </p>
      <p v-if="vendor" class="aik-note">
        <el-icon><ei-coin /></el-icon>
        <span>{{ $t('ai:api.cost', { vendor }) }}</span>
      </p>
      <p class="aik-note aik-muted">{{ $t('ai:api.local') }}</p>
      <p v-if="result" :class="['aik-result', result.ok ? 'ok' : 'err']" data-test="ai-api-result">{{ result.text }}</p>
    </el-form>
    <template #footer>
      <el-button v-if="configured" type="danger" plain @click="remove">{{ $t('ai:api.remove') }}</el-button>
      <div style="flex: 1" />
      <el-button :loading="testing" data-test="ai-api-test" @click="test">{{ $t('ai:api.test') }}</el-button>
      <el-button type="primary" :loading="saving" data-test="ai-api-save" @click="save">{{ $t('common:save') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.aik-hint { font-size: 12px; color: var(--nm-text-dim); margin-top: 4px; }
.aik-note { display: flex; gap: 6px; align-items: flex-start; font-size: 12.5px; line-height: 1.5; margin: 6px 0; }
.aik-note .el-icon { margin-top: 3px; flex: none; }
.aik-muted { color: var(--nm-text-dim); }
.aik-result { font-size: 12.5px; margin: 8px 0 0; }
.aik-result.ok { color: var(--nm-success); }
.aik-result.err { color: var(--nm-danger); }
:deep(.el-dialog__footer) { display: flex; gap: 8px; }
</style>
