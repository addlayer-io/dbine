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
/** The first tab: the name and the fields without a group of their own. */
const GENERAL = 'General';
const tabOf = (f: Field) => f.group || GENERAL;
/** General, then the engine's groups in order. */
const tabs = computed(() => {
  const out = [GENERAL];
  for (const f of fields.value) if (!out.includes(tabOf(f))) out.push(tabOf(f));
  return out;
});
const tab = ref(GENERAL);
/** The chosen tab's fields. */
const shown = computed(() => fields.value.filter((f) => visible(f) && tabOf(f) === tab.value));
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
  <el-dialog :model-value="open" :title="$t('core:actions.createDatabase.title')" width="660px" append-to-body @close="close">
    <el-form label-position="top" @submit.prevent="create">
      <el-tabs v-if="tabs.length > 1" v-model="tab" class="cd-tabs">
        <el-tab-pane v-for="g in tabs" :key="g" :label="tb(g)" :name="g" />
      </el-tabs>
      <div class="cd-grid">
        <el-form-item v-if="tab === GENERAL" :label="$t('createDatabase:name')" required class="wide">
          <el-input ref="nameInput" v-model="name" @input="script = null" @keydown.enter.prevent="create" />
        </el-form-item>
        <el-form-item v-for="f in shown" :key="f.key" :class="{ wide: f.kind.type === 'textarea' }">
          <template #label>
            <span class="cd-label">{{ tb(f.label) }}</span>
            <el-tooltip v-if="f.help" :content="tb(f.help)" placement="top" :show-after="200">
              <el-icon class="cd-info"><ei-info-filled /></el-icon>
            </el-tooltip>
          </template>
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
        </el-form-item>
      </div>

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
.cd-grid :deep(.el-select) { width: 100%; }
.cd-tabs { margin-top: -6px; }
.cd-tabs :deep(.el-tabs__content) { display: none; }
.cd-grid { display: grid; grid-template-columns: 1fr 1fr; gap: 0 14px; }
.cd-grid .wide { grid-column: 1 / -1; }
.cd-grid :deep(.el-form-item) { margin-bottom: 12px; }
.cd-grid :deep(.el-form-item__label) { display: flex; align-items: center; gap: 4px; line-height: 1.3; margin-bottom: 4px; }
.cd-label { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.cd-info { color: var(--nm-text-dim); cursor: help; flex-shrink: 0; }
.cd-script { margin-top: 12px; border: 1px solid var(--nm-border-soft); border-radius: 4px; }
.cd-script-head { display: flex; align-items: center; justify-content: space-between; padding: 4px 6px 4px 10px; border-bottom: 1px solid var(--nm-border-soft); font-size: 12px; }
.cd-script pre {
  margin: 0; padding: 8px 10px; max-height: 220px; overflow: auto; white-space: pre-wrap; word-break: break-word;
  font-family: var(--nm-mono); font-size: 12px; color: var(--nm-text);
}
</style>
