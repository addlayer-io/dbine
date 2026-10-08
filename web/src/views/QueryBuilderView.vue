<script setup lang="ts">
import { computed, onMounted, onUnmounted, reactive, ref, toRaw, watch } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import {
  emptySpec, queryBuilderApi,
  type Aggregate, type BuilderFeatures, type BuiltPreview, type Condition, type FilterOp, type JoinKind,
  type QuerySpec, type SpecColumn, type SpecJoin, type SpecTable,
} from '../api/queryBuilder';
import type { ForeignKeyDef, TableSchema } from '../api/schema-types';
import type { DbObject } from '../api/types';
import { tb } from '../i18n/backend';
import { newQuery } from '../composables/actions';
import { usePanZoom } from '../composables/usePanZoom';
import { dbKey, useConnectionsStore } from '../stores/connections';
import { useTabsStore, type QueryBuilderTab } from '../stores/tabs';
import CodeEditor from '../components/CodeEditor.vue';
import ContextMenu, { type MenuItem } from '../components/ContextMenu.vue';
import ResultGrid from '../components/ResultGrid.vue';

// "Diseñar consulta" (docs/constructor-de-consultas.md). Tables dragged
// from the list onto a canvas (same pan/zoom and SVG lines as the ER
// diagram), joins from foreign keys or by dragging a column onto another,
// and a grid with one row per column (alias, aggregate, sorting, filters in
// AND groups ORed together). The spec is kept with the tab; the backend
// writes the engine's SQL from it (build_query) and runs a read-only
// preview. Nothing here changes the database: "Abrir en una consulta"
// hands the SQL to a query tab for the user to run.

const props = defineProps<{ tab: QueryBuilderTab }>();
const { t } = useTranslation();
const conns = useConnectionsStore();
const tabs = useTabsStore();

const cid = props.tab.connectionId;
const db = props.tab.database;
const driver = computed(() => conns.driverOf(cid));

const spec = reactive<QuerySpec>({ ...emptySpec(db), ...structuredClone(toRaw(props.tab.spec ?? emptySpec(db))) });
const plain = (): QuerySpec => JSON.parse(JSON.stringify(spec));
const newId = () => crypto.randomUUID();

// -- structure -----------------------------------------------------------------------------
interface ColMeta { name: string; data_type: string; pk: boolean; fk: boolean }
const tkey = (schema: string | null, name: string) => `${schema ?? ''}\u0001${name}`;
/** Columns per table (`tkey`), from the schema read or the explorer's columns. */
const meta = reactive<Record<string, ColMeta[]>>({});
const fks = ref(new Map<string, ForeignKeyDef[]>());
const schemaRead = ref(false);
const connectError = ref<string | null>(null);

const kinds = computed(() => new Set((driver.value?.object_kinds ?? []).filter((k) => k.has_columns && k.browsable).map((k) => k.id)));
const objectsState = computed(() => conns.objects[dbKey(cid, db)]);
const available = computed<DbObject[]>(() => (objectsState.value?.items ?? []).filter((o) => kinds.value.has(o.kind)));

const search = ref('');
const groups = computed(() => {
  const q = search.value.trim().toLowerCase();
  const out = new Map<string, DbObject[]>();
  for (const o of available.value) {
    if (q && !`${o.schema ?? ''}.${o.name}`.toLowerCase().includes(q)) continue;
    const g = out.get(o.schema ?? '');
    if (g) g.push(o);
    else out.set(o.schema ?? '', [o]);
  }
  return [...out.entries()].sort((a, b) => a[0].localeCompare(b[0])).map(([schema, items]) => ({ schema, items: items.sort((a, b) => a.name.localeCompare(b.name)) }));
});
const kindLabel = (id: string) => tb(driver.value?.object_kinds.find((k) => k.id === id)?.label ?? id);

