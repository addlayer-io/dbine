<script setup lang="ts">
import { useTranslation } from 'i18next-vue';
import type { FormField } from '../api/scheduler';
import { isVisible, problemText, type FormCtx, type Problem, type Scope } from '../composables/schedulerForm';
import { tb } from '../i18n/backend';
import SchedulerField from './SchedulerField.vue';

// A labeled form of the scheduler designer: label, control, its help, and
// its problem once the checks ran. Used for a section's fields and for a
// list's row editor. `when` and `when_all` hide a field
// (composables/schedulerForm.ts).

defineProps<{
  fields: FormField[];
  values: Scope;
  /** Where `when` looks (the row's fields first, then the form's). */
  ctx: FormCtx;
  /** Field key → its problem (shown after a check), or null. */
  problems?: Record<string, Problem> | null;
  readOnly?: boolean;
  isNew?: boolean;
  dialect?: string;
  /** A `ref` field's choices where it is (`ctx`: its row's sources with
   *  `{field}`s fill from it), or null: free text (nothing listed). */
  choicesFor: (field: FormField, ctx: FormCtx) => [string, string][] | null;
}>();
const emit = defineEmits<{ update: [key: string, value: unknown] }>();
const { t } = useTranslation();

/** Code and long text take the whole width, under their label. */
const wide = (f: FormField) => f.kind.type === 'code';
</script>

<template>
  <div class="sff">
    <template v-for="(f, i) in fields" :key="`${i}:${f.key}`">
      <div v-if="isVisible(f, ctx)" class="sff-row" :class="{ wide: wide(f) }">
        <label class="sff-label">
          <span>{{ tb(f.label) }}</span>
          <span v-if="f.required && !f.read_only && !readOnly" class="sff-req" :title="$t('scheduled:designer.required')">*</span>
        </label>
        <div class="sff-ctl">
          <SchedulerField
            :field="f"
            :model-value="values[f.key]"
            :options="f.kind.type === 'ref' ? choicesFor(f, ctx) ?? [] : undefined"
            :free-text="f.kind.type === 'ref' && choicesFor(f, ctx) === null"
            :disabled="readOnly || f.read_only"
            :is-new="isNew"
            :dialect="dialect"
            :invalid="!!problems?.[f.key]"
            @update:model-value="emit('update', f.key, $event)"
          />
          <div v-if="problems?.[f.key]" class="sff-err">{{ problemText(problems[f.key], t) }}</div>
          <div v-if="f.help" class="sff-help">{{ tb(f.help) }}</div>
        </div>
      </div>
    </template>
  </div>
</template>

<style scoped>
.sff { display: flex; flex-direction: column; gap: 2px; }
.sff-row { display: grid; grid-template-columns: 220px minmax(0, 1fr); column-gap: 14px; align-items: start; padding: 5px 0; }
.sff-row.wide { grid-template-columns: minmax(0, 1fr); row-gap: 6px; }
.sff-label { display: flex; align-items: baseline; gap: 3px; padding-top: 5px; font-size: 12.5px; color: var(--nm-text); line-height: 1.35; }
.sff-row.wide .sff-label { padding-top: 0; }
.sff-req { color: var(--nm-danger); font-weight: 600; }
.sff-ctl { min-width: 0; }
.sff-err { margin-top: 3px; font-size: 11.5px; color: var(--nm-danger); }
.sff-help { margin-top: 3px; font-size: 11.5px; line-height: 1.45; color: var(--nm-text-dim); max-width: 640px; }
</style>
