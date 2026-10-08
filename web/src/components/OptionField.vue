<script setup lang="ts">
import type { Field, FieldChoices } from '../api/types';
import { tb } from '../i18n/backend';

// One option of a database form ("Nueva base de datos", "Propiedades"): a
// driver Field with its label and help, the server's suggestions as a
// filterable combo, and a checkbox that stores "true" / "".

defineProps<{ f: Field; modelValue: string | undefined; choices?: FieldChoices; placeholder: string; disabled?: boolean }>();
const emit = defineEmits<{ 'update:modelValue': [value: string] }>();
</script>

<template>
  <el-form-item :class="{ wide: f.kind.type === 'textarea' }">
    <template #label>
      <span class="of-label">{{ tb(f.label) }}</span>
      <el-tooltip v-if="f.help" :content="tb(f.help)" placement="top" :show-after="200">
        <el-icon class="of-info"><ei-info-filled /></el-icon>
      </el-tooltip>
    </template>
    <el-select
      v-if="f.kind.type === 'select'"
      :model-value="modelValue"
      clearable
      :disabled="disabled"
      :placeholder="placeholder"
      @update:model-value="(v: string) => emit('update:modelValue', v ?? '')"
    >
      <el-option v-for="[v, l] in f.kind.options" :key="v" :label="tb(l)" :value="v" />
    </el-select>
    <el-checkbox
      v-else-if="f.kind.type === 'bool'"
      :model-value="modelValue === 'true'"
      :disabled="disabled"
      @update:model-value="(on: string | number | boolean) => emit('update:modelValue', on ? 'true' : '')"
    />
    <el-select
      v-else-if="choices?.values.length"
      :model-value="modelValue"
      filterable
      allow-create
      default-first-option
      clearable
      :disabled="disabled"
      :placeholder="placeholder"
      @update:model-value="(v: string) => emit('update:modelValue', v ?? '')"
    >
      <el-option v-for="v in choices.values" :key="v" :label="v" :value="v" />
    </el-select>
    <el-input
      v-else
      :model-value="modelValue"
      :type="f.kind.type === 'number' ? 'number' : f.kind.type === 'textarea' ? 'textarea' : 'text'"
      :disabled="disabled"
      :placeholder="placeholder"
      @update:model-value="(v: string) => emit('update:modelValue', v)"
    />
  </el-form-item>
</template>

<style scoped>
.of-label { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.of-info { color: var(--nm-text-dim); cursor: help; flex-shrink: 0; }
:deep(.el-select) { width: 100%; }
</style>
