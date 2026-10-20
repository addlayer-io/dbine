<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { scheduledApi, type Step, type StepWhen } from '../api/scheduled';
import { tb } from '../i18n/backend';
import { MESSAGE_CHANNELS, messageProblems } from '../composables/messageStep';

// The "Enviar un mensaje" step of a scheduled task: a Teams or Slack
// webhook, a Telegram bot or an ntfy topic, the text and "Solo si…". The
// webhook URL and the tokens are kept in the keychain when the task is
// saved: here they come back empty, and empty means "unchanged" (for the
// channel they were typed for).

const props = defineProps<{ step: Step; taskId: string; taskName: string }>();
const emit = defineEmits<{ change: [] }>();
const { t } = useTranslation();

const WHEN: StepWhen[] = ['always', 'alert', 'errors'];
const config = props.step.config;

if (!config.channel) config.channel = 'teams';
if (!config.when) config.when = 'always';
if (config.text == null) config.text = '{results}';
for (const k of ['url', 'token', 'chat_id']) if (config[k] == null) config[k] = '';

/** The channel the saved secrets are for (a step saved before has an id). */
const keptChannel = props.step.id ? String(config.channel) : null;
const problems = computed(() => messageProblems(config, keptChannel));

const testing = ref(false);
const result = ref<{ ok: boolean; text: string } | null>(null);
watch(() => [config.channel, config.url, config.token, config.chat_id], () => { result.value = null; });

async function sendTest() {
  testing.value = true;
  result.value = null;
  try {
    const said = await scheduledApi.testMessage(props.taskId, props.taskName, props.step, t('scheduled:message.testText'));
    result.value = { ok: true, text: tb(said) };
  } catch (e) {
    result.value = { ok: false, text: errorMessage(e) };
  } finally {
    testing.value = false;
  }
}

function changed() {
  emit('change');
}
</script>

<template>
  <div class="sg">
    <el-form-item :label="$t('scheduled:message.channel')">
      <el-radio-group v-model="config.channel" size="small" @change="changed">
        <el-radio-button v-for="ch in MESSAGE_CHANNELS" :key="ch" :value="ch">{{ $t(`scheduled:message.channels.${ch}`) }}</el-radio-button>
      </el-radio-group>
    </el-form-item>

    <div v-if="config.channel === 'telegram'" class="sg-row">
      <el-form-item :label="$t('scheduled:message.botToken')" class="sg-grow">
        <el-input v-model="config.token" type="password" show-password autocomplete="off" :placeholder="$t('scheduled:step.secretKept')" @input="changed" />
      </el-form-item>
      <el-form-item :label="$t('scheduled:message.chatId')" class="sg-grow">
        <el-input v-model="config.chat_id" placeholder="-1001234567890" @input="changed" />
      </el-form-item>
    </div>
    <template v-else>
      <el-form-item :label="$t(`scheduled:message.urlLabel.${config.channel}`)">
        <el-input v-model="config.url" type="password" show-password autocomplete="off" :placeholder="$t('scheduled:step.secretKept')" @input="changed" />
      </el-form-item>
      <el-form-item v-if="config.channel === 'ntfy'" :label="$t('scheduled:message.ntfyToken')">
        <el-input v-model="config.token" type="password" show-password autocomplete="off" :placeholder="$t('scheduled:message.optional')" @input="changed" />
      </el-form-item>
    </template>
    <p class="sg-hint">{{ $t(`scheduled:message.howTo.${config.channel}`) }}</p>
    <div v-for="p in problems" :key="p" class="sg-problem"><el-icon><ei-warning-filled /></el-icon><span>{{ $t(`scheduled:message.problems.${p}`) }}</span></div>

    <div class="sg-test">
      <el-button size="small" :loading="testing" :disabled="problems.length > 0" @click="sendTest">{{ $t('scheduled:message.test') }}</el-button>
      <span v-if="result" :class="result.ok ? 'sg-ok' : 'sg-err'">{{ result.text }}</span>
    </div>

    <el-form-item :label="$t('scheduled:message.text')">
      <el-input v-model="config.text" type="textarea" :autosize="{ minRows: 2, maxRows: 8 }" @input="changed" />
    </el-form-item>
    <p class="sg-hint">{{ $t('scheduled:message.textHint') }}</p>

    <el-form-item :label="$t('scheduled:mail.when')">
      <el-select v-model="config.when" class="sg-when" @change="changed">
        <el-option v-for="w in WHEN" :key="w" :value="w" :label="$t(`scheduled:mail.whenOptions.${w}`)" />
      </el-select>
    </el-form-item>
    <p v-if="config.when === 'errors'" class="sg-hint">{{ $t('scheduled:mail.errorsHint') }}</p>
    <p class="sg-hint">{{ $t('scheduled:message.privacy') }}</p>
  </div>
</template>

<style scoped>
.sg-row { display: flex; gap: 12px; flex-wrap: wrap; }
.sg-grow { flex: 1; min-width: 220px; }
.sg-when { width: 320px; }
.sg-hint { margin: 0 0 10px; font-size: 11.5px; color: var(--nm-text-dim); line-height: 1.5; }
.sg-problem { display: flex; align-items: center; gap: 6px; margin: 0 0 8px; font-size: 12px; color: var(--nm-warning); }
.sg-test { display: flex; align-items: center; gap: 10px; margin: 0 0 12px; font-size: 12px; }
.sg-test .el-button { margin: 0; }
.sg-ok { color: var(--nm-success); }
.sg-err { color: var(--nm-danger); word-break: break-word; }
</style>
