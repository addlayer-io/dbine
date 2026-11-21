<script setup lang="ts">
// "Linaje" of a whole database (or a schema, or a kind's folder, or a Power BI model):
// every object a card, every dependency a line (solid: the engine's catalog, dashed: read
// from code), the server's jobs and the sources outside the database as cards of their own,
// the objects with no line in columns at the left. Past ~200 objects it opens as an
// overview by schema (one card per schema, line weight = dependencies crossing) and a
// click on a schema expands it. Engines whose catalog answers it load on open; for the rest
// the generic scan (every definition read) waits for "Analizar".
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import type { LineageScope } from '../api/types';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore } from '../stores/tabs';
import { KIND_ICONS } from '../composables/kindIcons';
import type { LineageNode } from '../composables/lineage';
import {
  OVERVIEW_ABOVE, applyFilters, buildModel, buildView, groupOf, hasCatalogLineage, kindsOf, layoutDb, noFilters, objectCount, schemaId, schemasOf,
  type DbModel, type Filters,
} from '../composables/databaseLineage';
import LineageCanvas from './LineageCanvas.vue';
import LineageDetail from './LineageDetail.vue';

const props = defineProps<{ connectionId: string; database: string; scope: LineageScope }>();
const conns = useConnectionsStore();
const tabs = useTabsStore();
const { t } = useTranslation();

const W = 214;
const H = 46;

type Phase = 'ask' | 'loading' | 'ready' | 'error';
const driver = computed(() => conns.driverOf(props.connectionId));
const phase = ref<Phase>('ask');
const error = ref<string | null>(null);
const model = shallowRef<DbModel | null>(null);
const filters = ref<Filters>(noFilters());
const text = ref('');
const expanded = ref<Set<string>>(new Set());
const selected = ref<string | null>(null);
const canvas = ref<InstanceType<typeof LineageCanvas> | null>(null);

let run = 0;
async function load() {
  const mine = ++run;
  phase.value = 'loading';
  error.value = null;
  try {
    const g = await api.getLineageGraph(props.connectionId, props.database, { ...props.scope, jobs: true });
    if (mine !== run) return;
    const m = buildModel(g);
    model.value = m;
    // A big database opens as an overview; a small one with everything expanded.
    expanded.value = new Set(objectCount(m) > OVERVIEW_ABOVE ? [] : schemasOf(m.nodes.values()));
    selected.value = null;
    filters.value = noFilters();
    text.value = '';
    phase.value = 'ready';
  } catch (e) {
    if (mine !== run) return;
    error.value = errorMessage(e);
    phase.value = 'error';
  }
}
/** The parent's "Actualizar": read again (a scan that waits for "Analizar" keeps waiting). */
function reload() {
  if (phase.value === 'ask') return;
  void load();
}
defineExpose({ reload });
onMounted(() => {
  if (hasCatalogLineage(driver.value?.id)) void load();
});
onBeforeUnmount(() => { run++; });

// The text filter waits for a pause in the typing: every change lays the diagram out again.
let timer: ReturnType<typeof setTimeout> | undefined;
watch(text, (v) => {
  clearTimeout(timer);
  timer = setTimeout(() => { filters.value = { ...filters.value, text: v }; }, 250);
});
onBeforeUnmount(() => clearTimeout(timer));

const kinds = computed(() => (model.value ? kindsOf(model.value.nodes.values()) : []));
const schemas = computed(() => (model.value ? schemasOf(model.value.nodes.values()) : []));
const filtered = computed(() => (model.value ? applyFilters(model.value, filters.value) : { nodes: [], edges: [] }));
const view = computed(() => buildView(filtered.value.nodes, filtered.value.edges, expanded.value));
const placed = computed(() => layoutDb(view.value.lineage, W, H));
const nodes = computed(() => [...view.value.lineage.nodes.values()]);
const sel = computed(() => (selected.value ? view.value.lineage.nodes.get(selected.value) ?? null : null));
const objects = computed(() => (model.value ? objectCount(model.value) : 0));
const overview = computed(() => objects.value > OVERVIEW_ABOVE);
const allExpanded = computed(() => schemas.value.every((s) => expanded.value.has(s)));

watch(view, () => { if (selected.value && !view.value.lineage.nodes.has(selected.value)) selected.value = null; });

