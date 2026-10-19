<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { api, errorKind, errorMessage } from '../api/client';
import { connStringApi, type ConnStringBuilt, type ConnStringInfo, type ConnStringNote } from '../api/connString';
import { useConnectionsStore } from '../stores/connections';
import { connStringTarget, copyText } from '../composables/connString';
import { askApproveSecretSource } from '../composables/secretSourceApproval';
import { askMasterPassword, unlockOf } from '../composables/secretSourceUnlock';

// "Copiar cadena de conexión…" (docs/connection-strings.md): the database,
// the format and "Incluir contraseña", with a live preview built by the
// backend. The preview never has a secret: placeholders, or the mask with
// "Incluir contraseña" until "Mostrar". "Mostrar" and "Copiar" with it on
// resolve the secrets the way a connect does (the source's confirmation,
// a master password, the password if it isn't kept), each time.

const { t } = useTranslation();
const conns = useConnectionsStore();

const id = computed(() => connStringTarget.value);
const conn = computed(() => (id.value ? conns.byId(id.value) : undefined));

const info = ref<ConnStringInfo | null>(null);
const loadError = ref<string | null>(null);
const database = ref('');
const format = ref('');
const include = ref(false);
const revealed = ref(false);
const built = ref<ConnStringBuilt | null>(null);
const buildError = ref<string | null>(null);
const busy = ref(false);
/** Bumped by each preview request: an older answer is dropped. */
let seq = 0;

const FORMAT_KEY = 'dbine.connString.format';
function rememberedFormat(driver: string): string | null {
  try { return (JSON.parse(localStorage.getItem(FORMAT_KEY) ?? '{}') as Record<string, string>)[driver] ?? null; } catch { return null; }
}
function rememberFormat(driver: string, f: string) {
  try {
    const all = JSON.parse(localStorage.getItem(FORMAT_KEY) ?? '{}') as Record<string, string>;
    all[driver] = f;
    localStorage.setItem(FORMAT_KEY, JSON.stringify(all));
  } catch { /* no storage: not remembered */ }
}

const databases = computed(() => {
  const live = id.value ? conns.live[id.value]?.databases ?? [] : [];
  const out = [...live];
  for (const d of [info.value?.database ?? '', database.value]) if (d && !out.includes(d)) out.unshift(d);
  return out;
});
const usesDatabase = computed(() => !!conns.driverOf(id.value ?? '')?.databases_label);

const formatLabel = (f: { id: string; label: string }) => f.label || t(`connString:format.${f.id}`);
const blocked = computed(() => info.value?.blocked ?? null);
const canInclude = computed(() => !!info.value?.has_secrets && !blocked.value);

watch(id, async (v) => {
  info.value = null;
  built.value = null;
  loadError.value = null;
  buildError.value = null;
  include.value = false;
  revealed.value = false;
  if (!v) return;
  try {
    const i = await connStringApi.info(v, '');
    if (id.value !== v) return;
    info.value = i;
    database.value = i.database;
    const driver = conn.value?.config.driver ?? '';
    const kept = rememberedFormat(driver);
    format.value = i.formats.find((f) => f.id === kept)?.id ?? i.formats[0]?.id ?? '';
    // The same database and format as last time don't trigger the watcher below.
    preview();
  } catch (e) {
    loadError.value = errorMessage(e);
  }
}, { immediate: true });

const request = (reveal: boolean, password: string | null = null) => connStringApi.build({
  connection_id: id.value!, database: database.value, format: format.value,
  include_secrets: include.value, reveal: include.value && reveal, password,
});

async function preview() {
  if (!id.value || !format.value) return;
  const n = ++seq;
  revealed.value = false;
  buildError.value = null;
  try {
    const b = await request(false);
    if (n === seq) built.value = b;
  } catch (e) {
    if (n === seq) buildError.value = errorMessage(e);
  }
}
watch([database, format, include], () => {
  if (info.value) preview();
});
watch(format, (f) => { if (f && conn.value) rememberFormat(conn.value.config.driver, f); });

async function askPassword(): Promise<string | null> {
  try {
    const { value } = await ElMessageBox.prompt(
      t('connString:passwordPrompt', { user: conn.value?.config.username ?? '', connection: conn.value?.name ?? '' }),
      t('connString:title'),
      { inputType: 'password', confirmButtonText: t('common:accept'), cancelButtonText: t('common:cancel') },
    );
    return value ?? '';
  } catch {
    return null;
  }
}

/** The string with its secrets, answering what a connect would ask. Null: the user gave up. */
async function resolved(password: string | null = null): Promise<ConnStringBuilt | null> {
  try {
    return await request(true, password);
  } catch (e) {
    if (await askApproveSecretSource(e)) return resolved(password);
    const unlock = unlockOf(e);
    if (unlock) {
      const typed = await askMasterPassword(unlock);
      if (typed === null) return null;
      await api.unlockSecretSource(id.value!, unlock.field, typed);
      return resolved(password);
    }
    if (errorKind(e) === 'password_required' || (password !== null && errorKind(e) === 'auth_failed')) {
      const typed = await askPassword();
      return typed === null ? null : resolved(typed);
    }
    throw e;
  }
}

async function toggleReveal() {
  if (revealed.value) {
    await preview();
    return;
  }
  busy.value = true;
  buildError.value = null;
  const n = ++seq;
  try {
    const b = await resolved();
    if (b && n === seq) {
      built.value = b;
      revealed.value = true;
    }
  } catch (e) {
    if (n === seq) buildError.value = errorMessage(e);
  } finally {
    busy.value = false;
  }
}

