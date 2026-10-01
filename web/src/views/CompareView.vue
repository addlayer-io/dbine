<script setup lang="ts">
import { computed, onMounted, reactive, ref, watch } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import { tb } from '../i18n/backend';
import {
  compareApi, type CodeObject, type CompareResult, type DbModel, type ItemDiff, type ObjectChange, type ObjectDiff,
  type Status, type SyncScript, type TableChange, type TableDiff,
} from '../api/compare';
import type { CheckDef, ColumnDef, ForeignKeyDef, IndexDef, TableSchema } from '../api/schema-types';
import CodeEditor from '../components/CodeEditor.vue';
import { newQuery } from '../composables/actions';
import { lineDiff } from '../composables/lineDiff';
import { useConnectionsStore } from '../stores/connections';
import { readJson, writeJson } from '../stores/storage';
import { useTabsStore, type CompareTab } from '../stores/tabs';

// "Comparar esquemas": two databases side by side, WinMerge style. Each
// difference can be carried to the other side (→ / ←); that only edits an
// in-memory copy of the target's schema. "Sincronizar" then asks the target's
// driver for the script (CREATE / ALTER / DROP), shows it, and runs it when
// the user says so. The comparison itself runs in the backend
// (dbine-schema/src/compare.rs) over these copies after every change.

const props = defineProps<{ tab: CompareTab }>();
const conns = useConnectionsStore();
const { t } = useTranslation();

type SideId = 'left' | 'right';
interface Side {
  connectionId: string;
  database: string;
  /** Only this schema ('' = all). */
  schema: string;
  /** As loaded from the server. */
  orig: DbModel | null;
  /** With the changes carried to this side. */
  work: DbModel | null;
  loading: boolean;
  error: string | null;
  warnings: string[];
}
const blank = (connectionId: string, database: string): Side => ({ connectionId, database, schema: '', orig: null, work: null, loading: false, error: null, warnings: [] });
/** Last time's picks (restored with the app), when their connections still exist. */
const kept = (s: SideId) => {
  const p = props.tab.picks?.[s];
  return p && conns.byId(p.connectionId) ? p : null;
};
const start = (s: SideId, connectionId: string, database: string): Side => {
  const p = kept(s);
  return p ? { ...blank(p.connectionId, p.database), schema: p.schema ?? '' } : blank(connectionId, database);
};
const sides = reactive<Record<SideId, Side>>({ left: start('left', props.tab.connectionId, props.tab.database), right: start('right', props.tab.connectionId, '') });
const tabs = useTabsStore();
watch(
  () => (['left', 'right'] as SideId[]).map((s) => [sides[s].connectionId, sides[s].database, sides[s].schema]),
  () => tabs.setPicks(props.tab.id, {
    left: { connectionId: sides.left.connectionId, database: sides.left.database, schema: sides.left.schema },
    right: { connectionId: sides.right.connectionId, database: sides.right.database, schema: sides.right.schema },
  }),
);
const other = (s: SideId): SideId => (s === 'left' ? 'right' : 'left');
/** A plain deep copy (the models are reactive: structuredClone can't take proxies). */
const clone = <T>(x: T): T => JSON.parse(JSON.stringify(x));

const connName = (id: string) => conns.byId(id)?.name ?? '';
const dbsOf = (id: string) => conns.live[id]?.databases ?? [];
const driverOf = (id: string) => conns.driverOf(id);

async function pickDefaults(s: SideId) {
  const side = sides[s];
  if (!side.connectionId || !(await conns.ensureConnected(side.connectionId))) return;
  if (side.database) return;
  const dbs = dbsOf(side.connectionId);
  // The right side starts on another database of the same connection.
  side.database = s === 'right' ? dbs.find((d) => d !== sides.left.database) ?? dbs[0] ?? '' : conns.live[side.connectionId]?.defaultDatabase ?? dbs[0] ?? '';
}
onMounted(async () => {
  await pickDefaults('left');
  await pickDefaults('right');
});
for (const s of ['left', 'right'] as SideId[]) {
  watch(() => sides[s].connectionId, () => {
    sides[s].database = '';
    pickDefaults(s);
  });
  watch(() => [sides[s].connectionId, sides[s].database], () => {
    sides[s].orig = null;
    sides[s].work = null;
    sides[s].schema = '';
    result.value = null;
  });
}

const schemasOf = (s: SideId) => {
  const m = sides[s].orig;
  if (!m) return [];
  return [...new Set([...m.tables.map((t) => t.schema), ...m.objects.map((o) => o.schema)].filter((x): x is string => !!x))].sort();
};

// -- compare ---------------------------------------------------------------------------------
const opts = reactive({ ignore_case: true, ignore_comments: false });
const onlyDiff = ref(true);
const filter = ref('');
/**
 * What was carried with an arrow in this comparison: list ids (`t:…`, `o:…`)
 * and detail rows (`t:…|section|name`). They stay visible with "Solo
 * diferencias" even after they become equal, so a row doesn't vanish under
 * the pointer right after its arrow.
 */
const touched = reactive(new Set<string>());
const tid = (t: TableDiff) => `t:${t.key.toLowerCase()}`;
const oid = (o: ObjectDiff) => `o:${o.kind}:${o.key.toLowerCase()}`;
const itemKey = (t: TableDiff, section: string, d: ItemDiff | null) => `${tid(t)}|${section}|${d ? d.name.toLowerCase() : ''}`;
const result = ref<CompareResult | null>(null);
/** The models the result's indexes point into (the work copies, filtered by schema). */
const views = reactive<Record<SideId, DbModel | null>>({ left: null, right: null });

// The objects list resizes like the explorer sidebar (its width is remembered).
const listWidth = ref(readJson('dbine.compareListWidth', 320));
function dragList(e: PointerEvent) {
  const start = e.clientX;
  const from = listWidth.value;
  const move = (ev: PointerEvent) => { listWidth.value = Math.min(720, Math.max(200, from + ev.clientX - start)); };
  const up = () => {
    window.removeEventListener('pointermove', move);
    window.removeEventListener('pointerup', up);
    writeJson('dbine.compareListWidth', listWidth.value);
  };
  window.addEventListener('pointermove', move);
  window.addEventListener('pointerup', up);
}
const comparing = ref(false);
/** Bumped by each comparison and by "Detener": a late result of an older one is ignored. */
let generation = 0;
class Stopped extends Error {}

async function load(s: SideId, gen: number) {
  const side = sides[s];
  side.loading = true;
  side.error = null;
  try {
    if (!(await conns.ensureConnected(side.connectionId))) throw new Error(t('compare:connectFailed'));
    const r = await compareApi.load(side.connectionId, side.database);
    if (gen !== generation) throw new Stopped();
    side.orig = { driver: r.driver, tables: r.tables, objects: r.objects };
    side.work = clone(side.orig);
    side.warnings = r.warnings;
  } catch (e) {
    if (gen === generation && !(e instanceof Stopped)) side.error = errorMessage(e);
    throw e;
  } finally {
    if (gen === generation) side.loading = false;
  }
}

