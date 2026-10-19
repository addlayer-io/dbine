<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from 'vue';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { scheduledApi, type NotebookPlan, type NotebookResolved } from '../api/scheduled';
import { isCredentialParam } from '../composables/notebookParams';
import { openPaths } from '../composables/openFiles';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';

// The "Ejecutar notebook" step of a scheduled task (docs/scheduled-tasks.md
// › Run a notebook): the notebook, where its cells run here, what the user
// allows it to do unattended (run without the confirmation, write cells),
// its parameters, and where the executed copy and the export go. The
// backend reads the notebook and says what it needs (`notebookPlan`); it
// runs nothing. Edits `config` in place.

const props = defineProps<{ config: Record<string, any> }>();
const emit = defineEmits<{ change: [] }>();
const { t } = useTranslation();
const conns = useConnectionsStore();

const c = props.config;
c.params ??= {};
c.targets ??= {};
c.unconfirmed ??= [];
c.write_cells ??= [];
c.save ??= { mode: 'folder', folder: '', file_name: '{notebook}-{datetime}' };
c.export ??= { format: 'html', folder: '', file_name: '{notebook}-{datetime}' };

const plan = ref<NotebookPlan | null>(null);
const planError = ref('');
const loading = ref(false);
let seq = 0;
let timer: ReturnType<typeof setTimeout> | undefined;

async function loadPlan() {
  const path = (c.path ?? '').trim();
  const mine = ++seq;
  if (!path) { plan.value = null; planError.value = ''; return; }
  loading.value = true;
  try {
    const p = await scheduledApi.notebookPlan({ path, params: c.params, targets: c.targets, unconfirmed: c.unconfirmed, write_cells: c.write_cells });
    if (mine === seq) { plan.value = p; planError.value = ''; }
  } catch (e) {
    if (mine === seq) { plan.value = null; planError.value = tb(errorMessage(e)); }
  } finally {
    if (mine === seq) loading.value = false;
  }
}
watch(() => JSON.stringify([c.path, c.targets]), () => { clearTimeout(timer); timer = setTimeout(loadPlan, 300); }, { immediate: true });
onBeforeUnmount(() => clearTimeout(timer));

function changed() {
  emit('change');
}

async function pickNotebook() {
  let picked: string | string[] | null = null;
  try {
    picked = await openDialog({ multiple: false, title: t('scheduled:notebook.pickTitle'), filters: [{ name: t('scheduled:notebook.filter'), extensions: ['ipynb'] }] });
  } catch { return; }
  if (typeof picked === 'string') { c.path = picked; changed(); }
}
async function pickFolder(o: Record<string, any>) {
  let picked: string | string[] | null = null;
  try { picked = await openDialog({ directory: true, title: t('scheduled:step.pickFolder') }); } catch { return; }
  if (typeof picked === 'string') { o.folder = picked; changed(); }
}

// -- names the file uses that aren't one connection here -----------------------------------
const choices = computed(() => {
  const seen = new Map<string, { name: string; candidates: { id: string; name: string }[] | null }>();
  for (const cell of plan.value?.cells ?? []) {
    const p = cell.problem;
    if (p && (p.kind === 'connection' || p.kind === 'ambiguous') && p.name != null && !seen.has(p.name)) {
      seen.set(p.name, { name: p.name, candidates: p.kind === 'ambiguous' ? p.candidates : null });
    }
  }
  // Names the step already settled stay editable.
  for (const name of Object.keys(c.targets)) if (!seen.has(name)) seen.set(name, { name, candidates: null });
  return [...seen.values()];
});
function bind(name: string, id: string) {
  if (id) c.targets[name] = { connection_id: id, database: '' };
  else delete c.targets[name];
  changed();
}

// -- "Permitir ejecutar en … sin confirmar" ------------------------------------------------
const same = (u: { connection_id: string; database: string }, r: NotebookResolved) => u.connection_id === r.connection_id && u.database === r.database;
const allowed = (r: NotebookResolved) => c.unconfirmed.some((u: any) => same(u, r));
function allow(r: NotebookResolved, on: boolean) {
  c.unconfirmed = c.unconfirmed.filter((u: any) => !same(u, r));
  if (on) c.unconfirmed.push({ connection_id: r.connection_id, database: r.database });
  changed();
}
const why = (r: NotebookResolved) => (r.confirm_run && r.environment ? t('scheduled:notebook.confirmEnv', { env: r.environment }) : t('scheduled:notebook.confirmProd'));

