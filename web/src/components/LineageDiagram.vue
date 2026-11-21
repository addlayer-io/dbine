<script setup lang="ts">
// An object's lineage: what it uses to the left, what uses it to the right,
// one level loaded; each card's `+` on either side loads the next level
// that way. Solid lines are in the engine's catalog, dashed ones come from
// reading the source. Sources outside the database are grey cards. Same
// canvas as the ER diagram: drag to pan, ⌘/Ctrl + wheel to zoom, F fits.
// A click selects a card (the lines that name the other are below); a
// double click opens it. The server's jobs that run an object are cards of
// their own to its right: a job's left side is what its steps use, and a
// double click opens it in Programadas › En los servidores.
// Across connections: a grey card of a source is clickable, and the detail panel finds which saved
// connection it is ("Ver su linaje en…": the grey card becomes the real object, read there, with
// its connection's badge); "Incluir otras conexiones" adds to the right what other connections
// (the Power BI tables fed by it) read from the object, and their `+` read on THEIR connection.
import { computed, nextTick, onMounted, ref, shallowRef, triggerRef, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import type { CrossUsers, ObjectRef, SourceCandidate } from '../api/types';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore } from '../stores/tabs';
import { KIND_ICONS } from '../composables/kindIcons';
import { MAX_NODES, addUsedBy, addUses, jobNodeId, layout, newLineage, nodeId, readerKind, replaceExternal, setCrossUsers, sideCounts, withoutCross, withoutRelations, type LineageNode } from '../composables/lineage';
import LineageCanvas from './LineageCanvas.vue';
import EngineIcon from './EngineIcon.vue';
import LineageDetail from './LineageDetail.vue';
import LineageSource from './LineageSource.vue';

const props = defineProps<{ connectionId: string; database: string; root: ObjectRef; rootJobId?: string | null }>();
const tabs = useTabsStore();
const conns = useConnectionsStore();
const { t } = useTranslation();

/** A card's size. */
const W = 214;
const H = 46;

const graph = shallowRef(newLineage(props.root, props.rootJobId));
const touch = () => triggerRef(graph);
/** Relations (foreign keys) go along with the lineage, or are left out to see the lineage alone. */
const relations = ref(true);
const relationCount = computed(() => graph.value.edges.filter((e) => e.relation === 'foreign_key').length);
/** "Incluir otras conexiones": what other connections read from the root. */
const cross = ref(false);
const crossState = ref<'idle' | 'loading' | 'done' | 'error'>('idle');
const crossInfo = ref<CrossUsers | null>(null);
const crossError = ref('');
const sees = computed(() => (cross.value ? graph.value : withoutCross(graph.value)));
const shown = computed(() => (relations.value ? sees.value : withoutRelations(sees.value)));
const placed = computed(() => layout(shown.value, W, H));
const nodes = computed(() => [...shown.value.nodes.values()]);
const canvas = ref<InstanceType<typeof LineageCanvas> | null>(null);
const selected = ref<string | null>(null);
const search = ref('');

/** The connection and database a card is read in: its own, else the diagram's. */
const connOf = (n: LineageNode) => ({ id: n.conn?.id ?? props.connectionId, database: n.conn?.database ?? props.database });
const driverOf = (n: LineageNode) => n.conn?.driver ?? conns.byId(n.conn?.id ?? '')?.config.driver ?? '';

/** Reads one side of a card: what it uses (`up`) or what uses it (`down`). */
async function load(id: string, side: 'up' | 'down') {
  const g = graph.value;
  const n = g.nodes.get(id);
  if (!n || n.external || n[side] === 'loading' || n[side] === 'done') return;
  n[side] = 'loading';
  n.error = undefined;
  touch();
  const target = { object: { kind: n.kind, schema: n.schema, name: n.name }, column: null };
  const c = connOf(n);
  try {
    const r = n.jobId
      ? await api.getJobReferences(c.id, c.database, n.jobId)
      : side === 'up'
        ? await api.getReferences(c.id, c.database, target)
        : await api.getDependents(c.id, c.database, target);
    if (graph.value !== g) return;
    if (side === 'up') addUses(g, id, r, n.conn);
    else addUsedBy(g, id, r, n.conn);
  } catch (e) {
    if (graph.value !== g) return;
    n[side] = 'error';
    n.error = errorMessage(e);
  }
  touch();
}
function start() {
  const id = props.rootJobId ? jobNodeId(props.rootJobId) : nodeId(props.root);
  void load(id, 'up');
  void load(id, 'down');
  if (cross.value) void loadCross(false);
}