async function compareBoth() {
  if (!sides.left.database && dbsOf(sides.left.connectionId).length) { ElMessage.warning(t('compare:pickLeftDatabase')); return; }
  if (!sides.right.connectionId) { ElMessage.warning(t('compare:pickRightConnection')); return; }
  if (sides.left.connectionId === sides.right.connectionId && sides.left.database === sides.right.database && sides.left.schema === sides.right.schema) {
    ElMessage.warning(t('compare:pickDifferent'));
    return;
  }
  const gen = ++generation;
  comparing.value = true;
  history.length = 0;
  touched.clear();
  try {
    await Promise.all([load('left', gen), load('right', gen)]);
    if (gen === generation) await recompare();
    // A fresh comparison doesn't pin an equal selection (only arrows do).
    if (onlyDiff.value && selected.value?.status === 'equal') selectedId.value = null;
  } catch {
    /* shown next to each side (or stopped) */
  } finally {
    if (gen === generation) comparing.value = false;
  }
}

/** "Detener": forget the comparison in progress and stop reading the schemas on the servers. */
function stopCompare() {
  generation++;
  comparing.value = false;
  for (const s of ['left', 'right'] as const) {
    const side = sides[s];
    if (!side.loading) continue;
    side.loading = false;
    // The structure is read on the connection's metadata session: interrupt
    // its query and drop it (the next read opens a fresh one).
    const key = `meta:${side.connectionId}:${side.database}`;
    api.cancelQuery(key).catch(() => {}).finally(() => api.closeSession(key).catch(() => {}));
  }
}

function filtered(s: SideId): DbModel | null {
  const side = sides[s];
  if (!side.work) return null;
  if (!side.schema) return { ...side.work };
  return {
    driver: side.work.driver,
    tables: side.work.tables.filter((t) => t.schema === side.schema),
    objects: side.work.objects.filter((o) => o.schema === side.schema),
  };
}
/** Comparing one schema against another: pair by name only. */
const ignoreSchema = computed(() => !!sides.left.schema && !!sides.right.schema);

