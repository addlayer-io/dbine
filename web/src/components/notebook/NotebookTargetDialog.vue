<script setup lang="ts">
import { computed, ref } from 'vue';
import type { NotebookTarget, ProjectTarget } from '../../api/types';
import { useConnectionsStore } from '../../stores/connections';
import { useProjectsStore } from '../../stores/projects';
import { connectionByName, targetOf } from '../../composables/notebookTargets';
import ProjectTargetPicker from '../ProjectTargetPicker.vue';
import EngineIcon from '../EngineIcon.vue';

// Where a notebook (or one cell) runs: a connection and database, the
// project's environment, or "DataFrame SQL" (DuckDB over the
// results of the cells above, on no connection). `map`: the file names a connection this machine
// doesn't have (or several share that name); the user says which one it
// is, once.

const props = defineProps<{
  mode: 'pick' | 'map';
  /** pick: the current target. */
  initial?: NotebookTarget | null;
  /** pick: the notebook's project (its environments are offered). */
  projectId?: string | null;
  /** pick, a cell: "Usar el del notebook" is offered. */
  inherit?: boolean;
  /** map: the name in the file, and its engine when known. */
  name?: string | null;
  engine?: string | null;
  /** map: several local connections have that name: only these are offered. */
  candidates?: string[] | null;
  /** The notebook (for names several connections share). */
  file?: string | null;
  /** pick, a write cell: where its DataFrame goes. Only a connection or an
   *  environment (no "DataFrame SQL", no "the notebook's"), never a
   *  read-only connection. */
  write?: boolean;
}>();
const emit = defineEmits<{
  /** null: the cell uses the notebook's target. */
  pick: [target: NotebookTarget | null];
  map: [connectionId: string];
  close: [];
}>();

const conns = useConnectionsStore();
const projects = useProjectsStore();

const envs = computed(() => (props.projectId ? projects.byId(props.projectId)?.manifest?.environments ?? [] : []));
type Kind = 'inherit' | 'env' | 'connection' | 'results';
const kind = ref<Kind>(props.initial?.results && !props.write ? 'results'
  : props.inherit && !props.initial ? 'inherit' : props.initial?.environment && envs.value.length ? 'env' : envs.value.length && !props.initial ? 'env' : 'connection');
const env = ref(props.initial?.environment ?? projects.activeTarget(props.projectId ?? '').env?.name ?? envs.value[0]?.name ?? '');
const direct = ref<ProjectTarget | null>((() => {
  const c = props.initial?.connection ? connectionByName(props.initial.connection, props.file) : undefined;
  return c ? { connection_id: c.id, database: props.initial?.database ?? c.config.database ?? '' } : null;
})());

const canPick = computed(() => (!props.write && (kind.value === 'inherit' || kind.value === 'results')) || (kind.value === 'env' ? !!env.value : kind.value === 'connection' && !!direct.value));
function confirmPick() {
  if (kind.value === 'inherit') emit('pick', null);
  else if (kind.value === 'results') emit('pick', { connection: null, database: null, environment: null, results: true });
  else if (kind.value === 'env') emit('pick', { connection: null, database: null, environment: env.value });
  else if (direct.value) emit('pick', targetOf(direct.value.connection_id, direct.value.database, props.file));
}

// -- map ------------------------------------------------------------------------------------
const showAll = ref(false);
const mapped = ref('');
const candidates = computed(() => conns.list
  .filter((c) => (props.candidates?.length ? props.candidates.includes(c.id) : showAll.value || !props.engine || c.config.driver === props.engine))
  .sort((a, b) => a.name.localeCompare(b.name)));
/** The same name: told apart by engine and where it points. */
function detail(id: string): string {
  const c = conns.byId(id);
  if (!c) return '';
  return [c.config.driver, c.config.host, c.config.database].filter(Boolean).join(' · ');
}
</script>

