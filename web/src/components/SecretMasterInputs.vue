<script setup lang="ts">
import { computed } from 'vue';
import { MASTER_SOURCES, type SourceDraft } from '../composables/secretSources';

// The master password (KeePass, Bitwarden, Delinea's password, Zoho's
// passphrase): where it comes from and, kept in the keychain, its input.
// Part of SecretSourcePicker's grid: the two cells and the help below.

const draft = defineModel<SourceDraft>({ required: true });
defineProps<{ editing: boolean; requiredCommand: boolean }>();
const masters = computed(() => MASTER_SOURCES.filter((m) => m !== 'none' || draft.value.kind === 'keepass'));
</script>

<template>
  <el-form-item :label="$t(`connection:source.input.master_${draft.kind}`)">
    <el-select v-model="draft.master" style="width: 100%">
      <el-option v-for="m in masters" :key="m" :value="m" :label="$t(`connection:source.master.${m}`)" />
    </el-select>
  </el-form-item>
  <el-form-item v-if="draft.master === 'keychain'" :label="$t(`connection:source.input.master_value_${draft.kind}`)">
    <el-input v-model="draft.token" type="password" show-password autocomplete="new-password" :placeholder="editing ? $t('connection:form.secretKeptFem') : ''" />
  </el-form-item>
  <el-form-item v-else-if="draft.master === 'command'" :label="$t('connection:source.input.master_command')" :required="requiredCommand">
    <el-input v-model="draft.master_command" class="ss-mono" :placeholder="$t('connection:source.placeholder.command')" spellcheck="false" />
  </el-form-item>
  <div v-else />
  <div class="ssm-help">{{ $t(`connection:source.help.${draft.kind}`) }}</div>
</template>

<style scoped>
.ssm-help { grid-column: 1 / -1; font-size: 11.5px; color: var(--nm-text-dim); line-height: 1.4; margin: 2px 0 8px; }
</style>