async function recompare() {
  const l = filtered('left');
  const r = filtered('right');
  if (!l || !r) return;
  views.left = l;
  views.right = r;
  try {
    result.value = await compareApi.compare(l, r, { ...opts, ignore_schema: ignoreSchema.value });
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}
watch(() => [opts.ignore_case, opts.ignore_comments, sides.left.schema, sides.right.schema], () => { if (result.value || (sides.left.work && sides.right.work)) recompare(); });

function swap() {
  const l = { ...sides.left };
  const r = { ...sides.right };
  // The watchers would clear what's loaded: restore it after them.
  sides.left.connectionId = r.connectionId;
  sides.right.connectionId = l.connectionId;
  setTimeout(() => {
    Object.assign(sides.left, r);
    Object.assign(sides.right, l);
    history.length = 0;
    touched.clear();
    recompare();
  });
}

// -- list ------------------------------------------------------------------------------------
const KIND_LABELS = computed((): Record<string, string> => ({
  view: t('compare:kinds.view'), materialized_view: t('compare:kinds.materializedView'), procedure: t('compare:kinds.procedure'),
  function: t('compare:kinds.function'), trigger: t('compare:kinds.trigger'), sequence: t('compare:kinds.sequence'),
  synonym: t('compare:kinds.synonym'), type: t('compare:kinds.type'), fulltext_catalog: t('compare:kinds.fulltextCatalog'),
  fulltext_stoplist: t('compare:kinds.fulltextStoplist'), domain: t('compare:kinds.domain'), virtual_table: t('compare:kinds.virtualTable'),
  dictionary: t('compare:kinds.dictionary'),
}));
const STATUS = computed((): Record<Status, { label: string; icon: string }> => ({
  equal: { label: t('compare:status.equal'), icon: '=' },
  changed: { label: t('compare:status.changed'), icon: '≠' },
  only_left: { label: t('compare:status.onlyLeft'), icon: '◧' },
  only_right: { label: t('compare:status.onlyRight'), icon: '◨' },
}));
const counts = computed(() => {
  const c: Record<Status, number> = { equal: 0, changed: 0, only_left: 0, only_right: 0 };
  for (const t of result.value?.tables ?? []) c[t.status]++;
  for (const o of result.value?.objects ?? []) c[o.status]++;
  return c;
});
// Touched rows and the selected one stay listed even when equal.
const shows = (status: Status, key: string, id: string) => (!onlyDiff.value || status !== 'equal' || touched.has(id) || id === selectedId.value) && (!filter.value || key.toLowerCase().includes(filter.value.trim().toLowerCase()));
const groups = computed(() => {
  const r = result.value;
  if (!r) return [];
  const out: { label: string; items: { id: string; key: string; status: Status; table?: TableDiff; object?: ObjectDiff }[] }[] = [];
  const tables = r.tables.filter((t) => shows(t.status, t.key, tid(t))).map((t) => ({ id: tid(t), key: t.key, status: t.status, table: t }));
  if (tables.length) out.push({ label: t('compare:kinds.table'), items: tables });
  const kinds = [...new Set(r.objects.map((o) => o.kind))];
  for (const k of kinds) {
    const items = r.objects.filter((o) => o.kind === k && shows(o.status, o.key, oid(o))).map((o) => ({ id: oid(o), key: o.key, status: o.status, object: o }));
    if (items.length) out.push({ label: KIND_LABELS.value[k] ?? k, items });
  }
  return out;
});
const selectedId = ref<string | null>(null);
const selected = computed(() => groups.value.flatMap((g) => g.items).find((i) => i.id === selectedId.value) ?? null);
// The selected object stays listed while it exists; if a change removed it
// from both sides, nothing is selected.
watch(result, () => { if (selectedId.value && !selected.value && result.value) selectedId.value = null; });

// -- pending changes ---------------------------------------------------------------------------
const tkey = (t: { schema: string | null; name: string }) => `${(t.schema ?? '').toLowerCase()}.${t.name.toLowerCase()}`;
const okey = (o: CodeObject) => `${o.kind}:${tkey(o)}`;

function changesOf(s: SideId): { tables: TableChange[]; objects: ObjectChange[] } {
  const side = sides[s];
  if (!side.orig || !side.work) return { tables: [], objects: [] };
  const tables: TableChange[] = [];
  const work = new Map(side.work.tables.map((t) => [tkey(t), t]));
  const orig = new Map(side.orig.tables.map((t) => [tkey(t), t]));
  for (const [k, o] of orig) {
    const w = work.get(k);
    if (!w) tables.push({ op: 'drop', table: o });
    else if (JSON.stringify(o) !== JSON.stringify(w)) tables.push({ op: 'alter', old: o, new: w });
  }
  for (const [k, w] of work) if (!orig.has(k)) tables.push({ op: 'create', table: w });
  const objects: ObjectChange[] = [];
  const wo = new Map(side.work.objects.map((o) => [okey(o), o]));
  const oo = new Map(side.orig.objects.map((o) => [okey(o), o]));
  for (const [k, o] of oo) {
    const w = wo.get(k);
    if (!w) objects.push({ op: 'drop', object: o });
    else if (w.definition !== o.definition) objects.push({ op: 'replace', object: w });
  }
  for (const [k, w] of wo) if (!oo.has(k)) objects.push({ op: 'create', object: w });
  return { tables, objects };
}
const pending = computed(() => ({ left: changesOf('left'), right: changesOf('right') }));
const pendingCount = (s: SideId) => pending.value[s].tables.length + pending.value[s].objects.length;
const pendingKeys = computed(() => {
  const set = new Set<string>();
  for (const s of ['left', 'right'] as SideId[]) {
    for (const c of pending.value[s].tables) set.add(`t:${tkey(c.op === 'alter' ? c.new : c.table)}`);
    for (const c of pending.value[s].objects) set.add(`o:${okey(c.object)}`);
  }
  return set;
});
const isPending = (item: { table?: TableDiff; object?: ObjectDiff }) => {
  if (item.table) {
    const t = item.table;
    const any = [views.left?.tables[t.left ?? -1], views.right?.tables[t.right ?? -1]].filter(Boolean) as TableSchema[];
    return any.some((x) => pendingKeys.value.has(`t:${tkey(x)}`)) || [...pendingKeys.value].some((k) => k === `t:${t.key.toLowerCase()}`);
  }
  if (item.object) {
    const o = item.object;
    const any = [views.left?.objects[o.left ?? -1], views.right?.objects[o.right ?? -1]].filter(Boolean) as CodeObject[];
    return any.some((x) => pendingKeys.value.has(`o:${okey(x)}`));
  }
  return false;
};

// -- undo --------------------------------------------------------------------------------------
const history: { left: string; right: string }[] = [];
const canUndo = ref(false);
function snapshot() {
  history.push({ left: JSON.stringify(sides.left.work), right: JSON.stringify(sides.right.work) });
  if (history.length > 50) history.shift();
  canUndo.value = true;
}
function undo() {
  const h = history.pop();
  canUndo.value = history.length > 0;
  if (!h) return;
  sides.left.work = JSON.parse(h.left);
  sides.right.work = JSON.parse(h.right);
  recompare();
}
function discard(s: SideId) {
  if (!sides[s].orig) return;
  snapshot();
  sides[s].work = clone(sides[s].orig);
  recompare();
}

// -- carrying changes ------------------------------------------------------------------------
/** The source's tables in the target's terms (engine and schema). */
async function convertFor(from: SideId, tables: TableSchema[]): Promise<TableSchema[]> {
  const src = sides[from];
  const dst = sides[other(from)];
  const targetSchema = ignoreSchema.value ? dst.schema : null;
  const r = await compareApi.convert(src.work!.driver, dst.work!.driver, clone(tables), targetSchema);
  if (r.warnings.length) ElMessage.warning({ message: r.warnings.slice(0, 3).map(tb).join(' · '), duration: 5000 });
  // References to the source schema follow the tables to the target's.
  if (ignoreSchema.value) {
    for (const t of r.tables) for (const fk of t.foreign_keys) if (fk.ref_schema === src.schema) fk.ref_schema = dst.schema;
  }
  return r.tables;
}

function replaceTable(s: SideId, old: TableSchema | null, next: TableSchema | null) {
  const list = sides[s].work!.tables;
  const i = old ? list.indexOf(old) : -1;
  if (next && i >= 0) list.splice(i, 1, next);
  else if (next) list.push(next);
  else if (i >= 0) list.splice(i, 1);
}

/** Make the other side's table like this one (`from`), whole. */
async function pushTable(t: TableDiff, from: SideId) {
  const to = other(from);
  const src = views[from]!.tables[t[from] ?? -1] ?? null;
  const dst = views[to]!.tables[t[to] ?? -1] ?? null;
  try {
    snapshot();
    touched.add(tid(t));
    if (!src) {
      replaceTable(to, dst, null);
    } else {
      const [conv] = await convertFor(from, [src]);
      // Keep the target's own spelling of its name.
      if (dst) { conv.name = dst.name; conv.schema = dst.schema; }
      replaceTable(to, dst, conv);
    }
    await recompare();
  } catch (e) {
    history.pop();
    ElMessage.error(errorMessage(e));
  }
}

type Section = 'columns' | 'indexes' | 'foreign_keys' | 'checks';
/** Carry one column, index or foreign key (or the primary key) to the other side. */
async function pushItem(t: TableDiff, section: Section | 'primary_key' | 'props', item: ItemDiff | null, from: SideId) {
  const to = other(from);
  const src = views[from]!.tables[t[from] ?? -1];
  const dst = views[to]!.tables[t[to] ?? -1];
  if (!src || !dst) return;
  try {
    snapshot();
    touched.add(tid(t));
    touched.add(itemKey(t, section, item));
    const [conv] = await convertFor(from, [src]);
    const next: TableSchema = clone(dst);
    if (section === 'primary_key') {
      next.primary_key = conv.primary_key ? { ...conv.primary_key } : null;
    } else if (section === 'props') {
      next.comment = conv.comment;
      next.options = { ...(conv.options ?? {}) };
    } else if (item) {
      const si = item[from];
      const di = item[to];
      // The converted table keeps the source's order.
      const srcItem = si !== null ? clone((conv[section] as unknown[])[si]) : null;
      if (section === 'checks' && !next.checks) next.checks = [];
      const list = next[section] as unknown[];
      if (!srcItem && di !== null) {
        list.splice(di, 1);
      } else if (srcItem && di !== null) {
        if (section === 'columns') (srcItem as ColumnDef).name = (list[di] as ColumnDef).name;
        list.splice(di, 1, srcItem);
      } else if (srcItem) {
        if (section === 'columns') {
          // After the nearest preceding column the target also has.
          let at = list.length;
          for (let k = (si ?? 0) - 1; k >= 0; k--) {
            const prev = conv.columns[k]?.name.toLowerCase();
            const j = next.columns.findIndex((c) => c.name.toLowerCase() === prev);
            if (j >= 0) { at = j + 1; break; }
          }
          list.splice(at, 0, srcItem);
        } else {
          list.push(srcItem);
        }
      }
    }
    replaceTable(to, dst, next);
    await recompare();
  } catch (e) {
    history.pop();
    ElMessage.error(errorMessage(e));
  }
}

async function pushObject(o: ObjectDiff, from: SideId) {
  const to = other(from);
  if (sides.left.work?.driver !== sides.right.work?.driver) {
    ElMessage.warning(t('compare:objectsSameEngine'));
    return;
  }
  const src = views[from]!.objects[o[from] ?? -1] ?? null;
  const dst = views[to]!.objects[o[to] ?? -1] ?? null;
  snapshot();
  touched.add(oid(o));
  const list = sides[to].work!.objects;
  const i = dst ? list.indexOf(dst) : -1;
  if (!src) { if (i >= 0) list.splice(i, 1); }
  else {
    const copy = clone(src);
    if (ignoreSchema.value) copy.schema = sides[to].schema;
    if (i >= 0) list.splice(i, 1, copy);
    else list.push(copy);
  }
  await recompare();
}

/** What an arrow does, for its tooltip. */
function arrowTip(status: Status, from: SideId, what: 'table' | 'object' | 'item' = 'table') {
  const to = from === 'left' ? 'Right' : 'Left';
  const srcMissing = (status === 'only_right' && from === 'left') || (status === 'only_left' && from === 'right');
  const dstMissing = (status === 'only_left' && from === 'left') || (status === 'only_right' && from === 'right');
  const action = srcMissing ? 'delete' : dstMissing ? 'create' : 'copy';
  return t(`compare:arrow.${what}.${action}${to}`);
}

// -- detail --------------------------------------------------------------------------------------
const selTable = computed(() => selected.value?.table ?? null);
const selObject = computed(() => selected.value?.object ?? null);
const tableOf = (s: SideId) => (selTable.value ? views[s]?.tables[selTable.value[s] ?? -1] ?? null : null);
const objectOf = (s: SideId) => (selObject.value ? views[s]?.objects[selObject.value[s] ?? -1] ?? null : null);

function colParts(c: ColumnDef) {
  return [
    { f: 'type', t: c.data_type },
    { f: 'nullable', t: c.nullable ? 'NULL' : 'NOT NULL' },
    { f: 'default', t: c.default_value ? `DEFAULT ${c.default_value}` : '' },
    { f: 'auto_increment', t: c.auto_increment ? t('compare:autoIncrement') : '' },
    { f: 'comment', t: c.comment ? `— ${c.comment}` : '' },
    { f: 'options', t: optText(c.options) },
  ].filter((p) => p.t);
}
function optText(o: Record<string, string> | undefined) {
  return Object.entries(o ?? {}).map(([k, v]) => `${k} = ${v}`).join(', ');
}
/** The table's own properties (comment, engine options), as shown. */
function propParts(t: TableSchema | null) {
  if (!t) return [];
  return [
    { f: 'comment', t: t.comment ? `— ${t.comment}` : '' },
    { f: 'options', t: optText(t.options) },
  ].filter((p) => p.t);
}
function ixParts(ix: IndexDef) {
  return [
    { f: 'unique', t: ix.unique ? 'UNIQUE' : '' },
    { f: 'kind', t: ix.kind ?? '' },
    { f: 'columns', t: `(${ix.columns.join(', ')})` },
    { f: 'include', t: ix.include?.length ? `INCLUDE (${ix.include.join(', ')})` : '' },
    { f: 'filter', t: ix.filter ? `WHERE ${ix.filter}` : '' },
    { f: 'options', t: Object.entries(ix.options ?? {}).map(([k, v]) => `${k} = ${v}`).join(', ') },
  ].filter((p) => p.t);
}
function checkParts(c: CheckDef) {
  return [{ f: 'expression', t: `CHECK ${c.expression}` }];
}
function fkParts(fk: ForeignKeyDef) {
  return [
    { f: 'columns', t: `(${fk.columns.join(', ')}) → ${fk.ref_schema ? fk.ref_schema + '.' : ''}${fk.ref_table} (${fk.ref_columns.join(', ')})` },
    { f: 'on_delete', t: fk.on_delete ? `ON DELETE ${fk.on_delete}` : '' },
    { f: 'on_update', t: fk.on_update ? `ON UPDATE ${fk.on_update}` : '' },
  ].filter((p) => p.t);
}
const SECTIONS = computed((): { id: Section; label: string }[] => [
  { id: 'columns', label: t('compare:sections.columns') },
  { id: 'indexes', label: t('compare:sections.indexes') },
  { id: 'foreign_keys', label: t('compare:sections.foreignKeys') },
  { id: 'checks', label: t('compare:sections.checks') },
]);
type Item = ColumnDef | IndexDef | ForeignKeyDef | CheckDef;
function itemOf(s: SideId, section: Section, d: ItemDiff): Item | null {
  const t = tableOf(s);
  const i = d[s];
  return t && i !== null ? ((t[section] ?? [])[i] as Item) : null;
}
function partsOf(section: Section, x: Item) {
  if (section === 'columns') return colParts(x as ColumnDef);
  if (section === 'indexes') return ixParts(x as IndexDef);
  if (section === 'checks') return checkParts(x as CheckDef);
  return fkParts(x as ForeignKeyDef);
}
function titleOf(section: Section, x: Item) {
  return section === 'foreign_keys' || section === 'checks' ? (x as ForeignKeyDef | CheckDef).name ?? '' : (x as ColumnDef).name;
}
const pkText = (t: TableSchema | null) => (t?.primary_key?.columns.length ? `PRIMARY KEY (${t.primary_key.columns.join(', ')})` : '');
/** A detail row that became equal by an arrow and isn't applied yet. */
const rowMark = (k: string) => touched.has(k) && !!selected.value && isPending(selected.value);
const codeDiff = computed(() => (selObject.value ? lineDiff(objectOf('left')?.definition ?? '', objectOf('right')?.definition ?? '') : []));

// -- sync --------------------------------------------------------------------------------------
// One "Sincronizar" for both sides: a script per side with pending changes,
// one tab each; "Ejecutar" runs left then right, each on its own connection,
// and stops at the first failure.
interface SyncTab { side: SideId; script: SyncScript | null; error: string | null; done: boolean }
const sync = reactive<{ open: boolean; tabs: SyncTab[]; active: SideId; loading: boolean; running: boolean; error: string | null }>({
  open: false, tabs: [], active: 'left', loading: false, running: false, error: null,
});
const SIDES: SideId[] = ['left', 'right'];
const sidesWithChanges = computed(() => SIDES.filter((s) => pendingCount(s) > 0));
const totalPending = computed(() => pendingCount('left') + pendingCount('right'));
const activeTab = computed(() => sync.tabs.find((x) => x.side === sync.active) ?? null);
function scriptText(tab: SyncTab | null) {
  if (!tab?.script) return '';
  const sep = driverOf(sides[tab.side].connectionId)?.script_separator ?? '';
  return tab.script.statements.join(sep ? `\n${sep}\n` : '\n\n');
}
const sideName = (s: SideId) => {
  const side = sides[s];
  const c = connName(side.connectionId);
  return side.database ? `${c} / ${side.database}` : c;
};
const tabLabel = (s: SideId) => `${s === 'left' ? t('compare:left') : t('compare:right')} · ${sideName(s)}`;
const where = (s: SideId) => `«${sides[s].database || connName(sides[s].connectionId)}» (${connName(sides[s].connectionId)})`;
const readOnly = (s: SideId) => !!conns.byId(sides[s].connectionId)?.config.read_only;
function syncBlocked(s: SideId): string | null {
  if (readOnly(s)) return t('compare:sync.readOnly');
  const d = driverOf(sides[s].connectionId);
  if (d && !d.supports_schema_sync) return t('compare:sync.unsupported', { engine: d.name });
  return null;
}
/** Why "Sincronizar" can't run: a side with changes that can't take them. */
const syncBlockedAny = computed(() => {
  for (const s of sidesWithChanges.value) {
    const b = syncBlocked(s);
    if (b) return `${s === 'left' ? t('compare:left') : t('compare:right')}: ${b}`;
  }
  return null;
});
const syncTip = computed(() => {
  if (!totalPending.value) return t('compare:sync.noChanges');
  if (syncBlockedAny.value) return syncBlockedAny.value;
  return t('compare:sync.changes', { count: totalPending.value, left: pendingCount('left'), right: pendingCount('right') });
});
/** Pending changes that came out as no statement and no warning: nothing would apply them. */
const emptyScript = (tab: SyncTab | null) => !!tab?.script && !tab.done && !tab.error && !tab.script.statements.length && !tab.script.warnings.length;
/** Something to run, and every side's script generated. */
const canRun = computed(() => sync.tabs.some((x) => !x.done && x.script?.statements.length) && sync.tabs.every((x) => x.done || (x.script && !x.error)));

async function openSync() {
  const list = sidesWithChanges.value;
  if (!list.length) return;
  sync.tabs = list.map((side) => ({ side, script: null, error: null, done: false }));
  sync.active = list[0];
  sync.error = null;
  sync.open = true;
  sync.loading = true;
  await Promise.all(sync.tabs.map(async (tab) => {
    const s = tab.side;
    try {
      const c = pending.value[s];
      tab.script = await compareApi.script(sides[s].connectionId, c.tables, c.objects, sides[s].work?.objects ?? []);
    } catch (e) {
      tab.error = errorMessage(e);
    }
  }));
  sync.loading = false;
}

function syncCopy(tab: SyncTab | null) {
  if (!tab?.script) return;
  navigator.clipboard.writeText(scriptText(tab)).then(() => ElMessage.success({ message: t('common:copied'), duration: 1200 })).catch(() => {});
}

function syncAsQuery(tab: SyncTab | null) {
  if (!tab?.script) return;
  const s = tab.side;
  newQuery(sides[s].connectionId, sides[s].database, scriptText(tab), t('compare:sync.queryTitle', { name: sides[s].database || connName(sides[s].connectionId) }));
  if (sync.tabs.length === 1) sync.open = false;
}

async function runSync() {
  const todo = sync.tabs.filter((x) => !x.done && x.script?.statements.length);
  if (!todo.length) return;
  const lines = todo.map((x) => t('compare:sync.confirm', { count: x.script!.statements.length, where: where(x.side), warnings: '' }));
  if (todo.length > 1) lines.push(t('compare:sync.confirmOrder'));
  if (todo.some((x) => x.script!.warnings.length)) lines.push(t('compare:sync.confirmWarnings').trim());
  try {
    await ElMessageBox.confirm(lines.join(' '), t('compare:sync.button'), {
      confirmButtonText: t('common:run'), cancelButtonText: t('common:cancel'), type: 'warning',
    });
  } catch {
    return;
  }
  sync.running = true;
  sync.error = null;
  try {
    for (const tab of todo) {
      const s = tab.side;
      const side = sides[s];
      const statements = tab.script!.statements;
      sync.active = s;
      let r: Awaited<ReturnType<typeof compareApi.run>>;
      try {
        r = await compareApi.run(side.connectionId, side.database, statements, crypto.randomUUID());
      } catch (e) {
        sync.error = t('compare:sync.failedSide', { side: tabLabel(s), message: errorMessage(e) });
        break;
      }
      // What's on this side's server now; on success its pending changes are done.
      const keep = side.work;
      try {
        await load(s, generation);
      } catch {
        /* shown next to the side */
      }
      if (r.failed) {
        side.work = keep;
        const [i, msg] = r.failed;
        sync.error = t('compare:sync.failedSide', {
          side: tabLabel(s),
          message: t('compare:sync.failed', { n: i + 1, total: statements.length, done: r.done, message: tb(msg) }),
        });
        break;
      }
      tab.done = true;
      ElMessage.success(t('compare:sync.done', { where: where(s) }));
    }
    history.length = 0;
    canUndo.value = false;
    const finished = sync.tabs.every((x) => x.done || !x.script?.statements.length);
    if (finished) {
      sync.open = false;
      touched.clear();
    }
    await recompare();
    // What the sync made equal leaves "Solo diferencias", the selected row too.
    if (finished && onlyDiff.value && selected.value?.status === 'equal') selectedId.value = null;
  } finally {
    sync.running = false;
  }
}
</script>

<template>
  <div class="cv">
    <div class="cv-bar">
      <template v-for="s in (['left', 'right'] as SideId[])" :key="s">
        <div class="cv-side">
          <span class="cv-tag">{{ s === 'left' ? $t('compare:left') : $t('compare:right') }}</span>
          <el-select v-model="sides[s].connectionId" class="cv-conn" size="small" filterable :placeholder="$t('compare:connection')">
            <el-option v-for="c in conns.list" :key="c.id" :label="c.name" :value="c.id" />
          </el-select>
          <el-select v-if="dbsOf(sides[s].connectionId).length" v-model="sides[s].database" class="cv-db" size="small" filterable :placeholder="$t('compare:database')">
            <el-option v-for="d in dbsOf(sides[s].connectionId)" :key="d" :label="d" :value="d" />
          </el-select>
          <el-select v-if="schemasOf(s).length" v-model="sides[s].schema" class="cv-schema" size="small" :placeholder="$t('compare:schema')" clearable>
            <el-option :label="$t('compare:allSchemas')" value="" />
            <el-option v-for="x in schemasOf(s)" :key="x" :label="x" :value="x" />
          </el-select>
          <el-icon v-if="sides[s].loading" class="is-loading"><ei-loading /></el-icon>
          <el-tooltip v-if="sides[s].error" :content="sides[s].error ?? ''" placement="bottom">
            <el-icon class="cv-err"><ei-warning-filled /></el-icon>
          </el-tooltip>
          <template v-if="result && pendingCount(s)">
            <span class="cv-pend" :title="$t('compare:pendingChanges')">{{ pendingCount(s) }}</span>
            <el-tooltip :content="$t('compare:discard')" placement="bottom">
              <el-button size="small" text circle @click="discard(s)"><el-icon><ei-close /></el-icon></el-button>
            </el-tooltip>
          </template>
        </div>
        <el-tooltip v-if="s === 'left'" :content="$t('compare:swap')" placement="bottom">
          <el-button size="small" circle @click="swap"><el-icon><ei-sort /></el-icon></el-button>
        </el-tooltip>
      </template>
      <div class="cv-actions">
        <el-tooltip v-if="result" :content="$t('compare:undo')" placement="bottom">
          <el-button size="small" circle :disabled="!canUndo" @click="undo"><el-icon><ei-refresh-left /></el-icon></el-button>
        </el-tooltip>
        <el-tooltip v-if="result" :content="syncTip" placement="bottom">
          <span class="cv-syncwrap">
            <el-button size="small" :type="totalPending ? 'primary' : 'default'" :disabled="!totalPending || !!syncBlockedAny" @click="openSync">
              {{ $t('compare:sync.button') }}<span v-if="totalPending" class="cv-count">{{ totalPending }}</span>
            </el-button>
          </span>
        </el-tooltip>
        <el-button v-if="comparing" type="danger" size="small" @click="stopCompare"><el-icon><ei-video-pause /></el-icon>&nbsp;{{ $t('common:stop') }}</el-button>
        <el-button v-else :type="result ? 'default' : 'primary'" size="small" @click="compareBoth"><el-icon v-if="result"><ei-refresh /></el-icon><template v-if="result">&nbsp;</template>{{ $t('compare:compare') }}</el-button>
      </div>
    </div>

    <div v-if="!result" class="cv-empty">
      <el-icon :size="36"><ei-files /></el-icon>
      <p><i18next :translation="$t('compare:empty.pick')"><template #compare><b>{{ $t('compare:compare') }}</b></template></i18next></p>
      <p class="nm-muted"><i18next :translation="$t('compare:empty.help')"><template #sync><b>{{ $t('compare:sync.button') }}</b></template></i18next></p>
    </div>

    <div v-else class="cv-body">
      <div class="cv-list" :style="{ width: listWidth + 'px' }">
        <div class="cv-sash" @pointerdown.prevent="dragList" />
        <div class="cv-summary">
          <span class="cv-chip changed">≠ {{ counts.changed }}</span>
          <span class="cv-chip only_left">◧ {{ counts.only_left }}</span>
          <span class="cv-chip only_right">◨ {{ counts.only_right }}</span>
          <span class="cv-chip equal">= {{ counts.equal }}</span>
        </div>
        <div class="cv-filters">
          <el-input v-model="filter" size="small" clearable :placeholder="$t('common:filter')"><template #prefix><el-icon><ei-search /></el-icon></template></el-input>
        </div>
        <div class="cv-checks">
          <el-checkbox v-model="onlyDiff" size="small">{{ $t('compare:onlyDiff') }}</el-checkbox>
          <el-checkbox v-model="opts.ignore_case" size="small">{{ $t('compare:ignoreCase') }}</el-checkbox>
          <el-checkbox v-model="opts.ignore_comments" size="small">{{ $t('compare:ignoreComments') }}</el-checkbox>
        </div>
        <div class="cv-items">
          <div v-if="!groups.length" class="cv-none-msg">{{ onlyDiff ? $t('compare:noDifferences') : $t('compare:noObjects') }}</div>
          <template v-for="g in groups" :key="g.label">
            <div class="cv-group">{{ g.label }} <span>{{ g.items.length }}</span></div>
            <div
              v-for="it in g.items"
              :key="it.id"
              class="cv-item"
              :class="{ sel: selectedId === it.id }"
              @click="selectedId = it.id"
            >
              <span class="cv-st" :class="it.status" :title="STATUS[it.status].label">{{ STATUS[it.status].icon }}</span>
              <span class="cv-name" :title="it.key">{{ it.key }}</span>
              <span v-if="isPending(it)" class="cv-dot" :title="$t('compare:pendingChanges')" />
              <span v-if="it.status !== 'equal'" class="cv-arrows" @click.stop>
                <button :title="arrowTip(it.status, 'right', it.table ? 'table' : 'object')" @click="it.table ? pushTable(it.table, 'right') : pushObject(it.object!, 'right')">←</button>
                <button :title="arrowTip(it.status, 'left', it.table ? 'table' : 'object')" @click="it.table ? pushTable(it.table, 'left') : pushObject(it.object!, 'left')">→</button>
              </span>
            </div>
          </template>
        </div>
      </div>

      <div class="cv-detail">
        <div v-if="!selected" class="cv-empty small">{{ $t('compare:pickItem') }}</div>

        <template v-else-if="selTable">
          <div class="cv-dhead">
            <div class="cv-dside">{{ tableOf('left') ? `${sides.left.database} · ${selTable.key}` : '—' }}</div>
            <div class="cv-mid">
              <button v-if="selTable.status !== 'equal'" :title="arrowTip(selTable.status, 'right')" @click="pushTable(selTable, 'right')">←</button>
              <button v-if="selTable.status !== 'equal'" :title="arrowTip(selTable.status, 'left')" @click="pushTable(selTable, 'left')">→</button>
            </div>
            <div class="cv-dside">{{ tableOf('right') ? `${sides.right.database} · ${selTable.key}` : '—' }}</div>
          </div>
          <div class="cv-grid">
            <template v-if="tableOf('left') && tableOf('right')">
              <template v-for="sec in SECTIONS" :key="sec.id">
                <div v-if="(selTable[sec.id] ?? []).length" class="cv-sec">{{ sec.label }}</div>
                <div v-for="d in (selTable[sec.id] ?? []).filter((x) => !onlyDiff || x.status !== 'equal' || sec.id === 'columns' || touched.has(itemKey(selTable!, sec.id, x)))" :key="sec.id + d.name + d.left + d.right" class="cv-row" :class="d.status">
                  <div class="cv-cell" :class="{ none: d.left === null }">
                    <template v-if="itemOf('left', sec.id, d)">
                      <b>{{ titleOf(sec.id, itemOf('left', sec.id, d)!) }}</b>
                      <span v-for="p in partsOf(sec.id, itemOf('left', sec.id, d)!)" :key="p.f" :class="{ hl: d.fields.includes(p.f) }">{{ p.t }}</span>
                    </template>
                  </div>
                  <div class="cv-mid">
                    <template v-if="d.status !== 'equal'">
                      <button :title="arrowTip(d.status, 'right', 'item')" @click="pushItem(selTable, sec.id, d, 'right')">←</button>
                      <button :title="arrowTip(d.status, 'left', 'item')" @click="pushItem(selTable, sec.id, d, 'left')">→</button>
                    </template>
                    <span v-else-if="rowMark(itemKey(selTable, sec.id, d))" class="cv-dot" :title="$t('compare:pendingChanges')" />
                  </div>
                  <div class="cv-cell" :class="{ none: d.right === null }">
                    <template v-if="itemOf('right', sec.id, d)">
                      <b>{{ titleOf(sec.id, itemOf('right', sec.id, d)!) }}</b>
                      <span v-for="p in partsOf(sec.id, itemOf('right', sec.id, d)!)" :key="p.f" :class="{ hl: d.fields.includes(p.f) }">{{ p.t }}</span>
                    </template>
                  </div>
                </div>
              </template>
              <template v-if="propParts(tableOf('left')).length || propParts(tableOf('right')).length">
                <div class="cv-sec">{{ $t('compare:tableProps') }}</div>
                <div class="cv-row" :class="selTable.fields.length ? 'changed' : 'equal'">
                  <div class="cv-cell"><span v-for="p in propParts(tableOf('left'))" :key="p.f" :class="{ hl: selTable.fields.includes(p.f) }">{{ p.t }}</span></div>
                  <div class="cv-mid">
                    <template v-if="selTable.fields.length">
                      <button :title="$t('compare:arrow.props.copyLeft')" @click="pushItem(selTable, 'props', null, 'right')">←</button>
                      <button :title="$t('compare:arrow.props.copyRight')" @click="pushItem(selTable, 'props', null, 'left')">→</button>
                    </template>
                    <span v-else-if="rowMark(itemKey(selTable, 'props', null))" class="cv-dot" :title="$t('compare:pendingChanges')" />
                  </div>
                  <div class="cv-cell"><span v-for="p in propParts(tableOf('right'))" :key="p.f" :class="{ hl: selTable.fields.includes(p.f) }">{{ p.t }}</span></div>
                </div>
              </template>
              <div v-if="pkText(tableOf('left')) || pkText(tableOf('right'))" class="cv-sec">{{ $t('compare:primaryKey') }}</div>
              <div v-if="pkText(tableOf('left')) || pkText(tableOf('right'))" class="cv-row" :class="selTable.primary_key">
                <div class="cv-cell" :class="{ none: !pkText(tableOf('left')) }"><span :class="{ hl: selTable.primary_key !== 'equal' }">{{ pkText(tableOf('left')) }}</span></div>
                <div class="cv-mid">
                  <template v-if="selTable.primary_key !== 'equal'">
                    <button :title="$t('compare:arrow.pk.copyLeft')" @click="pushItem(selTable, 'primary_key', null, 'right')">←</button>
                    <button :title="$t('compare:arrow.pk.copyRight')" @click="pushItem(selTable, 'primary_key', null, 'left')">→</button>
                  </template>
                  <span v-else-if="rowMark(itemKey(selTable, 'primary_key', null))" class="cv-dot" :title="$t('compare:pendingChanges')" />
                </div>
                <div class="cv-cell" :class="{ none: !pkText(tableOf('right')) }"><span :class="{ hl: selTable.primary_key !== 'equal' }">{{ pkText(tableOf('right')) }}</span></div>
              </div>
            </template>
            <template v-else>
              <!-- Only on one side: its columns, and the whole-table arrows above. -->
              <div class="cv-sec">{{ $t('compare:sections.columns') }}</div>
              <div v-for="c in (tableOf('left') ?? tableOf('right'))!.columns" :key="c.name" class="cv-row" :class="selTable.status">
                <div class="cv-cell" :class="{ none: !tableOf('left') }">
                  <template v-if="tableOf('left')"><b>{{ c.name }}</b><span v-for="p in colParts(c)" :key="p.f">{{ p.t }}</span></template>
                </div>
                <div class="cv-mid" />
                <div class="cv-cell" :class="{ none: !tableOf('right') }">
                  <template v-if="tableOf('right')"><b>{{ c.name }}</b><span v-for="p in colParts(c)" :key="p.f">{{ p.t }}</span></template>
                </div>
              </div>
            </template>
          </div>
        </template>

        <template v-else-if="selObject">
          <div class="cv-dhead">
            <div class="cv-dside">{{ objectOf('left') ? `${sides.left.database} · ${selObject.key}` : '—' }}</div>
            <div class="cv-mid">
              <button v-if="selObject.status !== 'equal'" :title="arrowTip(selObject.status, 'right', 'object')" @click="pushObject(selObject, 'right')">←</button>
              <button v-if="selObject.status !== 'equal'" :title="arrowTip(selObject.status, 'left', 'object')" @click="pushObject(selObject, 'left')">→</button>
            </div>
            <div class="cv-dside">{{ objectOf('right') ? `${sides.right.database} · ${selObject.key}` : '—' }}</div>
          </div>
          <div class="cv-code">
            <div v-for="(l, i) in codeDiff" :key="i" class="cv-cline" :class="l.kind">
              <pre class="cv-cl" :class="{ none: l.left === null }">{{ l.left ?? '' }}</pre>
              <pre class="cv-cl" :class="{ none: l.right === null }">{{ l.right ?? '' }}</pre>
            </div>
          </div>
        </template>
      </div>
    </div>

    <el-dialog
      :model-value="sync.open"
      :title="$t('compare:sync.button')"
      width="860px"
      top="6vh"
      append-to-body
      @close="sync.open = false"
    >
      <div v-if="sync.loading" class="cv-empty small"><el-icon class="is-loading"><ei-loading /></el-icon> {{ $t('compare:sync.generating') }}</div>
      <template v-else>
        <el-alert v-if="sync.error" type="error" :title="sync.error" :closable="false" show-icon style="margin-bottom: 8px" />
        <el-tabs v-model="sync.active" class="cv-stabs">
          <el-tab-pane v-for="tab in sync.tabs" :key="tab.side" :name="tab.side">
            <template #label>
              <span :title="tab.done ? $t('compare:sync.doneTab') : ''">{{ tabLabel(tab.side) }}</span>
              <span v-if="tab.script" class="cv-scount">{{ tab.script.statements.length }}</span>
              <el-icon v-if="tab.done" class="cv-ok"><ei-circle-check /></el-icon>
              <el-icon v-else-if="tab.error || emptyScript(tab)" class="cv-err"><ei-warning-filled /></el-icon>
            </template>
          </el-tab-pane>
        </el-tabs>
        <template v-if="activeTab">
          <el-alert v-if="activeTab.error" type="error" :title="activeTab.error" :closable="false" show-icon style="margin-bottom: 8px" />
          <el-alert v-for="w in activeTab.script?.warnings ?? []" :key="w" type="warning" :title="tb(w)" :closable="false" show-icon style="margin-bottom: 6px" />
          <el-alert v-if="emptyScript(activeTab)" type="warning" :title="$t('compare:sync.emptyScript')" :closable="false" show-icon style="margin-bottom: 6px" />
          <div v-if="activeTab.script && !emptyScript(activeTab)" class="cv-stools">
            <el-button size="small" text :disabled="!activeTab.script.statements.length" @click="syncCopy(activeTab)"><el-icon><ei-document-copy /></el-icon>&nbsp;{{ $t('common:copy') }}</el-button>
            <el-button size="small" text :disabled="!activeTab.script.statements.length" @click="syncAsQuery(activeTab)"><el-icon><ei-edit-pen /></el-icon>&nbsp;{{ $t('compare:sync.openAsQuery') }}</el-button>
          </div>
          <div v-if="activeTab.script && !emptyScript(activeTab)" class="cv-script">
            <CodeEditor :key="activeTab.side" :model-value="scriptText(activeTab)" :language="driverOf(sides[activeTab.side].connectionId)?.language" :dialect="driverOf(sides[activeTab.side].connectionId)?.dialect" read-only />
          </div>
        </template>
      </template>
      <template #footer>
        <el-button @click="sync.open = false">{{ $t('common:close') }}</el-button>
        <el-button type="danger" :disabled="!canRun" :loading="sync.running" @click="runSync">{{ $t('common:run') }}</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.cv { display: flex; flex-direction: column; height: 100%; min-height: 0; }
.cv-bar { display: flex; align-items: center; gap: 10px; padding: 8px 12px; border-bottom: 1px solid var(--nm-border); min-width: 0; }
/* One line whatever the width: the pickers shrink, the buttons don't. */
.cv-side { flex: 1 1 0; min-width: 0; display: flex; align-items: center; gap: 6px; }
.cv-side .cv-conn { flex: 1.3 1 150px; min-width: 80px; }
.cv-side .cv-db { flex: 1 1 120px; min-width: 70px; }
.cv-side .cv-schema { flex: 0.8 1 100px; min-width: 64px; }
.cv-side > :not(.el-select) { flex: none; }
.cv-syncwrap { display: inline-flex; }
.cv-pend { font-size: 11px; line-height: 16px; padding: 0 6px; border-radius: 8px; color: var(--nm-warning); background: color-mix(in srgb, var(--nm-warning) 16%, transparent); }
.cv-stabs :deep(.el-tabs__header) { margin-bottom: 8px; }
.cv-scount { margin-left: 6px; padding: 0 6px; border-radius: 8px; font-size: 11px; line-height: 16px; background: var(--ide-hover); }
.cv-ok { margin-left: 4px; color: var(--nm-success, #89d185); vertical-align: middle; }
.cv-stabs .cv-err { margin-left: 4px; vertical-align: middle; }
.cv-stools { display: flex; justify-content: flex-end; gap: 4px; margin-bottom: 4px; }
.cv-count { margin-left: 6px; padding: 0 6px; border-radius: 8px; font-size: 11px; line-height: 16px; background: rgba(255, 255, 255, 0.22); }
.cv-actions { flex: none; display: flex; align-items: center; gap: 6px; }
.cv-tag { font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.04em; color: var(--nm-text-muted); }
.cv-err { color: var(--nm-danger, #f14c4c); }
.cv-empty { flex: 1; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 6px; color: var(--nm-text-muted); text-align: center; padding: 24px; }
.cv-empty.small { flex-direction: row; }
.cv-empty p { margin: 0; max-width: 520px; }
.cv-body { flex: 1; min-height: 0; display: flex; }
.cv-list { position: relative; width: 320px; flex: none; display: flex; flex-direction: column; min-height: 0; border-right: 1px solid var(--nm-border); background: var(--ide-sidebar); }
/* Same handle as the explorer's (App.vue .ide-sash-x). */
.cv-sash { position: absolute; top: 0; right: -2px; width: 4px; height: 100%; cursor: col-resize; z-index: 10; }
.cv-sash:hover { background: var(--ide-focus); }
.cv-summary { display: flex; gap: 6px; padding: 8px 10px 4px; }
.cv-chip { font-size: 11.5px; padding: 1px 7px; border-radius: 10px; background: var(--ide-hover); }
.cv-filters { padding: 4px 10px; }
.cv-checks { display: flex; flex-wrap: wrap; gap: 0 10px; padding: 0 10px 6px; }
.cv-checks :deep(.el-checkbox) { margin-right: 0; height: 22px; }
.cv-items { flex: 1; overflow: auto; }
.cv-none-msg { padding: 16px; color: var(--nm-text-muted); font-size: 12.5px; }
.cv-group { padding: 8px 10px 3px; font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.04em; color: var(--nm-text-muted); }
.cv-group span { font-weight: 400; margin-left: 4px; }
.cv-item { display: flex; align-items: center; gap: 6px; height: 24px; padding: 0 8px 0 10px; font-size: 12.5px; cursor: pointer; }
.cv-item:hover { background: var(--ide-hover); }
.cv-item.sel { background: var(--ide-selection-focus); }
.cv-st { width: 16px; text-align: center; font-weight: 700; flex: none; }
.cv-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.cv-dot { width: 7px; height: 7px; border-radius: 50%; background: var(--nm-warning); flex: none; }
.cv-arrows { display: none; gap: 2px; }
.cv-item:hover .cv-arrows, .cv-item.sel .cv-arrows { display: inline-flex; }
.cv-arrows button, .cv-mid button {
  border: 1px solid var(--nm-border); background: var(--ide-editor); color: var(--nm-text-strong); border-radius: 4px;
  width: 24px; height: 20px; line-height: 16px; padding: 0; cursor: pointer; font-size: 13px;
}
.cv-arrows button:hover, .cv-mid button:hover { border-color: var(--ide-focus); background: color-mix(in srgb, var(--ide-focus) 20%, transparent); }
.changed { --st: var(--nm-warning); }
.only_left { --st: #3794ff; }
.only_right { --st: #89d185; }
.equal { --st: var(--nm-text-muted); }
.cv-st, .cv-chip { color: var(--st); }
.cv-detail { flex: 1; min-width: 0; display: flex; flex-direction: column; min-height: 0; }
.cv-dhead { display: grid; grid-template-columns: 1fr 64px 1fr; align-items: center; border-bottom: 1px solid var(--nm-border); background: var(--ide-sidebar); }
.cv-dside { padding: 8px 12px; font-weight: 600; color: var(--nm-text-strong); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.cv-mid { display: flex; justify-content: center; gap: 4px; }
.cv-grid { flex: 1; overflow: auto; font-size: 12.5px; }
.cv-sec { padding: 10px 12px 4px; font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.04em; color: var(--nm-text-muted); }
.cv-row { display: grid; grid-template-columns: 1fr 64px 1fr; align-items: stretch; border-bottom: 1px solid var(--nm-border-soft); }
.cv-row:not(.equal) { background: color-mix(in srgb, var(--st) 7%, transparent); }
.cv-row:not(.equal) .cv-cell:first-child { box-shadow: inset 3px 0 0 var(--st); }
.cv-mid { align-items: center; }
.cv-cell { display: flex; flex-wrap: wrap; align-items: center; gap: 2px 8px; padding: 4px 12px; min-width: 0; font-family: var(--nm-mono); font-size: 12px; }
.cv-cell b { font-family: var(--nm-font); color: var(--nm-text-strong); }
.cv-cell.none { background: repeating-linear-gradient(135deg, transparent 0 6px, color-mix(in srgb, var(--nm-text-muted) 10%, transparent) 6px 7px); }
.cv-cell .hl { color: var(--nm-warning); font-weight: 600; }
.cv-code { flex: 1; overflow: auto; font-family: var(--nm-mono); font-size: 12px; }
.cv-cline { display: grid; grid-template-columns: 1fr 1fr; }
.cv-cl { margin: 0; padding: 0 12px; white-space: pre-wrap; word-break: break-all; min-height: 18px; line-height: 18px; border-right: 1px solid var(--nm-border-soft); }
.cv-cline.changed .cv-cl { background: color-mix(in srgb, var(--nm-warning) 14%, transparent); }
.cv-cline.left .cv-cl:first-child { background: color-mix(in srgb, #3794ff 16%, transparent); }
.cv-cline.right .cv-cl:last-child { background: color-mix(in srgb, #89d185 16%, transparent); }
.cv-cl.none { background: repeating-linear-gradient(135deg, transparent 0 6px, color-mix(in srgb, var(--nm-text-muted) 10%, transparent) 6px 7px) !important; }
.cv-script { height: 52vh; border: 1px solid var(--nm-border); border-radius: 4px; overflow: hidden; display: flex; flex-direction: column; }
</style>
