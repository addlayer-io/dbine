<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { MAIL_PORTS, mailApi, newMailSettings, type MailSecurity, type MailSettings } from '../api/mail';
import { tb } from '../i18n/backend';

// Configuración › Correo: the SMTP server of the "Enviar un mail" steps
// (docs/tareas-programadas.md). Machine-local; the password goes to the vault.

const { t } = useTranslation();

const form = ref<MailSettings>(newMailSettings());
const password = ref('');
const passwordSaved = ref(false);
const configured = ref(false);
const saving = ref(false);
const testing = ref(false);
/** The last test: what it said, and whether it worked. */
const result = ref<{ ok: boolean; text: string } | null>(null);

const SECURITIES: MailSecurity[] = ['starttls', 'tls', 'none'];

async function load() {
  try {
    const v = await mailApi.get();
    form.value = v.settings ? { ...v.settings } : newMailSettings();
    passwordSaved.value = v.password_saved;
    configured.value = !!v.settings;
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}
onMounted(load);

/** Switching the security moves a usual port to the new one's. */
function pickSecurity(s: MailSecurity) {
  if (Object.values(MAIL_PORTS).includes(form.value.port)) form.value.port = MAIL_PORTS[s];
  form.value.security = s;
}

const passwordPlaceholder = computed(() => (passwordSaved.value ? t('mail:passwordSaved') : ''));
const canSave = computed(() => !!form.value.host.trim() && !!form.value.from_address.trim() && !!form.value.port);

async function save() {
  saving.value = true;
  try {
    const v = await mailApi.save(form.value, password.value);
    password.value = '';
    passwordSaved.value = v.password_saved;
    configured.value = true;
    ElMessage.success(t('mail:saved'));
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    saving.value = false;
  }
}

async function test() {
  let to: string;
  try {
    ({ value: to } = await ElMessageBox.prompt(t('mail:testPrompt'), t('mail:test'), {
      confirmButtonText: t('mail:testSend'),
      cancelButtonText: t('common:cancel'),
      inputPattern: /\S+@\S+/,
      inputErrorMessage: t('mail:testInvalid'),
    }));
  } catch {
    return;
  }
  testing.value = true;
  result.value = null;
  try {
    const summary = await mailApi.test(form.value, password.value, to);
    result.value = { ok: true, text: tb(summary) };
  } catch (e) {
    result.value = { ok: false, text: errorMessage(e) };
  } finally {
    testing.value = false;
  }
}
</script>

<template>
  <div class="ml">
    <h3>{{ $t('mail:title') }}</h3>
    <p class="ml-muted">{{ $t('mail:intro') }}</p>

    <el-form label-position="top" size="default" class="ml-form" @submit.prevent>
      <div class="ml-grid">
        <el-form-item :label="$t('mail:host')" class="ml-wide">
          <el-input v-model="form.host" placeholder="smtp.example.com" />
        </el-form-item>
        <el-form-item :label="$t('mail:port')">
          <el-input-number v-model="form.port" :min="1" :max="65535" :controls="false" style="width: 100%" />
        </el-form-item>
        <el-form-item :label="$t('mail:security')" class="ml-wide">
          <el-select :model-value="form.security" style="width: 100%" @update:model-value="pickSecurity">
            <el-option v-for="s in SECURITIES" :key="s" :value="s" :label="$t(`mail:securities.${s}`)" />
          </el-select>
        </el-form-item>
      </div>
      <div v-if="form.security === 'none'" class="ml-warn"><el-icon><ei-warning-filled /></el-icon>{{ $t('mail:noneWarning') }}</div>

      <div class="ml-grid">
        <el-form-item :label="$t('mail:user')" class="ml-wide">
          <el-input v-model="form.user" autocomplete="off" :placeholder="$t('mail:userPlaceholder')" />
        </el-form-item>
        <el-form-item :label="$t('mail:password')" class="ml-wide">
          <el-input v-model="password" type="password" show-password autocomplete="new-password" :disabled="!form.user.trim()" :placeholder="passwordPlaceholder" />
        </el-form-item>
        <el-form-item :label="$t('mail:fromAddress')" class="ml-wide">
          <el-input v-model="form.from_address" placeholder="reportes@example.com" />
        </el-form-item>
        <el-form-item :label="$t('mail:fromName')" class="ml-wide">
          <el-input v-model="form.from_name" />
        </el-form-item>
      </div>
      <p class="ml-muted ml-small">{{ $t('mail:passwordHelp') }}</p>

      <div class="ml-actions">
        <el-button type="primary" :disabled="!canSave" :loading="saving" @click="save">{{ $t('mail:save') }}</el-button>
        <el-button :disabled="!canSave" :loading="testing" @click="test">{{ $t('mail:test') }}</el-button>
        <span v-if="!configured" class="ml-muted ml-small">{{ $t('mail:notConfigured') }}</span>
      </div>
      <div v-if="result" class="ml-result" :class="result.ok ? 'ok' : 'err'">
        <el-icon><ei-circle-check-filled v-if="result.ok" /><ei-circle-close-filled v-else /></el-icon>
        <span>{{ result.ok ? $t('mail:testOk', { summary: result.text }) : $t('mail:testFailed', { error: result.text }) }}</span>
      </div>
    </el-form>
  </div>
</template>

<style scoped>
.ml h3 { margin: 0 0 4px; font-size: 16px; color: var(--nm-text-strong); }
.ml-muted { color: var(--nm-text-dim); font-size: 12.5px; line-height: 1.5; margin: 0 0 12px; }
.ml-small { font-size: 12px; margin: 0; }
.ml-grid { display: grid; grid-template-columns: repeat(4, 1fr); gap: 0 12px; }
.ml-wide { grid-column: span 2; }
.ml-grid > .el-form-item:not(.ml-wide) { grid-column: span 2; }
.ml-warn, .ml-result {
  display: flex; align-items: flex-start; gap: 8px; padding: 8px 12px; margin: 0 0 14px; border-radius: 6px; font-size: 12.5px; line-height: 1.5;
}
.ml-warn { border: 1px solid color-mix(in srgb, var(--nm-warning) 45%, transparent); background: color-mix(in srgb, var(--nm-warning) 8%, transparent); }
.ml-warn .el-icon { color: var(--nm-warning); flex: none; margin-top: 2px; }
.ml-actions { display: flex; align-items: center; gap: 8px; margin: 14px 0 12px; }
.ml-actions .el-button { margin: 0; }
.ml-result { user-select: text; word-break: break-word; }
.ml-result .el-icon { flex: none; margin-top: 2px; }
.ml-result.ok { border: 1px solid color-mix(in srgb, var(--nm-success) 45%, transparent); background: color-mix(in srgb, var(--nm-success) 8%, transparent); }
.ml-result.ok .el-icon { color: var(--nm-success); }
.ml-result.err { border: 1px solid color-mix(in srgb, var(--nm-danger) 45%, transparent); background: color-mix(in srgb, var(--nm-danger) 8%, transparent); }
.ml-result.err .el-icon { color: var(--nm-danger); }
</style>