/** Who reads the root from other connections (the backend caches it for a while; `refresh` asks again). */
async function loadCross(refresh: boolean) {
  const g = graph.value;
  crossState.value = 'loading';
  crossError.value = '';
  try {
    const r = await api.getCrossUsers(props.connectionId, props.database, props.root, refresh);
    if (graph.value !== g) return;
    setCrossUsers(g, nodeId(props.root), r.users, (id) => conns.byId(id)?.config.driver);
    crossInfo.value = r;
    crossState.value = 'done';
  } catch (e) {
    if (graph.value !== g) return;
    crossState.value = 'error';
    crossError.value = errorMessage(e);
  }
  touch();
  void nextTick(() => canvas.value?.fit());
}
watch(cross, (on) => {
  if (on && (crossState.value === 'idle' || crossState.value === 'error')) void loadCross(false);
});

/** "Ver su linaje en…": the grey card becomes the real object in the connection that holds it, and what it uses is read there. */
const following = ref(false);
async function follow(n: LineageNode, c: SourceCandidate) {
  if (!n.external || following.value) return;
  const g = graph.value;
  following.value = true;
  try {
    const found = await api.lineageLocateSource(c.connection_id, n.external);
    if (graph.value !== g) return;
    if (!found) {
      ElMessage.warning(t('dependencies:cross.notFound', { name: c.name }));
      return;
    }
    const id = replaceExternal(g, n.id, found, { id: c.connection_id, name: c.name, database: found.database, driver: c.driver });
    if (!id) return;
    selected.value = id;
    touch();
    void load(id, 'up');
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    following.value = false;
  }
}
/** Read again from the start (the parent's "Actualizar"). */
function reload() {
  graph.value = newLineage(props.root, props.rootJobId);
  crossInfo.value = null;
  selected.value = null;
  canvas.value?.markMoved();
  start();
  void nextTick(() => canvas.value?.fit());
}
defineExpose({ reload });
onMounted(start);

// -- cards -------------------------------------------------------------------------------------
const kindLabel = (n: LineageNode) => {
  const r = readerKind(n);
  if (r) return t(`dependencies:cross.${r}`);
  return n.external ? t('dependencies:lineage.external') : t(`dependencies:kind.${n.kind}`, { defaultValue: n.kind });
};
const icon = (n: LineageNode) => {
  const r = readerKind(n);
  if (r) return r === 'report' ? 'data-analysis' : 'data-board';
  return n.external ? 'connection' : n.jobId ? 'alarm-clock' : KIND_ICONS[n.kind] ?? 'document';
};
/** The second line: the kind and the schema, or where an outside source is. */
function sub(n: LineageNode) {
  if (n.external) {
    const x = n.external;
    return (readerKind(n) ? [kindLabel(n)] : [x.system, x.server, x.database]).filter(Boolean).join(' · ');
  }
  return [kindLabel(n), n.jobKind, n.schema].filter(Boolean).join(' · ');
}
const counts = (id: string) => sideCounts(graph.value, id);
/** A side's button: `+` to read it, a spinner, the count, or `!`. */
function sideText(n: LineageNode, side: 'up' | 'down') {
  const s = n[side];
  if (s === 'none') return '+';
  if (s === 'error') return '!';
  if (s === 'loading') return '';
  return String(side === 'up' ? counts(n.id).up : counts(n.id).down);
}
function sideTitle(n: LineageNode, side: 'up' | 'down') {
  if (n[side] === 'error') return t('dependencies:lineage.error', { error: n.error ?? '' });
  return side === 'up' ? t('dependencies:lineage.loadUses') : t('dependencies:lineage.loadUsedBy');
}

