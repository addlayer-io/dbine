<script setup lang="ts">
import { computed, ref } from 'vue';
import { errorMessage } from '../api/client';
import { projectsApi } from '../api/projects';
import type { ProjectEnvironment, ProjectManifest } from '../api/types';
import { useConnectionsStore } from '../stores/connections';
import { useProjectsStore } from '../stores/projects';

// "Definir entornos…": the environments of the repo's .dbine.json (names
// only, an optional engine, and whether running there asks first). Each
// person maps them to their own connections; the file never has
// credentials, and is committed with the project.

const props = defineProps<{ projectId: string }>();
const emit = defineEmits<{ close: [] }>();

const projects = useProjectsStore();
const conns = useConnectionsStore();
const project = computed(() => projects.byId(props.projectId));
const m = project.value?.manifest;

const name = ref(m?.name ?? project.value?.name ?? '');
const engine = ref<string | null>(m?.engine ?? null);
const envs = ref<ProjectEnvironment[]>(m?.environments.length
  ? m.environments.map((e) => ({ ...e }))
  : [{ name: 'dev', engine: null, confirm_run: false, description: '' }, { name: 'prod', engine: null, confirm_run: true, description: '' }]);
const defaultEnv = ref<string | null>(m?.default_environment ?? envs.value[0]?.name ?? null);
const busy = ref(false);
const error = ref<string | null>(null);

const NAME_RE = /^[A-Za-z0-9_.-]{1,40}$/;
const problem = computed(() => {
  const seen = new Set<string>();
  for (const e of envs.value) {
    if (!NAME_RE.test(e.name)) return 'name';
    const k = e.name.toLowerCase();
    if (seen.has(k)) return 'duplicate';
    seen.add(k);
  }
  return null;
});
const drivers = computed(() => [...conns.drivers].sort((a, b) => a.name.localeCompare(b.name)));

function add() {
  envs.value.push({ name: '', engine: null, confirm_run: false, description: '' });
}
function remove(i: number) {
  const [gone] = envs.value.splice(i, 1);
  if (gone && defaultEnv.value === gone.name) defaultEnv.value = envs.value[0]?.name ?? null;
}

async function save() {
  if (problem.value) return;
  busy.value = true;
  error.value = null;
  const manifest: ProjectManifest = {
    version: 1,
    name: name.value.trim() || null,
    engine: engine.value || null,
    environments: envs.value.map((e) => ({ ...e, name: e.name.trim(), engine: e.engine || null, description: e.description.trim() })),
    default_environment: envs.value.some((e) => e.name === defaultEnv.value) ? defaultEnv.value : envs.value[0]?.name ?? null,
  };
  try {
    projects.replace(await projectsApi.writeManifest(props.projectId, manifest));
    void projects.refreshStatus(props.projectId);
    emit('close');
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    busy.value = false;
  }
}
</script>

<template>
  <el-dialog
    :model-value="true"
    :title="$t('projects:envs.title', { name: project?.name ?? '' })"
    width="680px"
    append-to-body
    :close-on-click-modal="false"
    @update:model-value="(v: boolean) => { if (!v) emit('close'); }"
  >
    <p class="pe-lead">{{ $t('projects:envs.lead') }}</p>
    <div class="pe-two">
      <el-form-item :label="$t('projects:envs.name')" label-position="top">
        <el-input v-model="name" />
      </el-form-item>
      <el-form-item :label="$t('projects:envs.engine')" label-position="top">
        <el-select v-model="engine" clearable filterable :placeholder="$t('projects:envs.anyEngine')">
          <el-option v-for="d in drivers" :key="d.id" :value="d.id" :label="d.name" />
        </el-select>
      </el-form-item>
    </div>
    <table class="pe-table">
      <thead>
        <tr>
          <th>{{ $t('projects:envs.envName') }}</th>
          <th>{{ $t('projects:envs.envEngine') }}</th>
          <th>{{ $t('projects:envs.description') }}</th>
          <th :title="$t('projects:envs.confirmRunTip')">{{ $t('projects:envs.confirmRun') }}</th>
          <th :title="$t('projects:envs.defaultTip')">{{ $t('projects:envs.default') }}</th>
          <th />
        </tr>
      </thead>
      <tbody>
        <tr v-for="(e, i) in envs" :key="i">
          <td><el-input v-model="e.name" size="small" placeholder="qa" spellcheck="false" /></td>
          <td>
            <el-select v-model="e.engine" size="small" clearable filterable :placeholder="$t('projects:envs.inherit')">
              <el-option v-for="d in drivers" :key="d.id" :value="d.id" :label="d.name" />
            </el-select>
          </td>
          <td><el-input v-model="e.description" size="small" /></td>
          <td class="c"><el-checkbox v-model="e.confirm_run" /></td>
          <td class="c"><el-radio v-model="defaultEnv" :value="e.name">&nbsp;</el-radio></td>
          <td class="c"><button class="pe-del" :title="$t('common:delete')" @click="remove(i)"><el-icon><ei-delete /></el-icon></button></td>
        </tr>
      </tbody>
    </table>
    <el-button size="small" class="pe-add" @click="add"><el-icon><ei-plus /></el-icon>&nbsp;{{ $t('projects:envs.add') }}</el-button>
    <p v-if="problem" class="pe-err">{{ $t(`projects:envs.problem.${problem}`) }}</p>
    <p v-if="error" class="pe-err" role="alert">{{ error }}</p>
    <p class="pe-hint">{{ $t('projects:envs.noCredentials') }}</p>
    <template #footer>
      <el-button @click="emit('close')">{{ $t('common:cancel') }}</el-button>
      <el-button type="primary" :loading="busy" :disabled="!!problem" @click="save">{{ $t('projects:envs.save') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.pe-lead { margin: 0 0 12px; color: var(--nm-text); line-height: 1.5; }
.pe-two { display: grid; grid-template-columns: 1fr 1fr; gap: 10px; }
.pe-table { width: 100%; border-collapse: collapse; font-size: 12px; }
.pe-table th { text-align: left; font-weight: 600; color: var(--nm-text-dim); padding: 4px; white-space: nowrap; }
.pe-table td { padding: 3px 4px; }
.pe-table td.c, .pe-table th:nth-child(n + 4) { text-align: center; }
.pe-del { border: 0; background: none; color: var(--nm-text-dim); cursor: pointer; }
.pe-del:hover { color: var(--nm-danger); }
.pe-add { margin-top: 8px; }
.pe-err { margin: 8px 0 0; color: var(--nm-danger); font-size: 12px; }
.pe-hint { margin: 10px 0 0; color: var(--nm-text-dim); font-size: 12px; }
</style>