async function copy() {
  if (!built.value) return;
  busy.value = true;
  try {
    let b: ConnStringBuilt | null = built.value;
    // With "Incluir contraseña" the secret is resolved now (the preview
    // only had the mask, unless it's shown).
    if (include.value && !revealed.value) b = await resolved();
    if (!b) return;
    await copyText(b.text, b.secret);
    ElMessage.success({ message: b.secret ? t('connString:copiedSecret') : t('common:copied'), duration: b.secret ? 4000 : 1500 });
    close();
  } catch (e) {
    buildError.value = errorMessage(e);
  } finally {
    busy.value = false;
  }
}

function close() {
  connStringTarget.value = null;
}

const noteText = (n: ConnStringNote) => t(`connString:note.${n.code}`, n.args);
const notes = computed(() => [...(info.value?.notes ?? []), ...(built.value?.notes ?? [])]);
const blockedText = computed(() => (blocked.value ? t(`connString:blocked.${blocked.value}`) : ''));
</script>

<template>
  <el-dialog
    :model-value="!!id" width="680px" append-to-body :title="$t('connString:title')"
    :close-on-click-modal="false" @close="close"
  >
    <div v-if="loadError" class="cs-error" role="alert">{{ loadError }}</div>
    <template v-else>
      <p class="cs-sub">{{ conn?.name }}</p>
      <el-form label-position="left" label-width="150px" class="cs-form" @submit.prevent>
        <el-form-item v-if="usesDatabase" :label="$t('connString:database')">
          <el-select v-model="database" filterable allow-create default-first-option :placeholder="$t('connString:databaseDefault')" style="width: 100%">
            <el-option v-for="d in databases" :key="d" :label="d" :value="d" />
          </el-select>
        </el-form-item>
        <el-form-item :label="$t('connString:formatLabel')">
          <el-select v-model="format" :disabled="!info" style="width: 100%">
            <el-option v-for="f in info?.formats ?? []" :key="f.id" :label="formatLabel(f)" :value="f.id" />
          </el-select>
        </el-form-item>
        <el-form-item :label="$t('connString:includePassword')">
          <div class="cs-include">
            <el-switch v-model="include" :disabled="!canInclude" />
            <span v-if="blocked" class="cs-dim">{{ blockedText }}</span>
            <span v-else-if="info && !info.has_secrets" class="cs-dim">{{ $t('connString:noSecrets') }}</span>
            <span v-else-if="!include" class="cs-dim">{{ $t('connString:placeholderHint') }}</span>
          </div>
        </el-form-item>
      </el-form>

      <div class="cs-preview-head">
        <span>{{ $t('connString:preview') }}</span>
        <span style="flex: 1" />
        <el-button v-if="include" size="small" text :loading="busy && !revealed" @click="toggleReveal">
          {{ revealed ? $t('connString:hide') : $t('connString:show') }}
        </el-button>
      </div>
      <pre v-loading="!built && !buildError" class="cs-preview nm-selectable" :class="{ revealed }">{{ built?.text ?? '' }}</pre>
      <div v-if="buildError" class="cs-error" role="alert">{{ buildError }}</div>
      <ul v-if="notes.length" class="cs-notes">
        <li v-for="(n, i) in notes" :key="i"><el-icon><ei-info-filled /></el-icon><span>{{ noteText(n) }}</span></li>
      </ul>
    </template>

    <template #footer>
      <div class="cs-foot">
        <el-button @click="close">{{ $t('common:cancel') }}</el-button>
        <el-button type="primary" :loading="busy" :disabled="!built" @click="copy">{{ $t('common:copy') }}</el-button>
      </div>
    </template>
  </el-dialog>
</template>

<style scoped lang="scss">
.cs-sub { margin: -6px 0 12px; color: var(--nm-text-dim); font-size: 12.5px; }
.cs-form :deep(.el-form-item) { margin-bottom: 10px; }
.cs-include { display: flex; align-items: center; gap: 10px; min-width: 0; }
.cs-dim { color: var(--nm-text-dim); font-size: 12px; line-height: 1.35; }
.cs-preview-head { display: flex; align-items: center; margin-top: 6px; font-weight: 600; font-size: 13px; color: var(--nm-text-strong); min-height: 24px; }
.cs-preview {
  margin: 6px 0 0; padding: 10px 12px; min-height: 44px; max-height: 34vh; overflow: auto; border: 1px solid var(--nm-border); border-radius: 4px;
  background: var(--ide-editor, var(--nm-bg-elev)); font-family: var(--nm-mono); font-size: 12.5px; color: var(--nm-text-strong);
  white-space: pre-wrap; overflow-wrap: anywhere;
}
.cs-preview.revealed { border-color: color-mix(in srgb, var(--nm-warning, #d29922) 60%, var(--nm-border)); }
.cs-notes { list-style: none; margin: 10px 0 0; padding: 0; display: flex; flex-direction: column; gap: 4px; }
.cs-notes li { display: flex; gap: 6px; align-items: flex-start; color: var(--nm-text-dim); font-size: 12px; line-height: 1.4; }
.cs-notes .el-icon { flex: none; margin-top: 2px; }
.cs-error {
  margin-top: 8px; padding: 8px 10px; border-radius: 3px; border: 1px solid color-mix(in srgb, var(--nm-danger) 50%, transparent);
  background: color-mix(in srgb, var(--nm-danger) 10%, transparent); color: var(--nm-text); white-space: pre-wrap; font-size: 12px;
}
.cs-foot { display: flex; justify-content: flex-end; gap: 8px; }
.cs-foot .el-button { margin: 0; }
</style>