const q = computed(() => search.value.trim().toLowerCase());
const hits = computed(() => new Set(q.value ? nodes.value.filter((n) => `${n.schema ?? ''}.${n.name}`.toLowerCase().includes(q.value)).map((n) => n.id) : []));
const sel = computed(() => (selected.value ? graph.value.nodes.get(selected.value) ?? null : null));
const fullName = (n?: LineageNode | null) => (n ? (n.schema ? `${n.schema}.${n.name}` : n.name) : '');

function open(n: LineageNode) {
  if (n.external) return void (selected.value = n.id);
  const c = connOf(n);
  if (n.jobId) return tabs.openServerJob(c.id, n.jobId, n.name);
  const object = { kind: n.kind, schema: n.schema, name: n.name };
  tabs.openObject(c.id, c.database, object, ['table', 'collection'].includes(n.kind) ? 'structure' : 'definition');
}
/** The lineage with this card in the middle, in its own tab. */
function recenter(n: LineageNode) {
  if (n.external) return;
  const c = connOf(n);
  tabs.openDependencies(c.id, c.database, { kind: n.kind, schema: n.schema, name: n.name }, null, 'lineage', n.jobId ?? null);
}
</script>

<template>
  <div class="ln">
    <div class="ln-bar">
      <el-input v-model="search" size="small" clearable :placeholder="$t('dependencies:lineage.search')" style="width: 220px" />
      <el-tooltip :content="$t('dependencies:database.relationsTip')" placement="bottom" :show-after="300">
        <el-checkbox v-model="relations" size="small">{{ $t('dependencies:database.relations', { count: relationCount }) }}</el-checkbox>
      </el-tooltip>
      <el-tooltip v-if="!rootJobId" :content="$t('dependencies:cross.switchTip')" placement="bottom" :show-after="300">
        <el-switch v-model="cross" size="small" :active-text="$t('dependencies:cross.switch')" />
      </el-tooltip>
      <span class="ln-legend"><i class="ln-key solid" />{{ $t('dependencies:confidence.confirmed') }}</span>
      <span class="ln-legend"><i class="ln-key dashed" />{{ $t('dependencies:confidence.probable') }}</span>
      <span class="ln-hint">{{ $t('dependencies:lineage.hint') }}</span>
    </div>
    <div v-if="cross" class="ln-note ln-cross">
      <template v-if="crossState === 'loading'"><el-icon class="is-loading"><ei-loading /></el-icon> {{ $t('dependencies:cross.searching') }}</template>
      <template v-else-if="crossState === 'error'">{{ $t('dependencies:lineage.error', { error: crossError }) }}</template>
      <template v-else-if="crossInfo">
        <span v-if="!crossInfo.users.length">{{ $t('dependencies:cross.nobody') }}</span>
        <span v-if="crossInfo.searched.length">{{ $t('dependencies:cross.searched', { names: crossInfo.searched.map((s) => $t('dependencies:cross.searchedItem', { name: s.name, models: s.models, cached: s.cached ? $t('dependencies:cross.cached') : '' })).join('; ') }) }}</span>
      </template>
      <el-button v-if="crossState !== 'loading'" size="small" link @click="loadCross(true)">{{ $t('dependencies:cross.refresh') }}</el-button>
    </div>
    <p v-for="note in cross ? crossInfo?.notes ?? [] : []" :key="note" class="ln-note warn">{{ tb(note) }}</p>
    <p v-if="graph.capped" class="ln-note warn">{{ $t('dependencies:lineage.capped', { n: MAX_NODES }) }}</p>
    <p v-for="note in graph.notes" :key="note" class="ln-note warn">{{ tb(note) }}</p>
    <p v-if="graph.unreadable.length" class="ln-note warn">{{ $t('dependencies:unreadable', { names: graph.unreadable.join(', ') }) }}</p>
    <div class="ln-body">
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
        :title="fullName"
        @open="open"
      >
        <template #before="{ n }">
          <button
            v-if="!n.external"
            class="ln-side left"
            :class="n.up"
            :title="sideTitle(n, 'up')"
            :aria-label="sideTitle(n, 'up')"
            @click.stop="load(n.id, 'up')"
          ><el-icon v-if="n.up === 'loading'" class="is-loading"><ei-loading /></el-icon><template v-else>{{ sideText(n, 'up') }}</template></button>
        </template>
        <template #after="{ n }">
          <button
            v-if="!n.external && !n.jobId"
            class="ln-side right"
            :class="n.down"
            :title="sideTitle(n, 'down')"
            :aria-label="sideTitle(n, 'down')"
            @click.stop="load(n.id, 'down')"
          ><el-icon v-if="n.down === 'loading'" class="is-loading"><ei-loading /></el-icon><template v-else>{{ sideText(n, 'down') }}</template></button>
        </template>
        <template #badge="{ n }">
          <span v-if="n.conn" class="ln-conn" :title="$t('dependencies:cross.badge', { name: n.conn.name })">
            <EngineIcon :id="driverOf(n)" :name="driverOf(n)" :size="11" /><span>{{ n.conn.name }}</span>
          </span>
        </template>
      </LineageCanvas>
      <LineageDetail v-if="sel" :graph="graph" :sel="sel" :kind-label="kindLabel" @select="selected = $event">
        <template #icon><component :is="`ei-${icon(sel)}`" /></template>
        <template v-if="sel.external" #extra>
          <LineageSource :connection-id="connOf(sel).id" :source="sel.external" :busy="following" @follow="follow(sel, $event)" />
        </template>
        <template #actions>
          <template v-if="!sel.external">
            <el-button size="small" @click="open(sel)">{{ sel.jobId ? $t('dependencies:jobs.open') : $t('dependencies:lineage.open') }}</el-button>
            <el-button v-if="!sel.root" size="small" @click="recenter(sel)">{{ $t('dependencies:lineage.recenter') }}</el-button>
          </template>
        </template>
      </LineageDetail>
    </div>
  </div>
