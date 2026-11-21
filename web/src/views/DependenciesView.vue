<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import type { Confidence, Dependent, DependencyReport, JobDependent, Relation } from '../api/types';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore, type DependenciesTab } from '../stores/tabs';
import LineageDiagram from '../components/LineageDiagram.vue';
import DatabaseLineage from '../components/DatabaseLineage.vue';

// "Dependencias · <objeto>": what depends on a table, column, view or
// routine, and (an object, not a column) what it uses itself. Foreign keys,
// indexes and checks come from the catalog; views, routines and triggers
// from searching their source (probable, or "revisar" when the name only
// shows inside dynamic SQL). "Linaje" draws both directions as a diagram
// whose cards load further levels. On an engine with server jobs, the jobs
// whose steps run the object show too; a job's own tab (`tab.jobId`) shows
// what its steps use.

const props = defineProps<{ tab: DependenciesTab }>();
const conns = useConnectionsStore();
const tabs = useTabsStore();
const { t } = useTranslation();

const report = ref<DependencyReport | null>(null);
/** What the object itself uses (not for a column). */
const uses = ref<DependencyReport | null>(null);
const loading = ref(false);
const error = ref<string | null>(null);
const usesError = ref<string | null>(null);
const lineage = ref<InstanceType<typeof LineageDiagram> | null>(null);
const dbLineage = ref<InstanceType<typeof DatabaseLineage> | null>(null);

/** The list, or the lineage diagram (only for an object). */
const view = computed(() => (props.tab.column ? 'list' : props.tab.view ?? 'list'));
const setView = (v: 'list' | 'lineage') => tabs.setDependenciesView(props.tab.id, v);

const qualified = computed(() => {
  const o = props.tab.object;
  const name = o.schema ? `${o.schema}.${o.name}` : o.name;
  return props.tab.column ? `${name}.${props.tab.column}` : name;
});

async function load() {
  loading.value = true;
  error.value = null;
  usesError.value = null;
  if (props.tab.jobId) {
    // Nothing uses a job: only what its steps use.
    report.value = { items: [], scanned: 0, unreadable: [], note: null };
    try {
      uses.value = await api.getJobReferences(props.tab.connectionId, props.tab.database, props.tab.jobId);
    } catch (e) {
      usesError.value = errorMessage(e);
    }
    loading.value = false;
    return;
  }
  const target = { object: props.tab.object, column: props.tab.column };
  const own = props.tab.column
    ? Promise.resolve(null)
    : api.getReferences(props.tab.connectionId, props.tab.database, target).catch((e) => {
      usesError.value = errorMessage(e);
      return null;
    });
  try {
    report.value = await api.getDependents(props.tab.connectionId, props.tab.database, target);
  } catch (e) {
    error.value = errorMessage(e);
  }
  uses.value = await own;
  loading.value = false;
}
function refresh() {
  if (props.tab.scope) return dbLineage.value?.reload();
  if (view.value === 'lineage') lineage.value?.reload();
  else void load();
}
onMounted(load);

const ORDER: Relation[] = ['foreign_key', 'index', 'check', 'code'];
const groupsOf = (r: DependencyReport | null) =>
  ORDER.map((relation) => ({ relation, items: (r?.items ?? []).filter((d) => d.relation === relation) })).filter((g) => g.items.length);
const groups = computed(() => groupsOf(report.value));
const useGroups = computed(() => groupsOf(uses.value));
/** The server's jobs whose steps run the object. */
const jobs = computed(() => report.value?.jobs ?? []);
const counts = computed(() => {
  const c: Record<Confidence, number> = { confirmed: 0, probable: 0, review: 0 };
  for (const d of report.value?.items ?? []) c[d.confidence]++;
  for (const j of jobs.value) c[j.confidence]++;
  return c;
});
/** A job opens in its own tab (Programadas › En los servidores). */
const openJob = (j: JobDependent) => tabs.openServerJob(props.tab.connectionId, j.id, j.name);

