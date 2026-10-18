<script setup lang="ts">
import { computed } from 'vue';
import type { NotebookParameter, NotebookParamValue } from '../../api/types';

// One notebook parameter's value, with the input its type takes: text and
// numbers typed, dates and date-times picked (ISO text), booleans and
// lists chosen. Empty is null (the default, or NULL when there's none).

const props = withDefaults(defineProps<{
  param: NotebookParameter;
  modelValue: NotebookParamValue | undefined;
  placeholder?: string;
  size?: 'small' | 'default';
  invalid?: boolean;
}>(), { placeholder: '', size: 'small', invalid: false });
const emit = defineEmits<{ 'update:modelValue': [v: NotebookParamValue] }>();

const text = computed({
  get: () => (props.modelValue === null || props.modelValue === undefined ? '' : String(props.modelValue)),
  set: (v: string) => emit('update:modelValue', v === '' ? null : v),
});
/** Booleans as a choice: "" (none), "true", "false". */
const bool = computed({
  get: () => (props.modelValue === true || props.modelValue === 'true' ? 'true' : props.modelValue === false || props.modelValue === 'false' ? 'false' : ''),
  set: (v: string) => emit('update:modelValue', v === '' ? null : v === 'true'),
});
const date = computed({
  get: () => (typeof props.modelValue === 'string' && props.modelValue ? props.modelValue.replace(' ', 'T') : null),
  set: (v: string | null) => emit('update:modelValue', v || null),
});
</script>

<template>
  <el-input v-if="param.type === 'text' || param.type === 'number'" v-model="text" :size="size" :placeholder="placeholder" clearable :class="{ 'nbp-invalid': invalid }" :inputmode="param.type === 'number' ? 'decimal' : undefined" />
  <el-date-picker
    v-else-if="param.type === 'date'" v-model="date" type="date" value-format="YYYY-MM-DD" format="YYYY-MM-DD" :size="size" :placeholder="placeholder"
    style="width: 100%" :class="{ 'nbp-invalid': invalid }"
  />
  <el-date-picker
    v-else-if="param.type === 'datetime'" v-model="date" type="datetime" value-format="YYYY-MM-DDTHH:mm:ss" format="YYYY-MM-DD HH:mm:ss" :size="size" :placeholder="placeholder"
    style="width: 100%" :class="{ 'nbp-invalid': invalid }"
  />
  <el-select v-else-if="param.type === 'boolean'" v-model="bool" :size="size" :placeholder="placeholder" clearable :class="{ 'nbp-invalid': invalid }">
    <el-option value="true" :label="$t('notebook:params.true')" />
    <el-option value="false" :label="$t('notebook:params.false')" />
  </el-select>
  <el-select v-else v-model="text" :size="size" :placeholder="placeholder" clearable filterable :class="{ 'nbp-invalid': invalid }" :no-data-text="$t('notebook:params.noOptions')">
    <el-option v-for="o in param.options ?? []" :key="o" :value="o" :label="o" />
  </el-select>
</template>

<style scoped>
.nbp-invalid :deep(.el-input__wrapper), .nbp-invalid :deep(.el-select__wrapper) { box-shadow: 0 0 0 1px var(--nm-danger) inset; }
</style>
