<script setup lang="ts">
import { computed, nextTick, onMounted, reactive, ref } from 'vue';
import { paramLiteral, paramProblem, type ParamItem, type ParamTarget, type ParamType, type ParamValue } from '../composables/queryParams';

// Before a run whose text has parameters (`:name`, `?`, `@name`): a value and
// a type for each. Ejecutar puts them in the text as literals of the engine
// (shown at the right of each row); Cancelar doesn't run.

const props = defineProps<{
  items: ParamItem[];
  /** Starting values (the tab's last ones, or a guess). */
  values: Record<string, ParamValue>;
  target: ParamTarget;
  /** The engine has a NULL literal. */
  allowNull: boolean;
}>();
const emit = defineEmits<{
  /** Run with these values. */
  submit: [values: Record<string, ParamValue>];
  cancel: [];
  /** Run the text as it is, and stop looking for parameters in this tab. */
  disable: [];
}>();

const vals = reactive<Record<string, ParamValue>>(Object.fromEntries(props.items.map((i) => [i.key, { ...props.values[i.key] }])));
const tried = ref(false);
const types = computed<ParamType[]>(() => (props.allowNull ? ['text', 'number', 'date', 'null'] : ['text', 'number', 'date']));

function problem(key: string): string | null {
  const p = paramProblem(vals[key]);
  return p ? `queryParams:invalid.${p}` : null;
}
function preview(key: string): string {
  return problem(key) ? '' : paramLiteral(vals[key], props.target);
}
function submit() {
  tried.value = true;
  if (props.items.some((i) => problem(i.key))) return;
  emit('submit', { ...vals });
}

const box = ref<HTMLDivElement | null>(null);
onMounted(async () => {
  await nextTick();
  box.value?.querySelector<HTMLInputElement>('input:not([disabled]):not([readonly])')?.focus();
});
</script>

<template>
  <el-dialog
    :model-value="true"
    :title="$t('queryParams:title')"
    width="620px"
    append-to-body
    :close-on-click-modal="false"
    @update:model-value="(v: boolean) => { if (!v) emit('cancel'); }"
  >
    <p class="qp-intro">{{ $t('queryParams:intro') }}</p>
    <div ref="box" class="qp-rows">
      <div v-for="i in items" :key="i.key" class="qp-row">
        <code class="qp-name" :title="i.label">{{ i.label }}</code>
        <el-select v-model="vals[i.key].type" size="small" class="qp-type" :aria-label="$t('queryParams:type')">
          <el-option v-for="ty in types" :key="ty" :value="ty" :label="$t(`queryParams:types.${ty}`)" />
        </el-select>
        <el-input
          v-model="vals[i.key].value"
          size="small"
          class="qp-value"
          :disabled="vals[i.key].type === 'null'"
          :placeholder="vals[i.key].type === 'null' ? 'NULL' : $t(`queryParams:placeholder.${vals[i.key].type}`)"
          :aria-label="i.label"
          :class="{ bad: tried && problem(i.key) }"
          @keydown.enter.prevent="submit"
        />
        <code class="qp-lit nm-selectable" :title="preview(i.key)">{{ tried && problem(i.key) ? '' : preview(i.key) }}</code>
        <div v-if="tried && problem(i.key)" class="qp-err">{{ $t(problem(i.key)!) }}</div>
      </div>
    </div>
    <template #footer>
      <div class="qp-foot">
        <el-button link size="small" :title="$t('queryParams:disableTip')" @click="emit('disable')">{{ $t('queryParams:disable') }}</el-button>
        <div class="nm-spacer" />
        <el-button @click="emit('cancel')">{{ $t('common:cancel') }}</el-button>
        <el-button type="primary" @click="submit">{{ $t('common:run') }}</el-button>
      </div>
    </template>
  </el-dialog>
</template>

<style scoped>
.qp-intro { margin: 0 0 12px; color: var(--nm-text-dim); font-size: 12.5px; line-height: 1.5; }
.qp-rows { display: flex; flex-direction: column; gap: 8px; max-height: 50vh; overflow: auto; }
.qp-row { display: grid; grid-template-columns: minmax(80px, 140px) 104px 1fr minmax(80px, 150px); align-items: center; gap: 8px; }
.qp-name { font-family: var(--nm-mono); font-size: 12.5px; color: var(--nm-text-strong); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.qp-lit { font-family: var(--nm-mono); font-size: 11.5px; color: var(--nm-text-dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.qp-value.bad :deep(.el-input__wrapper) { box-shadow: 0 0 0 1px var(--nm-danger) inset; }
.qp-err { grid-column: 3 / 5; margin-top: -4px; font-size: 11.5px; color: var(--nm-danger); }
.qp-foot { display: flex; align-items: center; gap: 8px; }
</style>