function expand(schema: string, on = true) {
  const next = new Set(expanded.value);
  if (on) next.add(schema);
  else next.delete(schema);
  expanded.value = next;
  selected.value = on ? null : schemaId(schema);
}
function setAll(on: boolean) {
  expanded.value = new Set(on ? schemas.value : []);
  selected.value = null;
}

const schemaName = (s: string) => s || t('dependencies:database.noSchema');
/** The driver's own word for a kind, when the generic list doesn't have it. */
function kindText(kind: string) {
  if (kind === 'external') return t('dependencies:lineage.external');
  const own = driver.value?.object_kinds.find((k) => k.id === kind);
  return t(`dependencies:kind.${kind}`, { defaultValue: own ? tb(own.label) : kind });
}
const kindLabel = (n: LineageNode) => kindText(n.kind);
const icon = (n: LineageNode) => (n.kind === 'schema' ? 'folder' : n.external ? 'connection' : n.jobId ? 'alarm-clock' : KIND_ICONS[n.kind] ?? 'document');
function sub(n: LineageNode) {
  if (n.kind === 'schema') return t('dependencies:database.members', { count: n.members ?? 0 });
  if (n.external) return [n.external.system, n.external.server, n.external.database].filter(Boolean).join(' · ');
  return [kindLabel(n), n.jobKind, n.schema].filter(Boolean).join(' · ');
}
const title = (n: LineageNode) => (n.kind === 'schema' ? schemaName(n.name) : n.schema ? `${n.schema}.${n.name}` : n.name);

const hits = computed(() => new Set<string>());

function open(n: LineageNode) {
  if (n.kind === 'schema') return expand(n.name);
  if (n.external) return;
  if (n.jobId) return tabs.openServerJob(props.connectionId, n.jobId, n.name);
  const object = { kind: n.kind, schema: n.schema, name: n.name };
  tabs.openObject(props.connectionId, props.database, object, ['table', 'collection'].includes(n.kind) ? 'structure' : 'definition');
}
/** The object lineage with this card in the middle, in its own tab. */
function recenter(n: LineageNode) {
  tabs.openDependencies(props.connectionId, props.database, { kind: n.kind, schema: n.schema, name: n.name }, null, 'lineage', n.jobId ?? null);
}

const scopeText = computed(() => {
  const parts = [...(props.scope.schemas ?? [])];
  for (const k of props.scope.kinds ?? []) parts.push(kindText(k));
  return parts.join(' · ');
});
const summary = computed(() => t('dependencies:database.summary', {
  objects: objects.value, lines: model.value?.edges.length ?? 0, scanned: model.value?.scanned ?? 0,
}));
const filtering = computed(() => !!(filters.value.kinds.length || filters.value.text || filters.value.onlyConfirmed || filters.value.onlyConnected));
</script>

