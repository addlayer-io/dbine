<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { useTranslation } from 'i18next-vue';
import { mailApi } from '../api/mail';
import type { StepWhen } from '../api/scheduled';
import { useUiStore } from '../stores/ui';

// The "Enviar un mail" step of a scheduled task: recipients, subject, body,
// attachments and "Solo si…". Edits `config` in place; the server is
// Configuración › Correo.

const props = defineProps<{ config: Record<string, any> }>();
const emit = defineEmits<{ change: [] }>();
const { t } = useTranslation();
const ui = useUiStore();

const WHEN: StepWhen[] = ['always', 'alert', 'errors'];

/** No server saved yet: the step can't send. */
const noServer = ref(false);
onMounted(async () => {
  try { noServer.value = !(await mailApi.get()).settings; } catch { /* outside the app */ }
});

if (!Array.isArray(props.config.attachments)) props.config.attachments = [];
if (!props.config.when) props.config.when = 'always';

function addAttachment(path = '') {
  props.config.attachments.push(path);
  emit('change');
}
function removeAttachment(i: number) {
  props.config.attachments.splice(i, 1);
  emit('change');
}
async function browse(i: number | null) {
  let picked: string | string[] | null = null;
  try { picked = await openDialog({ multiple: false, title: t('scheduled:mail.pickFile') }); } catch { return; }
  if (typeof picked !== 'string') return;
  if (i === null) addAttachment(picked);
  else { props.config.attachments[i] = picked; emit('change'); }
}
</script>

<template>
  <div class="sm">
    <div v-if="noServer" class="sm-warn">
      <el-icon><ei-warning-filled /></el-icon>
      <span>{{ $t('scheduled:mail.noServer') }}</span>
      <el-button size="small" link type="primary" @click="ui.openSettings('mail')">{{ $t('scheduled:mail.configure') }}</el-button>
    </div>
    <div class="sm-row">
      <el-form-item :label="$t('scheduled:mail.to')" class="sm-grow">
        <el-input v-model="config.to" :placeholder="$t('scheduled:mail.toPlaceholder')" @input="emit('change')" />
      </el-form-item>
      <el-form-item :label="$t('scheduled:mail.cc')" class="sm-grow">
        <el-input v-model="config.cc" @input="emit('change')" />
      </el-form-item>
    </div>
    <el-form-item :label="$t('scheduled:mail.subject')">
      <el-input v-model="config.subject" @input="emit('change')" />
    </el-form-item>
    <el-form-item :label="$t('scheduled:mail.body')">
      <el-input v-model="config.body" type="textarea" :autosize="{ minRows: 4, maxRows: 12 }" @input="emit('change')" />
    </el-form-item>
    <el-form-item :label="$t('scheduled:mail.attachments')">
      <div class="sm-files">
        <div v-for="(_, i) in config.attachments" :key="i" class="sm-file">
          <el-input v-model="config.attachments[i]" placeholder="{steps.1.file}" @input="emit('change')">
            <template #append><el-button :title="$t('scheduled:mail.pickFile')" @click="browse(i)"><el-icon><ei-folder-opened /></el-icon></el-button></template>
          </el-input>
          <el-button text :title="$t('scheduled:mail.removeAttachment')" @click="removeAttachment(i)"><el-icon><ei-delete /></el-icon></el-button>
        </div>
        <div class="sm-file-actions">
          <el-button size="small" @click="addAttachment('{steps.1.file}')"><el-icon><ei-plus /></el-icon>&nbsp;{{ $t('scheduled:mail.addAttachment') }}</el-button>
          <el-button size="small" text @click="browse(null)">{{ $t('scheduled:mail.pickFile') }}</el-button>
        </div>
        <p class="sm-hint">{{ $t('scheduled:mail.attachmentsHint') }}</p>
      </div>
    </el-form-item>
    <el-form-item :label="$t('scheduled:mail.when')">
      <el-select v-model="config.when" class="sm-when" @change="emit('change')">
        <el-option v-for="w in WHEN" :key="w" :value="w" :label="$t(`scheduled:mail.whenOptions.${w}`)" />
      </el-select>
    </el-form-item>
    <p v-if="config.when === 'errors'" class="sm-hint">{{ $t('scheduled:mail.errorsHint') }}</p>
  </div>
</template>

<style scoped>
.sm-row { display: flex; gap: 12px; flex-wrap: wrap; }
.sm-grow { flex: 1; min-width: 220px; }
.sm-files { display: flex; flex-direction: column; gap: 6px; width: 100%; }
.sm-file { display: flex; gap: 4px; align-items: center; }
.sm-file .el-button { margin: 0; }
.sm-file-actions { display: flex; gap: 8px; align-items: center; }
.sm-file-actions .el-button { margin: 0; }
.sm-when { width: 320px; }
.sm-hint { margin: 0 0 10px; font-size: 11.5px; color: var(--nm-text-dim); line-height: 1.5; }
.sm-warn {
  display: flex; align-items: center; gap: 8px; padding: 6px 10px; margin: 0 0 10px; border-radius: 4px; font-size: 12px;
  border: 1px solid color-mix(in srgb, var(--nm-warning) 45%, transparent); background: color-mix(in srgb, var(--nm-warning) 8%, transparent);
}
.sm-warn .el-icon { color: var(--nm-warning); flex: none; }
</style>