async function loadColumns(st: SpecTable) {
  const k = tkey(st.schema, st.name);
  if (meta[k]) return;
  meta[k] = [];
  try {
    const items = await conns.loadColumns(cid, db, { kind: st.kind, schema: st.schema, name: st.name, parent: null });
    const fk = new Set((fks.value.get(k) ?? []).flatMap((f) => f.columns));
    meta[k] = items.map((c) => ({ name: c.name, data_type: c.data_type, pk: c.primary_key, fk: fk.has(c.name) }));
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

/** Keys, foreign keys and typed columns of every table (in the background:
 *  a big database takes a while, and the builder works without it). */
async function readSchema() {
  try {
    const all = await invoke<TableSchema[]>('database_schema', { args: { connection_id: cid, database: db } });
    const map = new Map<string, ForeignKeyDef[]>();
    for (const ts of all) {
      const k = tkey(ts.schema, ts.name);
      map.set(k, ts.foreign_keys);
      const pk = new Set(ts.primary_key?.columns ?? []);
      const fk = new Set(ts.foreign_keys.flatMap((f) => f.columns));
      if (ts.columns.length) meta[k] = ts.columns.map((c) => ({ name: c.name, data_type: c.data_type, pk: pk.has(c.name), fk: fk.has(c.name) }));
    }
    fks.value = map;
  } catch {
    // Engines without a schema read: no automatic joins.
  } finally {
    schemaRead.value = true;
  }
}

onMounted(async () => {
  if (!(await conns.ensureConnected(cid))) {
    connectError.value = t('queryBuilder:connectFailed');
    return;
  }
  conns.loadObjects(cid, db);
  for (const st of spec.tables) loadColumns(st);
  build();
  readSchema();
});

// -- the generated SQL ---------------------------------------------------------------------
const sql = ref('');
const warnings = ref<string[]>([]);
const features = ref<BuilderFeatures | null>(null);
const buildError = ref<string | null>(null);
let seq = 0;
async function build() {
  const my = ++seq;
  try {
    const r = await queryBuilderApi.build(cid, plain(), props.tab.id);
    if (my !== seq) return;
    sql.value = r.sql;
    warnings.value = r.warnings;
    features.value = r.features;
    buildError.value = null;
  } catch (e) {
    if (my === seq) buildError.value = errorMessage(e);
  }
}
let timer: ReturnType<typeof setTimeout> | undefined;
watch(spec, () => {
  clearTimeout(timer);
  timer = setTimeout(() => {
    tabs.setBuilderSpec(props.tab.id, plain());
    build();
  }, 250);
}, { deep: true });
onUnmounted(() => clearTimeout(timer));

const oneTable = computed(() => !!features.value && !features.value.joins.length);
const operators = computed<FilterOp[]>(() => features.value?.operators ?? []);
const aggregates = computed<Aggregate[]>(() => ['none', ...(features.value?.aggregates ?? [])]);

// -- tables on the canvas -------------------------------------------------------------------
const W = 220;
const HEAD = 30;
const ROW = 22;
const tableById = (id: string) => spec.tables.find((x) => x.id === id);
const colsOf = (st: SpecTable) => meta[tkey(st.schema, st.name)] ?? [];
const aliasOf = (st: SpecTable) => st.alias || st.name;
const heightOf = (st: SpecTable) => HEAD + (colsOf(st).length + 1) * ROW + 4;

function addTable(o: { kind: string; schema: string | null; name: string }, at?: { x: number; y: number }) {
  if (oneTable.value && spec.tables.length) {
    ElMessage.info(t('queryBuilder:oneTable'));
    return;
  }
  const right = spec.tables.reduce((m, x) => Math.max(m, x.x + W), 0);
  const taken = new Set(spec.tables.map(aliasOf).map((a) => a.toLowerCase()));
  let alias = '';
  for (let n = 2; taken.has((alias || o.name).toLowerCase()); n++) alias = `${o.name}${n}`;
  const st: SpecTable = { id: newId(), kind: o.kind, schema: o.schema, name: o.name, alias, x: Math.round(at?.x ?? (spec.tables.length ? right + 60 : 40)), y: Math.round(at?.y ?? 40) };
  autoJoins(st);
  spec.tables.push(st);
  loadColumns(st);
}

/** Joins from the foreign keys between `st` and the tables already there. */
function autoJoins(st: SpecTable) {
  if (oneTable.value) return;
  const refsTo = (f: ForeignKeyDef, x: SpecTable) => f.ref_table === x.name && (!f.ref_schema || !x.schema || f.ref_schema === x.schema);
  for (const other of spec.tables) {
    if (spec.joins.some((j) => (j.left === other.id && j.right === st.id) || (j.left === st.id && j.right === other.id))) continue;
    const mine = (fks.value.get(tkey(st.schema, st.name)) ?? []).find((f) => refsTo(f, other));
    const theirs = (fks.value.get(tkey(other.schema, other.name)) ?? []).find((f) => refsTo(f, st));
    if (mine) {
      spec.joins.push({ id: newId(), kind: 'inner', left: other.id, right: st.id, on: mine.columns.map((c, i) => ({ left: mine.ref_columns[i] ?? c, right: c })) });
    } else if (theirs) {
      spec.joins.push({ id: newId(), kind: 'inner', left: other.id, right: st.id, on: theirs.columns.map((c, i) => ({ left: c, right: theirs.ref_columns[i] ?? c })) });
    }
  }
}

function removeTable(id: string) {
  spec.tables = spec.tables.filter((x) => x.id !== id);
  spec.joins = spec.joins.filter((j) => j.left !== id && j.right !== id);
  spec.columns = spec.columns.filter((c) => c.table !== id);
}

const renaming = ref<string | null>(null);
function setAlias(st: SpecTable, v: string) {
  renaming.value = null;
  const a = v.trim();
  st.alias = a === st.name ? '' : a;
}

// -- column checkboxes ------------------------------------------------------------------------
function isShown(st: SpecTable, col: string) {
  return spec.columns.some((c) => c.table === st.id && c.column === col && c.show);
}
function newRow(st: SpecTable, col: string): SpecColumn {
  const m = colsOf(st).find((c) => c.name === col);
  return {
    id: newId(), table: st.id, column: col, data_type: m?.data_type ?? '', alias: '', show: true,
    sort: 'none', sort_order: null, aggregate: 'none', filters: [],
  };
}
function toggleColumn(st: SpecTable, col: string) {
  if (isShown(st, col)) {
    spec.columns = spec.columns.filter((c) => !(c.table === st.id && c.column === col));
    return;
  }
  const hidden = spec.columns.find((c) => c.table === st.id && c.column === col);
  if (hidden) hidden.show = true;
  else spec.columns.push(newRow(st, col));
}

// -- canvas ----------------------------------------------------------------------------------
const bounds = () => {
  if (!spec.tables.length) return { x: 0, y: 0, width: 600, height: 300 };
  const x = Math.min(...spec.tables.map((s) => s.x));
  const y = Math.min(...spec.tables.map((s) => s.y));
  const x2 = Math.max(...spec.tables.map((s) => s.x + W));
  const y2 = Math.max(...spec.tables.map((s) => s.y + heightOf(s)));
  return { x, y, width: x2 - x, height: y2 - y };
};
const pz = usePanZoom({ bounds, noPan: '.qb-card, .qb-join-pill', maxFitK: 1 });
const { vp, view, panning, onWheel, onPointerDown, fit } = pz;

/** Client coordinates → world. */
function toWorld(clientX: number, clientY: number) {
  const r = vp.value!.getBoundingClientRect();
  return { x: (clientX - r.left - view.x) / view.k, y: (clientY - r.top - view.y) / view.k };
}

function onCardDown(e: PointerEvent, st: SpecTable) {
  if (e.button !== 0) return;
  const start = { x: e.clientX, y: e.clientY, px: st.x, py: st.y };
  const move = (ev: PointerEvent) => {
    st.x = Math.round(start.px + (ev.clientX - start.x) / view.k);
    st.y = Math.round(start.py + (ev.clientY - start.y) / view.k);
  };
  const up = () => {
    window.removeEventListener('pointermove', move);
    window.removeEventListener('pointerup', up);
    pz.markMoved();
  };
  window.addEventListener('pointermove', move);
  window.addEventListener('pointerup', up);
}

// Drop from the list.
const DRAG_TYPE = 'application/x-dbine-table';
function onListDrag(e: DragEvent, o: DbObject) {
  e.dataTransfer?.setData(DRAG_TYPE, JSON.stringify({ kind: o.kind, schema: o.schema, name: o.name }));
  if (e.dataTransfer) e.dataTransfer.effectAllowed = 'copy';
}
function onDrop(e: DragEvent) {
  const raw = e.dataTransfer?.getData(DRAG_TYPE);
  if (!raw) return;
  const p = toWorld(e.clientX, e.clientY);
  addTable(JSON.parse(raw), { x: p.x - W / 2, y: p.y - HEAD / 2 });
}

// Joins: drag a column onto another.
function anchor(st: SpecTable, col: string, side: 'l' | 'r') {
  const i = colsOf(st).findIndex((c) => c.name === col);
  return { x: side === 'l' ? st.x : st.x + W, y: st.y + HEAD + (i < 0 ? 0 : i + 1) * ROW + ROW / 2 };
}
const link = ref<{ from: SpecTable; col: string; x: number; y: number } | null>(null);
function onColDown(e: PointerEvent, st: SpecTable, col: string) {
  if (e.button !== 0) return;
  const start = { x: e.clientX, y: e.clientY };
  let moved = false;
  const move = (ev: PointerEvent) => {
    if (!moved && Math.abs(ev.clientX - start.x) + Math.abs(ev.clientY - start.y) < 4) return;
    if (!moved && oneTable.value) return;
    moved = true;
    const p = toWorld(ev.clientX, ev.clientY);
    link.value = { from: st, col, x: p.x, y: p.y };
  };
  const up = (ev: PointerEvent) => {
    window.removeEventListener('pointermove', move);
    window.removeEventListener('pointerup', up);
    link.value = null;
    if (!moved) {
      toggleColumn(st, col);
      return;
    }
    const target = (document.elementFromPoint(ev.clientX, ev.clientY) as HTMLElement | null)?.closest<HTMLElement>('[data-qb-col]');
    const to = target && target.dataset.qbCol !== '*' ? tableById(target.dataset.qbTable ?? '') : undefined;
    if (to && to.id !== st.id) addPair(st, col, to, target!.dataset.qbCol!);
  };
  window.addEventListener('pointermove', move);
  window.addEventListener('pointerup', up);
}
function addPair(a: SpecTable, ac: string, b: SpecTable, bc: string) {
  const j = spec.joins.find((x) => (x.left === a.id && x.right === b.id) || (x.left === b.id && x.right === a.id));
  const pair = j?.left === b.id ? { left: bc, right: ac } : { left: ac, right: bc };
  if (j) {
    if (!j.on.some((p) => p.left === pair.left && p.right === pair.right)) j.on.push(pair);
  } else {
    spec.joins.push({ id: newId(), kind: 'inner', left: a.id, right: b.id, on: [pair] });
  }
}

interface Edge { key: string; join: SpecJoin; d: string; mid: { x: number; y: number } | null }
const edges = computed<Edge[]>(() => {
  const out: Edge[] = [];
  for (const j of spec.joins) {
    const a = tableById(j.left);
    const b = tableById(j.right);
    if (!a || !b) continue;
    j.on.forEach((p, i) => {
      let pa, pb, da: number, db: number;
      if (a.x + W + 30 <= b.x) { pa = anchor(a, p.left, 'r'); pb = anchor(b, p.right, 'l'); da = 1; db = -1; }
      else if (b.x + W + 30 <= a.x) { pa = anchor(a, p.left, 'l'); pb = anchor(b, p.right, 'r'); da = -1; db = 1; }
      else { pa = anchor(a, p.left, 'r'); pb = anchor(b, p.right, 'r'); da = db = 1; }
      const c = da !== db ? Math.max(40, Math.abs(pb.x - pa.x) / 2) : 50 + Math.min(60, Math.abs(pb.y - pa.y) * 0.15);
      const d = `M ${pa.x} ${pa.y} C ${pa.x + da * c} ${pa.y} ${pb.x + db * c} ${pb.y} ${pb.x} ${pb.y}`;
      const mid = i === 0 ? { x: (pa.x + pb.x) / 2 + (da === db ? c * 0.75 : 0), y: (pa.y + pb.y) / 2 } : null;
      out.push({ key: `${j.id}:${i}`, join: j, d, mid });
    });
    if (!j.on.length) {
      const pa = { x: a.x + W, y: a.y + HEAD / 2 };
      const pb = { x: b.x, y: b.y + HEAD / 2 };
      out.push({ key: `${j.id}:-`, join: j, d: `M ${pa.x} ${pa.y} L ${pb.x} ${pb.y}`, mid: { x: (pa.x + pb.x) / 2, y: (pa.y + pb.y) / 2 } });
    }
  }
  return out;
});
const linkPath = computed(() => {
  const l = link.value;
  if (!l) return '';
  const p = anchor(l.from, l.col, l.x < l.from.x + W / 2 ? 'l' : 'r');
  return `M ${p.x} ${p.y} L ${l.x} ${l.y}`;
});
const svgBox = computed(() => {
  const b = bounds();
  return { x: b.x - 400, y: b.y - 400, w: b.width + 800, h: b.height + 800 };
});

const JOIN_SHORT: Record<JoinKind, string> = { inner: 'INNER', left: 'LEFT', right: 'RIGHT', full: 'FULL' };
const menu = ref<{ x: number; y: number; items: MenuItem[] } | null>(null);
function onJoinClick(e: MouseEvent, j: SpecJoin) {
  const a = tableById(j.left);
  const b = tableById(j.right);
  const supported = features.value?.joins ?? [];
  const label = (k: JoinKind) => t(`queryBuilder:join.${k}`, { left: a ? aliasOf(a) : '', right: b ? aliasOf(b) : '' });
  menu.value = {
    x: e.clientX, y: e.clientY,
    items: [
      { label: t('queryBuilder:join.title'), header: true },
      ...(['inner', 'left', 'right', 'full'] as JoinKind[])
        .filter((k) => supported.includes(k) || j.kind === k)
        .map((k) => ({ label: label(k), checked: j.kind === k, action: () => { j.kind = k; } })),
      { label: t('queryBuilder:join.swap'), divided: true, action: () => { const l = j.left; j.left = j.right; j.right = l; j.on = j.on.map((p) => ({ left: p.right, right: p.left })); } },
      { label: t('queryBuilder:join.remove'), danger: true, divided: true, action: () => { spec.joins = spec.joins.filter((x) => x.id !== j.id); } },
    ],
  };
}

// -- grid ------------------------------------------------------------------------------------
const rowLabel = (c: SpecColumn) => {
  const st = tableById(c.table);
  return `${st ? aliasOf(st) : '?'}.${c.column}`;
};
function moveRow(i: number, by: number) {
  const j = i + by;
  if (j < 0 || j >= spec.columns.length) return;
  const [r] = spec.columns.splice(i, 1);
  spec.columns.splice(j, 0, r);
}
function removeRow(i: number) {
  spec.columns.splice(i, 1);
}
const addChoice = ref<string>('');
const addOptions = computed(() => spec.tables.map((st) => ({ table: st, cols: ['*', ...colsOf(st).map((c) => c.name)] })));
function addRow(v: string) {
  addChoice.value = '';
  const [table, col] = v.split('\u0001');
  const st = tableById(table);
  if (st && col) spec.columns.push(newRow(st, col));
}

function cell(c: SpecColumn, g: number): Condition | null {
  return c.filters[g] ?? null;
}
function setOp(c: SpecColumn, g: number, op: FilterOp | '') {
  while (c.filters.length <= g) c.filters.push(null);
  c.filters[g] = op ? { ...(c.filters[g] ?? { value: '', value2: '', raw: false }), op } : null;
}
function addGroup() {
  spec.filter_groups = Math.max(1, spec.filter_groups) + 1;
}
function removeGroup() {
  if (spec.filter_groups <= 1) return;
  spec.filter_groups--;
  for (const c of spec.columns) c.filters = c.filters.slice(0, spec.filter_groups);
}
const opLabel = (op: FilterOp) => t(`queryBuilder:op.${op}`);
const noValue = (op: FilterOp) => op === 'is_null' || op === 'is_not_null';

// -- actions ----------------------------------------------------------------------------------
function openInQuery() {
  if (!sql.value) return;
  newQuery(cid, db, sql.value, t('queryBuilder:queryName'));
}

const pane = ref<'sql' | 'preview'>('sql');
const preview = ref<BuiltPreview | null>(null);
const previewError = ref<string | null>(null);
const previewing = ref(false);
async function runPreview() {
  if (!sql.value || previewing.value) return;
  pane.value = 'preview';
  previewing.value = true;
  previewError.value = null;
  try {
    preview.value = await queryBuilderApi.preview(cid, plain(), props.tab.id);
  } catch (e) {
    preview.value = null;
    previewError.value = errorMessage(e);
  } finally {
    previewing.value = false;
  }
}
function cancelPreview() {
  api.cancelQuery(queryBuilderApi.previewKey(props.tab.id)).catch(() => {});
}

// Bottom panel height (drag the splitter).
const bottomH = ref(300);
function onSplitDown(e: PointerEvent) {
  const start = { y: e.clientY, h: bottomH.value };
  const move = (ev: PointerEvent) => { bottomH.value = Math.min(window.innerHeight - 200, Math.max(140, start.h - (ev.clientY - start.y))); };
  const up = () => { window.removeEventListener('pointermove', move); window.removeEventListener('pointerup', up); };
  window.addEventListener('pointermove', move);
  window.addEventListener('pointerup', up);
}
</script>

<template>
  <div class="qb">
    <div class="nm-toolbar qb-bar">
      <strong class="qb-title">{{ $t('queryBuilder:title') }}</strong>
      <span class="nm-muted">{{ tab.database || conns.byId(tab.connectionId)?.name }}</span>
      <el-checkbox v-model="spec.distinct" size="small" :disabled="features ? !features.distinct : false">DISTINCT</el-checkbox>
      <span class="nm-muted">{{ $t('queryBuilder:limit') }}</span>
      <el-input-number
        v-model="spec.limit"
        size="small"
        :min="1"
        :step="100"
        controls-position="right"
        :placeholder="$t('queryBuilder:noLimit')"
        :disabled="features ? !features.limit : false"
        class="qb-limit"
        :value-on-clear="null"
      />
      <div class="nm-spacer" />
      <el-button size="small" :disabled="!sql" @click="openInQuery"><el-icon><ei-document-add /></el-icon>&nbsp;{{ $t('queryBuilder:openInQuery') }}</el-button>
      <el-button v-if="!previewing" size="small" type="primary" :disabled="!sql" @click="runPreview"><el-icon><ei-caret-right /></el-icon>&nbsp;{{ $t('queryBuilder:preview') }}</el-button>
      <el-button v-else size="small" type="danger" @click="cancelPreview">{{ $t('common:cancel') }}</el-button>
    </div>

    <el-alert v-if="connectError" type="error" :title="connectError" :closable="false" class="qb-alert" />

    <div class="qb-top">
      <!-- tables -->
      <aside class="qb-list">
        <el-input v-model="search" size="small" clearable :placeholder="$t('queryBuilder:searchTables')" class="qb-search" />
        <div class="qb-list-body">
          <div v-if="objectsState?.status === 'loading' && !available.length" class="qb-list-empty">
            <el-icon class="is-loading"><ei-loading /></el-icon>
          </div>
          <div v-else-if="!groups.length" class="qb-list-empty">{{ $t('queryBuilder:noTables') }}</div>
          <template v-for="g in groups" :key="g.schema">
            <div v-if="g.schema" class="qb-schema">{{ g.schema }}</div>
            <div
              v-for="o in g.items"
              :key="`${o.kind}:${o.name}`"
              class="qb-item"
              draggable="true"
              :title="`${o.schema ? o.schema + '.' : ''}${o.name} · ${kindLabel(o.kind)}\n${$t('queryBuilder:dragHint')}`"
              @dragstart="onListDrag($event, o)"
              @dblclick="addTable(o)"
            >
              <el-icon :size="13"><ei-view v-if="o.kind === 'view'" /><ei-grid v-else /></el-icon>
              <span>{{ o.name }}</span>
            </div>
          </template>
        </div>
      </aside>

      <!-- canvas -->
      <div
        ref="vp"
        class="qb-canvas"
        :class="{ panning }"
        tabindex="0"
        @wheel="onWheel"
        @pointerdown="onPointerDown"
        @dragover.prevent
        @drop.prevent="onDrop"
      >
        <div class="qb-world" :style="{ transform: `translate(${view.x}px, ${view.y}px) scale(${view.k})` }">
          <svg
            class="qb-edges"
            :style="{ left: svgBox.x + 'px', top: svgBox.y + 'px' }"
            :width="svgBox.w"
            :height="svgBox.h"
            :viewBox="`${svgBox.x} ${svgBox.y} ${svgBox.w} ${svgBox.h}`"
          >
            <g v-for="e in edges" :key="e.key" class="qb-edge" :class="{ dashed: !e.join.on.length }">
              <path :d="e.d" class="qb-edge-line" />
              <path :d="e.d" class="qb-edge-hit" @click="onJoinClick($event, e.join)" />
            </g>
            <path v-if="link" :d="linkPath" class="qb-edge-line qb-link" />
          </svg>
          <template v-for="e in edges" :key="`p-${e.key}`">
            <button
              v-if="e.mid"
              class="qb-join-pill"
              :class="{ warn: features && !features.joins.includes(e.join.kind) }"
              :style="{ left: e.mid.x + 'px', top: e.mid.y + 'px' }"
              :title="$t('queryBuilder:join.hint')"
              @click="onJoinClick($event, e.join)"
            >{{ JOIN_SHORT[e.join.kind] }}</button>
          </template>

          <div
            v-for="st in spec.tables"
            :key="st.id"
            class="qb-card"
            :style="{ left: st.x + 'px', top: st.y + 'px', width: W + 'px' }"
          >
            <div class="qb-head" @pointerdown.stop="onCardDown($event, st)" @dblclick="renaming = st.id">
              <input
                v-if="renaming === st.id"
                class="qb-alias-input"
                :value="aliasOf(st)"
                :aria-label="$t('queryBuilder:alias')"
                autofocus
                @pointerdown.stop
                @keydown.enter="setAlias(st, ($event.target as HTMLInputElement).value)"
                @keydown.esc="renaming = null"
                @blur="setAlias(st, ($event.target as HTMLInputElement).value)"
              />
              <span v-else class="qb-name" :title="`${st.schema ? st.schema + '.' : ''}${st.name}\n${$t('queryBuilder:aliasHint')}`">
                <span v-if="st.schema" class="qb-sch">{{ st.schema }}.</span>{{ st.name }}
                <em v-if="st.alias" class="qb-alias">{{ st.alias }}</em>
              </span>
              <button class="qb-x" :title="$t('queryBuilder:removeTable')" @pointerdown.stop @click="removeTable(st.id)"><el-icon><ei-close /></el-icon></button>
            </div>
            <div class="qb-row qb-star" data-qb-col="*" :data-qb-table="st.id" @pointerdown.stop>
              <el-checkbox size="small" :model-value="isShown(st, '*')" @change="toggleColumn(st, '*')" />
              <span class="qb-col" @click="toggleColumn(st, '*')">{{ $t('queryBuilder:allColumns') }}</span>
            </div>
            <div
              v-for="c in colsOf(st)"
              :key="c.name"
              class="qb-row"
              :class="{ pk: c.pk }"
              :data-qb-col="c.name"
              :data-qb-table="st.id"
              @pointerdown.stop
            >
              <el-checkbox size="small" :model-value="isShown(st, c.name)" @change="toggleColumn(st, c.name)" />
              <span class="qb-col" :title="oneTable ? c.name : $t('queryBuilder:joinHint')" @pointerdown.stop="onColDown($event, st, c.name)">
                <span v-if="c.pk" class="qb-flag pk">PK</span><span v-else-if="c.fk" class="qb-flag fk">FK</span>{{ c.name }}
              </span>
              <span class="qb-type" :title="c.data_type">{{ c.data_type }}</span>
            </div>
          </div>
        </div>
        <div v-if="!spec.tables.length" class="qb-empty">
          <el-icon class="qb-empty-ic"><ei-set-up /></el-icon>
          <strong>{{ $t('queryBuilder:empty.title') }}</strong>
          <span>{{ oneTable ? $t('queryBuilder:empty.oneTable') : $t('queryBuilder:empty.text') }}</span>
        </div>
        <div v-if="spec.tables.length" class="qb-zoom" @pointerdown.stop>
          <button :title="$t('diagram:zoom.fit')" @click="fit"><el-icon><ei-full-screen /></el-icon></button>
          <span class="qb-zoom-pct">{{ Math.round(view.k * 100) }}%</span>
        </div>
      </div>
    </div>

    <div class="qb-split" @pointerdown="onSplitDown" />

    <div class="qb-bottom" :style="{ height: bottomH + 'px' }">
      <!-- grid -->
      <div class="qb-grid-wrap">
        <table class="qb-grid">
          <thead>
            <tr>
              <th>{{ $t('queryBuilder:grid.column') }}</th>
              <th>{{ $t('queryBuilder:grid.alias') }}</th>
              <th>{{ $t('queryBuilder:grid.show') }}</th>
              <th>{{ $t('queryBuilder:grid.aggregate') }}</th>
              <th>{{ $t('queryBuilder:grid.sort') }}</th>
              <th :title="$t('queryBuilder:grid.sortOrderTip')">{{ $t('queryBuilder:grid.sortOrder') }}</th>
              <th v-for="g in spec.filter_groups" :key="g" class="qb-fh">
                {{ g === 1 ? $t('queryBuilder:grid.filter') : $t('queryBuilder:grid.or') }}
                <button v-if="g === spec.filter_groups && g > 1" class="qb-mini" :title="$t('queryBuilder:grid.removeOr')" @click="removeGroup"><el-icon><ei-minus /></el-icon></button>
                <button v-if="g === spec.filter_groups && (features?.or_groups ?? true)" class="qb-mini" :title="$t('queryBuilder:grid.addOr')" @click="addGroup"><el-icon><ei-plus /></el-icon></button>
              </th>
              <th />
            </tr>
          </thead>
          <tbody>
            <tr v-for="(c, i) in spec.columns" :key="c.id">
              <td class="qb-cname" :title="c.data_type">{{ rowLabel(c) }}</td>
              <td><el-input v-model="c.alias" size="small" :disabled="c.column === '*' && c.aggregate === 'none'" /></td>
              <td class="qb-center"><el-checkbox v-model="c.show" size="small" /></td>
              <td>
                <el-select v-model="c.aggregate" size="small" class="qb-agg">
                  <el-option v-for="a in aggregates" :key="a" :value="a" :label="$t(`queryBuilder:agg.${a}`)" />
                </el-select>
              </td>
              <td>
                <el-select v-model="c.sort" size="small" class="qb-sort" :disabled="c.column === '*' || (features ? !features.order_by : false)">
                  <el-option value="none" :label="$t('queryBuilder:sort.none')" />
                  <el-option value="asc" :label="$t('queryBuilder:sort.asc')" />
                  <el-option value="desc" :label="$t('queryBuilder:sort.desc')" />
                </el-select>
              </td>
              <td>
                <el-input-number v-model="c.sort_order" size="small" :min="1" :controls="false" class="qb-num" :disabled="c.sort === 'none'" :value-on-clear="null" />
              </td>
              <td v-for="g in spec.filter_groups" :key="g" class="qb-filter">
                <el-select
                  :model-value="cell(c, g - 1)?.op ?? ''"
                  size="small"
                  clearable
                  class="qb-op"
                  :placeholder="'—'"
                  :disabled="c.column === '*'"
                  @update:model-value="(v: FilterOp | '') => setOp(c, g - 1, v ?? '')"
                >
                  <el-option v-for="op in operators" :key="op" :value="op" :label="opLabel(op)" />
                </el-select>
                <template v-if="cell(c, g - 1) && !noValue(cell(c, g - 1)!.op)">
                  <el-input v-model="cell(c, g - 1)!.value" size="small" class="qb-val" :placeholder="cell(c, g - 1)!.op.endsWith('in') ? $t('queryBuilder:listHint') : ''" />
                  <el-input v-if="cell(c, g - 1)!.op === 'between'" v-model="cell(c, g - 1)!.value2" size="small" class="qb-val" />
                  <button
                    class="qb-mini qb-raw"
                    :class="{ on: cell(c, g - 1)!.raw }"
                    :title="$t('queryBuilder:rawTip')"
                    @click="cell(c, g - 1)!.raw = !cell(c, g - 1)!.raw"
                  >ƒx</button>
                </template>
              </td>
              <td class="qb-acts">
                <button class="qb-mini" :title="$t('queryBuilder:grid.up')" :disabled="i === 0" @click="moveRow(i, -1)"><el-icon><ei-arrow-up /></el-icon></button>
                <button class="qb-mini" :title="$t('queryBuilder:grid.down')" :disabled="i === spec.columns.length - 1" @click="moveRow(i, 1)"><el-icon><ei-arrow-down /></el-icon></button>
                <button class="qb-mini" :title="$t('queryBuilder:grid.remove')" @click="removeRow(i)"><el-icon><ei-close /></el-icon></button>
              </td>
            </tr>
          </tbody>
        </table>
        <div class="qb-add">
          <el-select
            v-model="addChoice"
            size="small"
            filterable
            :placeholder="$t('queryBuilder:grid.addRow')"
            :disabled="!spec.tables.length"
            class="qb-add-select"
            @change="addRow"
          >
            <el-option-group v-for="o in addOptions" :key="o.table.id" :label="aliasOf(o.table)">
              <el-option v-for="col in o.cols" :key="col" :value="`${o.table.id}\u0001${col}`" :label="`${aliasOf(o.table)}.${col}`" />
            </el-option-group>
          </el-select>
          <span v-if="!spec.columns.length && spec.tables.length" class="nm-muted">{{ $t('queryBuilder:grid.empty') }}</span>
        </div>
      </div>

      <!-- SQL / preview -->
      <div class="qb-side">
        <div class="qb-seg">
          <button :class="{ on: pane === 'sql' }" @click="pane = 'sql'">SQL</button>
          <button :class="{ on: pane === 'preview' }" @click="pane = 'preview'">{{ $t('queryBuilder:previewTab') }}</button>
          <span v-if="pane === 'preview' && preview" class="nm-muted qb-meta">
            {{ $t('queryBuilder:rows', { count: preview.rows.length }) }}{{ preview.truncated ? ` · ${$t('queryBuilder:truncated')}` : '' }} · {{ preview.elapsed_ms }} ms
          </span>
        </div>
        <template v-if="pane === 'sql'">
          <el-alert v-if="buildError" type="error" :title="tb(buildError)" :closable="false" class="qb-alert" />
          <div v-if="warnings.length" class="qb-warn">
            <div v-for="w in warnings" :key="w"><el-icon><ei-warning-filled /></el-icon>{{ tb(w) }}</div>
          </div>
          <div class="qb-sql">
            <CodeEditor :model-value="sql" read-only :language="driver?.language ?? 'sql'" :dialect="driver?.dialect ?? ''" :placeholder="$t('queryBuilder:sqlPlaceholder')" />
          </div>
        </template>
        <template v-else>
          <div v-if="previewing" class="qb-list-empty"><el-icon class="is-loading"><ei-loading /></el-icon>&nbsp;{{ $t('queryBuilder:running') }}</div>
          <el-alert v-else-if="previewError" type="error" :title="tb(previewError)" :closable="false" class="qb-alert" />
          <div v-else-if="preview" class="qb-result">
            <ResultGrid v-if="preview.columns.length" :columns="preview.columns" :rows="preview.rows" />
            <div v-else class="qb-list-empty">{{ $t('queryBuilder:noRows') }}</div>
          </div>
          <div v-else class="qb-list-empty">{{ $t('queryBuilder:previewHint') }}</div>
        </template>
      </div>
    </div>

    <ContextMenu v-if="menu" :x="menu.x" :y="menu.y" :items="menu.items" @close="menu = null" />
  </div>
</template>

<style scoped>
.qb {
  --qb-card: #232428;
  --qb-head: #2a2c31;
  --qb-line: #34363c;
  --qb-edge: #75beff;
  display: flex; flex-direction: column; height: 100%; min-height: 0;
}
.qb-bar { flex-shrink: 0; gap: 10px; }
.qb-title { font-size: 12.5px; color: var(--nm-text-strong); }
.qb-limit { width: 120px; }
.qb-alert { margin: 8px 12px; width: auto; }

.qb-top { flex: 1; min-height: 120px; display: flex; }
.qb-list { width: 220px; flex-shrink: 0; display: flex; flex-direction: column; border-right: 1px solid var(--nm-border); background: var(--ide-sidebar); }
.qb-search { margin: 8px; width: auto; }
.qb-list-body { flex: 1; overflow: auto; padding-bottom: 8px; }
.qb-list-empty { display: flex; align-items: center; justify-content: center; padding: 16px; color: var(--nm-text-dim); font-size: 12px; text-align: center; }
.qb-schema { padding: 6px 10px 2px; font-size: 10.5px; font-weight: 600; letter-spacing: 0.06em; text-transform: uppercase; color: var(--nm-text-dim); }
.qb-item { display: flex; align-items: center; gap: 6px; padding: 2px 12px; font-size: 12.5px; color: var(--nm-text); cursor: grab; white-space: nowrap; }
.qb-item span { overflow: hidden; text-overflow: ellipsis; }
.qb-item:hover { background: var(--ide-hover, rgba(255, 255, 255, 0.05)); }

.qb-canvas {
  position: relative; flex: 1; min-width: 0; overflow: hidden; outline: none; cursor: grab;
  background-color: #1b1c1f;
  background-image: radial-gradient(circle, #2c2e33 1px, transparent 1.2px);
  background-size: 20px 20px;
}
.qb-canvas.panning { cursor: grabbing; }
.qb-world { position: absolute; left: 0; top: 0; transform-origin: 0 0; }
.qb-edges { position: absolute; overflow: visible; pointer-events: none; }
.qb-edge-line { fill: none; stroke: var(--qb-edge); stroke-width: 1.5; opacity: 0.8; }
.qb-edge.dashed .qb-edge-line { stroke-dasharray: 5 4; stroke: #e8a33d; }
.qb-edge-hit { fill: none; stroke: transparent; stroke-width: 12; pointer-events: stroke; cursor: pointer; }
.qb-edge:hover .qb-edge-line { opacity: 1; stroke-width: 2.2; }
.qb-link { stroke-dasharray: 4 3; opacity: 1; }
.qb-join-pill {
  position: absolute; transform: translate(-50%, -50%); z-index: 2; padding: 0 6px; height: 18px; border-radius: 9px;
  font: inherit; font-size: 10px; font-weight: 600; letter-spacing: 0.04em; cursor: pointer;
  color: #cfe6ff; background: #1f3550; border: 1px solid #3a6ea5;
}
.qb-join-pill.warn { color: #ffd59a; background: #4a3416; border-color: #a8772c; }

.qb-card {
  position: absolute; z-index: 1; display: flex; flex-direction: column; overflow: hidden; padding-bottom: 4px;
  background: var(--qb-card); border: 1px solid var(--qb-line); border-radius: 8px;
  box-shadow: 0 1px 2px rgba(0, 0, 0, 0.35), 0 4px 14px rgba(0, 0, 0, 0.22); cursor: default;
}
.qb-head {
  flex-shrink: 0; display: flex; align-items: center; gap: 6px; height: 30px; padding: 0 6px 0 10px;
  background: var(--qb-head); border-bottom: 1px solid var(--qb-line); cursor: move;
}
.qb-name { flex: 1; min-width: 0; font-size: 12.5px; font-weight: 600; color: #eceef1; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.qb-sch { font-weight: 400; color: #8b9098; }
.qb-alias { font-style: normal; font-weight: 400; margin-left: 6px; color: #75beff; }
.qb-alias-input { flex: 1; min-width: 0; font: inherit; font-size: 12px; padding: 2px 6px; color: var(--nm-text-strong); background: #1b1c1f; border: 1px solid var(--ide-focus); border-radius: 4px; outline: none; }
.qb-x { display: inline-flex; border: none; background: transparent; color: #8b9098; cursor: pointer; padding: 2px; border-radius: 4px; }
.qb-x:hover { color: #fff; background: rgba(255, 255, 255, 0.08); }
.qb-row { flex-shrink: 0; display: flex; align-items: center; gap: 6px; height: 22px; padding: 0 8px; font-size: 12px; color: var(--nm-text); }
.qb-row :deep(.el-checkbox) { height: 18px; }
.qb-row:hover { background: rgba(55, 148, 255, 0.1); }
.qb-row.pk .qb-col { color: #eceef1; font-weight: 600; }
.qb-star .qb-col { color: var(--nm-text-dim); font-style: italic; cursor: pointer; }
.qb-col { flex: 1; min-width: 0; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; cursor: crosshair; user-select: none; }
.qb-type { flex: 0 0 auto; max-width: 45%; font-family: var(--nm-mono); font-size: 10.5px; color: #7f848c; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.qb-flag { display: inline-block; font-size: 9px; font-weight: 700; line-height: 13px; padding: 0 3px; margin-right: 4px; border-radius: 3px; }
.qb-flag.pk { background: rgba(226, 192, 141, 0.16); color: #e2c08d; }
.qb-flag.fk { background: rgba(117, 190, 255, 0.14); color: #75beff; }

.qb-empty {
  position: absolute; inset: 0; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 6px;
  color: var(--nm-text-dim); font-size: 12.5px; pointer-events: none; text-align: center; padding: 0 24px;
}
.qb-empty strong { color: var(--nm-text-strong); font-size: 13.5px; font-weight: 600; }
.qb-empty-ic { font-size: 34px; color: #4a4d55; margin-bottom: 6px; }
.qb-zoom {
  position: absolute; left: 12px; bottom: 12px; display: flex; align-items: center; gap: 4px; padding: 3px;
  background: rgba(37, 37, 38, 0.92); border: 1px solid #3c3c3c; border-radius: 8px;
}
.qb-zoom button { display: inline-flex; align-items: center; justify-content: center; height: 24px; min-width: 24px; border: none; border-radius: 5px; background: transparent; color: var(--nm-text); cursor: pointer; }
.qb-zoom button:hover { background: rgba(255, 255, 255, 0.08); }
.qb-zoom-pct { font-size: 11.5px; color: var(--nm-text-dim); padding: 0 6px; font-variant-numeric: tabular-nums; }

.qb-split { flex-shrink: 0; height: 5px; cursor: row-resize; background: var(--nm-border-soft); }
.qb-split:hover { background: var(--ide-focus); }
.qb-bottom { flex-shrink: 0; display: flex; min-height: 0; background: var(--ide-editor); }
.qb-grid-wrap { flex: 1; min-width: 0; overflow: auto; padding: 6px 8px; }
.qb-grid { border-collapse: collapse; font-size: 12px; }
.qb-grid th { position: sticky; top: 0; z-index: 1; background: var(--ide-editor); text-align: left; font-weight: 600; font-size: 11px; color: var(--nm-text-dim); padding: 4px 6px; white-space: nowrap; border-bottom: 1px solid var(--nm-border); }
.qb-grid td { padding: 3px 6px; border-bottom: 1px solid var(--nm-border-soft); vertical-align: middle; white-space: nowrap; }
.qb-cname { font-family: var(--nm-mono); font-size: 11.5px; color: var(--nm-text-strong); }
.qb-center { text-align: center; }
.qb-grid :deep(.el-input) { width: 120px; }
.qb-agg { width: 140px; }
.qb-sort { width: 110px; }
.qb-num { width: 56px; }
.qb-filter { display: flex; align-items: center; gap: 4px; }
.qb-op { width: 120px; }
.qb-filter :deep(.el-input.qb-val) { width: 110px; }
.qb-fh { white-space: nowrap; }
.qb-mini { display: inline-flex; align-items: center; justify-content: center; height: 20px; min-width: 20px; padding: 0 4px; border: none; border-radius: 4px; background: transparent; color: var(--nm-text-dim); cursor: pointer; font: inherit; font-size: 11px; vertical-align: middle; }
.qb-mini:hover:not(:disabled) { color: var(--nm-text-strong); background: rgba(255, 255, 255, 0.08); }
.qb-mini:disabled { opacity: 0.35; cursor: default; }
.qb-raw { font-style: italic; font-weight: 600; }
.qb-raw.on { color: #75beff; background: rgba(55, 148, 255, 0.16); }
.qb-acts { text-align: right; }
.qb-add { display: flex; align-items: center; gap: 10px; padding: 8px 0; }
.qb-add-select { width: 260px; }

.qb-side { width: 42%; min-width: 300px; flex-shrink: 0; display: flex; flex-direction: column; border-left: 1px solid var(--nm-border); min-height: 0; }
.qb-seg { flex-shrink: 0; display: flex; align-items: center; gap: 0; padding: 6px 8px; border-bottom: 1px solid var(--nm-border-soft); }
.qb-seg button { padding: 3px 12px; font: inherit; font-size: 12px; border: 1px solid var(--nm-border); background: transparent; color: var(--nm-text-dim); cursor: pointer; }
.qb-seg button:first-child { border-radius: 4px 0 0 4px; }
.qb-seg button:nth-child(2) { border-radius: 0 4px 4px 0; border-left: none; }
.qb-seg button.on { background: var(--ide-selection); color: var(--nm-text-strong); }
.qb-meta { margin-left: 10px; }
.qb-warn { flex-shrink: 0; display: flex; flex-direction: column; gap: 3px; padding: 6px 10px; font-size: 11.5px; color: #e8c37d; border-bottom: 1px solid var(--nm-border-soft); }
.qb-warn > div { display: flex; align-items: flex-start; gap: 6px; }
.qb-warn .el-icon { flex-shrink: 0; margin-top: 2px; }
.qb-sql { flex: 1; min-height: 0; overflow: hidden; }
.qb-sql > :deep(*) { height: 100%; }
.qb-result { flex: 1; min-height: 0; display: flex; flex-direction: column; }
.qb-result > :deep(*) { flex: 1; min-height: 0; }
</style>
