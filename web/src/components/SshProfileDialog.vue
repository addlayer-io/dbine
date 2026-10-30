<script setup lang="ts">
import { computed, reactive, ref } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { sshProfilesApi } from '../api/sshProfiles';
import type { SshProfile } from '../api/types';
import { draftFrom, missingInputs, type SourceDraft } from '../composables/secretSources';
import { loadSshProfiles, optionsFromValues, tunnelSources, valuesFromOptions } from '../composables/sshProfiles';
import SshTunnelSection, { type SshValues } from './SshTunnelSection.vue';

// New / edit SSH tunnel profile (docs/ssh-tunnels.md, "Profiles"): its name
// and the tunnel's SSH side (servers, user, authentication, accepted keys).
// Secrets are never read back: left empty when editing, the stored ones stay.
// "Guardar como perfil" opens it with a connection's current tunnel
// (`initial`); the secrets that form left empty are copied server side from
// that connection's keychain entry (`fromConnection`).

const props = defineProps<{
  /** The profile edited; null: a new one. */
  profile: SshProfile | null;
  /** A new one's starting values (a connection's tunnel). */
  initial?: { values: SshValues; drafts: Record<string, SourceDraft> } | null;
  /** The saved connection `initial` comes from. */
  fromConnection?: string | null;
  /** The engine of the form it's opened from (for messages). */
  driver?: string;
}>();
const emit = defineEmits<{ saved: [profile: SshProfile]; close: [] }>();
const { t } = useTranslation();

const name = ref(props.profile?.name ?? '');
const values = ref<SshValues>(props.initial ? { ...props.initial.values, enabled: true, profile: '' } : valuesFromOptions(props.profile?.options ?? {}));
const drafts = reactive<Record<string, SourceDraft>>({});
{
  const src = tunnelSources(props.profile?.options ?? {});
  for (const k of ['ssh.password', 'ssh.passphrase'] as const) {
    const d = props.initial?.drafts[k];
    drafts[k] = d ? (JSON.parse(JSON.stringify(d)) as SourceDraft) : draftFrom(src[k]);
  }
}
const saving = ref(false);
/** Secrets already stored: the profile's, or the connection's to copy. */
const secretsKept = computed(() => !!props.profile || !!props.fromConnection);

const missing = computed(() => {
  const out: string[] = [];
  if (!name.value.trim()) out.push(t('tunnel:profiles.name'));
  const s = values.value;
  if (!s.host.trim()) out.push(t('tunnel:host'));
  if (!s.user.trim()) out.push(t('tunnel:user'));
  if (s.auth === 'key' && !s.key_path.trim()) out.push(t('tunnel:keyPath'));
  const field = s.auth === 'password' ? 'ssh.password' : s.auth === 'key' ? 'ssh.passphrase' : null;
  if (field) for (const k of missingInputs(drafts[field])) out.push(`${t(field === 'ssh.password' ? 'tunnel:password' : 'tunnel:passphrase')} · ${t(`connection:source.input.${k}`)}`);
  return out;
});

async function save() {
  if (missing.value.length) {
    ElMessage.warning(t('connection:form.missingFields', { fields: missing.value.join(', ') }));
    return;
  }
  saving.value = true;
  try {
    const saved = await sshProfilesApi.save(
      { id: props.profile?.id ?? '', name: name.value.trim(), options: optionsFromValues(values.value, drafts), updated_at: '' },
      props.profile ? null : props.fromConnection ?? null,
    );
    await loadSshProfiles();
    ElMessage.success(t('tunnel:profiles.saved'));
    emit('saved', saved);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    saving.value = false;
  }
}
</script>

<template>
  <el-dialog
    :model-value="true"
    :title="profile ? $t('tunnel:profiles.editTitle', { name: profile.name }) : $t('tunnel:profiles.newTitle')"
    width="520px"
    append-to-body
    @close="emit('close')"
  >
    <el-form label-position="top" class="spd" @submit.prevent="save">
      <el-form-item :label="$t('tunnel:profiles.name')" required>
        <el-input v-model="name" :placeholder="$t('tunnel:profiles.namePlaceholder')" autofocus />
      </el-form-item>
      <p class="spd-help">{{ $t('tunnel:profiles.dialogHelp') }}</p>
      <SshTunnelSection
        v-model="values" bare :editing="secretsKept" :drafts="drafts"
        :connection-id="profile?.id ?? fromConnection ?? null" :driver="driver ?? ''"
      />
    </el-form>
    <template #footer>
      <el-button @click="emit('close')">{{ $t('common:cancel') }}</el-button>
      <el-button type="primary" :loading="saving" @click="save">{{ profile ? $t('common:save') : $t('tunnel:profiles.create') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.spd-help { font-size: 11.5px; color: var(--nm-text-dim); line-height: 1.4; margin: -4px 0 10px; }
</style>
