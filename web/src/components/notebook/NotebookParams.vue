<script setup lang="ts">
import { ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import type { NotebookParameter, NotebookParamType, NotebookParamValue } from '../../api/types';
import { PARAM_TYPES, newParamName, paramNameProblem, paramValueProblem } from '../../composables/notebookParams';
import NotebookParamInput from './NotebookParamInput.vue';

// "Parámetros": the notebook's parameters, at its top (docs/notebooks.md#parameters).
// Each one: name, type, default, required, description; a list also its
// values. The file keeps them (metadata.dbine.parameters); the cells use
// them as `{{name}}`.

const props = defineProps<{
  parameters: NotebookParameter[];
  /** How many cells use each one (`{{name}}` in their text). */
  usage: Record<string, number>;
  disabled?: boolean;
}>();
const emit = defineEmits<{
  change: [parameters: NotebookParameter[]];
  /** A parameter renamed: the cells' `{{old}}` become `{{name}}`. */
  rename: [from: string, to: string];
}>();
const { t } = useTranslation();

/** Names being typed (committed on blur / Enter), by row. */
const names = ref<string[]>([]);
/** A list's values as typed, one per line, by row. */
const options = ref<string[]>([]);
watch(() => props.parameters, (ps) => {
  names.value = ps.map((p) => p.name);
  options.value = ps.map((p) => (p.options ?? []).join('\n'));
}, { immediate: true, deep: true });

function update(i: number, patch: Partial<NotebookParameter>) {
  const list = props.parameters.map((p, k) => (k === i ? { ...p, ...patch } : p));
  emit('change', list);
}
function commitName(i: number) {
  const name = names.value[i].trim();
  const old = props.parameters[i].name;
  if (name === old) return;
  const problem = paramNameProblem(props.parameters, i, name);
  if (problem) {
    ElMessage.error({ message: t(`notebook:params.name.${problem}`, { name }), duration: 4000 });
    names.value[i] = old;
    return;
  }
  update(i, { name });
  emit('rename', old, name);
}
function setType(i: number, type: NotebookParamType) {
  const p = props.parameters[i];
  // A default of another type would be invalid: it goes.
  const keep = p.default != null && !paramValueProblem({ ...p, type }, p.default);
  update(i, { type, default: keep ? p.default : null, options: type === 'list' ? p.options ?? [] : undefined });
}
function setDefault(i: number, v: NotebookParamValue) {
  update(i, { default: v });
}
function commitOptions(i: number) {
  const list = [...new Set(options.value[i].split('\n').map((o) => o.trim()).filter(Boolean))];
  const p = props.parameters[i];
  update(i, { options: list, default: p.default != null && !list.includes(String(p.default)) ? null : p.default });
}
function add() {
  emit('change', [...props.parameters, { name: newParamName(props.parameters), type: 'text', default: null, description: null, required: false }]);
}
function remove(i: number) {
  emit('change', props.parameters.filter((_, k) => k !== i));
}
const example = (name: string) => `{{${name}}}`;
const OPEN = '{{';
const CLOSE = '}}';
</script>

<template>
  <div class="nbp" role="region" :aria-label="$t('notebook:params.title')">
    <div v-if="parameters.length" class="nbp-grid">
      <div class="nbp-head">{{ $t('notebook:params.name.label') }}</div>
      <div class="nbp-head">{{ $t('notebook:params.type') }}</div>
      <div class="nbp-head">{{ $t('notebook:params.default') }}</div>
      <div class="nbp-head nbp-c">{{ $t('notebook:params.required') }}</div>
      <div class="nbp-head">{{ $t('notebook:params.description') }}</div>
      <div class="nbp-head" />
      <template v-for="(p, i) in parameters" :key="i">
        <div class="nbp-name">
          <el-input
            v-model="names[i]" size="small" :disabled="disabled" :aria-label="$t('notebook:params.name.label')"
            @blur="commitName(i)" @keydown.enter.prevent="commitName(i)"
          >
            <template #prefix><span class="nbp-braces" v-text="OPEN" /></template>
            <template #suffix><span class="nbp-braces" v-text="CLOSE" /></template>
          </el-input>
          <span class="nbp-usage nm-muted" :title="$t('notebook:params.usageTip', { example: example(p.name) })">
            {{ usage[p.name] ? $t('notebook:params.usage', { count: usage[p.name] }) : $t('notebook:params.unused') }}
          </span>
        </div>
        <el-select :model-value="p.type" size="small" :disabled="disabled" :aria-label="$t('notebook:params.type')" @update:model-value="(v: NotebookParamType) => setType(i, v)">
          <el-option v-for="ty in PARAM_TYPES" :key="ty" :value="ty" :label="$t(`notebook:params.types.${ty}`)" />
        </el-select>
        <div class="nbp-default">
          <NotebookParamInput
            :param="p" :model-value="p.default" :placeholder="$t('notebook:params.noDefault')"
            :invalid="!!paramValueProblem(p, p.default)" @update:model-value="(v: NotebookParamValue) => setDefault(i, v)"
          />
          <el-input
            v-if="p.type === 'list'" v-model="options[i]" type="textarea" :autosize="{ minRows: 2, maxRows: 6 }" size="small"
            :placeholder="$t('notebook:params.optionsPlaceholder')" :disabled="disabled" :aria-label="$t('notebook:params.options')"
            @blur="commitOptions(i)"
          />
        </div>
        <div class="nbp-c">
          <el-checkbox :model-value="!!p.required" :disabled="disabled" :aria-label="$t('notebook:params.required')" @update:model-value="(v: boolean) => update(i, { required: v })" />
        </div>
        <el-input
          :model-value="p.description ?? ''" size="small" :disabled="disabled" :placeholder="$t('notebook:params.descriptionPlaceholder')"
          :aria-label="$t('notebook:params.description')" @update:model-value="(v: string) => update(i, { description: v || null })"
        />
        <el-button size="small" link :disabled="disabled" :aria-label="$t('notebook:params.remove')" :title="$t('notebook:params.remove')" @click="remove(i)">
          <el-icon><ei-delete /></el-icon>
        </el-button>
      </template>
    </div>
    <div class="nbp-foot">
      <el-button size="small" :disabled="disabled" @click="add"><el-icon><ei-plus /></el-icon>&nbsp;{{ $t('notebook:params.add') }}</el-button>
      <span class="nm-muted nbp-hint">{{ $t('notebook:params.hint', { example: example(parameters[0]?.name ?? 'mes') }) }}</span>
    </div>
  </div>
</template>

<style scoped>
.nbp { padding: 8px 12px 10px; border-bottom: 1px solid var(--nm-border); background: var(--nm-bg-soft, var(--ide-sidebar)); }
.nbp-grid {
  display: grid; grid-template-columns: minmax(150px, 1.1fr) 130px minmax(160px, 1.3fr) 76px minmax(140px, 1.5fr) 28px;
  gap: 6px 10px; align-items: start; margin-bottom: 8px;
}
.nbp-head { font-size: 11px; color: var(--nm-text-muted); text-transform: none; padding-bottom: 2px; }
.nbp-c { display: flex; justify-content: center; }
.nbp-name { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
.nbp-name :deep(input) { font-family: var(--nm-mono); }
.nbp-braces { font-family: var(--nm-mono); font-size: 11px; color: var(--nm-text-muted); }
.nbp-usage { font-size: 11px; padding-left: 2px; }
.nbp-default { display: flex; flex-direction: column; gap: 4px; min-width: 0; }
.nbp-default :deep(.el-date-editor) { --el-date-editor-width: 100%; width: 100%; }
.nbp-foot { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; }
.nbp-hint { font-size: 11.5px; }
</style>
