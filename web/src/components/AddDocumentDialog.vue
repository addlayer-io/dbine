<script setup lang="ts">
import { ref, watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import CodeEditor from './CodeEditor.vue';

// "Agregar documento": a new document (or row, when the grid has no columns
// to fill, like an empty collection) typed as JSON. An object is one
// document, an array several. Nothing runs here: the pane turns them into
// the engine's insert code and "Guardar" shows it before running.

const props = defineProps<{
  open: boolean;
  /** Document engines say "documento"; the rest, "fila". */
  documents: boolean;
  /** Field names the result shows, for the starting template. */
  fields: string[];
}>();
const emit = defineEmits<{
  close: [];
  add: [docs: Record<string, unknown>[]];
}>();

const { t } = useTranslation();
const text = ref('');
const error = ref<string | null>(null);

/** `{ "field": null, … }` with the result's fields, minus a generated `_id`. */
function template() {
  const names = props.fields.filter((f) => f !== '_id');
  if (!names.length) return '{\n  \n}';
  return `{\n${names.map((n) => `  ${JSON.stringify(n)}: null`).join(',\n')}\n}`;
}

watch(() => props.open, (o) => {
  if (!o) return;
  text.value = template();
  error.value = null;
}, { immediate: true });

const isObject = (v: unknown): v is Record<string, unknown> => typeof v === 'object' && v !== null && !Array.isArray(v);

function add() {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text.value);
  } catch (e) {
    error.value = t('results:addDocument.invalidJson', { error: e instanceof Error ? e.message : String(e) });
    return;
  }
  const docs = Array.isArray(parsed) ? parsed : [parsed];
  if (!docs.length || !docs.every(isObject)) { error.value = t('results:addDocument.notObject'); return; }
  if (docs.some((d) => !Object.keys(d).length)) { error.value = t('results:addDocument.empty'); return; }
  emit('add', docs);
}
</script>

<template>
  <el-dialog
    :model-value="open"
    :title="documents ? $t('results:addDocument.titleDocument') : $t('results:addDocument.titleRow')"
    width="620px"
    append-to-body
    @close="emit('close')"
  >
    <p class="ad-hint">{{ documents ? $t('results:addDocument.hintDocument') : $t('results:addDocument.hintRow') }}</p>
    <div class="ad-editor">
      <CodeEditor v-model="text" language="json" @save="add" />
    </div>
    <div v-if="error" class="ad-error" role="alert">{{ error }}</div>
    <template #footer>
      <el-button @click="emit('close')">{{ $t('common:cancel') }}</el-button>
      <el-button type="primary" @click="add">{{ $t('results:addDocument.add') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.ad-hint { margin: 0 0 8px; color: var(--nm-text-dim); font-size: 12px; }
.ad-editor { height: 280px; border: 1px solid var(--nm-border); border-radius: 3px; overflow: hidden; display: flex; }
.ad-editor > * { flex: 1; min-width: 0; }
.ad-error { margin-top: 8px; color: var(--nm-danger); font-size: 12px; white-space: pre-wrap; }
</style>