const fullName = (d: Dependent) => (d.schema ? `${d.schema}.${d.name}` : d.name);
// Views, routines and triggers open their source; a key, index or check
// opens its table's structure.
function open(d: Dependent) {
  const object = { kind: d.kind, schema: d.schema, name: d.name };
  tabs.openObject(props.tab.connectionId, props.tab.database, object, d.relation === 'code' ? 'definition' : 'structure');
}

async function copy() {
  const line = (dir: string, d: Dependent) =>
    [dir, t(`dependencies:relation.${d.relation}`), d.kind, fullName(d), t(`dependencies:confidence.${d.confidence}`), d.detail ?? d.mentions.map((m) => `${m.line}: ${m.text}`).join(' | ')].join('\t');
  const jobLine = (j: JobDependent) =>
    [t('dependencies:usedByTitle'), t('dependencies:jobs.title'), j.kind, j.name, t(`dependencies:confidence.${j.confidence}`), (j.steps ?? []).join(', ')].join('\t');
  const lines = [
    ...(uses.value?.items ?? []).map((d) => line(t('dependencies:usesTitle'), d)),
    ...(report.value?.items ?? []).map((d) => line(t('dependencies:usedByTitle'), d)),
    ...jobs.value.map(jobLine),
  ];
  await navigator.clipboard.writeText(lines.join('\n'));
  ElMessage.success(t('dependencies:copied'));
}
</script>

