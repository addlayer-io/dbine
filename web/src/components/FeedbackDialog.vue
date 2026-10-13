<script setup lang="ts">
import { computed, ref, watch, type Component } from 'vue';
import { ElMessage } from 'element-plus';
import { ChatLineRound, Coin, MagicStick, QuestionFilled, WarningFilled } from '@element-plus/icons-vue';
import { errorMessage } from '../api/client';
import { feedbackApi } from '../api/feedback';
import type { FeedbackSystemInfo } from '../api/types';
import { buildFeedbackUrl, diagnosticsText, FEEDBACK_KINDS, MAX_TITLE, type FeedbackDiagnostics, type FeedbackKind } from '../composables/feedbackUrl';
import { language } from '../i18n';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore } from '../stores/tabs';

// "Enviar comentarios" (docs/feedback.md): the user picks a kind, writes a
// title and a description, and DBine opens the repo's issue form on GitHub
// with everything filled in; the user only presses "Create" there. The app
// sends nothing itself and the labels come from the form.

const props = defineProps<{ modelValue: boolean }>();
const emit = defineEmits<{ 'update:modelValue': [boolean] }>();

const conns = useConnectionsStore();
const tabs = useTabsStore();

const ICONS: Record<FeedbackKind, Component> = {
  bug: WarningFilled,
  enhancement: MagicStick,
  'driver-request': Coin,
  question: QuestionFilled,
};

const kind = ref<FeedbackKind>('bug');
const title = ref('');
const description = ref('');
const steps = ref('');
const withDiagnostics = ref(true);
const opened = ref(false);
const busy = ref(false);
const system = ref<FeedbackSystemInfo | null>(null);

// Diagnostics help with errors; for the rest they're opt-in.
watch(kind, (k) => { withDiagnostics.value = k === 'bug'; });

watch(() => props.modelValue, (open) => {
  if (open && !system.value) {
    feedbackApi.systemInfo()
      .then((s) => { system.value = s; })
      .catch(() => { system.value = { app_version: '', os: '', arch: '' }; });
  }
}, { immediate: true });

/** Exactly what the diagnostics say: the active tab's engine and server
 *  version, never its host, user, database or connection name. */
const diagnostics = computed<FeedbackDiagnostics>(() => {
  const id = tabs.active?.connectionId ?? '';
  const live = id ? conns.live[id] : undefined;
  const connected = live?.status === 'connected';
  return {
    app_version: system.value?.app_version ?? '',
    os: system.value?.os ?? '',
    arch: system.value?.arch ?? '',
    locale: language.value,
    engine: connected && kind.value !== 'driver-request' ? conns.byId(id)?.config.driver ?? null : null,
    server_version: connected && kind.value !== 'driver-request' ? live?.serverVersion ?? null : null,
  };
});
const preview = computed(() => diagnosticsText(diagnostics.value));

const built = computed(() => buildFeedbackUrl({
  kind: kind.value,
  title: title.value,
  description: description.value,
  steps: steps.value,
  diagnostics: withDiagnostics.value ? diagnostics.value : null,
}));

const titleOk = computed(() => title.value.trim().length >= (kind.value === 'driver-request' ? 2 : 5));
const descriptionOk = computed(() => description.value.trim().length >= 10);
const ready = computed(() => titleOk.value && descriptionOk.value && !!system.value);

async function open() {
  if (!ready.value || busy.value) return;
  busy.value = true;
  try {
    await feedbackApi.openIssueForm(built.value.url);
    opened.value = true;
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    busy.value = false;
  }
}

function close() {
  emit('update:modelValue', false);
}

// After a report went out, the next one starts blank.
function closed() {
  if (!opened.value) return;
  opened.value = false;
  title.value = '';
  description.value = '';
  steps.value = '';
  kind.value = 'bug';
  withDiagnostics.value = true;
}
</script>

