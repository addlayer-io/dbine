<script setup lang="ts">
import { computed, reactive } from 'vue';
import type { NotebookParameter, NotebookParamValue } from '../../api/types';
import { effectiveValue, paramValueProblem } from '../../composables/notebookParams';
import NotebookParamInput from './NotebookParamInput.vue';

// "Valores de los parámetros": what a run asks for when a required
// parameter has no value (prefilled with this session's values, else the
// defaults), and what the toolbar's values open to change them.

const props = defineProps<{
  parameters: NotebookParameter[];
  /** This session's values (the defaults fill the rest). */
  values: Record<string, NotebookParamValue>;
  /** The ones the run uses (shown first); null: all. */
  names: string[] | null;
  /** 'run': the run waits for it ("Ejecutar"); 'edit': just the values ("Aplicar"). */
  mode: 'run' | 'edit';
}>();
const emit = defineEmits<{ submit: [values: Record<string, NotebookParamValue>]; cancel: [] }>();

const shown = computed(() => {
  const used = props.names;
  if (!used) return props.parameters;
  return [...props.parameters.filter((p) => used.includes(p.name)), ...props.parameters.filter((p) => !used.includes(p.name))];
});
const draft = reactive<Record<string, NotebookParamValue>>(Object.fromEntries(props.parameters.map((p) => [p.name, effectiveValue(p, props.values)])));
const problem = (p: NotebookParameter) => {
  const v = draft[p.name];
  // Changing values isn't a run: a required one may stay empty until then.
  if (props.mode === 'run' && p.required && (v === null || v === undefined || (typeof v === 'string' && !v.trim()))) return 'required';
  return paramValueProblem(p, v);
};
const touched = reactive(new Set<string>());
/** Problems show once the user tried to run, or edited that field. */
const tried = reactive({ on: false });
const visible = (p: NotebookParameter) => (tried.on || touched.has(p.name)) && !!problem(p);
const ok = computed(() => props.parameters.every((p) => !problem(p)));
function set(name: string, v: NotebookParamValue) {
  draft[name] = v;
  touched.add(name);
}
function submit() {
  tried.on = true;
  if (!ok.value) return;
  emit('submit', { ...draft });
}
const unused = (p: NotebookParameter) => !!props.names && !props.names.includes(p.name);
</script>

<template>
  <el-dialog
    :model-value="true" :title="$t('notebook:params.dialogTitle')" width="560px" append-to-body :close-on-click-modal="false"
    @update:model-value="(v: boolean) => { if (!v) emit('cancel'); }"
  >
    <p class="nbpd-intro">{{ mode === 'run' ? $t('notebook:params.dialogIntro') : $t('notebook:params.dialogEditIntro') }}</p>
    <form class="nbpd-form" @submit.prevent="submit">
      <template v-for="p in shown" :key="p.name">
        <label class="nbpd-label" :class="{ dim: unused(p) }" :for="`nbpd-${p.name}`">
          <code>{{ p.name }}</code><span v-if="p.required" class="nbpd-req" :title="$t('notebook:params.required')">*</span>
          <span class="nbpd-type">{{ $t(`notebook:params.types.${p.type}`) }}</span>
        </label>
        <div class="nbpd-field">
          <NotebookParamInput
            :id="`nbpd-${p.name}`" :param="p" :model-value="draft[p.name]" size="default" :invalid="visible(p)"
            :placeholder="p.required ? $t('notebook:params.requiredPlaceholder') : $t('notebook:params.emptyIsNull')" @update:model-value="(v: NotebookParamValue) => set(p.name, v)"
          />
          <div v-if="visible(p)" class="nbpd-err" role="alert">{{ $t(`notebook:params.problem.${problem(p)}`) }}</div>
          <div v-else-if="p.description" class="nbpd-desc nm-muted">{{ p.description }}</div>
          <div v-else-if="unused(p)" class="nbpd-desc nm-muted">{{ $t('notebook:params.notUsedHere') }}</div>
        </div>
      </template>
      <button type="submit" hidden />
    </form>
    <template #footer>
      <el-button @click="emit('cancel')">{{ $t('common:cancel') }}</el-button>
      <el-button type="primary" @click="submit">{{ mode === 'run' ? $t('common:run') : $t('notebook:params.apply') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.nbpd-intro { margin: 0 0 12px; color: var(--nm-text-dim); font-size: 13px; }
.nbpd-form { display: grid; grid-template-columns: minmax(120px, auto) 1fr; gap: 10px 14px; align-items: start; }
.nbpd-label { display: flex; align-items: baseline; gap: 4px; padding-top: 7px; font-size: 13px; flex-wrap: wrap; }
.nbpd-label code { font-family: var(--nm-mono); font-size: 12.5px; color: var(--nm-text); }
.nbpd-label.dim code { color: var(--nm-text-dim); }
.nbpd-req { color: var(--nm-danger); }
.nbpd-type { font-size: 11px; color: var(--nm-text-muted); }
.nbpd-field { min-width: 0; display: flex; flex-direction: column; gap: 3px; }
.nbpd-err { font-size: 12px; color: var(--nm-danger); }
.nbpd-desc { font-size: 12px; }
</style>
