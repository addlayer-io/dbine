<script setup lang="ts">
import { computed } from 'vue';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { useTranslation } from 'i18next-vue';
import type { Step } from '../api/scheduled';
import type { ProjectTarget } from '../api/types';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import CodeEditor from './CodeEditor.vue';
import OptionField from './OptionField.vue';
import ProjectTargetPicker from './ProjectTargetPicker.vue';
import ScheduledMailStep from './ScheduledMailStep.vue';
import ScheduledDocStep from './ScheduledDocStep.vue';

// One step of a scheduled task (docs/scheduled-tasks.md): its database,
// its script or query, and where its file goes. Edits `step.config` in place.

const props = defineProps<{ step: Step; index: number; count: number }>();
const emit = defineEmits<{ remove: []; move: [delta: number]; change: [] }>();
const { t } = useTranslation();
const conns = useConnectionsStore();
const c = computed(() => props.step.config);

const FORMATS = ['csv', 'csv_semicolon', 'csv_excel', 'tsv', 'xlsx', 'json', 'json_lines', 'xml', 'sql'];

function targetOf(o: { connection_id: string; database: string }): ProjectTarget | null {
  return o.connection_id ? { connection_id: o.connection_id, database: o.database } : null;
}
function setTarget(o: Record<string, any>, v: ProjectTarget | null) {
  o.connection_id = v?.connection_id ?? '';
  o.database = v?.database ?? '';
  emit('change');
}

const driver = computed(() => (c.value.connection_id ? conns.driverOf(c.value.connection_id) : undefined));
const language = computed(() => (driver.value?.language === 'sql' || !driver.value ? 'sql' : driver.value.language) as any);
const backupSpec = computed(() => driver.value?.backup ?? null);
const nativeBackup = computed(() => c.value.mode === 'native' && !!backupSpec.value);

async function browse() {
  let picked: string | string[] | null = null;
  try { picked = await openDialog({ directory: true, title: t('scheduled:step.pickFolder') }); } catch { return; }
  if (typeof picked === 'string') {
    c.value.folder = picked;
    emit('change');
  }
}

const label = computed(() => t(`scheduled:kinds.${props.step.kind}`));
</script>