<template>
  <DatabaseLineage v-if="tab.scope" ref="dbLineage" :connection-id="tab.connectionId" :database="tab.database" :scope="tab.scope" />
  <div v-else class="dp" v-loading="loading && !report && view === 'list'">
    <header class="dp-head">
      <h2>{{ $t('dependencies:tab') }} · {{ qualified }}</h2>
      <span class="dp-dim">{{ tab.database || conns.byId(tab.connectionId)?.name }}</span>
      <div v-if="!tab.column" class="dp-mode" role="group" :aria-label="$t('dependencies:view.label')">
        <button :class="{ on: view === 'list' }" :aria-pressed="view === 'list'" @click="setView('list')">{{ $t('dependencies:view.list') }}</button>
        <button :class="{ on: view === 'lineage' }" :aria-pressed="view === 'lineage'" @click="setView('lineage')">{{ $t('dependencies:view.lineage') }}</button>
      </div>
      <span style="flex: 1" />
      <el-button size="small" :loading="loading && view === 'list'" @click="refresh">
        <el-icon><ei-refresh /></el-icon><span>{{ $t('dependencies:refresh') }}</span>
      </el-button>
      <el-button v-if="view === 'list'" size="small" :disabled="!report?.items.length && !uses?.items.length && !jobs.length" @click="copy">
        <el-icon><ei-document-copy /></el-icon><span>{{ $t('dependencies:copy') }}</span>
      </el-button>
    </header>
    <LineageDiagram v-if="view === 'lineage'" ref="lineage" :connection-id="tab.connectionId" :database="tab.database" :root="tab.object" :root-job-id="tab.jobId ?? null" />
    <template v-else>
      <div v-if="error" class="dp-error">{{ error }}</div>
      <template v-else-if="report">
        <p v-if="!tab.jobId" class="dp-note">
          {{ $t('dependencies:summary', { confirmed: counts.confirmed, probable: counts.probable, review: counts.review, scanned: report.scanned }) }}
          <br>{{ $t('dependencies:outside') }}
        </p>
        <p v-if="report.note" class="dp-note warn">{{ tb(report.note) }}</p>
        <p v-for="n in report.job_notes ?? []" :key="n" class="dp-note warn">{{ tb(n) }}</p>
        <p v-if="report.unreadable.length" class="dp-note warn">{{ $t('dependencies:unreadable', { names: report.unreadable.join(', ') }) }}</p>
        <div class="dp-body">
          <template v-if="!tab.column">
            <h2 class="dp-dir">{{ $t('dependencies:usesTitle') }}</h2>
            <p v-if="usesError" class="dp-note warn">{{ usesError }}</p>
            <p v-else-if="uses && !useGroups.length && !uses.external?.length" class="dp-dim">{{ $t('dependencies:usesNone') }}</p>
            <p v-if="uses?.note" class="dp-note warn">{{ tb(uses.note) }}</p>
            <section v-if="uses?.external?.length">
              <h3>{{ $t('dependencies:lineage.external') }} <span class="dp-dim">{{ uses.external.length }}</span></h3>
              <div v-for="(x, i) in uses.external" :key="i" class="dp-item">
                <div class="dp-line">
                  <span class="dp-tag">{{ x.system ?? $t('dependencies:lineage.external') }}</span>
                  <span class="dp-name">{{ [x.server, x.database, x.schema, x.name].filter(Boolean).join(' · ') }}</span>
                </div>
                <div v-if="x.detail" class="dp-detail nm-selectable">{{ x.detail }}</div>
              </div>
            </section>
            <section v-for="g in useGroups" :key="`u:${g.relation}`">
              <h3>{{ $t(`dependencies:relationUses.${g.relation}`) }} <span class="dp-dim">{{ g.items.length }}</span></h3>
              <div v-for="d in g.items" :key="`u:${d.kind}:${fullName(d)}:${d.detail ?? ''}`" class="dp-item">
                <div class="dp-line">
                  <span class="dp-tag">{{ $t(`dependencies:kind.${d.kind}`, { defaultValue: d.kind }) }}</span>
                  <a class="dp-name" @click="open(d)">{{ fullName(d) }}</a>
                  <span class="dp-badge" :class="d.confidence" :title="$t(`dependencies:confidenceTip.${d.confidence}`)">{{ $t(`dependencies:confidence.${d.confidence}`) }}</span>
                </div>
                <div v-if="d.detail" class="dp-detail nm-selectable">{{ d.detail }}</div>
                <div v-for="m in d.mentions" :key="m.line" class="dp-mention nm-selectable">
                  <span class="dp-ln">{{ m.line }}</span><code>{{ m.text }}</code>
                </div>
              </div>
            </section>
            <h2 v-if="!tab.jobId" class="dp-dir">{{ $t('dependencies:usedByTitle') }}</h2>
          </template>
          <p v-if="!tab.jobId && !groups.length && !jobs.length" class="dp-dim">{{ $t('dependencies:none') }}</p>
          <section v-for="g in groups" :key="g.relation">
            <h3>{{ $t(`dependencies:relation.${g.relation}`) }} <span class="dp-dim">{{ g.items.length }}</span></h3>
            <div v-for="d in g.items" :key="`${d.kind}:${fullName(d)}:${d.detail ?? ''}`" class="dp-item">
              <div class="dp-line">
                <span class="dp-tag">{{ $t(`dependencies:kind.${d.kind}`, { defaultValue: d.kind }) }}</span>
                <a class="dp-name" @click="open(d)">{{ fullName(d) }}</a>
                <span v-if="d.parent" class="dp-dim">{{ $t('dependencies:on', { parent: d.parent }) }}</span>
                <span class="dp-badge" :class="d.confidence" :title="$t(`dependencies:confidenceTip.${d.confidence}`)">{{ $t(`dependencies:confidence.${d.confidence}`) }}</span>
              </div>
              <div v-if="d.detail" class="dp-detail nm-selectable">{{ d.detail }}</div>
              <div v-for="m in d.mentions" :key="m.line" class="dp-mention nm-selectable">
                <span class="dp-ln">{{ m.line }}</span><code>{{ m.text }}</code>
                <span v-if="m.dynamic" class="dp-dyn">{{ $t('dependencies:dynamic') }}</span>
              </div>
            </div>
          </section>
          <section v-if="jobs.length">
            <h3>{{ $t('dependencies:jobs.title') }} <span class="dp-dim">{{ jobs.length }}</span></h3>
            <div v-for="j in jobs" :key="`job:${j.id}`" class="dp-item">
              <div class="dp-line">
                <span class="dp-tag">{{ j.kind }}</span>
                <a class="dp-name" :title="$t('dependencies:jobs.open')" @click="openJob(j)">{{ j.group ? `${j.group}.${j.name}` : j.name }}</a>
                <span class="dp-badge" :class="j.confidence" :title="$t(`dependencies:confidenceTip.${j.confidence}`)">{{ $t(`dependencies:confidence.${j.confidence}`) }}</span>
              </div>
              <div v-if="j.steps?.length" class="dp-detail nm-selectable">{{ $t('dependencies:jobs.steps', { steps: j.steps.join(', ') }) }}</div>
              <div v-for="(m, i) in j.mentions ?? []" :key="i" class="dp-mention nm-selectable">
                <span class="dp-ln">{{ m.line }}</span><code>{{ m.text }}</code>
                <span v-if="m.dynamic" class="dp-dyn">{{ $t('dependencies:dynamic') }}</span>
              </div>
            </div>
          </section>
        </div>
      </template>
    </template>
  </div>
