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
import { ElMessage } from 'element-plus';
import type { LineageScope, LineageStep, SourceCandidate } from '../api/types';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore } from '../stores/tabs';
import { KIND_ICONS } from '../composables/kindIcons';
import { ageParts } from '../composables/relativeAge';
import { openLineage, panelCalls } from '../composables/lineageOpen';
import { failedObjects, isPartial, retryChunk, runChunks } from '../composables/lineageChunks';
import type { LineageGraph } from '../api/types';
import { readerKind, type LineageNode } from '../composables/lineage';
import {
  OVERVIEW_ABOVE, applyFilters, buildModel, buildView, groupOf, hasCatalogLineage, isRelation, typesOf, checkedTypes, hiddenFor, isLastChecked, typesCount, layoutDb, noFilters, objectCount, schemaId, schemasOf,
  type DbModel, type Filters,
} from '../composables/databaseLineage';
import LineageCanvas from './LineageCanvas.vue';
import LineageDetail from './LineageDetail.vue';
import LineageSource from './LineageSource.vue';
import LineageInternalNote from './LineageInternalNote.vue';

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

function show(g: LineageGraph) {
  const m = buildModel(g);
  model.value = m;
  // A big database opens as an overview; a small one with everything expanded.
  expanded.value = new Set(objectCount(m) > OVERVIEW_ABOVE ? [] : schemasOf(m.nodes.values()));
  selected.value = null;
  filters.value = noFilters();
  text.value = '';
  phase.value = 'ready';
}
// The cache's age ("Actualizado hace X · actualizando…") while the server is asked again.
const updatedAt = ref<string | null>(null);
const refreshing = ref(false);
const refreshError = ref<string | null>(null);
const now = ref(Date.now());
const ticker = setInterval(() => { now.value = Date.now(); }, 30000);
onBeforeUnmount(() => clearInterval(ticker));
const updatedText = computed(() => {
  if (!updatedAt.value) return '';
  const { unit, n } = ageParts(updatedAt.value, now.value);
  return t('dependencies:database.updated', { ago: t(`dependencies:age.${unit}`, { n }) });
});

// A big model (Power BI) is read in chunks, each its own short call, and the diagram fills in as they
// come (laid out again at each stage, not at each chunk); the cache holds what was read so far.
const CHUNKED = new Set(['powerbi']);
const chunked = computed(() => CHUNKED.has(driver.value?.id ?? ''));
/** A complete cached graph younger than this is shown as it is, not read again. */
const FRESH_MS = 6 * 60 * 60 * 1000;
const next = ref<LineageStep | null>(null);
const failed = ref<LineageStep[]>([]);
const retrying = ref(false);
const partial = computed(() => isPartial({ next: next.value, failed: failed.value } as LineageGraph));
const failedCount = computed(() => failedObjects({ failed: failed.value } as LineageGraph));
const progressText = computed(() => {
  const n = next.value;
  if (!n) return '';
  return t(`dependencies:database.partial.stage.${n.stage}`, { done: n.offset, total: n.total, defaultValue: n.stage });
});
const percent = computed(() => {
  const n = next.value;
  if (!n || n.stage === 'structure') return 0;
  if (n.stage === 'consumers') return 95;
  return Math.min(90, Math.round((n.offset / Math.max(n.total, 1)) * 90));
});
function track(g: LineageGraph) {
  next.value = g.next ?? null;
  failed.value = g.failed ?? [];
}
/** The graph so far drawn again, keeping what the user opened, selected or typed. */
function refresh(g: LineageGraph) {
  if (!model.value) return show(g);
  model.value = buildModel(g);
  phase.value = 'ready';
}
async function loadChunked(force: boolean) {
  const mine = ++run;
  const shown = force && !!model.value;
  if (!shown) phase.value = 'loading';
  refreshing.value = true;
  error.value = null;
  refreshError.value = null;
  const calls = panelCalls(api, props.connectionId, props.database, props.scope);
  await runChunks(
    { cached: calls.cached, chunk: calls.chunk },
    {
      cached: (g, at) => { show(g); track(g); updatedAt.value = at; },
      progress: (g) => track(g),
      stage: (g) => refresh(g),
      done: (g) => {
        track(g);
        refresh(g);
        if (!isPartial(g)) updatedAt.value = new Date().toISOString();
        now.value = Date.now();
      },
      failed: (e, kept) => {
        if (kept || model.value) refreshError.value = errorMessage(e);
        else { error.value = errorMessage(e); phase.value = 'error'; }
      },
    },
    {
      useCache: !force,
      reuseComplete: (at) => !!at && Date.now() - Date.parse(at) < FRESH_MS,
      cancelled: () => mine !== run,
    },
  );
  if (mine === run) refreshing.value = false;
}
/** "Reintentar": just one chunk that was given up. */
async function retryOne(step: LineageStep) {
  if (retrying.value) return;
  retrying.value = true;
  try {
    const { chunk } = panelCalls(api, props.connectionId, props.database, props.scope);
    const g = await retryChunk({ cached: async () => ({ graph: null, updated_at: null }), chunk }, step);
    track(g);
    refresh(g);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    retrying.value = false;
  }
}