<template>
  <section class="se">
    <header class="se-head">
      <span class="se-n">{{ index + 1 }}</span>
      <strong>{{ label }}</strong>
      <el-input v-model="step.name" size="small" class="se-name" :placeholder="$t('scheduled:step.namePlaceholder')" @input="emit('change')" />
      <span class="se-sp" />
      <el-select v-model="step.on_error" size="small" class="se-onerr" @change="emit('change')">
        <el-option value="stop" :label="$t('scheduled:step.onError.stop')" />
        <el-option value="continue" :label="$t('scheduled:step.onError.continue')" />
      </el-select>
      <el-button size="small" text :disabled="index === 0" :title="$t('scheduled:step.up')" @click="emit('move', -1)"><el-icon><ei-arrow-up /></el-icon></el-button>
      <el-button size="small" text :disabled="index === count - 1" :title="$t('scheduled:step.down')" @click="emit('move', 1)"><el-icon><ei-arrow-down /></el-icon></el-button>
      <el-button size="small" text :title="$t('scheduled:step.remove')" @click="emit('remove')"><el-icon><ei-delete /></el-icon></el-button>
    </header>

    <el-form label-position="top" size="small" class="se-body">
      <!-- Ejecutar un script / Exportar a archivo / Backup: one database. -->
      <template v-if="step.kind !== 'compare_schemas' && step.kind !== 'send_mail'">
        <el-form-item :label="$t('scheduled:step.database')">
          <ProjectTargetPicker :model-value="targetOf(c as any)" class="se-target" @update:model-value="(v) => setTarget(c, v)" />
        </el-form-item>
      </template>

      <template v-if="step.kind === 'run_script' || step.kind === 'export'">
        <el-form-item :label="step.kind === 'export' ? $t('scheduled:step.query') : $t('scheduled:step.script')">
          <div class="se-editor">
            <CodeEditor v-model="c.sql" :language="language" :dialect="driver?.id ?? ''" @update:model-value="emit('change')" />
          </div>
        </el-form-item>
        <el-checkbox v-if="step.kind === 'run_script'" :model-value="c.continue_on_error === true" @update:model-value="(v: any) => { c.continue_on_error = v ? true : null; emit('change'); }">
          {{ $t('scheduled:step.continueOnError') }}
        </el-checkbox>
      </template>

      <template v-if="step.kind === 'export'">
        <div class="se-row">
          <el-form-item :label="$t('scheduled:step.format')">
            <el-select v-model="c.options.format" @change="emit('change')">
              <el-option v-for="f in FORMATS" :key="f" :value="f" :label="$t(`scheduled:formats.${f}`)" />
            </el-select>
          </el-form-item>
          <el-checkbox v-model="c.options.header" class="se-check" @change="emit('change')">{{ $t('scheduled:step.header') }}</el-checkbox>
        </div>
      </template>

      <template v-if="step.kind === 'compare_schemas'">
        <el-form-item :label="$t('scheduled:step.source')">
          <ProjectTargetPicker :model-value="targetOf(c.source)" class="se-target" @update:model-value="(v) => setTarget(c.source, v)" />
        </el-form-item>
        <el-form-item :label="$t('scheduled:step.target')">
          <ProjectTargetPicker :model-value="targetOf(c.target)" class="se-target" @update:model-value="(v) => setTarget(c.target, v)" />
        </el-form-item>
        <div class="se-checks">
          <el-checkbox v-model="c.options.ignore_case" @change="emit('change')">{{ $t('scheduled:step.ignoreCase') }}</el-checkbox>
          <el-checkbox v-model="c.options.ignore_schema" @change="emit('change')">{{ $t('scheduled:step.ignoreSchema') }}</el-checkbox>
          <el-checkbox v-model="c.options.ignore_comments" @change="emit('change')">{{ $t('scheduled:step.ignoreComments') }}</el-checkbox>
          <el-checkbox v-model="c.include_drops" @change="emit('change')">{{ $t('scheduled:step.includeDrops') }}</el-checkbox>
        </div>
        <p class="se-hint">{{ $t('scheduled:step.compareHint') }}</p>
      </template>

      <template v-if="step.kind === 'backup'">
        <el-radio-group v-model="c.mode" class="se-mode" @change="emit('change')">
          <el-radio value="native" :disabled="!!driver && !backupSpec">{{ $t('scheduled:step.native') }}</el-radio>
          <el-radio value="copy">{{ $t('scheduled:step.copy') }}</el-radio>
        </el-radio-group>
        <p v-if="driver && !backupSpec" class="se-hint">{{ $t('scheduled:step.noNative') }}</p>
        <div v-if="nativeBackup" class="se-grid">
          <OptionField
            v-for="f in backupSpec!.backup_options"
            :key="f.key"
            v-model="c.options[f.key]"
            :f="f"
            :placeholder="f.secret || f.kind.type === 'password' ? $t('scheduled:step.secretKept') : (f.placeholder ? tb(f.placeholder) : '')"
            @update:model-value="emit('change')"
          />
        </div>
        <p v-if="nativeBackup && backupSpec?.note" class="se-hint">{{ tb(backupSpec.note) }}</p>
        <el-checkbox v-if="c.mode === 'copy'" v-model="c.data" @change="emit('change')">{{ $t('scheduled:step.withData') }}</el-checkbox>
      </template>

      <ScheduledMailStep v-if="step.kind === 'send_mail'" :config="c" @change="emit('change')" />
      <ScheduledDocStep v-if="step.kind === 'document'" :config="c" @change="emit('change')" />

      <!-- Where the file goes. -->
      <div v-if="step.kind === 'export' || step.kind === 'compare_schemas' || step.kind === 'document' || (step.kind === 'backup' && c.mode === 'copy')" class="se-row">
        <el-form-item :label="$t('scheduled:step.folder')" class="se-grow">
          <el-input v-model="c.folder" @input="emit('change')">
            <template #append><el-button @click="browse"><el-icon><ei-folder-opened /></el-icon></el-button></template>
          </el-input>
        </el-form-item>
        <el-form-item :label="$t('scheduled:step.fileName')" class="se-grow">
          <el-input v-model="c.file_name" placeholder="{task}-{datetime}" @input="emit('change')" />
        </el-form-item>
      </div>
    </el-form>
  </section>
</template>

<style scoped>
.se { border: 1px solid var(--nm-border-soft); border-radius: 4px; margin-bottom: 10px; background: var(--nm-bg); }
.se-head { display: flex; align-items: center; gap: 8px; padding: 6px 10px; border-bottom: 1px solid var(--nm-border-soft); }
.se-head .el-button { margin: 0; }
.se-n { display: inline-flex; align-items: center; justify-content: center; width: 20px; height: 20px; border-radius: 50%; background: var(--ide-hover); font-size: 11px; color: var(--nm-text-dim); }
.se-name { width: 220px; }
.se-onerr { width: 190px; }
.se-sp { flex: 1; }
.se-body { padding: 10px 12px 4px; }
.se-target { max-width: 520px; }
.se-target :deep(.tp-conn), .se-target :deep(.tp-db) { width: 100%; }
.se-editor { width: 100%; height: 160px; border: 1px solid var(--nm-border-soft); border-radius: 3px; overflow: hidden; }
.se-row { display: flex; gap: 12px; align-items: flex-end; flex-wrap: wrap; }
.se-grow { flex: 1; min-width: 220px; }
.se-check { margin-bottom: 18px; }
.se-checks { display: flex; flex-wrap: wrap; gap: 4px 16px; margin-bottom: 8px; }
.se-mode { margin-bottom: 10px; }
.se-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(220px, 1fr)); gap: 0 12px; }
.se-grid :deep(.wide) { grid-column: 1 / -1; }
.se-hint { margin: 0 0 10px; font-size: 11.5px; color: var(--nm-text-dim); }
</style>
