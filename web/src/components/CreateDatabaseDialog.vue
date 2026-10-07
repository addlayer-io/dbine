<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import type { Field, FieldChoices } from '../api/types';
import { tb } from '../i18n/backend';
import { newQuery, runCreateDatabase } from '../composables/actions';
import { useConnectionsStore } from '../stores/connections';

// "Nueva base de datos" (docs/crear-bases.md): the name and, folded under
// "Opciones avanzadas", the clauses the engine's CREATE DATABASE takes
// (Driver::create_database_fields), with the server's suggestions
// (collations, default paths, users…). An empty option is the server's
// default. "Ver script" shows what "Crear" runs.

const props = defineProps<{ connectionId: string }>();
const emit = defineEmits<{ close: [] }>();
const { t } = useTranslation();
const conns = useConnectionsStore();

const open = ref(true);
const name = ref('');
const values = ref<Record<string, string>>({});
const fields = computed<Field[]>(() => conns.driverOf(props.connectionId)?.create_database_fields ?? []);
const choices = ref<Record<string, FieldChoices>>({});
const advanced = ref<string[]>([]);
const script = ref<string | null>(null);
const busy = ref(false);
const nameInput = ref<{ focus: () => void } | null>(null);

onMounted(async () => {
  setTimeout(() => nameInput.value?.focus(), 50);
  if (!fields.value.length) return;
  for (const f of fields.value) if (f.default) values.value[f.key] = f.default;
  try {
    const list = await invoke<FieldChoices[]>('create_database_choices', { args: { connection_id: props.connectionId } });
    choices.value = Object.fromEntries(list.map((c) => [c.key, c]));
  } catch {
    // Suggestions are a help: the fields still take any value.
  }
});

/** Shown only when the field it depends on has one of the listed values. */
function visible(f: Field): boolean {
  return !f.when || f.when.values.includes(values.value[f.when.key] ?? '');
}

/** The options that apply: visible fields with a value. */
function options(): Record<string, string> {
  const out: Record<string, string> = {};
  for (const f of fields.value) {
    const v = (values.value[f.key] ?? '').trim();
    if (v && visible(f)) out[f.key] = v;
  }
  return out;
}

function placeholder(f: Field): string {
  const d = choices.value[f.key]?.default;
  return d ? t('createDatabase:serverDefault', { value: d }) : f.placeholder ? tb(f.placeholder) : t('createDatabase:default');
}

const valid = computed(() => !!name.value.trim());

async function showScript() {
  if (!valid.value) return;
  busy.value = true;
  try {
    script.value = await invoke<string>('create_database_script', { args: { connection_id: props.connectionId, name: name.value.trim(), options: options() } });
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    busy.value = false;
  }
}

async function openInQuery() {
  if (!script.value) return;
  await newQuery(props.connectionId, '', script.value, t('createDatabase:scriptName', { name: name.value.trim() }));
  close();
}

function create() {
  if (!valid.value) return;
  runCreateDatabase(props.connectionId, name.value, options());
  close();
}

function close() {
  open.value = false;
  emit('close');
}
</script>

<template>
  <el-dialog :model-value="open" :title="$t('core:actions.createDatabase.title')" width="560px" append-to-body @close="close">
    <el-form label-position="top" @submit.prevent="create">
      <el-form-item :label="$t('createDatabase:name')" required>
        <el-input ref="nameInput" v-model="name" @input="script = null" @keydown.enter.prevent="create" />
      </el-form-item>

      <el-collapse v-if="fields.length" v-model="advanced" class="cd-adv">
        <el-collapse-item name="adv" :title="$t('createDatabase:advanced')">
          <template v-for="f in fields" :key="f.key">
            <el-form-item v-if="visible(f)" :label="tb(f.label)">
              <el-select
                v-if="f.kind.type === 'select'"
                v-model="values[f.key]"
                clearable
                :placeholder="placeholder(f)"
                @change="script = null"
              >
                <el-option v-for="[v, l] in f.kind.options" :key="v" :label="tb(l)" :value="v" />
              </el-select>
              <el-checkbox
                v-else-if="f.kind.type === 'bool'"
                :model-value="values[f.key] === 'true'"
                @update:model-value="(on: string | number | boolean) => { values[f.key] = on ? 'true' : ''; script = null; }"
              />
              <el-select
                v-else-if="choices[f.key]?.values.length"
                v-model="values[f.key]"
                filterable
                allow-create
                default-first-option
                clearable
                :placeholder="placeholder(f)"
                @change="script = null"
              >
                <el-option v-for="v in choices[f.key].values" :key="v" :label="v" :value="v" />
              </el-select>
              <el-input
                v-else
                v-model="values[f.key]"
                :type="f.kind.type === 'number' ? 'number' : f.kind.type === 'textarea' ? 'textarea' : 'text'"
                :placeholder="placeholder(f)"
                @input="script = null"
              />
              <div v-if="f.help" class="cd-help">{{ tb(f.help) }}</div>
            </el-form-item>
          </template>
        </el-collapse-item>
      </el-collapse>

      <div v-if="script !== null" class="cd-script">
        <div class="cd-script-head">
          <strong>{{ $t('createDatabase:script') }}</strong>
          <el-button size="small" text @click="openInQuery">{{ $t('createDatabase:openInQuery') }}</el-button>
        </div>
        <pre>{{ script }}</pre>
      </div>
    </el-form>
    <template #footer>
      <el-button @click="close">{{ $t('common:cancel') }}</el-button>
      <el-button v-if="fields.length" :disabled="!valid" :loading="busy" @click="showScript">{{ $t('createDatabase:showScript') }}</el-button>
      <el-button type="primary" :disabled="!valid" @click="create">{{ $t('core:actions.create') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.cd-adv { margin-top: 4px; border-top: 0; }
.cd-adv :deep(.el-collapse-item__header) { font-size: 12.5px; }
.cd-adv :deep(.el-select) { width: 100%; }
.cd-help { font-size: 11.5px; color: var(--nm-text-dim); line-height: 1.4; margin-top: 2px; }
.cd-script { margin-top: 12px; border: 1px solid var(--nm-border-soft); border-radius: 4px; }
.cd-script-head { display: flex; align-items: center; justify-content: space-between; padding: 4px 6px 4px 10px; border-bottom: 1px solid var(--nm-border-soft); font-size: 12px; }
.cd-script pre {
  margin: 0; padding: 8px 10px; max-height: 220px; overflow: auto; white-space: pre-wrap; word-break: break-word;
  font-family: var(--nm-mono); font-size: 12px; color: var(--nm-text);
}
</style>
