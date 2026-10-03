<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { projectsApi } from '../api/projects';
import type { FolderInspect, ProjectTarget } from '../api/types';
import { useConnectionsStore } from '../stores/connections';
import { useProjectsStore } from '../stores/projects';

// "Vincular proyecto…" (a folder already on this machine) and "Clonar
// repositorio…" (git clone into a folder, then link it). Opened from the
// Proyectos sidebar, or from a database's "Proyectos" node in the Explorer:
// then that database becomes the project's base.

const props = defineProps<{ mode: 'link' | 'clone'; target: ProjectTarget | null }>();
const emit = defineEmits<{ close: [] }>();

const { t } = useTranslation();
const projects = useProjectsStore();
const conns = useConnectionsStore();

const mode = ref(props.mode);
const busy = ref(false);
const error = ref<string | null>(null);

// -- link a folder ----------------------------------------------------------------------
const path = ref('');
/** What's typed; inspected once it changes (Enter, leaving the field). */
const pathInput = ref('');
const name = ref('');
const init = ref(false);
const inspect = ref<FolderInspect | null>(null);
const inspecting = ref(false);
let inspectSeq = 0;

async function pickFolder() {
  try {
    const picked = await openDialog({ directory: true, title: t('projects:link.pickTitle') });
    if (typeof picked === 'string') path.value = pathInput.value = picked;
  } catch { /* cancelled */ }
}

watch(path, async (p) => {
  const seq = ++inspectSeq;
  inspect.value = null;
  error.value = null;
  if (!p.trim()) return;
  inspecting.value = true;
  try {
    const r = await projectsApi.inspectFolder(p.trim());
    if (seq !== inspectSeq) return;
    inspect.value = r;
    if (!name.value || name.value === lastSuggested) name.value = lastSuggested = r.suggested_name;
  } catch (e) {
    if (seq === inspectSeq) error.value = errorMessage(e);
  } finally {
    if (seq === inspectSeq) inspecting.value = false;
  }
});
let lastSuggested = '';

const linkOk = computed(() => {
  const i = inspect.value;
  return !!i && i.exists && !i.already_linked && (i.is_repo || init.value);
});

// -- clone --------------------------------------------------------------------------------
const url = ref('');
const parentDir = ref('');
const cloneName = ref('');
const branch = ref('');
onMounted(async () => {
  try { parentDir.value = await projectsApi.defaultDir(); } catch { /* typed by hand */ }
});
async function pickParent() {
  try {
    const picked = await openDialog({ directory: true, title: t('projects:clone.pickTitle') });
    if (typeof picked === 'string') parentDir.value = picked;
  } catch { /* cancelled */ }
}
const cloneOk = computed(() => !!url.value.trim() && !!parentDir.value.trim());

// -- go -------------------------------------------------------------------------------------
/** A preset base (from the Explorer): direct when the repo has no
 *  environments; with environments, the user says which one gets it. */
async function afterLink(id: string) {
  const target = props.target;
  if (!target) return;
  const p = projects.byId(id);
  if (p?.manifest?.environments.length) await projects.useDatabase(id, target.connection_id, target.database);
}

async function submit() {
  error.value = null;
  busy.value = true;
  const preset = props.target ? { direct: props.target, environments: {}, active_environment: null } : null;
  try {
    if (mode.value === 'link') {
      const i = inspect.value!;
      const info = await projects.link({
        path: i.repo_root ?? i.path, name: name.value.trim() || null, init: !i.is_repo && init.value,
        binding: i.has_manifest ? null : preset,
      });
      emit('close');
      await afterLink(info.id);
    } else {
      // A background task: the dialog closes, the Tareas panel follows it.
      const job = projects.clone({
        url: url.value.trim(), parentDir: parentDir.value.trim(), name: cloneName.value.trim() || null,
        branch: branch.value.trim() || null, binding: preset,
      });
      emit('close');
      job.then((info) => afterLink(info.id)).catch((e) => ElMessage.error({ message: errorMessage(e), duration: 7000 }));
    }
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    busy.value = false;
  }
}

const presetLabel = computed(() => {
  const x = props.target;
  if (!x) return '';
  return `${conns.byId(x.connection_id)?.name ?? ''}${x.database ? ` › ${x.database}` : ''}`;
});
</script>