<template>
  <el-dialog
    :model-value="modelValue"
    width="580px"
    append-to-body
    :close-on-click-modal="false"
    class="fb-dialog"
    @update:model-value="emit('update:modelValue', $event)"
    @closed="closed"
  >
    <template #header>
      <div class="fb-head">
        <el-icon :size="18"><ChatLineRound /></el-icon>
        <span>{{ $t('feedback:title') }}</span>
      </div>
    </template>

    <div v-if="opened" class="fb-done">
      <p class="fb-done-title">{{ $t('feedback:done.title') }}</p>
      <p>{{ $t('feedback:done.text') }}</p>
    </div>

    <div v-else class="fb-body">
      <p class="fb-lead">{{ $t('feedback:lead') }}</p>

      <div class="fb-kinds" role="radiogroup" :aria-label="$t('feedback:kindLabel')">
        <button
          v-for="k in FEEDBACK_KINDS"
          :key="k"
          type="button"
          role="radio"
          class="fb-kind"
          :class="{ active: kind === k }"
          :aria-checked="kind === k"
          @click="kind = k"
        >
          <el-icon :size="18"><component :is="ICONS[k]" /></el-icon>
          <span class="fb-kind-name">{{ $t(`feedback:kinds.${k}.name`) }}</span>
          <span class="fb-kind-hint">{{ $t(`feedback:kinds.${k}.hint`) }}</span>
        </button>
      </div>

      <label class="fb-label" for="fb-title">{{ kind === 'driver-request' ? $t('feedback:engine') : $t('feedback:titleField') }}</label>
      <el-input
        id="fb-title"
        v-model="title"
        :maxlength="MAX_TITLE"
        :placeholder="$t(`feedback:kinds.${kind}.titlePlaceholder`)"
      />

      <label class="fb-label" for="fb-description">{{ $t('feedback:description') }}</label>
      <el-input
        id="fb-description"
        v-model="description"
        type="textarea"
        :autosize="{ minRows: 4, maxRows: 10 }"
        :placeholder="$t(`feedback:kinds.${kind}.placeholder`)"
      />

      <template v-if="kind === 'bug'">
        <label class="fb-label" for="fb-steps">{{ $t('feedback:steps') }} <span class="fb-optional">{{ $t('feedback:optional') }}</span></label>
        <el-input
          id="fb-steps"
          v-model="steps"
          type="textarea"
          :autosize="{ minRows: 2, maxRows: 6 }"
          :placeholder="$t('feedback:stepsPlaceholder')"
        />
      </template>

      <el-checkbox v-model="withDiagnostics" class="fb-diag-check">{{ $t('feedback:diagnostics') }}</el-checkbox>
      <div v-if="withDiagnostics" class="fb-diag">
        <pre class="nm-selectable">{{ preview }}</pre>
        <p>{{ $t('feedback:diagnosticsNever') }}</p>
      </div>

      <p v-if="built.truncated" class="fb-truncated">{{ $t('feedback:truncated') }}</p>

      <p class="fb-public">
        <el-icon :size="14"><WarningFilled /></el-icon>
        <span>{{ $t('feedback:publicNote') }}</span>
      </p>
    </div>

    <template #footer>
      <template v-if="opened">
        <el-button @click="open">{{ $t('feedback:openAgain') }}</el-button>
        <el-button type="primary" @click="close">{{ $t('common:close') }}</el-button>
      </template>
      <template v-else>
        <el-button @click="close">{{ $t('common:cancel') }}</el-button>
        <el-button type="primary" :disabled="!ready" :loading="busy" @click="open">{{ $t('feedback:continue') }}</el-button>
      </template>
    </template>
  </el-dialog>
</template>

<style scoped lang="scss">
.fb-head { display: flex; align-items: center; gap: 8px; font-size: 15px; color: var(--nm-text-strong); }
.fb-body { display: flex; flex-direction: column; }
.fb-lead { margin: 0 0 12px; color: var(--nm-text-dim); line-height: 1.5; }
.fb-kinds { display: grid; grid-template-columns: repeat(2, 1fr); gap: 8px; margin-bottom: 14px; }
.fb-kind {
  display: grid; grid-template-columns: auto 1fr; grid-template-rows: auto auto; column-gap: 10px; row-gap: 2px;
  align-items: center; text-align: left; padding: 10px 12px; cursor: pointer;
  background: var(--nm-bg-card); color: var(--nm-text); border: 1px solid var(--nm-border); border-radius: var(--nm-radius-sm);
  font: inherit;
  .el-icon { grid-row: 1 / span 2; color: var(--nm-text-muted); }
  &:hover { border-color: var(--nm-text-muted); }
  &.active { border-color: var(--nm-accent); box-shadow: inset 0 0 0 1px var(--nm-accent); .el-icon { color: var(--nm-accent); } }
  &:focus-visible { outline: 1px solid var(--ide-focus); outline-offset: 1px; }
}
.fb-kind-name { font-weight: 600; color: var(--nm-text-strong); }
.fb-kind-hint { font-size: 12px; color: var(--nm-text-muted); line-height: 1.35; }
.fb-label { margin: 10px 0 4px; font-size: 12.5px; color: var(--nm-text-dim); }
.fb-optional { color: var(--nm-text-muted); }
.fb-diag-check { margin-top: 12px; }
.fb-diag {
  margin-top: 4px; padding: 8px 10px; border: 1px solid var(--nm-border-soft); border-radius: var(--nm-radius-sm); background: var(--nm-bg-elev);
  pre { margin: 0; font-family: var(--nm-mono); font-size: 12px; white-space: pre-wrap; color: var(--nm-text); }
  p { margin: 6px 0 0; font-size: 12px; color: var(--nm-text-muted); }
}
.fb-truncated { margin: 10px 0 0; font-size: 12.5px; color: var(--nm-warning); }
.fb-public {
  display: flex; gap: 6px; align-items: flex-start; margin: 14px 0 0; font-size: 12.5px; line-height: 1.5; color: var(--nm-text-dim);
  .el-icon { margin-top: 2px; color: var(--nm-warning); flex: none; }
}
.fb-done { padding: 4px 2px; line-height: 1.55; }
.fb-done p { margin: 0 0 6px; }
.fb-done-title { font-weight: 600; color: var(--nm-text-strong); }
</style>