let run = 0;
/** `force` (the "Actualizar" button) skips the cache and keeps the drawn graph while the server answers. */
async function load(force = false) {
  if (chunked.value) return loadChunked(force);
  const mine = ++run;
  const shown = force && !!model.value;
  if (!shown) phase.value = 'loading';
  refreshing.value = true;
  error.value = null;
  refreshError.value = null;
  const calls = panelCalls(api, props.connectionId, props.database, props.scope);
  await openLineage(
    { cached: calls.cached, fetch: calls.fetch },
    {
      cached: (g, at) => { if (mine === run) { show(g); updatedAt.value = at; } },
      fresh: (g) => {
        if (mine !== run) return;
        show(g);
        updatedAt.value = new Date().toISOString();
        now.value = Date.now();
      },
      failed: (e, kept) => {
        if (mine !== run) return;
        if (kept) refreshError.value = errorMessage(e);
        else { error.value = errorMessage(e); phase.value = 'error'; }
      },
    },
    { useCache: !force, shown },
  );
  if (mine === run) refreshing.value = false;
}
/** The parent's "Actualizar": read again (a scan that waits for "Analizar" keeps waiting). */
function reload() {
  if (phase.value === 'ask') return;
  void load(true);
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

/** The types on the cards and how many of each (the "Tipos" list). */
const types = computed(() => (model.value ? typesOf(model.value.nodes.values()) : []));
const shownTypes = computed(() => checkedTypes(types.value, filters.value.hidden));
/** The checked types: the others are hidden. At least one stays checked. */
function showTypes(list: string[]) {
  const hidden = hiddenFor(types.value, list);
  if (hidden) filters.value = { ...filters.value, hidden };
}
const showAllTypes = () => { filters.value = { ...filters.value, hidden: [] }; };
const typesLabel = computed(() => {
  const n = typesCount(types.value, filters.value.hidden);
  return n ? t('dependencies:database.typesCount', n) : t('dependencies:database.kinds');
});
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
const kindLabel = (n: LineageNode) => (readerKind(n) ? t(`dependencies:cross.${readerKind(n)}`) : kindText(n.kind));
const icon = (n: LineageNode) => (n.kind === 'schema' ? 'folder' : readerKind(n) ? (readerKind(n) === 'report' ? 'data-analysis' : 'data-board') : n.external ? 'connection' : n.jobId ? 'alarm-clock' : KIND_ICONS[n.kind] ?? 'document');
function sub(n: LineageNode) {
  if (n.kind === 'schema') return t('dependencies:database.members', { count: n.members ?? 0 });
  if (n.external) return (readerKind(n) ? [kindLabel(n)] : [n.external.system, n.external.server, n.external.database]).filter(Boolean).join(' · ');
  return [kindLabel(n), n.jobKind, n.schema].filter(Boolean).join(' · ');
}
const title = (n: LineageNode) => (n.kind === 'schema' ? schemaName(n.name) : n.schema ? `${n.schema}.${n.name}` : n.name);

const hits = computed(() => new Set<string>());

function open(n: LineageNode) {
  if (n.kind === 'schema') return expand(n.name);
  if (n.external) return void (selected.value = n.id);
  if (n.jobId) return tabs.openServerJob(props.connectionId, n.jobId, n.name);
  const object = { kind: n.kind, schema: n.schema, name: n.name };
  tabs.openObject(props.connectionId, props.database, object, ['table', 'collection'].includes(n.kind) ? 'structure' : 'definition');
}
/** A source outside the database, followed into the connection that holds it: its lineage opens there, in its own tab. */
const following = ref(false);
async function follow(n: LineageNode, c: SourceCandidate) {
  if (!n.external || following.value) return;
  following.value = true;
  try {
    const found = await api.lineageLocateSource(c.connection_id, n.external);
    if (!found) return void ElMessage.warning(t('dependencies:cross.notFound', { name: c.name }));
    tabs.openDependencies(c.connection_id, found.database, found.object, null, 'lineage', null);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    following.value = false;
  }
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
const relationCount = computed(() => (model.value?.edges ?? []).filter(isRelation).length);
const filtering = computed(() => !!(filters.value.hidden.length || filters.value.text || filters.value.onlyConfirmed || filters.value.onlyConnected));
</script>

<template>
  <div class="dl">
    <header class="dl-head">
      <h2>{{ $t('dependencies:database.tab') }} · {{ database || conns.byId(connectionId)?.name }}</h2>
      <span v-if="scopeText" class="dl-dim">{{ scopeText }}</span>
      <LineageInternalNote :connection-id="connectionId" />
      <span style="flex: 1" />
      <el-button size="small" :loading="phase === 'loading' || refreshing" :disabled="phase === 'ask'" @click="reload">
        <el-icon><ei-refresh /></el-icon><span>{{ $t('dependencies:refresh') }}</span>
      </el-button>
    </header>

    <div v-if="phase === 'ask'" class="dl-center">
      <p>{{ $t('dependencies:database.ask.text') }}</p>
      <el-button type="primary" @click="load()">{{ $t('dependencies:database.ask.button') }}</el-button>
    </div>
    <div v-else-if="phase === 'loading'" v-loading="true" class="dl-center dl-wait">{{ $t('dependencies:database.loading') }}</div>
    <div v-else-if="phase === 'error'" class="dl-center">
      <p class="dl-error">{{ $t('dependencies:database.error', { error }) }}</p>
      <el-button size="small" @click="load()">{{ $t('dependencies:database.retry') }}</el-button>
    </div>

    <template v-else-if="model">
      <div class="dl-bar">
        <el-input v-model="text" size="small" clearable :placeholder="$t('dependencies:database.search')" style="width: 200px" />
        <el-popover v-if="types.length > 1" placement="bottom-start" trigger="click" :width="250">
          <template #reference>
            <el-button size="small" :type="filters.hidden.length ? 'primary' : undefined" :plain="!!filters.hidden.length" data-test="lineage-types">
              {{ typesLabel }}<el-icon class="el-icon--right"><ei-arrow-down /></el-icon>
            </el-button>
          </template>
          <div class="dl-types">
            <el-checkbox :model-value="!filters.hidden.length" :indeterminate="!!filters.hidden.length" @change="showAllTypes">{{ $t('dependencies:database.allTypes') }}</el-checkbox>
            <el-checkbox-group :model-value="shownTypes" @update:model-value="(v: unknown) => showTypes(v as string[])">
              <el-checkbox v-for="x in types" :key="x.type" :value="x.type" :disabled="isLastChecked(shownTypes, x.type)">
                {{ kindText(x.type) }}<span class="dl-type-count">{{ x.count }}</span>
              </el-checkbox>
            </el-checkbox-group>
          </div>
        </el-popover>
        <el-checkbox v-model="filters.onlyConfirmed" size="small">{{ $t('dependencies:database.onlyConfirmed') }}</el-checkbox>
        <el-checkbox v-model="filters.onlyConnected" size="small">{{ $t('dependencies:database.onlyConnected') }}</el-checkbox>
        <el-tooltip :content="$t('dependencies:database.relationsTip')" placement="bottom" :show-after="300">
          <el-checkbox v-model="filters.relations" size="small">{{ $t('dependencies:database.relations', { count: relationCount }) }}</el-checkbox>
        </el-tooltip>
        <el-button v-if="schemas.length > 1" size="small" @click="setAll(!allExpanded)">{{ allExpanded ? $t('dependencies:database.collapseAll') : $t('dependencies:database.expandAll') }}</el-button>
        <span class="dl-legend"><i class="dl-key solid" />{{ $t('dependencies:confidence.confirmed') }}</span>
        <span class="dl-legend"><i class="dl-key dashed" />{{ $t('dependencies:confidence.probable') }}</span>
      </div>
      <p class="dl-note">{{ summary }}<template v-if="updatedText"> · {{ updatedText }}<template v-if="refreshing"> · {{ $t('dependencies:database.updating') }}</template></template></p>
      <div v-if="partial" class="dl-note warn dl-partial">
        <strong>{{ $t('dependencies:database.partial.title') }}</strong>
        <span v-if="next">{{ progressText }}</span>
        <span v-if="failedCount">{{ $t('dependencies:database.partial.failed', { count: failedCount }) }}</span>
        <el-progress v-if="refreshing && next" :percentage="percent" :stroke-width="6" style="width: 160px" />
        <el-button v-if="!refreshing" size="small" type="primary" @click="load()">{{ $t('dependencies:database.partial.complete') }}</el-button>
        <el-button v-for="f in failed" :key="`${f.stage}@${f.offset}`" size="small" :loading="retrying" @click="retryOne(f)">
          {{ $t('dependencies:database.partial.retry', { from: f.offset + 1, to: f.offset + f.len }) }}
        </el-button>
      </div>
      <p v-if="refreshError" class="dl-note dl-error">{{ $t('dependencies:database.refreshFailed', { error: refreshError }) }}</p>
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
          <template v-if="sel.external" #extra>
            <LineageSource :connection-id="connectionId" :source="sel.external" :busy="following" @follow="follow(sel, $event)" />
          </template>
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
.dl-types { display: flex; flex-direction: column; max-height: 360px; overflow-y: auto; }
.dl-types :deep(.el-checkbox-group) { display: flex; flex-direction: column; border-top: 1px solid var(--el-border-color-lighter); margin-top: 2px; padding-top: 2px; }
.dl-types :deep(.el-checkbox) { margin-right: 0; height: 26px; }
.dl-types :deep(.el-checkbox__label) { display: flex; flex: 1; gap: 8px; }
.dl-types :deep(.el-checkbox) { width: 100%; }
.dl-type-count { margin-left: auto; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; }
.dl-types :deep(.el-checkbox__input.is-checked + .el-checkbox__label) { color: var(--el-text-color-regular); }
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
.dl-partial { display: flex; align-items: center; flex-wrap: wrap; gap: 10px; }
</style>
