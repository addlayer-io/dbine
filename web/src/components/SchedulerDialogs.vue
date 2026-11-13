<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import { ElMessage } from 'element-plus';
import CodeEditor from './CodeEditor.vue';
import SchedulerActionForm from './SchedulerActionForm.vue';
import { answerScript, answerStep, schedulerDialogs as d, useDialogsHost } from '../composables/schedulerDialogs';
import { tb } from '../i18n/backend';
import { scriptWarnings } from '../composables/schedulerForm';

// Draws the scheduler's dialogs (composables/schedulerDialogs.ts): a
// script to read (Copiar, Abrir en una consulta) or to confirm before it
// runs, the steps a job can start at, and an action's form
// (SchedulerActionForm.vue; its script's confirmation opens over it).
// Mounted by each view that may ask; only the first one mounted draws.

const { t } = useTranslation();
const host = useDialogsHost();
const step = ref<string>('');

watch(() => d.step, (r) => { step.value = r?.steps[0]?.name ?? ''; });

/** What the driver warned about in the script it's about to run ("⚠"
 *  comment lines): listed above it, and the yes becomes "Continuar". */
const warnings = computed(() => (d.script?.mode === 'confirm' ? scriptWarnings(d.script.script).map((w) => tb(w)) : []));

async function copy() {
  if (!d.script) return;
  try {
    await navigator.clipboard.writeText(d.script.script);
    ElMessage.success(t('common:copied'));
  } catch {
    ElMessage.error(t('common:error'));
  }
}

function open() {
  const go = d.script?.openInQuery;
  answerScript(false);
  go?.();
}
</script>

<template>
  <template v-if="host">
    <SchedulerActionForm />

    <el-dialog
      :model-value="!!d.script"
      :title="d.script?.title"
      width="min(860px, 92vw)"
      append-to-body
      class="schd"
      @close="answerScript(false)"
    >
      <template v-if="d.script">
        <p v-if="d.script.message" class="schd-msg">{{ d.script.message }}</p>
        <el-alert v-if="d.script.warning" type="warning" :title="d.script.warning" :closable="false" show-icon class="schd-warn" />
        <el-alert v-if="d.script.notice" type="info" :title="d.script.notice" :closable="false" show-icon class="schd-warn" />
        <el-alert v-if="warnings.length" type="warning" :title="$t('scheduled:designer.consequences')" :closable="false" show-icon class="schd-warn">
          <ul class="schd-list">
            <li v-for="(w, i) in warnings" :key="i">{{ w }}</li>
          </ul>
        </el-alert>
        <div class="schd-code">
          <CodeEditor :model-value="d.script.script" language="sql" :dialect="d.script.dialect" read-only />
        </div>
      </template>
      <template #footer>
        <template v-if="d.script?.mode === 'view'">
          <el-button @click="copy">{{ $t('common:copy') }}</el-button>
          <el-button v-if="d.script.openInQuery" @click="open">{{ $t('scheduled:designer.openInQuery') }}</el-button>
          <el-button type="primary" @click="answerScript(false)">{{ $t('common:close') }}</el-button>
        </template>
        <template v-else-if="d.script">
          <el-button @click="answerScript(false)">{{ $t('common:cancel') }}</el-button>
          <el-button :type="d.script.danger ? 'danger' : 'primary'" @click="answerScript(true)">{{ warnings.length ? $t('scheduled:designer.continue') : (d.script.okLabel ?? $t('common:accept')) }}</el-button>
        </template>
      </template>
    </el-dialog>

    <el-dialog
      :model-value="!!d.step"
      :title="d.step?.title"
      width="min(520px, 92vw)"
      append-to-body
      @close="answerStep(null)"
    >
      <template v-if="d.step">
        <p class="schd-msg">{{ d.step.message }}</p>
        <el-radio-group v-model="step" class="schd-steps">
          <el-radio v-for="(s, i) in d.step.steps" :key="i" :value="s.name" class="schd-step">
            <span class="schd-num">{{ i + 1 }}</span>
            <span class="schd-name">{{ s.name }}</span>
            <span v-if="s.kind" class="schd-kind">{{ tb(s.kind) }}</span>
          </el-radio>
        </el-radio-group>
      </template>
      <template #footer>
        <el-button @click="answerStep(null)">{{ $t('common:cancel') }}</el-button>
        <el-button type="primary" :disabled="!step" @click="answerStep(step)">{{ d.step?.okLabel }}</el-button>
      </template>
    </el-dialog>
  </template>
</template>

<style scoped>
.schd-msg { margin: 0 0 10px; font-size: 13px; line-height: 1.5; color: var(--nm-text); }
.schd-warn { margin-bottom: 10px; }
.schd-list { margin: 4px 0 0; padding-left: 18px; }
.schd-code { height: min(52vh, 440px); border: 1px solid var(--nm-border-soft); border-radius: 4px; overflow: hidden; background: var(--ide-editor); }
.schd-steps { display: flex; flex-direction: column; align-items: stretch; gap: 2px; width: 100%; }
.schd-step { margin: 0; height: auto; padding: 5px 8px; border-radius: 4px; }
.schd-step:hover { background: var(--ide-hover); }
.schd-step :deep(.el-radio__label) { display: flex; align-items: baseline; gap: 8px; min-width: 0; }
.schd-num { flex: none; width: 18px; text-align: right; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; }
.schd-name { color: var(--nm-text-strong); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.schd-kind { flex: none; font-size: 10.5px; padding: 0 5px; border-radius: 3px; color: var(--nm-text-dim); background: var(--ide-hover); }
</style>
