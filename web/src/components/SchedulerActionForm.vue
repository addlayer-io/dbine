<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { errorMessage } from '../api/client';
import type { FormField, FormRow, FormSection } from '../api/scheduler';
import { answerForm, schedulerDialogs as d } from '../composables/schedulerDialogs';
import {
  formValues, initialLists, initialValues, listVisible, refOptions, sectionVisible, topContext, validate,
  type FormCtx, type Scope,
} from '../composables/schedulerForm';
import { tb } from '../i18n/backend';
import SchedulerFormFields from './SchedulerFormFields.vue';
import SchedulerFormList from './SchedulerFormList.vue';

// The form of a scheduler action that asks for values first (a time range,
// a job's parameters, a running chain's step), as the driver describes it
// for that object: its fields and lists drawn and checked like the
// designer's (`when`, choices, `{field}` sources, required fields and
// bounds), its sections one under the other. "OK" hands the values to the
// request's `submit` — the action's script, its confirmation, the run —
// while the form waits underneath; what fails there shows here with the
// values kept, and a "no" to the script comes back to the form. Drawn by
// SchedulerDialogs.vue.

const values = ref<Scope>({});
const lists = ref<Record<string, FormRow[]>>({});
/** OK was pressed once: problems show from then on. */
const showErrors = ref(false);
const error = ref<string | null>(null);
const busy = ref(false);

// A new request starts from the form's values (and its fields' defaults).
watch(() => d.form, (r) => {
  if (!r) return;
  values.value = initialValues(r.form, true);
  lists.value = initialLists(r.form);
  showErrors.value = false;
  error.value = null;
  busy.value = false;
}, { immediate: true });

const readOnly = computed(() => !!d.form?.form.read_only);
const top = computed<FormCtx>(() => (d.form ? topContext(d.form.form, values.value) : { fields: [], values: values.value }));
/** The sections that show now (a field may pick them). */
const sections = computed<FormSection[]>(() => (d.form ? d.form.form.sections.filter((s) => sectionVisible(s, top.value)) : []));
const shownLists = (s: FormSection) => (s.lists ?? []).filter((l) => listVisible(l, top.value));
const problems = computed(() => (d.form ? validate(d.form.form, values.value, lists.value, true) : null));
const shown = computed(() => (showErrors.value ? problems.value : null));

/** A top-level `ref` field's choices (a source with `{field}`s fills from
 *  the form's values), or null: free text. */
function choicesFor(f: FormField, ctx: FormCtx = top.value): [string, string][] | null {
  return d.form && f.kind.type === 'ref' ? refOptions(f.kind.source, d.form.form, lists.value, ctx) : [];
}

function listChanged(next: { values: Scope; lists: Record<string, FormRow[]> }) {
  if (next.values !== values.value) values.value = next.values;
  lists.value = next.lists;
}

async function ok() {
  const r = d.form;
  if (!r || busy.value || readOnly.value) return;
  if (problems.value?.count) {
    showErrors.value = true;
    return;
  }
  error.value = null;
  busy.value = true;
  try {
    const done = await r.submit(formValues(r.form, values.value, lists.value));
    if (done && d.form === r) answerForm(true);
  } catch (e) {
    if (d.form === r) error.value = errorMessage(e);
  } finally {
    busy.value = false;
  }
}

/** Closing (X, Escape, Cancelar) waits while the action's script is asked
 *  or the action runs. */
function beforeClose(done: () => void) {
  if (!busy.value) done();
}

function cancel() {
  if (!busy.value) answerForm(false);
}
</script>

<template>
  <el-dialog
    :model-value="!!d.form"
    :title="d.form?.title"
    width="min(760px, 92vw)"
    append-to-body
    :close-on-click-modal="false"
    :before-close="beforeClose"
    class="saf"
    @close="answerForm(false)"
  >
    <template v-if="d.form">
      <p v-if="d.form.message" class="saf-msg">{{ d.form.message }}</p>
      <div v-if="d.form.form.notes?.length" class="saf-notes">
        <div v-for="(n, i) in d.form.form.notes" :key="i" class="saf-notice">
          <el-icon><ei-info-filled /></el-icon><span>{{ tb(n) }}</span>
        </div>
      </div>
      <div class="saf-body">
        <section v-for="s in sections" :key="s.key" class="saf-section">
          <h3 v-if="sections.length > 1" class="nm-section-title saf-title">{{ tb(s.label) }}</h3>
          <SchedulerFormFields
            v-if="s.fields?.length"
            :fields="s.fields"
            :values="values"
            :ctx="top"
            :problems="shown?.fields"
            :read-only="readOnly"
            :is-new="true"
            :dialect="d.form.dialect"
            :choices-for="choicesFor"
            @update="(k, v) => (values[k] = v)"
          />
          <SchedulerFormList
            v-for="l in shownLists(s)"
            :key="l.key"
            :list="l"
            :form="d.form.form"
            :values="values"
            :lists="lists"
            :top="top"
            :read-only="readOnly"
            :is-new="true"
            :dialect="d.form.dialect"
            :problems="shown"
            @change="listChanged"
          />
        </section>
      </div>
      <p v-if="shown?.count" class="saf-invalid">{{ $t('scheduled:designer.invalid') }}</p>
      <el-alert v-if="error" type="error" :title="error" :closable="false" show-icon class="saf-error" />
    </template>
    <template #footer>
      <template v-if="readOnly">
        <el-button type="primary" @click="cancel">{{ $t('common:close') }}</el-button>
      </template>
      <template v-else>
        <el-button :disabled="busy" @click="cancel">{{ $t('common:cancel') }}</el-button>
        <el-button type="primary" :loading="busy" @click="ok">{{ d.form?.okLabel }}</el-button>
      </template>
    </template>
  </el-dialog>
</template>

<style scoped>
.saf-msg { margin: 0 0 10px; font-size: 13px; line-height: 1.5; color: var(--nm-text); }
.saf-notes { display: flex; flex-direction: column; gap: 4px; margin-bottom: 8px; }
.saf-notice { display: flex; align-items: baseline; gap: 6px; font-size: 12px; line-height: 1.45; color: var(--nm-text-dim); }
.saf-notice .el-icon { flex: none; position: relative; top: 1px; color: var(--nm-info); }
.saf-body { max-height: min(58vh, 560px); overflow: auto; padding-right: 6px; display: flex; flex-direction: column; gap: 14px; }
.saf-section { display: flex; flex-direction: column; gap: 12px; }
.saf-title { margin: 0; }
.saf-body :deep(.sff-row) { grid-template-columns: 180px minmax(0, 1fr); }
.saf-body :deep(.sff-row.wide) { grid-template-columns: minmax(0, 1fr); }
.saf-invalid { margin: 10px 0 0; font-size: 12px; color: var(--nm-danger); }
.saf-error { margin-top: 10px; }
.saf-error :deep(.el-alert__title) { white-space: pre-wrap; }
</style>