<template>
  <el-dialog
    :model-value="true"
    :title="mode === 'map' ? $t('notebook:target.mapTitle') : write ? $t('notebook:write.targetTitle') : inherit ? $t('notebook:target.cellTitle') : $t('notebook:target.title')"
    width="480px"
    append-to-body
    :close-on-click-modal="false"
    @update:model-value="(v: boolean) => { if (!v) emit('close'); }"
  >
    <template v-if="mode === 'pick'">
      <el-radio-group v-model="kind" class="ntd-kinds">
        <el-radio v-if="inherit && !write" value="inherit">{{ $t('notebook:target.inherit') }}</el-radio>
        <el-radio v-if="envs.length" value="env">{{ $t('notebook:target.environment') }}</el-radio>
        <el-radio value="connection">{{ $t('notebook:target.connection') }}</el-radio>
        <el-radio v-if="!write" value="results">{{ $t('notebook:target.results') }}</el-radio>
      </el-radio-group>
      <div v-if="kind === 'env'" class="ntd-env">
        <el-select v-model="env" style="width: 100%">
          <el-option v-for="e in envs" :key="e.name" :value="e.name" :label="e.name">
            <span>{{ e.name }}</span><span v-if="e.description" class="ntd-dim"> · {{ e.description }}</span>
          </el-option>
        </el-select>
        <p class="ntd-dim">{{ $t('notebook:target.environmentHelp') }}</p>
      </div>
      <ProjectTargetPicker v-else-if="kind === 'connection'" v-model="direct" :writable="write" />
      <p v-if="write && kind === 'connection'" class="ntd-dim">{{ $t('notebook:write.targetHelp') }}</p>
      <p v-else-if="kind === 'results'" class="ntd-dim">{{ $t('notebook:target.resultsHelp') }}</p>
      <p v-else class="ntd-dim">{{ $t('notebook:target.inheritHelp') }}</p>
    </template>

    <template v-else>
      <p class="ntd-text">{{ props.candidates?.length ? $t('notebook:target.ambiguousIntro', { name: name ?? '' }) : $t('notebook:target.mapIntro', { name: name ?? '' }) }}</p>
      <el-select v-model="mapped" filterable style="width: 100%" :placeholder="$t('projects:picker.connection')">
        <el-option v-for="c in candidates" :key="c.id" :value="c.id" :label="c.name">
          <span class="ntd-opt"><EngineIcon :id="c.config.driver" :name="c.name" :size="14" />{{ c.name }}<span v-if="props.candidates?.length && detail(c.id)" class="ntd-detail">· {{ detail(c.id) }}</span></span>
        </el-option>
      </el-select>
      <el-checkbox v-if="engine && !props.candidates?.length" v-model="showAll" class="ntd-all">{{ $t('projects:picker.showAll') }}</el-checkbox>
      <p class="ntd-dim">{{ props.candidates?.length ? $t('notebook:target.ambiguousHelp') : $t('notebook:target.mapHelp') }}</p>
    </template>

    <template #footer>
      <el-button @click="emit('close')">{{ $t('common:cancel') }}</el-button>
      <el-button v-if="mode === 'pick'" type="primary" :disabled="!canPick" @click="confirmPick">{{ $t('notebook:target.use') }}</el-button>
      <el-button v-else type="primary" :disabled="!mapped" @click="emit('map', mapped)">{{ $t('notebook:target.use') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.ntd-kinds { display: flex; flex-direction: column; align-items: flex-start; gap: 2px; margin-bottom: 10px; }
.ntd-env { display: flex; flex-direction: column; gap: 6px; }
.ntd-dim { margin: 6px 0 0; font-size: 12px; line-height: 1.5; color: var(--nm-text-dim); }
.ntd-text { margin: 0 0 10px; color: var(--nm-text); line-height: 1.5; }
.ntd-opt { display: inline-flex; align-items: center; gap: 6px; }
.ntd-all { margin-top: 6px; }
.ntd-detail { font-size: 12px; color: var(--nm-text-dim); }
</style>