<template>
  <div class="dl">
    <header class="dl-head">
      <h2>{{ $t('dependencies:database.tab') }} · {{ database || conns.byId(connectionId)?.name }}</h2>
      <span v-if="scopeText" class="dl-dim">{{ scopeText }}</span>
      <span style="flex: 1" />
      <el-button size="small" :loading="phase === 'loading'" :disabled="phase === 'ask'" @click="reload">
        <el-icon><ei-refresh /></el-icon><span>{{ $t('dependencies:refresh') }}</span>
      </el-button>
    </header>

    <div v-if="phase === 'ask'" class="dl-center">
      <p>{{ $t('dependencies:database.ask.text') }}</p>
      <el-button type="primary" @click="load">{{ $t('dependencies:database.ask.button') }}</el-button>
    </div>
    <div v-else-if="phase === 'loading'" v-loading="true" class="dl-center dl-wait">{{ $t('dependencies:database.loading') }}</div>
    <div v-else-if="phase === 'error'" class="dl-center">
      <p class="dl-error">{{ $t('dependencies:database.error', { error }) }}</p>
      <el-button size="small" @click="load">{{ $t('dependencies:database.retry') }}</el-button>
    </div>

    <template v-else-if="model">
      <div class="dl-bar">
        <el-input v-model="text" size="small" clearable :placeholder="$t('dependencies:database.search')" style="width: 200px" />
        <el-select v-model="filters.kinds" size="small" multiple collapse-tags collapse-tags-tooltip clearable :placeholder="$t('dependencies:database.kinds')" style="width: 190px">
          <el-option v-for="k in kinds" :key="k" :value="k" :label="kindText(k)" />
        </el-select>
        <el-checkbox v-model="filters.onlyConfirmed" size="small">{{ $t('dependencies:database.onlyConfirmed') }}</el-checkbox>
        <el-checkbox v-model="filters.onlyConnected" size="small">{{ $t('dependencies:database.onlyConnected') }}</el-checkbox>
        <el-button v-if="schemas.length > 1" size="small" @click="setAll(!allExpanded)">{{ allExpanded ? $t('dependencies:database.collapseAll') : $t('dependencies:database.expandAll') }}</el-button>
        <span class="dl-legend"><i class="dl-key solid" />{{ $t('dependencies:confidence.confirmed') }}</span>
        <span class="dl-legend"><i class="dl-key dashed" />{{ $t('dependencies:confidence.probable') }}</span>
      </div>
      <p class="dl-note">{{ summary }}</p>
      <p v-if="overview && !expanded.size" class="dl-note">{{ $t('dependencies:database.overview', { objects, schemas: schemas.length }) }}</p>
      <p v-if="model.truncated" class="dl-note warn">{{ $t('dependencies:database.truncated') }}</p>
      <p v-if="view.hidden" class="dl-note warn">{{ $t('dependencies:database.hidden', { hidden: view.hidden }) }}</p>
      <p v-for="note in model.notes" :key="note" class="dl-note warn">{{ tb(note) }}</p>
      <p v-if="model.unreadable.length" class="dl-note warn">{{ $t('dependencies:unreadable', { names: model.unreadable.join(', ') }) }}</p>
      <p v-if="!nodes.length" class="dl-center">{{ filtering ? $t('dependencies:database.noMatch') : $t('dependencies:database.empty') }}</p>
      <div v-else class="dl-body">
        <LineageCanvas
          ref="canvas"
          v-model:selected="selected"
          :nodes="nodes"
          :placed="placed"
          :w="W"
          :h="H"
          :hits="hits"
          :icon="icon"
          :sub="sub"
          :title="title"
          @open="open"
        />
        <LineageDetail v-if="sel" :graph="view.lineage" :sel="sel" :kind-label="kindLabel" @select="selected = $event">
          <template #icon><component :is="`ei-${icon(sel)}`" /></template>
          <template #actions>
            <template v-if="sel.kind === 'schema'">
              <el-button size="small" @click="expand(sel.name)">{{ $t('dependencies:database.expand') }}</el-button>
            </template>
            <template v-else-if="!sel.external">
              <el-button size="small" @click="open(sel)">{{ sel.jobId ? $t('dependencies:jobs.open') : $t('dependencies:lineage.open') }}</el-button>
              <el-button size="small" @click="recenter(sel)">{{ $t('dependencies:lineage.recenter') }}</el-button>
              <el-button v-if="groupOf(sel) !== null && overview" size="small" @click="expand(groupOf(sel)!, false)">{{ $t('dependencies:database.collapse') }}</el-button>
            </template>
          </template>
        </LineageDetail>
      </div>
    </template>
  </div>
</template>

<style scoped>
.dl { height: 100%; background: var(--ide-editor); min-height: 0; display: flex; flex-direction: column; padding: 10px 14px; }
.dl-head { display: flex; align-items: center; gap: 10px; margin-bottom: 6px; }
.dl-head h2 { margin: 0; font-size: 15px; color: var(--nm-text-strong); }
.dl-dim { color: var(--nm-text-dim); font-size: 12px; }
.dl-bar { display: flex; align-items: center; flex-wrap: wrap; gap: 12px; padding: 6px 0; font-size: 12px; color: var(--nm-text-dim); }
.dl-legend { display: inline-flex; align-items: center; gap: 5px; margin-left: auto; }
.dl-legend + .dl-legend { margin-left: 0; }
.dl-key { display: inline-block; width: 22px; border-top: 2px solid var(--nm-text-dim); }
.dl-key.dashed { border-top-style: dashed; }
.dl-note { margin: 3px 0; font-size: 12px; color: var(--nm-text-dim); }
.dl-note.warn { color: var(--nm-text); }
.dl-body { flex: 1; min-height: 0; display: flex; gap: 10px; }
.dl-center { flex: 1; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 12px; color: var(--nm-text-dim); font-size: 13px; text-align: center; padding: 0 40px; }
.dl-center p { max-width: 520px; margin: 0; }
.dl-wait { min-height: 160px; }
.dl-error { color: var(--nm-danger); }
</style>
