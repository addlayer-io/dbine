<script setup lang="ts">
import { computed, defineAsyncComponent, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { listOf, locale } from '../i18n';
import { tb } from '../i18n/backend';
import { save } from '@tauri-apps/plugin-dialog';
import { api, errorMessage } from '../api/client';
import type { Cell, Message, MessageLevel, ObjectRef, QueryOutcome, ResultColumn, SortKey, StatementResult } from '../api/types';
import { sortRows } from '../composables/gridSort';
import { buildChanges, buildInserts, inferColumnSources, inferTarget, resolveEditing, rowKey, skippedKeyColumns, type EditSetup, type Edits } from '../composables/gridEdit';
import ResultGrid from './ResultGrid.vue';
import AddDocumentDialog from './AddDocumentDialog.vue';
import JsonTreeView from './JsonTreeView.vue';
import PlanView from './PlanView.vue';
import ChartView from './ChartView.vue';
import type { AggregateInfo, ChartPoints, ChartSettings, ChartState, MapSettings } from '../composables/chartSettings';
import { detectSpatial } from '../composables/geo';
import { wholeChartByDefault } from '../composables/chartWhole';
// Leaflet and the base map load only when a map is shown.
const MapView = defineAsyncComponent(() => import('./MapView.vue'));
import ExportDialog from './ExportDialog.vue';
import type { FilterState } from '../composables/gridFilter';
import { EXPORT_FORMATS, defaultOptions, exportApi, fileName, type ExportFormat } from '../composables/export';
import { inCodeEditor, isSaveShortcut, modalOpen } from '../composables/shortcuts';
import { bindingLabel, registerCommand } from '../composables/commands';
import { inFocusedEditor } from '../stores/editorGroups';
import { startTask, useTasksStore, type TaskHandle } from '../stores/tasks';
import { useConnectionsStore } from '../stores/connections';
import { jumpFilters, linksFor, openFiltered, relationsSchema, type FkLink, type GridLinks } from '../composables/fkJump';

// What a run produced: one sub-tab per result set, "Plan de ejecución" when
// the run asked for plans, and "Mensajes" (server messages, affected rows,
// the error). Opens on the error, else the plan when there's no data.

const props = defineProps<{
  outcome: QueryOutcome | null;
  running: boolean;
  /** What produced the outcome, so an export can run it again for every row
   *  (not a DataFrame SQL cell's, `notebookTab`: only its chart uses it). */
  source?: { connectionId: string; database: string; sql: string; notebookTab?: string } | null;
  /** Base name for exported files. */
  title?: string;
  /** Driver's SQL dialect (identifier quoting of SQL exports). */
  dialect?: string;
  /** The table the rows come from, when known (copy as SQL / Mongo). */
  table?: { schema: string | null; name: string } | null;
  /** Its key columns (copy as SQL UPDATEs). */
  keyColumns?: string[];
  /** Where edited cells' UPDATE code comes from: the tab's table, or the
   *  query (`script`) the rows came from. */
  editSource?: { connectionId: string; database: string; language: string; target?: ObjectRef | null; script?: string } | null;
  /** Column filters under the headers (a table's data). */
  filterable?: boolean;
  filters?: Record<string, FilterState>;
  /** A table's data: the parent sorts (on the server) and this is its sort;
   *  a header click emits `sort`. Absent (a query's results): a header
   *  click sorts the loaded rows here. */
  sort?: SortKey[];
  /** The editor's line of the script's first line, minus one: messages then
   *  show "Línea N" and a click on one emits `goto`. Absent: no lines. */
  lineOffset?: number | null;
  /** The parent shows the run's status (time, rows) itself. */
  hideStatus?: boolean;
  /** Sub-tab names by result index (a run on several databases: the merged
   *  grid, or each database's result). Absent: "Resultado N". */
  labels?: string[] | null;
  /** The view to start in (a notebook cell's saved one). Absent: the grid,
   *  or the JSON tree a document engine remembers. */
  initialView?: View;
  /** Chart settings to start from (by column name); `update:chartSettings`
   *  emits the user's changes with the plotted points. */
  chartSettings?: ChartSettings | null;
  /** Points kept with those settings (a whole-result aggregate) and how they were computed. */
  chartPoints?: ChartPoints | null;
  chartAggregate?: AggregateInfo | null;
  /** Changes when the chart shown is another one (a notebook cell's charts). */
  chartKey?: string | number;
  /** The map view's source to start from (by column name). */
  mapSettings?: MapSettings | null;
}>();
const emit = defineEmits<{
  'update:view': [view: View];
  'update:chartSettings': [state: ChartState];
  'update:mapSettings': [settings: MapSettings];
  /** "Agregar a la query": the generated DELETE / UPDATE code. */
  script: [code: string];
  filter: [column: string, state: FilterState | null];
  /** A header click on a table's data (see the `sort` prop). */
  sort: [keys: SortKey[]];
  /** The edits were saved (run) on the server: a table's data reloads. */
  applied: [];
  /** A message's line was clicked: where it is in the script that ran
   *  (`offset`, a JS index, when known; `line` 1-based). */
  goto: [at: { offset: number | null; line: number | null }];
}>();

const { t } = useTranslation();

const sets = computed(() => (props.outcome?.results ?? []).map((r, i) => ({ ...r, i })).filter((r) => r.columns.length));
const active = ref<number | 'messages' | 'plan'>(0);
/** How the current result set is shown. Document engines remember table or
 *  JSON tree between results (a per-viewer preference). */
export type View = 'table' | 'chart' | 'tree' | 'map';
const VIEW_KEY = 'dbine.resultsView.json';
const documents = computed(() => props.editSource?.language === 'json');
function savedView(): View {
  try { return documents.value && localStorage.getItem(VIEW_KEY) === 'tree' ? 'tree' : 'table'; } catch { return 'table'; }
}
const show = ref<View>(props.initialView ?? savedView());
// A query tab knows its engine once a script ran: the saved view applies then.
watch(documents, (d) => { if (d && show.value === 'table' && !props.initialView) show.value = savedView(); });
function setView(v: View) {
  if (show.value !== v) emit('update:view', v);
  show.value = v;
  if (documents.value && v !== 'chart' && v !== 'map') {
    try { localStorage.setItem(VIEW_KEY, v); } catch { /* storage off */ }
  }
}
/** The user picked a sub-tab during this outcome. */
const picked = ref(false);
function pick(v: number | 'messages' | 'plan') {
  active.value = v;
  picked.value = true;
}
// A live run (statement by statement) starts on the messages; the first
// result set to arrive takes over unless the user chose a sub-tab.
watch(() => sets.value.length, (n, old) => {
  if (props.running && !old && n && active.value === 'messages' && !picked.value) active.value = sets.value[0].i;
});

watch(
  () => props.outcome,
  (o) => {
    picked.value = false;
    if (!o) return;
    active.value = o.error && !o.plans.length ? 'messages'
      : o.plans.length && !sets.value.length ? 'plan'
      : sets.value.length ? sets.value[0].i
      : 'messages';
  },
);

const current = computed(() => (typeof active.value === 'number' ? props.outcome?.results[active.value] ?? null : null));
/** The result's spatial columns (or a latitude / longitude pair): the "Mapa"
 *  view is offered only when there's one. */
const spatial = computed(() => {
  const c = current.value;
  if (!c?.rows.length) return null;
  const d = detectSpatial(c.columns, c.rows);
  return d.geometry.length || d.lat !== null ? d : null;
});
/** The map view asked for on a result without anything to map: the grid. */
const view = computed<View>(() => (show.value === 'map' && !spatial.value ? 'table' : show.value));

// -- editing (the changes become DELETE / UPDATE code; nothing runs) --------------------
const edits = reactive<Record<number, Edits>>({});
/** Rows marked for deletion, per result set. */
const deletes = reactive<Record<number, Set<number>>>({});
/** New rows at the end of the grid, per result set: column index → value
 *  (cells left out take the column's default). */
const added = reactive<Record<number, Record<number, Cell>[]>>({});
/** Documents (or rows) typed in "Agregar documento", pending. */
const docs = ref<Record<string, unknown>[]>([]);
watch(() => props.outcome, () => {
  for (const k of Object.keys(edits)) delete edits[Number(k)];
  for (const k of Object.keys(deletes)) delete deletes[Number(k)];
  for (const k of Object.keys(added)) delete added[Number(k)];
  docs.value = [];
});
const currentEdits = computed(() => (typeof active.value === 'number' ? edits[active.value] ?? {} : {}));
const currentDeletes = computed(() => (typeof active.value === 'number' ? deletes[active.value] ?? new Set<number>() : new Set<number>()));
/** The edits that count: a row marked for deletion drops its own. */
const liveEdits = computed(() => Object.entries(currentEdits.value).filter(([r]) => !currentDeletes.value.has(Number(r))));
const editCount = computed(() => liveEdits.value.reduce((n, [, r]) => n + Object.keys(r).length, 0));
const editRows = computed(() => liveEdits.value.length);
const deleteCount = computed(() => currentDeletes.value.size);
const currentAdded = computed(() => (typeof active.value === 'number' ? added[active.value] ?? [] : []));
const newCount = computed(() => currentAdded.value.length + docs.value.length);
const pending = computed(() => editCount.value + deleteCount.value + newCount.value > 0);
// -- sort ------------------------------------------------------------------------------
/** A query's results sorted on the loaded rows, per result set. */
const localSort = reactive<Record<number, SortKey[]>>({});
/** Each result set's rows in the order they arrived ("no sort" again). */
const arrived = new WeakMap<object, Cell[][]>();
/** Bumped on each local sort: the rows are replaced in the result object,
 *  which needn't be reactive. */
const sortTick = ref(0);
watch(() => props.outcome, () => {
  for (const k of Object.keys(localSort)) delete localSort[Number(k)];
});
const gridSort = computed(() => props.sort ?? (typeof active.value === 'number' ? localSort[active.value] ?? [] : []));
/** Sorted here, on the loaded rows of a cut result: the banner says so. */
const sortedLoaded = computed(() => !props.sort && !!current.value?.truncated && gridSort.value.length > 0);
function onSort(keys: SortKey[]) {
  if (props.sort) { emit('sort', keys); return; }
  const r = current.value;
  const i = active.value;
  if (!r || typeof i !== 'number') return;
  if (!arrived.has(r.rows)) arrived.set(r.rows, r.rows.slice());
  const first = arrived.get(r.rows)!;
  const before = r.rows;
  let after: Cell[][];
  if (keys.length) {
    after = sortRows(before, r.columns, keys);
  } else {
    // Back to the arrival order; rows that came later (saved inserts) stay last.
    const live = new Set(before);
    const known = new Set(first);
    after = first.filter((row) => live.has(row)).concat(before.filter((row) => !known.has(row)));
  }
  arrived.set(after, first);
  // Pending edits and deletions follow their rows; new rows stay at the end.
  const at = new Map(after.map((row, k) => [row, k]));
  const move = (k: number) => (k < before.length ? at.get(before[k]) ?? k : k);
  if (edits[i]) edits[i] = Object.fromEntries(Object.entries(edits[i]).map(([k, v]) => [move(Number(k)), v]));
  if (deletes[i]) deletes[i] = new Set([...deletes[i]].map(move));
  r.rows = after;
  localSort[i] = keys;
  sortTick.value++;
}

/** The grid's rows: the result's, then the new ones (blank: the grid shows
 *  their cells as the column default until set). */
const gridRows = computed(() => {
  void sortTick.value;
  const r = current.value;
  if (!r) return [];
  const n = currentAdded.value.length;
  if (!n) return r.rows;
  const blank: Cell[] = r.columns.map(() => null);
  return r.rows.concat(Array.from({ length: n }, () => blank));
});
/** The edits the grid shows: the result rows' and the new rows' values. */
const gridEdits = computed<Edits>(() => {
  const n = currentAdded.value.length;
  if (!n || !current.value) return currentEdits.value;
  const base = current.value.rows.length;
  const out: Edits = { ...currentEdits.value };
  currentAdded.value.forEach((cells, k) => { out[base + k] = cells; });
  return out;
});

/** How the current result can be edited, or why not. */
const setup = ref<EditSetup | string | null>(null);
let setupSeq = 0;
// Keyed by content: the parent builds `editSource` inline (a new object each
// render), watching its identity would loop.
const editKey = computed(() => {
  const e = props.editSource;
  return e ? JSON.stringify([e.connectionId, e.database, e.language, e.target ?? null, e.script ?? '']) : '';
});
watch(
  () => [editKey.value, active.value, props.outcome] as const,
  async () => {
    const seq = ++setupSeq;
    setup.value = null;
    const src = props.editSource;
    const r = current.value;
    if (!src || !r || typeof active.value !== 'number') return;
    let target = src.target ?? null;
    if (!target) {
      const pos = sets.value.findIndex((x) => x.i === active.value);
      const inferred = inferTarget(src.script ?? '', pos, src.language);
      if (!inferred.target) { setup.value = inferred.reason; return; }
      target = inferred.target;
    }
    try {
      const res = await resolveEditing(src.connectionId, src.database, target, r.columns.map((c) => c.name));
      if (seq === setupSeq) setup.value = res;
    } catch (e) {
      if (seq === setupSeq) setup.value = errorMessage(e);
    }
  },
  { immediate: true },
);
// -- jumps by foreign key (composables/fkJump) ---------------------------------------------
// Where the result's table is known (a table's data, or a query on one
// table whose columns map to its own) and the engine reports foreign keys.
const conns = useConnectionsStore();
const links = ref<GridLinks | null>(null);
let linksSeq = 0;
watch(
  () => [editKey.value, active.value, props.outcome] as const,
  async () => {
    const seq = ++linksSeq;
    links.value = null;
    const src = props.editSource;
    const r = current.value;
    if (!src || !r || typeof active.value !== 'number') return;
    if (!conns.driverOf(src.connectionId)?.capabilities.foreign_keys) return;
    let target = src.target ?? null;
    let source: ((name: string) => string | null) | null = (n) => n;
    if (!target) {
      const pos = sets.value.findIndex((x) => x.i === active.value);
      target = inferTarget(src.script ?? '', pos, src.language).target;
      source = inferColumnSources(src.script ?? '', pos);
    }
    if (!target || !source) return;
    try {
      const tables = await relationsSchema(src.connectionId, src.database);
      if (seq !== linksSeq) return;
      const found = linksFor(tables, target, r.columns.map((c) => c.name), source);
      links.value = found.parents.length || found.children.length ? found : null;
    } catch {
      // No relations to offer: the grid works as before.
    }
  },
  { immediate: true },
);
function follow(link: FkLink, row: number) {
  const src = props.editSource;
  const values = link.cols.map((c) => current.value?.rows[row]?.[c] ?? null);
  const filters = jumpFilters(link, values);
  if (!src || !filters) return;
  openFiltered(src.connectionId, src.database, link.table, filters);
}

const canEdit = computed(() => !!setup.value && typeof setup.value !== 'string');
const noEditReason = computed(() => (typeof setup.value === 'string' ? setup.value : props.editSource ? t('results:edit.lookingUpTable') : null));

// Whether the engine writes DELETEs (Drill, ksqlDB, InfluxDB with Flux or v3
// SQL don't): asked once per table with no rows, the error is the reason.
const deleteReason = ref<string | null>(null);
let deleteSeq = 0;
watch(
  () => (canEdit.value && props.editSource ? JSON.stringify([props.editSource.connectionId, (setup.value as EditSetup).target]) : ''),
  async (k) => {
    const seq = ++deleteSeq;
    deleteReason.value = null;
    const s = setup.value;
    const src = props.editSource;
    if (!k || !s || typeof s === 'string' || !src) return;
    try {
      await invoke<string>('update_script', { args: { connection_id: src.connectionId, target: s.target, changes: [], deletes: [] } });
    } catch (e) {
      if (seq === deleteSeq) deleteReason.value = errorMessage(e);
    }
  },
  { immediate: true },
);
const canDelete = computed(() => canEdit.value && !deleteReason.value);

// -- new rows / documents (INSERT code: the driver's `insert_script`) --------------------
/** Where new rows go: the edit setup's table, or the tab's own object (an
 *  empty collection has no result columns, so no setup). */
const insertTarget = computed<ObjectRef | null>(() => {
  const s = setup.value;
  if (s && typeof s === 'object') return s.target;
  return props.editSource?.target ?? null;
});
/** Document engines add documents as JSON; a result with no columns too. */
const addsDocuments = computed(() => props.editSource?.language === 'json' || !current.value);
// Whether the engine writes INSERTs (InfluxDB doesn't): asked once per
// table with no rows, the error is the reason.
const insertReason = ref<string | null>(null);
let insertSeq = 0;
watch(
  () => (insertTarget.value && props.editSource ? JSON.stringify([props.editSource.connectionId, insertTarget.value, current.value?.columns.map((c) => c.name) ?? []]) : ''),
  async (k) => {
    const seq = ++insertSeq;
    insertReason.value = null;
    const target = insertTarget.value;
    const src = props.editSource;
    if (!k || !target || !src) return;
    try {
      const columns = current.value?.columns.map((c) => c.name) ?? [];
      await invoke<string>('update_script', { args: { connection_id: src.connectionId, target, changes: [], inserts: [], columns } });
    } catch (e) {
      if (seq === insertSeq) insertReason.value = errorMessage(e);
    }
  },
  { immediate: true },
);
const canInsert = computed(() => !!insertTarget.value && !insertReason.value && !applying.value);
const grid = ref<InstanceType<typeof ResultGrid> | null>(null);
/** Totals of the cells selected in the grid (two or more). */
const stats = computed(() => (view.value === 'table' ? grid.value?.selectionStats ?? null : null));
const fmtNum = (x: number) => x.toLocaleString(locale(), { maximumFractionDigits: 4 });
const statsText = computed(() => {
  const s = stats.value;
  if (!s) return '';
  const parts = [t('results:grid.stats.count', { n: fmtNum(s.count) })];
  if (s.numbers) parts.push(t('results:grid.stats.sum', { v: fmtNum(s.sum) }), t('results:grid.stats.avg', { v: fmtNum(s.avg) }), t('results:grid.stats.min', { v: fmtNum(s.min) }), t('results:grid.stats.max', { v: fmtNum(s.max) }));
  return parts.join(' · ');
});
const statsTitle = computed(() => (stats.value ? t('results:grid.stats.title', { cells: fmtNum(stats.value.cells), numbers: fmtNum(stats.value.numbers) }) : ''));
const docOpen = ref(false);
/** "Agregar fila": a blank row at the end of the grid, its first cell in
 *  edit; document engines (and results with no columns) open the JSON editor. */
function addRow() {
  if (!canInsert.value) {
    if (insertReason.value) ElMessage.info({ message: t('results:insert.cant', { reason: insertReason.value }), duration: 3500 });
    return;
  }
  if (addsDocuments.value || typeof active.value !== 'number' || !current.value) { docOpen.value = true; return; }
  const list = (added[active.value] ??= []);
  list.push({});
  const r = current.value.rows.length + list.length - 1;
  nextTick(() => grid.value?.editRow(r));
}
function addDocuments(list: Record<string, unknown>[]) {
  docs.value = [...docs.value, ...list];
  docOpen.value = false;
}
const noDeleteReason = computed(() => deleteReason.value ?? noEditReason.value);
/** The edits bar's note: no primary key, and the columns left out of the WHERE. */
const editNote = computed(() => {
  const s = setup.value;
  const r = current.value;
  if (!s || typeof s === 'string' || !r) return null;
  const skipped = skippedKeyColumns(r.columns, s, props.dialect ?? '');
  const parts = [s.note, skipped.length ? t('core:gridEdit.skippedColumns', { columns: skipped.join(', ') }) : null].filter(Boolean);
  return parts.length ? parts.join(' ') : null;
});

function onDelete(rows: number[], mark: boolean) {
  // Locked while a save runs: what it sends is fixed, and its rows go when it ends.
  if (typeof active.value !== 'number' || applying.value) return;
  // New rows are dropped (from the last, so the indexes hold).
  const base = current.value?.rows.length ?? 0;
  const fresh = rows.filter((r) => r >= base).sort((a, b) => b - a);
  if (fresh.length) {
    const list = added[active.value] ?? [];
    for (const r of fresh) list.splice(r - base, 1);
    rows = rows.filter((r) => r < base);
    if (!rows.length) return;
  }
  const cur = deletes[active.value] ?? new Set<number>();
  const next = new Set(cur);
  for (const r of rows) {
    if (mark) next.add(r);
    else next.delete(r);
  }
  if (next.size) deletes[active.value] = next;
  else delete deletes[active.value];
}

/** Columns the grid doesn't edit: Oracle's SDO_GEOMETRY, which the driver
 *  sends as EWKT text for the map (writing that text back would fail). */
const readOnlyColumns = computed<Record<number, string>>(() => {
  const out: Record<number, string> = {};
  current.value?.columns.forEach((c, i) => {
    if (/^sdo_geometry$/i.test(c.type_name)) out[i] = t('results:edit.convertedSpatial', { column: c.name });
  });
  return out;
});

function onEdit(r: number, c: number, value: Cell | undefined) {
  if (typeof active.value !== 'number' || applying.value) return;
  if (value !== undefined && readOnlyColumns.value[c]) return;
  const base = current.value?.rows.length ?? 0;
  if (r >= base) {
    const cells = added[active.value]?.[r - base];
    if (!cells) return;
    if (value === undefined) delete cells[c];
    else cells[c] = value;
    return;
  }
  const set = (edits[active.value] ??= {});
  if (value === undefined) {
    if (set[r]) {
      delete set[r][c];
      if (!Object.keys(set[r]).length) delete set[r];
    }
  } else {
    (set[r] ??= {})[c] = value;
  }
}
function discard() {
  if (typeof active.value !== 'number' || applying.value) return;
  delete edits[active.value];
  delete deletes[active.value];
  delete added[active.value];
  docs.value = [];
}

// The code, regenerated as the edits change. Not shown in the bar (it grows
// with every edit): "Guardar" shows it in full before running.
const code = ref('');
const codeError = ref<string | null>(null);
let codeTimer: ReturnType<typeof setTimeout> | undefined;
watch(
  () => [currentEdits.value, currentDeletes.value, currentAdded.value, docs.value, setup.value] as const,
  () => {
    clearTimeout(codeTimer);
    codeTimer = setTimeout(generate, 150);
  },
  { deep: true },
);
/** Document engines get nested fields as objects, not their JSON text: the
 *  driver sends them as text for the grid, and an edit (here or in the JSON
 *  tree) writes the field back the same way. Only fields that are objects or
 *  arrays (by their type, or their value when the engine gives no type). */
function asDocValue(col: ResultColumn | undefined, v: unknown): unknown {
  if (!documents.value || typeof v !== 'string') return v;
  const t = v.trim();
  if (!((t.startsWith('{') && t.endsWith('}')) || (t.startsWith('[') && t.endsWith(']')))) return v;
  const ty = (col?.type_name ?? '').toLowerCase();
  if (ty && !/object|array|document|nested|map|list|json/.test(ty)) return v;
  try { return JSON.parse(t); } catch { return v; }
}
function asDocPairs(columns: ResultColumn[], pairs: [string, unknown][]): [string, unknown][] {
  if (!documents.value) return pairs;
  const byName = new Map(columns.map((c) => [c.name, c]));
  return pairs.map(([k, v]) => [k, asDocValue(byName.get(k), v)]);
}
async function generate() {
  const s = setup.value;
  const r = current.value;
  const src = props.editSource;
  const target = insertTarget.value;
  if (!pending.value || !src || !target) { code.value = ''; codeError.value = null; return; }
  const dialect = props.dialect ?? '';
  try {
    const editable = !!s && typeof s === 'object' && !!r;
    const changes = (editable ? buildChanges(r.columns, r.rows, currentEdits.value, s, dialect, currentDeletes.value) : [])
      .map((ch) => ({ ...ch, set: asDocPairs(r!.columns, ch.set), row: asDocPairs(r!.columns, ch.row) }));
    // In row order, so the script reads like the grid.
    const keys = editable ? [...currentDeletes.value].sort((a, b) => a - b).map((i) => rowKey(r.columns, r.rows[i], s, dialect)) : [];
    const inserts = [...(r ? buildInserts(r.columns, currentAdded.value).map((row) => asDocPairs(r.columns, row)) : []), ...docs.value.map((d) => Object.entries(d))];
    if (!changes.length && !keys.length && !inserts.length) { code.value = ''; codeError.value = null; return; }
    // Without deletes / inserts the call stays as before (older behavior, same code).
    const args = {
      connection_id: src.connectionId, target, changes,
      ...(keys.length ? { deletes: keys } : {}),
      ...(inserts.length ? { inserts, identity: editable ? s.identity : [] } : {}),
    };
    code.value = await invoke<string>('update_script', { args });
    codeError.value = null;
  } catch (e) {
    code.value = '';
    codeError.value = errorMessage(e);
  }
}
/** "2 celdas modificadas en 1 fila y 3 filas para eliminar": what's pending.
 *  Each part is a whole phrase (its two counts in one message), joined as a list. */
function pendingText(key: 'summary' | 'hint') {
  const parts = [
    editCount.value ? t('results:edit.changed', { count: editCount.value, rows: editRows.value }) : null,
    deleteCount.value ? t('results:edit.deletes', { count: deleteCount.value }) : null,
    currentAdded.value.length ? t('results:edit.newRows', { count: currentAdded.value.length }) : null,
    docs.value.length ? t(props.editSource?.language === 'json' ? 'results:edit.newDocs' : 'results:edit.newRows', { count: docs.value.length }) : null,
  ].filter((p): p is string => !!p);
  const list = listOf(parts);
  if (key === 'summary') return t('results:edit.pending', { list });
  // The hint explains the order the statements run in when there's more than one kind.
  if (newCount.value) return t('applyEdits:hintInserts', { list });
  return editCount.value && deleteCount.value ? t('applyEdits:hintBoth', { list }) : t('applyEdits:hint', { list });
}
const summary = computed(() => pendingText('summary'));
const applyHint = computed(() => pendingText('hint'));
function addToQuery() {
  if (!code.value || applying.value) return;
  emit('script', code.value);
  discard();
}
// "Guardar": the code in a dialog to review (and copy), then run on its
// own session. Nothing runs without that click. The run is a task
// (stores/tasks.ts): "Seguir en segundo plano" closes the dialog and the
// save goes on, also if the tab closes; "Guardando…" in the edits bar or
// "Ver detalle" opens the dialog again while it runs. The grid is locked
// meanwhile (no edit can be made that the save would then drop). Once it
// ends, "Ver detalle" shows the task's own detail (its SQL and outcome).
const applyOpen = ref(false);
const applying = ref(false);
const applyError = ref<string | null>(null);
/** The code being run (the edits may change meanwhile). */
const applyingCode = ref('');
let applyTask: TaskHandle | null = null;
const applyTaskId = ref<string | null>(null);
const tasks = useTasksStore();
const applyCancelling = computed(() => !!(applying.value && applyTaskId.value && tasks.byId(applyTaskId.value)?.cancelling));
/** The dialog's "Cancelar": closes it, or stops the save while it runs. */
function cancelApply() {
  if (applying.value && applyTaskId.value) tasks.cancel(applyTaskId.value);
  else applyOpen.value = false;
}
let alive = true;
onBeforeUnmount(() => {
  alive = false;
  // The tab closed mid-save: it goes on, says so when it ends, and its
  // detail is the task's own (this dialog is gone).
  if (applying.value && applyTask) {
    applyTask.setReopen(undefined);
    applyTask.background();
  }
});
/** A cell still being edited commits (its input commits on blur) and the
 *  code is generated now instead of after the debounce, so a save takes it. */
async function flushEdits() {
  const el = document.activeElement;
  if (el instanceof HTMLElement && root.value?.contains(el)) el.blur();
  await nextTick();
  clearTimeout(codeTimer);
  await generate();
}
async function reviewApply() {
  // Mid-save it shows the run (sent to the background or not).
  if (applying.value) { applyOpen.value = true; return; }
  await flushEdits();
  if (!code.value || applying.value) return;
  applyError.value = null;
  applyOpen.value = true;
}
function applyInBackground() {
  applyTask?.background();
  applyOpen.value = false;
}
let applyStarting = false;
async function applyEdits() {
  if (applying.value || applyStarting) return;
  applyStarting = true;
  try { await flushEdits(); } finally { applyStarting = false; }
  const src = props.editSource;
  const r = current.value;
  const at = active.value;
  if (!src || !code.value || applying.value) return;
  applying.value = true;
  applyError.value = null;
  const sql = applyingCode.value = code.value;
  // What's saved, as it is now: the grid takes it only if it still shows these rows.
  const outcome = props.outcome;
  const edited = liveEdits.value.map(([row, cols]) => [row, { ...cols }] as const);
  const gone = [...currentDeletes.value].sort((a, b) => b - a);
  const fresh = currentAdded.value.map((cells) => ({ ...cells }));
  const inserted = fresh.length + docs.value.length;
  const target = setup.value && typeof setup.value === 'object' ? setup.value.target : null;
  const where = target ? (target.schema ? `${target.schema}.${target.name}` : target.name) : (props.title || src.database);
  const sessionId = `apply:${Date.now()}:${Math.random().toString(36).slice(2)}`;
  const task = applyTask = startTask({
    kind: 'apply-edits',
    title: t('tasks:dialogs.applyEdits', { table: where }),
    connectionId: src.connectionId, database: src.database,
    cancel: () => api.cancelQuery(sessionId),
    reopen: () => { if (alive) applyOpen.value = true; },
  });
  applyTaskId.value = task.id;
  task.log(sql);
  try {
    const o = await api.executeQuery({ sessionId, connectionId: src.connectionId, database: src.database, sql, maxRows: 10, record: true });
    if (o.error) {
      applyError.value = tb(o.error);
      if (task.isCancelling) task.cancelled(); else task.fail(applyError.value);
      return;
    }
    const affected = o.results.reduce((n, x) => n + (x.rows_affected ?? 0), 0);
    const done = !affected ? t('applyEdits:done') : gone.length || inserted ? t('applyEdits:doneAffected', { count: affected }) : t('applyEdits:doneRows', { count: affected });
    task.finish(undefined, done);
    if (!alive) return;
    // The grid shows the saved values right away (a table's data also
    // reloads): edited cells take their values, deleted rows go.
    // New rows join the grid with the values set (a default the server
    // filled in shows when the table's data reloads).
    if (props.outcome === outcome && r && typeof at === 'number') {
      for (const [row, cols] of edited) {
        for (const [col, value] of Object.entries(cols)) r.rows[Number(row)][Number(col)] = value as Cell;
      }
      for (const i of gone) r.rows.splice(i, 1);
      for (const cells of fresh) r.rows.push(r.columns.map((_, c) => (c in cells ? cells[c] : null)));
      r.total_rows = Math.max(0, r.total_rows - gone.length + fresh.length);
      delete edits[at];
      delete deletes[at];
      delete added[at];
    }
    if (props.outcome === outcome) docs.value = [];
    const shown = applyOpen.value;
    applyOpen.value = false;
    if (shown) ElMessage.success(done);
    emit('applied');
  } catch (e) {
    applyError.value = errorMessage(e);
    if (task.isCancelling) task.cancelled(); else task.fail(e);
  } finally {
    applying.value = false;
    applyTask = null;
    applyTaskId.value = null;
    // The dialog now shows the grid's next edits, not this run.
    task.setReopen(undefined);
    api.closeSession(sessionId).catch(() => {});
  }
}
/** Mid-save, the SQL that's running; otherwise the pending edits' code. */
async function copyCode() {
  const text = applying.value ? applyingCode.value : code.value;
  try { await navigator.clipboard.writeText(text); ElMessage.success({ message: t('results:edit.codeCopied'), duration: 1200 }); } catch { /* ignore */ }
}

// -- export ---------------------------------------------------------------------------------
const exporting = ref<{ format: ExportFormat } | null>(null);
/** The source an export runs again (a connection's query). */
const runSource = computed(() => (props.source?.notebookTab ? null : props.source ?? null));
/** What "Exportar todo" runs again: a connection's query, or a DataFrame SQL
 *  cell's SQL on its tab's DuckDB session (over the DataFrames its run read). */
const exportSource = computed(() =>
  props.source && typeof active.value === 'number' ? { ...props.source, resultIndex: active.value } : null);
/** A DataFrame SQL cell's result cut for display: its charts start over the
 *  whole result (DuckDB aggregates it locally, no server is involved). */
const chartWholeByDefault = computed(() => wholeChartByDefault(props.source, current.value));

/** Quick export: the rows already loaded, straight to a file. */
async function quickExport(format: ExportFormat) {
  const r = current.value;
  if (!r) return;
  const fmt = EXPORT_FORMATS.find((f) => f.id === format)!;
  let path: string | null = null;
  try {
    path = await save({ defaultPath: fileName(props.title ?? t('results:defaultFileName'), format), filters: [{ name: fmt.filter, extensions: [fmt.ext] }] });
  } catch { /* outside Tauri */ }
  if (!path) return;
  const options = defaultOptions(format, props.title ?? t('results:defaultTableName'), props.dialect ?? '');
  // A result cut for display exports every row: its query runs again and
  // streams to the file (as the advanced export's "all" does).
  const all = r.truncated ? exportSource.value : null;
  if (all) return quickExportAll(path, options, all, r.total_rows);
  try {
    const res = await exportApi.rows(path, options, r.columns, r.rows, runSource.value?.connectionId ?? props.editSource?.connectionId);
    ElMessage.success({
      message: r.truncated
        ? t('results:export.doneLoaded', { count: res.rows, rows: res.rows.toLocaleString(locale()) })
        : t('results:export.done', { count: res.rows, rows: res.rows.toLocaleString(locale()) }),
      duration: r.truncated ? 5000 : 2500,
    });
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

/** Quick export of a cut result: the whole result, streamed, as a task. */
async function quickExportAll(path: string, options: ReturnType<typeof defaultOptions>, all: NonNullable<typeof exportSource.value>, totalRows?: number | null) {
  const exportId = crypto.randomUUID();
  const h = startTask({
    kind: 'export',
    title: t('tasks:genImportExport.export', { file: path.split(/[\\/]/).pop() ?? path }),
    connectionId: all.connectionId || undefined,
    database: all.notebookTab ? undefined : all.database,
    cancel: () => exportApi.cancel(exportId),
  });
  try {
    let done = 0;
    const known = all.notebookTab && totalRows ? totalRows : undefined;
    await h.listen<{ id: string; rows: number; total?: number }>('export-progress', (e) => {
      if (e.payload.id !== exportId) return;
      done = Math.max(done, e.payload.rows);
      const total = e.payload.total ?? known;
      h.progress({ done, total: total != null && done <= total ? total : undefined, unit: 'rows' });
    });
    const res = await exportApi.query({ exportId, ...all, path, options });
    h.finish(res, t('tasks:dialogs.rows', { count: res.rows, n: res.rows.toLocaleString(locale()) }));
    ElMessage.success({ message: t('results:export.done', { count: res.rows, rows: res.rows.toLocaleString(locale()) }), duration: 2500 });
  } catch (e) {
    if (h.isCancelling) h.cancelled(); else { h.fail(e); ElMessage.error(errorMessage(e)); }
  }
}

function onExportCommand(cmd: string) {
  if (cmd === 'advanced') exporting.value = { format: 'csv' };
  else quickExport(cmd as ExportFormat);
}

// ⌘E (the registry's `results.export`) opens the advanced export, when this
// pane is the visible one.
const root = ref<HTMLDivElement | null>(null);
function onKey(e: KeyboardEvent) {
  if (isSaveShortcut(e)) saveShortcut(e);
}
const unregisterExport = registerCommand('results.export', () => { exporting.value = { format: 'csv' }; }, () => !!current.value && inFocusedEditor(root.value));
onMounted(() => window.addEventListener('keydown', onKey));
onBeforeUnmount(() => { window.removeEventListener('keydown', onKey); unregisterExport(); });

// ⌘S (Ctrl+S on Windows / Linux) is "Guardar": the review dialog, when this
// pane is the visible one and its grid has pending edits. Inside the SQL
// editor ⌘S saves the query (CodeEditor), and with a dialog open (the
// review included) it does nothing: running is always the "Ejecutar" click.
async function saveShortcut(e: KeyboardEvent) {
  if (inCodeEditor(e)) return;
  e.preventDefault();
  if (applyOpen.value || modalOpen() || view.value === 'chart' || view.value === 'map' || !current.value || !inFocusedEditor(root.value)) return;
  // A cell being edited commits on the key (ResultGrid); its code is
  // generated now instead of after the debounce.
  await nextTick();
  if (!pending.value) return;
  clearTimeout(codeTimer);
  await generate();
  if (!applyOpen.value && !modalOpen()) reviewApply();
}

/** A line of "Mensajes": a statement's outcome, a server message or an error. */
interface Entry {
  level: MessageLevel;
  text: string;
  code?: string | null;
  /** 1-based line of the script that ran, and the error's position in it. */
  line?: number | null;
  offset?: number | null;
}

/** "120 filas" or "5.000 filas (se muestran 1.000)": one message each, not glued pieces. */
function rowsText(r: StatementResult) {
  const rows = r.total_rows.toLocaleString(locale());
  return r.truncated
    ? t('results:messages.rowsShown', { count: r.total_rows, rows, shown: r.rows.length.toLocaleString(locale()) })
    : t('results:messages.rows', { count: r.total_rows, rows });
}

/** "INSERT 0 3 · 3 filas afectadas · 12 ms": a statement of a run statement by statement. */
function statementEntry(r: StatementResult): Entry {
  const parts: string[] = [];
  if (r.tag) parts.push(r.tag);
  if (r.columns.length) {
    parts.push(rowsText(r));
  } else if (r.rows_affected != null) {
    parts.push(t('results:messages.affected', { count: r.rows_affected, rows: r.rows_affected.toLocaleString(locale()) }));
  } else if (!r.tag) {
    parts.push(t('results:messages.completed'));
  }
  if (r.elapsed_ms != null) parts.push(t('results:messages.elapsed', { ms: r.elapsed_ms.toLocaleString(locale()) }));
  return { level: 'info', text: parts.join(' · '), line: r.line ?? null, offset: r.offset ?? null };
}

/** Statement by statement (results carry their statement, messages a log):
 *  each statement's messages and errors, then its outcome, in script order. */
function scriptEntries(o: QueryOutcome): Entry[] {
  const log = o.log ?? [];
  const errors = [...(o.errors ?? [])];
  const fromLog = (m: Message): Entry => {
    let offset: number | null = null;
    if (m.level === 'error') {
      const k = errors.findIndex((e) => e.message === m.text && e.statement === m.statement);
      if (k >= 0) offset = errors.splice(k, 1)[0].offset;
    }
    return { level: m.level, text: tb(m.text), code: m.code, line: m.line, offset };
  };
  const out: Entry[] = [];
  const stmts = [...new Set([...o.results.map((r) => r.statement), ...log.map((m) => m.statement)])]
    .filter((s): s is number => s != null)
    .sort((a, b) => a - b);
  for (const s of stmts) {
    for (const m of log) if (m.statement === s) out.push(fromLog(m));
    for (const r of o.results) if (r.statement === s) out.push(statementEntry(r));
  }
  for (const r of o.results) if (r.statement == null) out.push(statementEntry(r));
  for (const m of log) if (m.statement == null) out.push(fromLog(m));
  // Messages a driver pushed as plain text after its last logged one.
  const logged = log.filter((m) => m.level !== 'error').length;
  for (const m of o.messages.slice(logged)) out.push({ level: 'info', text: tb(m) });
  if (o.error && !log.some((m) => m.level === 'error')) out.push({ level: 'error', text: tb(o.error) });
  return out;
}

const messages = computed<Entry[]>(() => {
  const o = props.outcome;
  if (!o) return [];
  if (o.log?.length || o.results.some((r) => r.statement != null)) return scriptEntries(o);
  const out: Entry[] = [];
  o.results.forEach((r, i) => {
    if (r.columns.length) {
      out.push({ level: 'info', text: t('results:messages.statement', { n: i + 1, text: rowsText(r) }) });
    } else if (r.rows_affected !== null) {
      out.push({ level: 'info', text: t('results:messages.statement', { n: i + 1, text: t('results:messages.affected', { count: r.rows_affected, rows: r.rows_affected.toLocaleString(locale()) }) }) });
    } else {
      out.push({ level: 'info', text: t('results:messages.statement', { n: i + 1, text: t('results:messages.completed') }) });
    }
  });
  for (const m of o.messages) out.push({ level: 'info', text: tb(m) });
  if (o.error) out.push({ level: 'error', text: tb(o.error) });
  return out;
});

const showLines = computed(() => props.lineOffset != null);
/** "Completado" / "Completado con errores" and the total time (as SSMS). */
const statusText = computed(() => {
  const o = props.outcome;
  if (!o || props.running) return '';
  const failed = !!o.error || !!o.errors?.length;
  return `${failed ? t('results:status.doneWithErrors') : t('results:status.done')} · ${t('results:messages.elapsed', { ms: o.elapsed_ms.toLocaleString(locale()) })}`;
});
</script>

<template>
  <div ref="root" class="rp">
    <div class="nm-subtabs">
      <button
        v-for="s in sets"
        :key="s.i"
        class="nm-subtab"
        :class="{ active: active === s.i }"
        @click="pick(s.i)"
      >
        <template v-if="labels?.[s.i]">{{ labels[s.i] }}</template>
        <template v-else>{{ $t('results:tabs.result') }} {{ sets.length > 1 ? s.i + 1 : '' }}</template>
        <span class="rp-count">{{ s.total_rows.toLocaleString(locale()) }}{{ s.truncated ? '+' : '' }}</span>
      </button>
      <button v-if="outcome?.plans.length" class="nm-subtab" :class="{ active: active === 'plan' }" @click="pick('plan')">
        <el-icon><ei-share /></el-icon> {{ $t('results:tabs.plan') }}
      </button>
      <button class="nm-subtab" :class="{ active: active === 'messages' }" @click="pick('messages')">
        {{ $t('results:tabs.messages') }}
        <el-icon v-if="outcome?.error" color="var(--nm-danger)"><ei-circle-close-filled /></el-icon>
      </button>
      <div class="nm-spacer" />
      <button
        v-if="insertTarget && outcome && !outcome.error && (current || !sets.length)"
        class="rp-export"
        :class="{ off: !canInsert }"
        :aria-disabled="!canInsert"
        :title="insertReason ? $t('results:insert.cant', { reason: insertReason }) : undefined"
        @click="addRow"
      >
        <el-icon><ei-plus /></el-icon> {{ addsDocuments && editSource?.language === 'json' ? $t('results:insert.addDocument') : $t('results:insert.addRow') }}
      </button>
      <el-dropdown v-if="current" trigger="click" @command="onExportCommand">
        <button class="rp-export" :title="$t('results:export.title')">
          <el-icon><ei-download /></el-icon> {{ $t('common:export') }} <el-icon><ei-arrow-down /></el-icon>
        </button>
        <template #dropdown>
          <el-dropdown-menu>
            <el-dropdown-item command="advanced">
              <span class="rp-menu-row"><span>{{ $t('results:export.advanced') }}</span><kbd>{{ bindingLabel('results.export') }}</kbd></span>
            </el-dropdown-item>
            <el-dropdown-item v-for="(f, i) in EXPORT_FORMATS" :key="f.id" :command="f.id" :divided="i === 0">{{ $t(`results:export.format.${f.id}`, f.label) }}</el-dropdown-item>
          </el-dropdown-menu>
        </template>
      </el-dropdown>
      <div v-if="current" class="rp-mode" role="group" :aria-label="$t('results:view.label')">
        <button :class="{ on: view === 'table' }" :title="$t('results:view.table')" @click="setView('table')"><el-icon><ei-grid /></el-icon></button>
        <button :class="{ on: view === 'tree' }" :title="$t('results:view.tree')" @click="setView('tree')"><span class="rp-json">{ }</span></button>
        <button :class="{ on: view === 'chart' }" :title="$t('results:view.chart')" @click="setView('chart')"><el-icon><ei-histogram /></el-icon></button>
        <button v-if="spatial" :class="{ on: view === 'map' }" :title="$t('results:view.map')" @click="setView('map')"><el-icon><ei-map-location /></el-icon></button>
      </div>
      <!-- A notebook cell's charts (add, rename, remove, switch). -->
      <slot v-if="current && view === 'chart'" name="chartTabs" />
      <span v-if="stats" class="rp-status rp-stats" :title="statsTitle">{{ statsText }}</span>
      <template v-if="!hideStatus">
        <span v-if="running" class="rp-status"><el-icon class="is-loading"><ei-loading /></el-icon> {{ $t('results:running') }}</span>
        <span v-else-if="outcome" class="rp-status">{{ outcome.elapsed_ms.toLocaleString(locale()) }} ms</span>
      </template>
    </div>

    <div v-if="pending && ((view !== 'chart' && view !== 'map') || !current)" class="rp-edits">
      <div class="rp-edits-bar">
        <el-icon><ei-edit /></el-icon>
        <span>{{ summary }}</span>
        <div class="nm-spacer" />
        <!-- Not :loading: Element Plus disables a loading button, and this one reopens the running save. -->
        <el-button v-if="applying" size="small" type="primary" @click="reviewApply">
          <el-icon class="is-loading"><ei-loading /></el-icon>&nbsp;{{ $t('tasks:dialogs.saving') }}
        </el-button>
        <el-button v-else size="small" type="primary" :disabled="!code" @click="reviewApply">{{ $t('applyEdits:save') }}</el-button>
        <el-button size="small" :disabled="!code || applying" @click="addToQuery">{{ $t('results:edit.addToQuery') }}</el-button>
        <el-button size="small" :disabled="!code" @click="copyCode">{{ $t('common:copy') }}</el-button>
        <el-button size="small" text :disabled="applying" @click="discard">{{ $t('results:edit.discard') }}</el-button>
      </div>
      <div v-if="editNote" class="rp-edits-note">{{ editNote }}</div>
      <div v-if="codeError" class="rp-edits-note err">{{ codeError }}</div>
    </div>
    <div v-if="!outcome && !running" class="rp-empty nm-muted">
      <i18next :translation="$t('results:empty')"><template #key><kbd>{{ bindingLabel('editor.run') }}</kbd></template></i18next>
    </div>
    <PlanView v-else-if="active === 'plan' && outcome?.plans.length" :plans="outcome.plans" />
    <template v-else-if="current">
      <div v-if="current.truncated" class="rp-trunc">
        {{ $t(sortedLoaded ? 'results:truncatedSorted' : 'results:truncated', { shown: current.rows.length.toLocaleString(locale()), total: current.total_rows.toLocaleString(locale()) }) }}
      </div>
      <ChartView
        v-if="view === 'chart'"
        :key="`${active}:${chartKey ?? ''}`"
        :columns="current.columns"
        :rows="current.rows"
        :settings="chartSettings"
        :stored-points="chartPoints"
        :stored-aggregate="chartAggregate"
        :source="source"
        :whole-by-default="chartWholeByDefault"
        @update:settings="(s: ChartState) => emit('update:chartSettings', s)"
      />
      <MapView
        v-else-if="view === 'map'"
        :key="`map-${active}`"
        :columns="current.columns"
        :rows="current.rows"
        :detected="spatial"
        :settings="mapSettings"
        @update:settings="(m: MapSettings) => emit('update:mapSettings', m)"
      />
      <JsonTreeView
        v-else-if="view === 'tree'"
        :columns="current.columns"
        :rows="gridRows"
        :edits="gridEdits"
        :deleted="currentDeletes"
        :added="currentAdded.length"
        :filterable="filterable"
        :editable="canEdit && !applying"
        :no-edit-reason="noEditReason"
        :insertable="canInsert"
        :deletable="canDelete && !applying"
        :no-delete-reason="noDeleteReason"
        @edit="onEdit"
        @delete="onDelete"
        @filter="(c, st) => emit('filter', c, st)"
      />
      <template v-else>
      <ResultGrid
        ref="grid"
        :columns="current.columns"
        :rows="gridRows"
        :copy-context="{ table: table ?? null, dialect: dialect ?? '', keyColumns }"
        :editable="canEdit && !applying"
        :no-edit-reason="noEditReason"
        :read-only-columns="readOnlyColumns"
        :edits="gridEdits"
        :deleted="currentDeletes"
        :deletable="canDelete && !applying"
        :no-delete-reason="noDeleteReason"
        :filterable="filterable"
        :filters="filters"
        :added="currentAdded.length"
        :insertable="canInsert"
        sortable
        :sort="gridSort"
        :loaded-of="current.truncated ? current.total_rows : null"
        :links="links"
        @follow="follow"
        @edit="onEdit"
        @delete="onDelete"
        @add="addRow"
        @filter="(c, st) => emit('filter', c, st)"
        @sort="onSort"
      />
      </template>
    </template>
    <div v-else class="rp-messages nm-selectable">
      <div v-for="(m, i) in messages" :key="i" :class="['rp-msg', m.level]">
        <button
          v-if="showLines && m.line != null"
          class="rp-msg-line"
          :title="$t('results:messages.goToLine')"
          @click="emit('goto', { offset: m.offset ?? null, line: m.line ?? null })"
        >{{ $t('results:messages.line', { line: m.line + (lineOffset ?? 0) }) }}</button>
        <span v-if="m.code" class="rp-msg-code">{{ m.code }}</span>
        <span>{{ m.text }}</span>
      </div>
      <div v-if="running" class="rp-msg rp-msg-status"><el-icon class="is-loading"><ei-loading /></el-icon> {{ $t('results:running') }}</div>
      <div v-else-if="statusText" class="rp-msg rp-msg-status">{{ statusText }}</div>
    </div>
    <AddDocumentDialog
      :open="docOpen"
      :documents="editSource?.language === 'json'"
      :fields="current?.columns.map((c) => c.name) ?? []"
      @close="docOpen = false"
      @add="addDocuments"
    />
    <ExportDialog
      v-if="exporting && current"
      :columns="current.columns"
      :rows="current.rows"
      :total-rows="current.total_rows"
      :truncated="current.truncated"
      :source="exportSource"
      :title="title ?? $t('results:defaultFileName')"
      :dialect="dialect ?? ''"
      :initial-format="exporting.format"
      @close="exporting = null"
    />
      <el-dialog
        v-model="applyOpen" :title="$t('applyEdits:title')" width="720px" append-to-body
        :close-on-click-modal="!applying" :close-on-press-escape="!applying" :show-close="!applying"
      >
      <p v-if="!applying" class="rp-apply-hint">{{ applyHint }}</p>
      <pre class="rp-apply-code nm-selectable">{{ applying ? applyingCode : code }}</pre>
      <div v-if="applyError" class="rp-apply-error" role="alert"><el-icon><ei-circle-close-filled /></el-icon><span>{{ applyError }}</span></div>
      <template #footer>
        <div class="rp-apply-foot">
          <el-button @click="copyCode">{{ $t('common:copy') }}</el-button>
          <el-button :disabled="applying" @click="applyOpen = false; addToQuery()">{{ $t('results:edit.addToQuery') }}</el-button>
          <span class="rp-apply-sp" />
          <el-button v-if="applying" @click="applyInBackground">{{ $t('tasks:panel.background') }}</el-button>
          <el-button :disabled="applyCancelling" @click="cancelApply">{{ $t('common:cancel') }}</el-button>
          <el-button type="primary" :loading="applying" @click="applyEdits">{{ $t('applyEdits:run') }}</el-button>
        </div>
      </template>
    </el-dialog>
</div>
</template>

<style scoped>
.rp { display: flex; flex-direction: column; min-height: 0; height: 100%; }
.rp-count {
  font-size: 10.5px;
  padding: 0 5px;
  border-radius: 8px;
  background: var(--ide-button-2);
  color: var(--nm-text);
  letter-spacing: 0;
}
.rp-export {
  display: inline-flex; align-items: center; gap: 4px; margin: 0 2px; padding: 2px 8px; align-self: center;
  font: inherit; font-size: 11.5px; color: var(--nm-text); background: transparent;
  border: 1px solid var(--nm-border); border-radius: 3px; cursor: pointer;
}
.rp-export.off { opacity: 0.5; cursor: default; }
.rp-export:hover:not(.off) { color: var(--nm-text-strong); border-color: var(--nm-border-hover); }
.rp-menu-row { display: flex; justify-content: space-between; gap: 24px; width: 100%; }
.rp-menu-row kbd { font-family: var(--nm-mono); font-size: 11px; color: var(--nm-text-dim); }
.rp-mode { display: inline-flex; margin: 0 6px; border: 1px solid var(--nm-border); border-radius: 3px; overflow: hidden; align-self: center; }
.rp-mode button { display: inline-flex; align-items: center; padding: 2px 7px; border: none; background: transparent; color: var(--nm-text-dim); cursor: pointer; }
.rp-mode button.on { background: var(--ide-selection); color: var(--nm-text-strong); }
.rp-json { font-family: var(--nm-mono); font-size: 11px; font-weight: 600; line-height: 1; }
.rp-mode button:hover { color: var(--nm-text-strong); }
.rp-stats { color: var(--nm-text); font-variant-numeric: tabular-nums; }
.rp-status { display: inline-flex; align-items: center; gap: 4px; padding: 0 10px; font-size: 11.5px; color: var(--nm-text-dim); }
.rp-empty { padding: 16px; }
.rp-empty kbd { font-family: var(--nm-mono); padding: 0 4px; border: 1px solid var(--nm-border); border-radius: 3px; }
.rp-trunc { padding: 3px 10px; font-size: 11.5px; color: var(--nm-warning); border-bottom: 1px solid var(--nm-border-soft); }
.rp-messages { flex: 1; overflow: auto; padding: 8px 12px; font-family: var(--nm-mono); font-size: 12px; }
.rp-msg { padding: 2px 0; white-space: pre-wrap; }
.rp-msg.error { color: var(--nm-danger); }
.rp-msg.warning { color: var(--nm-warning); }
.rp-msg-line {
  margin-right: 6px; padding: 0; font: inherit; color: var(--ide-focus, var(--nm-text-dim));
  background: transparent; border: none; cursor: pointer; text-decoration: underline dotted;
}
.rp-msg-line:hover { text-decoration: underline; }
.rp-msg-code { margin-right: 6px; padding: 0 4px; border-radius: 3px; background: var(--ide-button-2); color: var(--nm-text); }
.rp-msg-status { margin-top: 6px; color: var(--nm-text-dim); }
.rp-edits { border-bottom: 1px solid var(--nm-border); background: color-mix(in srgb, var(--nm-warning) 6%, transparent); }
.rp-edits-bar { display: flex; align-items: center; gap: 6px; padding: 4px 8px; font-size: 12px; color: var(--nm-text); }
.rp-edits-bar .el-icon { color: var(--nm-warning); }
.rp-edits-note { padding: 0 10px 4px; font-size: 11.5px; color: var(--nm-text-dim); }
.rp-edits-note.err { color: var(--nm-danger); }
.rp-apply-foot { display: flex; align-items: center; gap: 8px; }
.rp-apply-foot .el-button { margin: 0; }
.rp-apply-sp { flex: 1; }
.rp-apply-hint { margin: 0 0 8px; color: var(--nm-text-dim); font-size: 12.5px; }
.rp-apply-code { margin: 0; padding: 10px 12px; max-height: 50vh; overflow: auto; border: 1px solid var(--nm-border); border-radius: 4px; background: var(--ide-editor, var(--nm-bg-elev)); font-family: var(--nm-mono); font-size: 12.5px; color: var(--nm-text-strong); white-space: pre-wrap; }
.rp-apply-error { display: flex; gap: 6px; margin-top: 10px; padding: 8px 10px; border-radius: 3px; color: var(--nm-text); border: 1px solid color-mix(in srgb, var(--nm-danger) 50%, transparent); background: color-mix(in srgb, var(--nm-danger) 10%, transparent); white-space: pre-wrap; word-break: break-word; }
.rp-apply-error .el-icon { color: var(--nm-danger); margin-top: 2px; flex: none; }
</style>