</template>

<style scoped>
.ln { flex: 1; min-height: 0; display: flex; flex-direction: column; }
.ln-bar { display: flex; align-items: center; gap: 12px; padding: 8px 0; font-size: 12px; color: var(--nm-text-dim); }
.ln-legend { display: inline-flex; align-items: center; gap: 5px; }
.ln-key { display: inline-block; width: 22px; border-top: 2px solid var(--nm-text-dim); }
.ln-key.dashed { border-top-style: dashed; }
.ln-hint { margin-left: auto; }
.ln-cross { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.ln-conn {
  position: absolute; left: 8px; top: -9px; max-width: calc(100% - 16px); display: inline-flex; align-items: center; gap: 4px;
  padding: 0 6px; height: 16px; font-size: 10px; color: var(--nm-text); background: var(--ide-editor);
  border: 1px solid var(--nm-border); border-radius: 8px; pointer-events: none;
}
.ln-conn span { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.ln-note { margin: 4px 0; font-size: 12px; color: var(--nm-text-dim); }
.ln-note.warn { color: var(--nm-text); }
.ln-body { flex: 1; min-height: 0; display: flex; gap: 10px; }
.ln-side {
  position: absolute; top: 50%; transform: translateY(-50%); min-width: 18px; height: 18px; padding: 0 3px;
  display: inline-flex; align-items: center; justify-content: center; font: 600 10.5px var(--nm-font);
  color: var(--nm-text); background: var(--ide-editor); border: 1px solid var(--nm-border); border-radius: 9px; cursor: pointer;
}
.ln-side.left { left: -10px; }
.ln-side.right { right: -10px; }
.ln-side.none { color: var(--ide-focus); border-color: var(--ide-focus); }
.ln-side.done { cursor: default; color: var(--nm-text-dim); }
.ln-side.error { color: var(--nm-danger); border-color: var(--nm-danger); }
</style>