<template>
  <el-dialog
    :model-value="true"
    :title="mode === 'link' ? $t('projects:link.title') : $t('projects:clone.title')"
    width="560px"
    append-to-body
    :close-on-click-modal="false"
    @update:model-value="(v: boolean) => { if (!v) emit('close'); }"
  >
    <el-radio-group v-model="mode" size="small" class="pl-mode">
      <el-radio-button value="link">{{ $t('projects:link.tab') }}</el-radio-button>
      <el-radio-button value="clone">{{ $t('projects:clone.tab') }}</el-radio-button>
    </el-radio-group>
    <p v-if="target" class="pl-preset">{{ $t('projects:link.preset', { target: presetLabel }) }}</p>

    <el-form v-if="mode === 'link'" label-position="top" @submit.prevent>
      <el-form-item :label="$t('projects:link.folder')">
        <div class="pl-row">
          <el-input v-model="pathInput" :placeholder="$t('projects:link.folderPlaceholder')" spellcheck="false" @change="path = pathInput" />
          <el-button @click="pickFolder">{{ $t('projects:link.choose') }}</el-button>
        </div>
      </el-form-item>
      <div v-if="inspecting" class="pl-note nm-muted"><el-icon class="is-loading"><ei-loading /></el-icon> {{ $t('common:loading') }}</div>
      <template v-else-if="inspect">
        <div v-if="!inspect.exists" class="pl-note err">{{ $t('projects:link.notFound') }}</div>
        <div v-else-if="inspect.already_linked" class="pl-note err">{{ $t('projects:link.alreadyLinked', { name: projects.byId(inspect.already_linked)?.name ?? '' }) }}</div>
        <template v-else-if="inspect.is_repo">
          <div v-if="inspect.repo_root && inspect.repo_root !== inspect.path" class="pl-note warn">{{ $t('projects:link.useRoot', { root: inspect.repo_root }) }}</div>
          <div v-else class="pl-note ok">{{ $t('projects:link.isRepo') }}</div>
          <div v-if="inspect.has_manifest" class="pl-note">{{ $t('projects:link.hasManifest') }}</div>
        </template>
        <template v-else>
          <div class="pl-note warn">{{ $t('projects:link.notRepo') }}</div>
          <el-checkbox v-model="init">{{ $t('projects:link.init') }}</el-checkbox>
        </template>
      </template>
      <el-form-item :label="$t('projects:link.name')" class="pl-name">
        <el-input v-model="name" :placeholder="inspect?.suggested_name ?? ''" />
      </el-form-item>
    </el-form>

    <el-form v-else label-position="top" @submit.prevent>
      <el-form-item :label="$t('projects:clone.url')">
        <el-input v-model="url" placeholder="git@github.com:empresa/scripts.git" spellcheck="false" />
      </el-form-item>
      <el-form-item :label="$t('projects:clone.parent')">
        <div class="pl-row">
          <el-input v-model="parentDir" spellcheck="false" />
          <el-button @click="pickParent">{{ $t('projects:link.choose') }}</el-button>
        </div>
      </el-form-item>
      <div class="pl-two">
        <el-form-item :label="$t('projects:clone.name')">
          <el-input v-model="cloneName" :placeholder="$t('projects:clone.namePlaceholder')" />
        </el-form-item>
        <el-form-item :label="$t('projects:clone.branch')">
          <el-input v-model="branch" :placeholder="$t('projects:clone.branchPlaceholder')" />
        </el-form-item>
      </div>
      <p class="pl-hint">{{ $t('projects:clone.credentials') }}</p>
    </el-form>

    <div v-if="error" class="pl-note err" role="alert">{{ error }}</div>
    <template #footer>
      <el-button @click="emit('close')">{{ $t('common:cancel') }}</el-button>
      <el-button type="primary" :loading="busy" :disabled="mode === 'link' ? !linkOk : !cloneOk" @click="submit">
        {{ mode === 'link' ? $t('projects:link.submit') : $t('projects:clone.submit') }}
      </el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.pl-mode { margin-bottom: 12px; }
.pl-preset { margin: 0 0 10px; font-size: 12px; color: var(--nm-text-dim); }
.pl-row { display: flex; gap: 6px; width: 100%; }
.pl-two { display: grid; grid-template-columns: 1fr 1fr; gap: 10px; }
.pl-name { margin-top: 12px; }
.pl-note { margin: 4px 0 8px; font-size: 12px; line-height: 1.45; word-break: break-word; }
.pl-note.ok { color: var(--nm-success); }
.pl-note.warn { color: var(--nm-warning); }
.pl-note.err { color: var(--nm-danger); }
.pl-hint { margin: 0; font-size: 12px; color: var(--nm-text-dim); line-height: 1.45; }
</style>