// -- write cells ---------------------------------------------------------------------------
type WriteCellPlan = NotebookPlan['write_cells'][number];
const authorized = (w: WriteCellPlan) => c.write_cells.some((a: any) => a.cell_id === w.cell_id && a.target === w.target && a.mode === w.mode);
function authorize(w: WriteCellPlan, on: boolean) {
  c.write_cells = c.write_cells.filter((a: any) => a.cell_id !== w.cell_id);
  if (on) c.write_cells.push({ cell_id: w.cell_id, target: w.target, mode: w.mode });
  changed();
}

const SAVE_MODES = ['none', 'beside', 'folder'] as const;
const FORMATS = ['none', 'html', 'markdown'] as const;
const sizeOf = (n: number) => (n < 1024 ? `${n} B` : n < 1048576 ? `${(n / 1024).toFixed(1)} KB` : `${(n / 1048576).toFixed(1)} MB`);
</script>

<template>
  <div class="sn">
    <el-form-item :label="$t('scheduled:notebook.file')">
      <el-input v-model="c.path" placeholder="/…/reporte.ipynb" @input="changed">
        <template #append><el-button :title="$t('scheduled:notebook.pick')" @click="pickNotebook"><el-icon><ei-folder-opened /></el-icon></el-button></template>
      </el-input>
    </el-form-item>
    <p class="sn-hint">{{ $t('scheduled:notebook.hint') }}</p>
    <p v-if="loading && !plan" class="sn-hint">{{ $t('scheduled:notebook.loading') }}</p>
    <div v-if="planError" class="sn-warn"><el-icon><ei-warning-filled /></el-icon><span>{{ $t('scheduled:notebook.planError', { error: planError }) }}</span></div>

    <template v-if="plan">
      <div v-for="(w, i) in plan.warnings" :key="`w${i}`" class="sn-warn"><el-icon><ei-warning-filled /></el-icon><span>{{ tb(w) }}</span></div>

      <!-- Where each cell runs. -->
      <div class="sn-sub">{{ $t('scheduled:notebook.cells') }}</div>
      <p v-if="!plan.cells.length" class="sn-hint">{{ $t('scheduled:notebook.noCells') }}</p>
      <ul v-else class="sn-cells">
        <li v-for="cell in plan.cells" :key="cell.id" :class="{ bad: !!cell.problem }">
          <code class="sn-name">{{ cell.name }}</code>
          <span v-if="cell.dataframes" class="sn-tag">{{ $t('scheduled:notebook.dataframes') }}</span>
          <template v-else-if="cell.target">
            <span class="sn-where">{{ cell.target.label }}</span>
            <span v-if="cell.target.production || cell.target.confirm_run" class="sn-prod">PROD</span>
            <span v-if="cell.target.read_only" class="sn-dim">· {{ $t('scheduled:notebook.readOnly') }}</span>
          </template>
          <span v-else-if="cell.problem" class="sn-err">{{ tb(cell.problem.message) }}</span>
          <div v-if="cell.writes" class="sn-writes">{{ $t('scheduled:notebook.writes', { what: cell.writes }) }}</div>
        </li>
      </ul>
      <div v-for="ch in choices" :key="ch.name" class="sn-choice">
        <code>{{ ch.name }}</code>
        <el-select
          :model-value="c.targets[ch.name]?.connection_id ?? ''" size="small" filterable clearable class="sn-conn"
          :placeholder="$t('scheduled:notebook.choose')" @update:model-value="(v: string) => bind(ch.name, v ?? '')"
        >
          <el-option v-for="o in ch.candidates ?? conns.list" :key="o.id" :value="o.id" :label="o.name" />
        </el-select>
      </div>

      <!-- Confirmations nobody can answer. -->
      <template v-if="plan.confirm.length">
        <div class="sn-sub">{{ $t('scheduled:notebook.confirmTitle') }}</div>
        <p class="sn-hint">{{ $t('scheduled:notebook.confirmHint') }}</p>
        <div v-for="r in plan.confirm" :key="`${r.connection_id}/${r.database}`" class="sn-check">
          <el-checkbox :model-value="allowed(r)" @update:model-value="(v: any) => allow(r, !!v)">
            {{ $t('scheduled:notebook.allowUnconfirmed', { where: r.label }) }}
          </el-checkbox>
          <span class="sn-prod">{{ why(r) }}</span>
        </div>
      </template>

      <!-- Files: only those with "Permitir siempre". -->
      <template v-if="plan.files.length || plan.pending.length || plan.blocked.length || plan.file_problems.length">
        <div class="sn-sub">{{ $t('scheduled:notebook.filesTitle') }}</div>
        <p v-if="plan.files.length" class="sn-hint">{{ $t('scheduled:notebook.filesOk', { count: plan.files.length }) }}</p>
        <div v-if="plan.pending.length" class="sn-warn col">
          <div><el-icon><ei-warning-filled /></el-icon> {{ $t('scheduled:notebook.filesPending') }}</div>
          <ul class="sn-files">
            <li v-for="f in plan.pending" :key="f.path"><code>{{ f.path }}</code> <span class="sn-dim">{{ sizeOf(f.size) }}</span></li>
          </ul>
          <el-button size="small" link type="primary" @click="openPaths([c.path])">{{ $t('scheduled:notebook.openNotebook') }}</el-button>
        </div>
        <div v-for="(b, i) in plan.blocked" :key="`b${i}`" class="sn-warn"><el-icon><ei-warning-filled /></el-icon><span>{{ tb(b) }}</span></div>
        <p v-for="(p, i) in plan.file_problems" :key="`p${i}`" class="sn-hint sn-err">{{ p }}</p>
      </template>

      <!-- Write cells: each one authorized as it is. -->
      <template v-if="plan.write_cells.length">
        <div class="sn-sub">{{ $t('scheduled:notebook.writeTitle') }}</div>
        <p class="sn-hint">{{ $t('scheduled:notebook.writeHint') }}</p>
        <el-checkbox
          v-for="w in plan.write_cells" :key="w.cell_id" :model-value="authorized(w)" class="sn-block"
          @update:model-value="(v: any) => authorize(w, !!v)"
        >{{ $t('scheduled:notebook.writeAllow', { name: w.name, target: w.target, mode: w.mode }) }}</el-checkbox>
      </template>

      <!-- Parameters the notebook declares. -->
      <template v-if="plan.parameters.length">
        <div class="sn-sub">{{ $t('scheduled:notebook.params') }}</div>
        <p class="sn-hint">{{ $t('scheduled:notebook.paramsHint') }}</p>
        <div class="sn-grid">
          <el-form-item v-for="p in plan.parameters" :key="p" :label="p">
            <!-- A credential's value goes to the keychain: the task keeps none. -->
            <el-input
              v-if="isCredentialParam(p)" type="password" show-password autocomplete="off" :placeholder="$t('scheduled:step.secretKept')"
              :model-value="c.params[p] ?? ''" @update:model-value="(v: string) => { c.params[p] = v; changed(); }"
            />
            <el-input v-else :model-value="c.params[p] ?? ''" @update:model-value="(v: string) => { c.params[p] = v; changed(); }" />
          </el-form-item>
        </div>
      </template>
    </template>

    <!-- The executed copy. -->
    <div class="sn-sub">{{ $t('scheduled:notebook.save') }}</div>
    <el-radio-group v-model="c.save.mode" size="small" class="sn-mode" @change="changed">
      <el-radio v-for="m in SAVE_MODES" :key="m" :value="m">{{ $t(`scheduled:notebook.saveModes.${m}`) }}</el-radio>
    </el-radio-group>
    <div v-if="c.save.mode !== 'none'" class="sn-row">
      <el-form-item v-if="c.save.mode === 'folder'" :label="$t('scheduled:step.folder')" class="sn-grow">
        <el-input v-model="c.save.folder" @input="changed">
          <template #append><el-button @click="pickFolder(c.save)"><el-icon><ei-folder-opened /></el-icon></el-button></template>
        </el-input>
      </el-form-item>
      <el-form-item :label="$t('scheduled:step.fileName')" class="sn-grow">
        <el-input v-model="c.save.file_name" placeholder="{notebook}-{datetime}" @input="changed" />
      </el-form-item>
    </div>
    <p class="sn-hint">{{ $t('scheduled:notebook.saveHint') }}</p>

    <!-- The export. -->
    <div class="sn-sub">{{ $t('scheduled:notebook.export') }}</div>
    <el-radio-group v-model="c.export.format" size="small" class="sn-mode" @change="changed">
      <el-radio v-for="f in FORMATS" :key="f" :value="f">{{ $t(`scheduled:notebook.formats.${f}`) }}</el-radio>
    </el-radio-group>
    <div v-if="c.export.format !== 'none'" class="sn-row">
      <el-form-item :label="$t('scheduled:step.folder')" class="sn-grow">
        <el-input v-model="c.export.folder" :placeholder="$t('scheduled:notebook.exportFolderHint')" @input="changed">
          <template #append><el-button @click="pickFolder(c.export)"><el-icon><ei-folder-opened /></el-icon></el-button></template>
        </el-input>
      </el-form-item>
      <el-form-item :label="$t('scheduled:step.fileName')" class="sn-grow">
        <el-input v-model="c.export.file_name" placeholder="{notebook}-{datetime}" @input="changed" />
      </el-form-item>
    </div>
    <p v-if="c.export.format !== 'none'" class="sn-hint">{{ $t('scheduled:notebook.chartsHint') }}</p>
    <p class="sn-hint">{{ $t('scheduled:notebook.outputsHint') }}</p>
  </div>