</template>

<style scoped>
.dp { height: 100%; min-height: 0; display: flex; flex-direction: column; padding: 16px 22px 16px; background: var(--ide-editor); }
.dp-head { display: flex; align-items: center; gap: 8px; }
.dp-head h2 { margin: 0; font-size: 17px; color: var(--nm-text-strong); }
.dp-head .el-button { margin: 0; }
.dp-head .el-button .el-icon + span { margin-left: 4px; }
.dp-mode { display: inline-flex; margin-left: 8px; border: 1px solid var(--nm-border); border-radius: 4px; overflow: hidden; }
.dp-mode button { padding: 2px 10px; font: inherit; font-size: 12px; color: var(--nm-text); background: transparent; border: none; cursor: pointer; }
.dp-mode button + button { border-left: 1px solid var(--nm-border); }
.dp-mode button.on { color: var(--nm-text-strong); background: color-mix(in srgb, var(--ide-focus) 22%, transparent); }
.dp-dim { color: var(--nm-text-dim); font-size: 12px; }
.dp-note { margin: 10px 0 0; font-size: 12px; color: var(--nm-text-dim); }
.dp-note.warn { color: var(--nm-text); }
.dp-error { margin: 10px 0; padding: 8px 10px; border-radius: 3px; border: 1px solid color-mix(in srgb, var(--nm-danger) 50%, transparent); background: color-mix(in srgb, var(--nm-danger) 10%, transparent); color: var(--nm-text); white-space: pre-wrap; font-size: 12px; }
.dp-body { flex: 1; min-height: 0; overflow: auto; margin-top: 12px; }
.dp-body h2.dp-dir { margin: 18px 0 2px; font-size: 14px; color: var(--nm-text-strong); }
.dp-body h2.dp-dir:first-child { margin-top: 4px; }
.dp-body h3 { margin: 14px 0 6px; font-size: 13px; font-weight: 600; color: var(--nm-text-strong); }
.dp-item { padding: 6px 0; border-bottom: 1px solid var(--nm-border-soft); font-size: 12.5px; }
.dp-line { display: flex; align-items: center; gap: 8px; }
.dp-tag { font-size: 10.5px; padding: 0 5px; border-radius: 3px; border: 1px solid var(--nm-border); color: var(--nm-text-dim); }
.dp-name { font-weight: 500; color: var(--nm-text-strong); cursor: pointer; }
a.dp-name:hover { text-decoration: underline; }
.dp-badge { font-size: 10.5px; padding: 0 6px; border-radius: 8px; }
.dp-badge.confirmed { background: color-mix(in srgb, var(--nm-success) 20%, transparent); color: var(--nm-success); }
.dp-badge.probable { background: color-mix(in srgb, var(--nm-accent) 18%, transparent); color: var(--nm-text); }
.dp-badge.review { background: color-mix(in srgb, var(--nm-warning) 22%, transparent); color: var(--nm-warning); }
.dp-detail { margin: 3px 0 0 2px; color: var(--nm-text-dim); font-family: var(--nm-mono, monospace); font-size: 12px; }
.dp-mention { display: flex; align-items: baseline; gap: 8px; margin: 2px 0 0 2px; }
.dp-ln { min-width: 34px; text-align: right; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; font-size: 11.5px; }
.dp-mention code { white-space: pre; overflow: hidden; text-overflow: ellipsis; color: var(--nm-text); }
.dp-dyn { font-size: 10.5px; color: var(--nm-warning); }
</style>
