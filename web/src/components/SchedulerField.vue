<script setup lang="ts">
import { computed } from 'vue';
import type { FormField } from '../api/scheduler';
import { codeMode, fieldOptions, humanDuration, splitOffset } from '../composables/schedulerForm';
import { tb } from '../i18n/backend';
import CodeEditor from './CodeEditor.vue';

// One field of the scheduler designer, drawn by its kind (api/scheduler.ts
// `FormKind`). Option labels come in Spanish from the driver (`tb`);
// placeholders are examples and show as they come. Values keep the formats
// the contract documents: dates YYYY-MM-DD, times HH:MM:SS, date-times RFC
// 3339 (their offset kept as it came), durations in seconds. A `ref` whose
// source hangs from another field (`perf_counter:{object}`) takes free text
// while the driver lists nothing for it (`freeText`).

const props = defineProps<{
  field: FormField;
  modelValue: unknown;
  /** A `ref` field's choices, resolved by the designer (`list:` sources included). */
  options?: [string, string][];
  /** A `ref` with nothing listed for its filled-in source: typed, not picked. */
  freeText?: boolean;
  disabled?: boolean;
  /** The object is new: a secret has nothing to keep. */
  isNew?: boolean;
  /** The driver's SQL dialect, for `code` fields in "sql". */
  dialect?: string;
  invalid?: boolean;
}>();
const emit = defineEmits<{ 'update:modelValue': [value: unknown] }>();

const kind = computed(() => props.field.kind);
const set = (v: unknown) => emit('update:modelValue', v);

const text = computed({
  get: () => (props.modelValue === null || props.modelValue === undefined ? '' : String(props.modelValue)),
  set: (v: string) => set(v),
});
const num = computed({
  get: () => (typeof props.modelValue === 'number' ? props.modelValue : props.modelValue === null || props.modelValue === undefined || props.modelValue === '' ? null : Number(props.modelValue)),
  set: (v: number | null | undefined) => set(v ?? null),
});
const bool = computed({ get: () => props.modelValue === true || props.modelValue === 'true', set: (v: boolean) => set(v) });
/** A list value (multi select, multiple ref). */
const many = computed({
  get: () => (Array.isArray(props.modelValue) ? props.modelValue.map(String) : props.modelValue == null || props.modelValue === '' ? [] : [String(props.modelValue)]),
  set: (v: string[]) => set(v),
});
/** A single choice. "" is a value like any other (SQL Server's "Cuenta de
 *  servicio del Agente" proxy option): only null means nothing chosen. A ""
 *  no option has shows as nothing chosen (and goes back as it came). */
const one = computed({
  get: () => {
    if (props.modelValue === null || props.modelValue === undefined) return null;
    const v = String(props.modelValue);
    return v === '' && !choices.value.some(([x]) => x === '') ? null : v;
  },
  set: (v: string | null | undefined) => set(v === undefined ? null : v),
});
/** A date-time without its offset, which goes back as it came. */
const dateTime = computed({
  get: () => (typeof props.modelValue === 'string' && props.modelValue ? splitOffset(props.modelValue)[0] : ''),
  set: (v: string | null) => {
    const offset = typeof props.modelValue === 'string' ? splitOffset(props.modelValue)[1] : '';
    set(v ? v + offset : null);
  },
});
const pick = computed({
  get: () => (typeof props.modelValue === 'string' && props.modelValue ? props.modelValue : ''),
  set: (v: string | null) => set(v || null),
});

/** Select options, with a value the list doesn't have kept (shown as it is). */
const choices = computed<[string, string][]>(() => {
  const base = fieldOptions(props.field, props.options).map(([v, l]): [string, string] => [v, tb(l)]);
  const current = Array.isArray(props.modelValue) ? props.modelValue.map(String) : props.modelValue == null || props.modelValue === '' ? [] : [String(props.modelValue)];
  const extra = current.filter((v) => !base.some(([x]) => x === v)).map((v): [string, string] => [v, v]);
  return [...base, ...extra];
});
const multiple = computed(() => kind.value.type === 'multi_select' || (kind.value.type === 'ref' && !!kind.value.multiple));
const code = computed(() => (kind.value.type === 'code' ? codeMode(kind.value.language, props.dialect ?? '') : null));
const placeholder = computed(() => props.field.placeholder ?? '');
</script>

