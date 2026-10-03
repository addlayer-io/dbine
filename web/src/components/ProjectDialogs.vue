<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { errorMessage } from '../api/client';
import { projectsApi } from '../api/projects';
import type { ProjectTarget } from '../api/types';
import { useConnectionsStore } from '../stores/connections';
import { useProjectsStore } from '../stores/projects';
import ProjectEnvironmentsDialog from './ProjectEnvironmentsDialog.vue';
import ProjectLinkDialog from './ProjectLinkDialog.vue';
import ProjectTargetPicker from './ProjectTargetPicker.vue';

// The Proyectos dialogs, mounted once (App.vue): the sidebar, the Explorer
// and the query editor open them through `projects.dialog`.

const projects = useProjectsStore();
const conns = useConnectionsStore();
const d = computed(() => projects.dialog);
const close = () => { projects.dialog = null; };
const project = computed(() => (d.value && 'projectId' in d.value ? projects.byId(d.value.projectId) : undefined));

// -- the base of a project (direct, or an environment's) --------------------------------
const picked = ref<ProjectTarget | null>(null);
const pickerKey = ref(0);
watch(d, (x) => {
  error.value = null;
  if (x?.kind === 'target') {
    const p = projects.byId(x.projectId);
    picked.value = (x.alias ? p?.binding.environments[x.alias] : p?.binding.direct) ?? null;
    pickerKey.value++;
  }
  if (x?.kind === 'assign') assignTo.value = projects.byId(x.projectId)?.manifest?.environments[0]?.name ?? null;
  if (x?.kind === 'identity') { idName.value = ''; idEmail.value = ''; idGlobal.value = true; }
});
const pickerEngine = computed(() => {
  const x = d.value;
  if (x?.kind !== 'target') return null;
  const m = project.value?.manifest;
  return (x.alias ? m?.environments.find((e) => e.name === x.alias)?.engine : null) ?? m?.engine ?? null;
});
async function saveTarget(target: ProjectTarget | null) {
  const x = d.value;
  if (x?.kind !== 'target') return;
  close();
  await projects.setTarget(x.projectId, x.alias, target);
  // Mapping an environment from the editor's chip: it becomes the active one too.
  if (x.alias && target && project.value?.binding.active_environment !== x.alias) await projects.setEnvironment(x.projectId, x.alias);
}

// -- "¿Asignar esta base a qué entorno?" ---------------------------------------------------
const assignTo = ref<string | null>(null);
async function assign() {
  const x = d.value;
  if (x?.kind !== 'assign' || !assignTo.value) return;
  close();
  const p = projects.byId(x.projectId);
  if (!p) return;
  await projects.setBinding(x.projectId, {
    ...p.binding, environments: { ...p.binding.environments, [assignTo.value]: x.target }, active_environment: assignTo.value,
  });
}
const targetLabel = (x: ProjectTarget | null | undefined) =>
  x ? `${conns.byId(x.connection_id)?.name ?? '?'}${x.database ? ` › ${x.database}` : ''}` : '';

// -- git identity (name and email), asked by a commit -------------------------------------
const idName = ref('');
const idEmail = ref('');
const idGlobal = ref(true);
const busy = ref(false);
const error = ref<string | null>(null);
async function saveIdentity() {
  const x = d.value;
  if (x?.kind !== 'identity') return;
  busy.value = true;
  try {
    await projectsApi.setIdentity(x.projectId, idName.value.trim(), idEmail.value.trim(), idGlobal.value);
    close();
    void projects.refreshStatus(x.projectId);
    x.retry?.();
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    busy.value = false;
  }
}
</script>

<template>
  <ProjectLinkDialog v-if="d?.kind === 'link'" :mode="d.mode" :target="d.target" @close="close" />
  <ProjectEnvironmentsDialog v-else-if="d?.kind === 'environments'" :project-id="d.projectId" @close="close" />

  <el-dialog
    :model-value="d?.kind === 'target'"
    :title="d?.kind === 'target' && d.alias ? $t('projects:base.pickEnvTitle', { env: d.alias }) : $t('projects:base.pickTitle')"
    width="440px"
    append-to-body
    @update:model-value="(v: boolean) => { if (!v) close(); }"
  >
    <p class="pd-lead">{{ $t('projects:base.pickLead', { name: project?.name ?? '' }) }}</p>
    <ProjectTargetPicker v-if="d?.kind === 'target'" :key="pickerKey" v-model="picked" :engine="pickerEngine" />
    <template #footer>
      <el-button v-if="d?.kind === 'target' && (d.alias ? project?.binding.environments[d.alias] : project?.binding.direct)" class="pd-left" @click="saveTarget(null)">
        {{ $t('projects:base.clear') }}
      </el-button>
      <el-button @click="close">{{ $t('common:cancel') }}</el-button>
      <el-button type="primary" :disabled="!picked" @click="saveTarget(picked)">{{ $t('projects:base.use') }}</el-button>
    </template>
  </el-dialog>

  <el-dialog
    :model-value="d?.kind === 'assign'"
    :title="$t('projects:base.assignTitle')"
    width="440px"
    append-to-body
    @update:model-value="(v: boolean) => { if (!v) close(); }"
  >
    <template v-if="d?.kind === 'assign'">
      <p class="pd-lead">{{ $t('projects:base.assignLead', { target: targetLabel(d.target), name: project?.name ?? '' }) }}</p>
      <el-radio-group v-model="assignTo" class="pd-envs">
        <el-radio v-for="e in project?.manifest?.environments ?? []" :key="e.name" :value="e.name">
          <b>{{ e.name }}</b>
          <span class="nm-muted"> · {{ targetLabel(project?.binding.environments[e.name]) || $t('projects:base.unassigned') }}</span>
        </el-radio>
      </el-radio-group>
    </template>
    <template #footer>
      <el-button @click="close">{{ $t('common:cancel') }}</el-button>
      <el-button type="primary" :disabled="!assignTo" @click="assign">{{ $t('projects:base.assign') }}</el-button>
    </template>
  </el-dialog>

  <el-dialog
    :model-value="d?.kind === 'identity'"
    :title="$t('projects:identity.title')"
    width="440px"
    append-to-body
    @update:model-value="(v: boolean) => { if (!v) close(); }"
  >
    <p class="pd-lead">{{ $t('projects:identity.lead') }}</p>
    <el-form label-position="top" @submit.prevent="saveIdentity">
      <el-form-item :label="$t('projects:identity.name')"><el-input v-model="idName" /></el-form-item>
      <el-form-item :label="$t('projects:identity.email')"><el-input v-model="idEmail" type="email" /></el-form-item>
      <el-checkbox v-model="idGlobal">{{ $t('projects:identity.global') }}</el-checkbox>
    </el-form>
    <p v-if="error" class="pd-err" role="alert">{{ error }}</p>
    <template #footer>
      <el-button @click="close">{{ $t('common:cancel') }}</el-button>
      <el-button type="primary" :loading="busy" :disabled="!idName.trim() || !idEmail.trim()" @click="saveIdentity">{{ $t('projects:identity.save') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.pd-lead { margin: 0 0 12px; color: var(--nm-text); line-height: 1.5; }
.pd-envs { display: flex; flex-direction: column; align-items: flex-start; gap: 4px; }
.pd-left { float: left; }
.pd-err { margin: 8px 0 0; color: var(--nm-danger); font-size: 12px; }
</style>