</template>

<style scoped>
.sn-hint { margin: 0 0 10px; font-size: 11.5px; color: var(--nm-text-dim); line-height: 1.5; }
.sn-sub { margin: 12px 0 6px; font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: .04em; color: var(--nm-text-dim); }
.sn-cells { list-style: none; margin: 0 0 8px; padding: 0; border: 1px solid var(--nm-border-soft); border-radius: 4px; }
.sn-cells li { display: flex; flex-wrap: wrap; align-items: baseline; gap: 8px; padding: 4px 10px; font-size: 12.5px; border-bottom: 1px solid var(--nm-border-soft); }
.sn-cells li:last-child { border-bottom: 0; }
.sn-cells li.bad { background: color-mix(in srgb, var(--nm-danger) 6%, transparent); }
.sn-name { font-family: var(--nm-mono); font-size: 11.5px; min-width: 70px; }
.sn-where { color: var(--nm-text); }
.sn-tag { font-size: 11px; padding: 0 6px; border-radius: 3px; background: var(--ide-hover); color: var(--nm-text-dim); }
.sn-prod { font-size: 10px; font-weight: 700; padding: 0 4px; border-radius: 3px; color: #fff; background: var(--nm-danger); }
.sn-dim { color: var(--nm-text-dim); font-size: 11.5px; }
.sn-err { color: var(--nm-danger); }
.sn-writes { flex-basis: 100%; font-size: 11.5px; color: var(--nm-warning); }
.sn-choice { display: flex; align-items: center; gap: 8px; margin-bottom: 6px; font-size: 12.5px; }
.sn-choice code { font-family: var(--nm-mono); font-size: 11.5px; }
.sn-conn { width: 280px; }
.sn-check { display: flex; align-items: center; gap: 8px; }
.sn-block { display: flex; }
.sn-files { margin: 6px 0 4px; padding-left: 18px; font-size: 11.5px; }
.sn-files code { font-family: var(--nm-mono); word-break: break-all; }
.sn-mode { margin-bottom: 8px; }
.sn-row { display: flex; gap: 12px; align-items: flex-end; flex-wrap: wrap; }
.sn-grow { flex: 1; min-width: 220px; }
.sn-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(220px, 1fr)); gap: 0 12px; }
.sn-warn {
  display: flex; align-items: center; gap: 8px; padding: 6px 10px; margin: 0 0 10px; border-radius: 4px; font-size: 12px; line-height: 1.5;
  border: 1px solid color-mix(in srgb, var(--nm-warning) 45%, transparent); background: color-mix(in srgb, var(--nm-warning) 8%, transparent);
}
.sn-warn.col { flex-direction: column; align-items: flex-start; }
.sn-warn .el-icon { color: var(--nm-warning); flex: none; }
</style>