<template>
  <div class="sf" :class="{ invalid }">
    <el-input
      v-if="kind.type === 'text' || kind.type === 'cron'"
      v-model="text"
      :class="{ mono: kind.type === 'cron' }"
      :placeholder="placeholder"
      :disabled="disabled"
      class="sf-input"
    />
    <el-input
      v-else-if="kind.type === 'textarea'"
      v-model="text"
      type="textarea"
      :autosize="{ minRows: 3, maxRows: 12 }"
      :placeholder="placeholder"
      :disabled="disabled"
    />
    <el-input
      v-else-if="kind.type === 'secret'"
      v-model="text"
      type="password"
      show-password
      autocomplete="new-password"
      :placeholder="isNew ? placeholder : $t('scheduled:designer.secretKeep')"
      :disabled="disabled"
      class="sf-input"
    />
    <!-- No min/max on the control: it would clamp a server value as it
         mounts (even disabled) and mark the form modified. The bounds are
         checked next to the field (schedulerForm.fieldProblem). -->
    <el-input-number
      v-else-if="kind.type === 'number'"
      v-model="num"
      :placeholder="placeholder"
      :disabled="disabled"
      controls-position="right"
      class="sf-num"
    />
    <div v-else-if="kind.type === 'duration'" class="sf-inline">
      <el-input-number v-model="num" :disabled="disabled" controls-position="right" class="sf-num" />
      <span class="sf-unit">{{ $t('scheduled:designer.seconds') }}</span>
      <span v-if="typeof num === 'number' && num >= 60" class="sf-unit">= {{ humanDuration(num) }}</span>
    </div>
    <el-switch v-else-if="kind.type === 'bool'" v-model="bool" :disabled="disabled" />
    <el-input v-else-if="kind.type === 'ref' && freeText && !multiple" v-model="text" :placeholder="placeholder" :disabled="disabled" class="sf-input" />
    <el-select
      v-else-if="kind.type === 'ref' && freeText"
      v-model="many"
      multiple
      filterable
      allow-create
      default-first-option
      :reserve-keyword="false"
      :placeholder="placeholder"
      :disabled="disabled"
      class="sf-input"
    />
    <el-select
      v-else-if="(kind.type === 'select' || kind.type === 'ref') && !multiple"
      v-model="one"
      :empty-values="[null, undefined]"
      :value-on-clear="null"
      :placeholder="placeholder"
      :disabled="disabled"
      :clearable="!field.required"
      :filterable="kind.type === 'ref' || choices.length > 8"
      class="sf-input"
    >
      <el-option v-for="[v, l] in choices" :key="v" :value="v" :label="l" />
    </el-select>
    <el-select
      v-else-if="multiple"
      v-model="many"
      multiple
      :placeholder="placeholder"
      :disabled="disabled"
      :filterable="kind.type === 'ref' || choices.length > 8"
      class="sf-input"
    >
      <el-option v-for="[v, l] in choices" :key="v" :value="v" :label="l" />
    </el-select>
    <div v-else-if="kind.type === 'code' && code" class="sf-code">
      <CodeEditor v-model="text" :language="code.language" :dialect="code.dialect" :read-only="disabled" :placeholder="placeholder" />
    </div>
    <el-date-picker
      v-else-if="kind.type === 'date'"
      v-model="pick"
      type="date"
      value-format="YYYY-MM-DD"
      :placeholder="placeholder"
      :disabled="disabled"
      class="sf-date"
    />
    <el-time-picker
      v-else-if="kind.type === 'time'"
      v-model="pick"
      value-format="HH:mm:ss"
      format="HH:mm:ss"
      :placeholder="placeholder"
      :disabled="disabled"
      class="sf-date"
    />
    <el-date-picker
      v-else-if="kind.type === 'date_time'"
      v-model="dateTime"
      type="datetime"
      value-format="YYYY-MM-DD[T]HH:mm:ss"
      format="YYYY-MM-DD HH:mm:ss"
      :placeholder="placeholder"
      :disabled="disabled"
      class="sf-date wide"
    />
    <el-input v-else v-model="text" :placeholder="placeholder" :disabled="disabled" class="sf-input" />
  </div>
</template>

<style scoped>
.sf { min-width: 0; }
.sf-input { width: 100%; max-width: 440px; }
.sf-num { width: 170px; }
.sf-date { width: 170px; }
.sf-date.wide { width: 220px; }
.sf-inline { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.sf-unit { font-size: 12px; color: var(--nm-text-dim); }
.mono :deep(input) { font-family: var(--nm-mono); font-size: 12px; }
.sf-code { height: 230px; min-height: 90px; resize: vertical; overflow: hidden; border: 1px solid var(--nm-border); border-radius: 4px; background: var(--ide-editor); }
.sf.invalid :deep(.el-input__wrapper),
.sf.invalid :deep(.el-select__wrapper),
.sf.invalid :deep(.el-textarea__inner) { box-shadow: 0 0 0 1px var(--nm-danger) inset !important; }
.sf.invalid .sf-code { border-color: var(--nm-danger); }
</style>
