<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, reactive, ref, shallowRef, watch } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import { tb } from '../i18n/backend';
import {
  compareApi, type CodeObject, type CompareResult, type DbModel, type ItemDiff, type ObjectChange, type ObjectDiff,
  type Status, type SyncScript, type TableChange, type TableDiff,
} from '../api/compare';
import type { CheckDef, ColumnDef, ForeignKeyDef, IndexDef, KeyDef, TableSchema } from '../api/schema-types';
import CodeEditor from '../components/CodeEditor.vue';
import { newQuery } from '../composables/actions';
import { lineDiff } from '../composables/lineDiff';
import CodeDiff from '../components/CodeDiff.vue';
import { badgeClass, indexUsageEntry, loadIndexUsage, seekTip, sharePct, usageBadge } from '../composables/indexUsage';
import type { Dependent, DependencyReport, DependencyTarget, IndexUsage } from '../api/types';
import { useConnectionsStore } from '../stores/connections';
import { readJson, writeJson } from '../stores/storage';
import { useTabsStore, type CompareTab } from '../stores/tabs';
import { startTask, useTasksStore, type TaskHandle } from '../stores/tasks';

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
    forgetDependents(side.connectionId, side.database);
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
  applied.clear();
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
    applied.clear();
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
  type Row = { id: string; key: string; status: Status; table?: TableDiff; object?: ObjectDiff; mark: Mark | null; ghost?: string };
  const out: { label: string; items: Row[] }[] = [];
  // Marked elements an arrow removed from both sides stay listed, to undo it.
  const here = new Set([...r.tables.map((x) => `t:${tableCk(x)}`), ...r.objects.map((o) => `o:${o.kind}:${objectCk(o)}`)]);
  const ghosts: Row[] = [...applied]
    .filter(([k, m]) => m.type !== 'item' && !here.has(k) && shows('changed', m.label, ''))
    .map(([k, m]) => ({ id: `g:${k}`, key: m.label, status: 'equal', mark: m, ghost: k }));
  const tables: Row[] = r.tables.filter((t) => shows(t.status, t.key, tid(t))).map((t) => ({ id: tid(t), key: t.key, status: t.status, table: t, mark: tableMark(t) }));
  tables.push(...ghosts.filter((g) => g.mark!.type === 'table'));
  if (tables.length) out.push({ label: t('compare:kinds.table'), items: tables });
  const kinds = [...new Set([...r.objects.map((o) => o.kind), ...ghosts.flatMap((g) => (g.mark!.okind ? [g.mark!.okind] : []))])];
  for (const k of kinds) {
    const items: Row[] = r.objects.filter((o) => o.kind === k && shows(o.status, o.key, oid(o))).map((o) => ({ id: oid(o), key: o.key, status: o.status, object: o, mark: objectMark(o) }));
    items.push(...ghosts.filter((g) => g.mark!.okind === k));
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
const isPending = (item: { table?: TableDiff; object?: ObjectDiff; ghost?: string }) => {
  if (item.ghost) return true;
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
// Each step also keeps the arrows' marks (`applied`), so an undo brings back
// which way each element had been carried.
const history: { left: string; right: string; marks: string }[] = [];
const canUndo = ref(false);
function snapshot() {
  history.push({ left: JSON.stringify(sides.left.work), right: JSON.stringify(sides.right.work), marks: JSON.stringify([...applied]) });
  if (history.length > 50) history.shift();
  canUndo.value = true;
}
function undo() {
  const h = history.pop();
  canUndo.value = history.length > 0;
  if (!h) return;
  sides.left.work = JSON.parse(h.left);
  sides.right.work = JSON.parse(h.right);
  applied.clear();
  for (const [k, m] of JSON.parse(h.marks) as [string, Mark][]) applied.set(k, m);
  prune();
  recompare();
}
function discard(s: SideId) {
  if (!sides[s].orig) return;
  snapshot();
  sides[s].work = clone(sides[s].orig);
  prune();
  recompare();
}

// -- per-element pushes: the pure part -------------------------------------------------------
// Plain functions over TableSchema values (no component state).
type Section = 'columns' | 'indexes' | 'foreign_keys' | 'checks';
/** What an item arrow carries: an item of a section, the primary key, or the comment and options. */
type ItemSection = Section | 'primary_key' | 'props';
const lc = (s: string | null | undefined) => (s ?? '').toLowerCase();
const colsKey = (cols: string[]) => cols.map(lc).join(',');
/** A table's or object's key the way the backend pairs them (case-insensitive here). */
const pairKey = (x: { schema: string | null; name: string }, noSchema: boolean) => (x.schema && !noSchema ? `${lc(x.schema)}.${lc(x.name)}` : lc(x.name));
/** JSON with sorted object keys: equal values give equal text. */
const sj = (x: unknown) => JSON.stringify(x ?? null, (_k, v) => (v && typeof v === 'object' && !Array.isArray(v) ? Object.fromEntries(Object.entries(v).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))) : v));
/** How an item is told apart, as the backend pairs them: [first try, fallback]. */
function itemIds(section: Section, x: Item): [string, string] {
  if (section === 'columns') return [lc((x as ColumnDef).name), ''];
  if (section === 'indexes') {
    const ix = x as IndexDef;
    return [lc(ix.name), `${colsKey(ix.columns)}|${ix.unique}`];
  }
  if (section === 'foreign_keys') {
    const fk = x as ForeignKeyDef;
    return [`${colsKey(fk.columns)}>${lc(fk.ref_table)}(${colsKey(fk.ref_columns)})`, lc(fk.name)];
  }
  const c = x as CheckDef;
  return [lc(c.name), c.expression.replace(/[\s()]/g, '').toLowerCase()];
}
/** Where `list` has the item that any of `probe` stands for (-1: nowhere). */
function findItem(section: Section, list: Item[] | undefined, probe: Item[]): number {
  if (!list) return -1;
  const ids = probe.map((p) => itemIds(section, p));
  for (const k of [0, 1] as const) {
    const i = list.findIndex((x) => {
      const id = itemIds(section, x)[k];
      return !!id && ids.some((p) => p[k] === id);
    });
    if (i >= 0) return i;
  }
  return -1;
}
/** Put `next` where `old` is (or at `at` when there's no `old`), or drop `old` when `next` is null. */
function replaceIn<T>(list: T[], old: T | null, next: T | null, at = list.length) {
  const i = old ? list.indexOf(old) : -1;
  if (next && i >= 0) list.splice(i, 1, next);
  else if (next) list.splice(Math.min(Math.max(at, 0), list.length), 0, next);
  else if (i >= 0) list.splice(i, 1);
}
/** Insert a column after the nearest one before it in `order` that `list` also has. */
function insertColumn(list: ColumnDef[], col: ColumnDef, order: ColumnDef[], idx: number, fallback: number) {
  let at = fallback;
  for (let k = idx - 1; k >= 0; k--) {
    const j = list.findIndex((c) => c.name.toLowerCase() === lc(order[k]?.name));
    if (j >= 0) { at = j + 1; break; }
  }
  list.splice(at, 0, col);
}
/** What an item arrow replaced; for a section item, with its position. */
type Before = { item: Item | null; at: number } | KeyDef | Pick<TableSchema, 'comment' | 'options'> | null;
const isSection = (section: ItemSection): section is Section => section !== 'primary_key' && section !== 'props';
/** The piece `before` keeps, comparable with `pieceOf`. */
const beforePiece = (section: ItemSection, before: Before | undefined) => (isSection(section) ? (before as { item: Item | null } | null)?.item ?? null : before ?? null);
/** That item (or the primary key, or the comment and options) as `t` has it. */
function pieceOf(t: TableSchema, section: ItemSection, probe: Item[]): unknown {
  if (section === 'primary_key') return t.primary_key;
  if (section === 'props') return { comment: t.comment, options: t.options };
  const list = t[section] as Item[] | undefined;
  const i = findItem(section, list, probe);
  return i >= 0 ? list![i] : null;
}
/**
 * `dst` with one item of `src` (already in dst's terms) carried over, and
 * what `dst` had there before. No item in `src` drops it from `dst`.
 */
function carryItem(dst: TableSchema, src: TableSchema, section: ItemSection, probe: Item[]): { next: TableSchema; before: Before } {
  const next: TableSchema = clone(dst);
  let before = clone(pieceOf(dst, section, probe) ?? null) as Before;
  if (isSection(section)) before = { item: before as Item | null, at: findItem(section, dst[section] as Item[] | undefined, probe) };
  if (section === 'primary_key') {
    next.primary_key = src.primary_key ? clone(src.primary_key) : null;
  } else if (section === 'props') {
    next.comment = src.comment;
    next.options = { ...(src.options ?? {}) };
  } else {
    const si = findItem(section, src[section] as Item[] | undefined, probe);
    const item = si >= 0 ? clone((src[section] as Item[])[si]) : null;
    if (section === 'checks' && !next.checks) next.checks = [];
    const list = next[section] as Item[];
    const di = findItem(section, list, probe);
    if (!item) {
      if (di >= 0) list.splice(di, 1);
    } else if (di >= 0) {
      // A column keeps the target's spelling of its name.
      if (section === 'columns') (item as ColumnDef).name = (list[di] as ColumnDef).name;
      list.splice(di, 1, item);
    } else if (section === 'columns') {
      insertColumn(list as ColumnDef[], item as ColumnDef, src.columns, si, list.length);
    } else {
      list.push(item);
    }
  }
  return { next, before };
}
/**
 * `cur` with that item put back as `base` (the side's original table) has it:
 * restored, removed, or re-added. Without an original table (it was created
 * by a whole-table arrow) `before`, what the item arrow replaced, stands in.
 */
function restoreItem(cur: TableSchema, base: TableSchema | null, before: Before | undefined, section: ItemSection, probe: Item[]): TableSchema {
  const next: TableSchema = clone(cur);
  if (section === 'primary_key') {
    const pk = base ? base.primary_key : (before as KeyDef | null);
    next.primary_key = pk ? clone(pk) : null;
    return next;
  }
  if (section === 'props') {
    const p = base ?? (before as Pick<TableSchema, 'comment' | 'options'> | null);
    next.comment = p?.comment ?? null;
    next.options = p?.options ? clone(p.options) : (p?.options as Record<string, string>);
    return next;
  }
  const orig = base ? (base[section] as Item[] | undefined) : undefined;
  const b = before as { item: Item | null; at: number } | null | undefined;
  const oi = base ? findItem(section, orig, probe) : b?.item ? b.at : -1;
  const item = base ? (oi >= 0 ? clone(orig![oi]) : null) : b?.item ? clone(b.item) : null;
  if (section === 'checks' && !next.checks) next.checks = [];
  const list = next[section] as Item[];
  const ci = findItem(section, list, probe);
  if (!item) {
    if (ci >= 0) list.splice(ci, 1);
  } else if (ci >= 0) {
    list.splice(ci, 1, item);
  } else if (section === 'columns') {
    insertColumn(list as ColumnDef[], item as ColumnDef, (base ?? cur).columns, oi, 0);
  } else {
    list.splice(oi >= 0 ? Math.min(oi, list.length) : list.length, 0, item);
  }
  // Back to no CHECK list at all when that's how the table came.
  if (section === 'checks' && !next.checks!.length && base && !base.checks) delete next.checks;
  return next;
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

function replaceTable(s: SideId, old: TableSchema | null, next: TableSchema | null, at?: number) {
  replaceIn(sides[s].work!.tables, old, next, at);
}

/**
 * Which way each element was carried. Its arrows stay after both sides
 * become equal, with the one used lit: clicking it again puts back only that
 * element on the side it went to, as `orig` has it (restored, removed or
 * re-added); the other arrow puts it back the same way and then carries it
 * in the new direction. Other elements' changes stay as they are.
 *
 * Keys: `t:<table>` and `o:<kind>:<key>` by the backend's pairing (stable
 * when a push removes one side), `t:<table>|primary_key` and `|props`, and
 * `t:<table>|<section>|<n>` for items, which are found by what they are
 * (`findItem` over `probe`), since a column or index can be named
 * differently on each side.
 *
 * Items and the whole table: a whole-table arrow replaces the target's
 * table, so it drops the item marks aimed at that side. An item arrow used
 * after it (on what still differs, e.g. between engines) is like any other:
 * undoing it restores that item from the side's original table.
 *
 * "Eliminar" (the trash) is a mark too, in the same keys: it drops the
 * element on one side or both (`drop`), so an element has either an arrow or
 * a drop. Picking the lit option again puts it back on every side it was
 * dropped from; another option or an arrow puts it back first.
 */
interface Mark {
  type: 'table' | 'object' | 'item';
  /** The side it was carried from (for a drop, a side it was not dropped from, or 'left'). */
  from: SideId;
  /** "Eliminar": dropped on that side or on both, instead of carried (`from` is then ignored). */
  drop?: DropTo;
  /** The table's (or object's) pairing key. */
  ck: string;
  label: string;
  okind?: string;
  section?: ItemSection;
  /** The item as each side had it when carried: what identifies it. */
  probe?: Item[];
  /** What the item arrow replaced (used when the target had no original table). */
  before?: Before;
  /** The same, per side, for a drop (it may hit both sides). */
  beforeBy?: Partial<Record<SideId, Before>>;
  /**
   * What went with a drop, put back by undoing it (`ck`: the holder's `tkey`,
   * any schema; `at`: where it was): for a table, the foreign keys of other
   * tables that referenced it; for a column, its table's indexes, foreign
   * keys and CHECKs that use it.
   */
  cascade?: { side: SideId; ck: string; section: Section; item: Item; at: number }[];
}
type DropTo = SideId | 'both';
/** The sides a mark changed: where it was carried to, or where it was dropped. */
const targets = (m: Mark): SideId[] => (m.drop === 'both' ? ['left', 'right'] : m.drop ? [m.drop] : [other(m.from)]);
/** An arrow that's lit (a drop lights the trash instead). */
const lit = (m: Mark | null | undefined, from: SideId) => !!m && !m.drop && m.from === from;
/** A drop shows the element struck through on that side. */
const struck = (m: Mark | null | undefined, s: SideId) => !!m?.drop && targets(m).includes(s);
const applied = reactive(new Map<string, Mark>());
let markSeq = 0;

function findTable(m: DbModel | null | undefined, s: SideId, ck: string): TableSchema | null {
  return m?.tables.find((x) => (!sides[s].schema || x.schema === sides[s].schema) && pairKey(x, ignoreSchema.value) === ck) ?? null;
}
function findObject(m: DbModel | null | undefined, s: SideId, kind: string, ck: string): CodeObject | null {
  return m?.objects.find((x) => x.kind === kind && (!sides[s].schema || x.schema === sides[s].schema) && pairKey(x, ignoreSchema.value) === ck) ?? null;
}
const tableCk = (t: TableDiff) => {
  const x = views.left?.tables[t.left ?? -1] ?? views.right?.tables[t.right ?? -1];
  return x ? pairKey(x, ignoreSchema.value) : lc(t.key);
};
const objectCk = (o: ObjectDiff) => {
  const x = views.left?.objects[o.left ?? -1] ?? views.right?.objects[o.right ?? -1];
  return x ? pairKey(x, ignoreSchema.value) : lc(o.key);
};
const itemAt = (s: SideId, t: TableDiff, section: Section, d: ItemDiff): Item | null => {
  const x = views[s]?.tables[t[s] ?? -1];
  const i = d[s];
  return x && i !== null ? ((x[section] ?? []) as Item[])[i] ?? null : null;
};
const tableMark = (t: TableDiff) => applied.get(`t:${tableCk(t)}`) ?? null;
const objectMark = (o: ObjectDiff) => applied.get(`o:${o.kind}:${objectCk(o)}`) ?? null;
const keyMark = (t: TableDiff, section: 'primary_key' | 'props') => applied.get(`t:${tableCk(t)}|${section}`) ?? null;
function itemMarkKey(t: TableDiff, section: Section, d: ItemDiff): string | null {
  const ck = tableCk(t);
  const here = (['left', 'right'] as SideId[]).map((s) => itemAt(s, t, section, d)).filter((x): x is Item => !!x);
  for (const [k, m] of applied) {
    if (m.type === 'item' && m.ck === ck && m.section === section && here.some((x) => findItem(section, m.probe, [x]) >= 0)) return k;
  }
  return null;
}
const itemMark = (t: TableDiff, section: Section, d: ItemDiff) => {
  const k = itemMarkKey(t, section, d);
  return k ? applied.get(k) ?? null : null;
};
/** Item marks of a table whose item is on neither side now (an arrow removed it): rows to undo them. */
function ghostItems(t: TableDiff, section: Section): [string, Mark][] {
  const ck = tableCk(t);
  const seen = new Set((t[section] ?? []).map((d) => itemMarkKey(t, section, d)));
  return [...applied].filter(([k, m]) => m.type === 'item' && m.ck === ck && m.section === section && !seen.has(k));
}

/** Whether side `to` still differs from `orig` in that element. */
function appliedOn(m: Mark, to: SideId): boolean {
  const side = sides[to];
  if (!side.work) return false;
  if (m.type === 'object') return sj(findObject(side.work, to, m.okind!, m.ck)) !== sj(findObject(side.orig, to, m.okind!, m.ck));
  const cur = findTable(side.work, to, m.ck);
  const base = findTable(side.orig, to, m.ck);
  if (m.type === 'table') return sj(cur) !== sj(base);
  if (!cur) return false;
  return sj(pieceOf(cur, m.section!, m.probe ?? [])) !== sj(base ? pieceOf(base, m.section!, m.probe ?? []) : beforePiece(m.section!, m.beforeBy?.[to] ?? m.before));
}
/** Whether a side the mark changed still differs from `orig` in that element. */
const stillApplied = (m: Mark) => targets(m).some((s) => appliedOn(m, s));
/** Forget the marks whose element is back as it was (a revert, undo, discard or sync). */
function prune() {
  narrowDrops();
  for (const [k, m] of applied) if (!stillApplied(m)) applied.delete(k);
}

/** Put the element back on the side(s) it was carried to or dropped from, as `orig` has it. */
function revert(m: Mark) {
  for (const to of targets(m)) {
    const side = sides[to];
    if (!side.work) continue;
    if (m.type === 'object') {
      const o = findObject(side.orig, to, m.okind!, m.ck);
      replaceIn(side.work.objects, findObject(side.work, to, m.okind!, m.ck), o ? clone(o) : null, o ? side.orig!.objects.indexOf(o) : undefined);
      continue;
    }
    const cur = findTable(side.work, to, m.ck);
    const base = findTable(side.orig, to, m.ck);
    if (m.type === 'table') replaceTable(to, cur, base ? clone(base) : null, base ? side.orig!.tables.indexOf(base) : undefined);
    else if (cur) replaceTable(to, cur, restoreItem(cur, base, m.beforeBy?.[to] ?? m.before, m.section!, m.probe ?? []));
  }
  // What went with the drop (other tables' foreign keys, a column's indexes…), where it was.
  for (const c of [...(m.cascade ?? [])].sort((a, b) => a.at - b.at)) {
    const cur = sides[c.side].work?.tables.find((x) => tkey(x) === c.ck);
    if (!cur || findItem(c.section, cur[c.section] as Item[] | undefined, [c.item]) >= 0) continue;
    const next = clone(cur);
    if (c.section === 'checks' && !next.checks) next.checks = [];
    const list = next[c.section] as Item[];
    list.splice(Math.min(c.at, list.length), 0, clone(c.item));
    replaceTable(c.side, cur, next);
  }
}

/** Make the other side's table like this one (`from`), whole. */
async function pushTable(ck: string, from: SideId) {
  const to = other(from);
  const src = findTable(sides[from].work, from, ck);
  const dst = findTable(sides[to].work, to, ck);
  if (!src) {
    replaceTable(to, dst, null);
  } else {
    const [conv] = await convertFor(from, [src]);
    // Keep the target's own spelling of its name.
    if (dst) { conv.name = dst.name; conv.schema = dst.schema; }
    replaceTable(to, dst, conv);
  }
  // The whole table replaced what single items had carried to (or dropped from) that side.
  for (const [k, m] of applied) {
    if (m.type !== 'item' || m.ck !== ck || !targets(m).includes(to)) continue;
    if (m.drop === 'both') {
      narrow(m, to);
    } else {
      applied.delete(k);
    }
  }
}

/** Carry one column, index, foreign key or CHECK (or the primary key, or the comment and options) to the other side. */
async function pushItem(m: Mark) {
  const to = other(m.from);
  const src = findTable(sides[m.from].work, m.from, m.ck);
  const dst = findTable(sides[to].work, to, m.ck);
  if (!src || !dst) return;
  const [conv] = await convertFor(m.from, [src]);
  const { next, before } = carryItem(dst, conv, m.section!, m.probe ?? []);
  m.before = before;
  replaceTable(to, dst, next);
}

function pushObject(m: Mark) {
  const to = other(m.from);
  const src = findObject(sides[m.from].work, m.from, m.okind!, m.ck);
  const dst = findObject(sides[to].work, to, m.okind!, m.ck);
  let copy: CodeObject | null = null;
  if (src) {
    copy = clone(src);
    if (ignoreSchema.value) copy.schema = sides[to].schema;
  }
  replaceIn(sides[to].work!.objects, dst, copy);
}

/** A drop on both sides that no longer holds on `gone` (its table was replaced or dropped there): a drop on the other side only. */
function narrow(m: Mark, gone: SideId) {
  m.drop = other(gone);
  m.from = gone;
  m.cascade = m.cascade?.filter((c) => c.side !== gone);
  if (m.beforeBy) delete m.beforeBy[gone];
}
/** Whether `fk` (of `owner`) points at `target`. */
const refersTo = (fk: ForeignKeyDef, owner: { schema: string | null }, target: { schema: string | null; name: string }) =>
  lc(fk.ref_table) === lc(target.name) && lc(fk.ref_schema ?? owner.schema) === lc(target.schema);
/** The other tables of side `s` with foreign keys to `target`, and those keys. */
function referencing(s: SideId, target: TableSchema): { table: TableSchema; fks: ForeignKeyDef[] }[] {
  const out: { table: TableSchema; fks: ForeignKeyDef[] }[] = [];
  for (const x of sides[s].work?.tables ?? []) {
    if (tkey(x) === tkey(target)) continue;
    const fks = x.foreign_keys.filter((fk) => refersTo(fk, x, target));
    if (fks.length) out.push({ table: x, fks });
  }
  return out;
}
/**
 * `dst` without that item (no primary key; for the props, no comment, the
 * options stay), and what it had there: `carryItem` from a table that lacks it.
 */
function removeItem(dst: TableSchema, section: ItemSection, probe: Item[]) {
  return carryItem(dst, { ...dst, primary_key: null, comment: null, columns: [], indexes: [], foreign_keys: [], checks: [] }, section, probe);
}
/** "Eliminar": take the element out of each target side's work copy (sync turns that into DROP / ALTER). */
function applyDrop(m: Mark) {
  m.beforeBy = {};
  m.cascade = [];
  for (const s of targets(m)) {
    const w = sides[s].work;
    if (!w) continue;
    if (m.type === 'object') {
      replaceIn(w.objects, findObject(w, s, m.okind!, m.ck), null);
      continue;
    }
    const cur = findTable(w, s, m.ck);
    if (!cur) continue;
    if (m.type === 'table') {
      // DROP TABLE fails while other tables reference it: their keys go first (sync drops foreign keys before tables).
      stripReferences(m, s, cur);
      replaceTable(s, cur, null);
      continue;
    }
    const { next, before } = removeItem(cur, m.section!, m.probe ?? []);
    m.beforeBy[s] = before;
    // A column takes along what uses it: the script drops those first (no
    // engine drops a column a multi-column CHECK or foreign key still uses).
    const col = m.section === 'columns' ? (pieceOf(cur, 'columns', m.probe ?? []) as ColumnDef | null) : null;
    if (col && cascadesColumns(s)) {
      for (const [section, items] of usersOf(next, col.name)) {
        const list = next[section] as Item[];
        for (const x of items) m.cascade.push({ side: s, ck: tkey(cur), section, item: clone(x), at: (cur[section] as Item[]).findIndex((y) => sj(y) === sj(x)) });
        (next as unknown as Record<Section, Item[]>)[section] = list.filter((x) => !items.includes(x));
      }
    }
    replaceTable(s, cur, next);
  }
}
/** Whether `text` names `name` as a whole identifier (quoted or not, any case). */
function mentionsName(text: string | null | undefined, name: string): boolean {
  if (!text || !name) return false;
  const esc = name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return new RegExp(`(^|[^\\p{L}\\p{N}_$#@])${esc}($|[^\\p{L}\\p{N}_$#@])`, 'iu').test(text);
}
/** Engines with a fixed schema: there a column's indexes and constraints go with it (elsewhere a field isn't dropped at all). */
const cascadesColumns = (s: SideId) => ['sql', 'cql'].includes(driverOf(sides[s].connectionId)?.language ?? 'sql');
/** `t`'s indexes, foreign keys and CHECKs that use column `col`. */
function usersOf(t: TableSchema, col: string): [Section, Item[]][] {
  const ix = t.indexes.filter((x) => x.columns.some((c) => lc(c) === lc(col) || mentionsName(c, col)) || x.include?.some((c) => lc(c) === lc(col)) || mentionsName(x.filter, col));
  const fks = t.foreign_keys.filter((fk) => fk.columns.some((c) => lc(c) === lc(col)));
  const checks = (t.checks ?? []).filter((c) => mentionsName(c.expression, col));
  return ([['indexes', ix], ['foreign_keys', fks], ['checks', checks]] as [Section, Item[]][]).filter(([, xs]) => xs.length);
}
/** Take out the foreign keys of `s`'s other tables that point at `target` (a dropped table), recording them in `m`. */
function stripReferences(m: Mark, s: SideId, target: TableSchema) {
  m.cascade ??= [];
  for (const r of referencing(s, target)) {
    const next = clone(r.table);
    const ck = tkey(r.table);
    for (const fk of r.fks) m.cascade.push({ side: s, ck, section: 'foreign_keys', item: clone(fk), at: r.table.foreign_keys.indexOf(fk) });
    next.foreign_keys = next.foreign_keys.filter((_fk, i) => !r.fks.includes(r.table.foreign_keys[i]));
    replaceTable(s, r.table, next);
  }
}
/**
 * A table still dropped keeps no foreign key pointing at it: undoing another
 * drop or an arrow can bring back a table that has one (sync would then fail
 * on DROP TABLE). Strip it again, so undoing the table drop puts it back.
 */
function recascade() {
  for (const m of applied.values()) {
    if (m.type !== 'table' || !m.drop) continue;
    for (const s of targets(m)) {
      const gone = findTable(sides[s].orig, s, m.ck);
      if (gone && sides[s].work && !findTable(sides[s].work, s, m.ck)) stripReferences(m, s, gone);
    }
  }
}

/** A drop on both sides whose element one side lost another way (its whole table dropped or replaced there): the other side's only. */
function narrowDrops() {
  for (const m of applied.values()) {
    if (m.drop !== 'both') continue;
    const held = SIDES.filter((s) => appliedOn(m, s));
    if (held.length === 1) narrow(m, other(held[0]));
  }
}

/**
 * An arrow or a drop: carries the element `m.from` → the other side (or
 * drops it where `m.drop` says), or, when it already was, puts it back first
 * (and stops there if it was this same action). One undo step either way.
 */
async function act(key: string, m: Mark) {
  const cur = applied.get(key);
  const same = !!cur && (cur.drop ?? null) === (m.drop ?? null) && (!!m.drop || cur.from === m.from);
  if (!same && !m.drop && m.type === 'object' && sides.left.work?.driver !== sides.right.work?.driver) {
    ElMessage.warning(t('compare:objectsSameEngine'));
    return;
  }
  if (cur?.probe) m.probe = [...(m.probe ?? []), ...cur.probe];
  // The list row changes id when the element leaves both sides or comes back: keep it selected.
  const keepSel = !!selected.value && rowMarkKey(selected.value) === key;
  snapshot();
  try {
    if (cur) {
      revert(cur);
      applied.delete(key);
    }
    if (!same) {
      if (m.drop) applyDrop(m);
      else if (m.type === 'table') await pushTable(m.ck, m.from);
      else if (m.type === 'object') pushObject(m);
      else await pushItem(m);
      applied.set(key, m);
    }
    recascade();
    prune();
    await recompare();
    if (keepSel) {
      const row = groups.value.flatMap((g) => g.items).find((i) => rowMarkKey(i) === key);
      if (row) selectedId.value = row.id;
    }
  } catch (e) {
    undo();
    ElMessage.error(errorMessage(e));
  }
}
/** A list row's key in `applied`. */
const rowMarkKey = (it: { table?: TableDiff; object?: ObjectDiff; ghost?: string }) =>
  it.ghost ?? (it.table ? `t:${tableCk(it.table)}` : it.object ? `o:${it.object.kind}:${objectCk(it.object)}` : '');
/** What a drop leaves the mark's `from` as: a side it wasn't dropped from. */
const keptSide = (to: DropTo): SideId => (to === 'both' ? 'left' : other(to));
/** The mark of an arrow (`from`) or a drop (`to`). */
const how = (dir: { from?: SideId; to?: DropTo }) => (dir.to ? { from: keptSide(dir.to), drop: dir.to } : { from: dir.from! });
function arrowTable(td: TableDiff, from: SideId, to?: DropTo) {
  touched.add(tid(td));
  const ck = tableCk(td);
  return act(`t:${ck}`, { type: 'table', ...how({ from, to }), ck, label: td.key });
}
function arrowObject(o: ObjectDiff, from: SideId, to?: DropTo) {
  touched.add(oid(o));
  const ck = objectCk(o);
  return act(`o:${o.kind}:${ck}`, { type: 'object', ...how({ from, to }), ck, okind: o.kind, label: o.key });
}
function arrowItem(td: TableDiff, section: Section, d: ItemDiff, from: SideId, to?: DropTo) {
  touched.add(tid(td));
  touched.add(itemKey(td, section, d));
  const ck = tableCk(td);
  const probe = itemProbe(td, section, d);
  return act(itemMarkKey(td, section, d) ?? `t:${ck}|${section}|${++markSeq}`, { type: 'item', ...how({ from, to }), ck, section, probe, label: d.name });
}
function arrowKey(td: TableDiff, section: 'primary_key' | 'props', from: SideId, to?: DropTo) {
  touched.add(tid(td));
  touched.add(itemKey(td, section, null));
  const ck = tableCk(td);
  return act(`t:${ck}|${section}`, { type: 'item', ...how({ from, to }), ck, section, label: section });
}
/** The arrows (or the trash) of an element a change removed from both sides. */
function arrowGhost(key: string, from: SideId, to?: DropTo) {
  const m = applied.get(key);
  // What the old action kept to undo itself doesn't carry over to the new one.
  if (m) return act(key, { ...m, ...how({ from, to }), drop: to, probe: [], beforeBy: undefined, cascade: undefined });
}
function arrowRow(it: { table?: TableDiff; object?: ObjectDiff; ghost?: string }, from: SideId, to?: DropTo) {
  if (it.ghost) return arrowGhost(it.ghost, from, to);
  return it.table ? arrowTable(it.table, from, to) : arrowObject(it.object!, from, to);
}
const itemProbe = (td: TableDiff, section: Section, d: ItemDiff) =>
  (['left', 'right'] as SideId[]).map((s) => itemAt(s, td, section, d)).filter((x): x is Item => !!x).map(clone);
/** Whether each side had the element at first: what the arrows would do once it's put back. */
function origStatus(m: Mark): Status {
  const had = (s: SideId) => {
    const side = sides[s];
    if (m.type === 'object') return !!findObject(side.orig, s, m.okind!, m.ck);
    const x = findTable(side.orig, s, m.ck);
    if (m.type === 'table' || !x) return !!x;
    return m.section === 'primary_key' || m.section === 'props' || findItem(m.section!, x[m.section!] as Item[] | undefined, m.probe ?? []) >= 0;
  };
  const l = had('left');
  const r = had('right');
  return l && !r ? 'only_left' : r && !l ? 'only_right' : 'changed';
}
/** An arrow's tooltip; the lit one undoes that element. */
function tip(m: Mark | null, status: Status, from: SideId, what: 'table' | 'object' | 'item') {
  if (lit(m, from)) return t('compare:arrow.revert');
  return arrowTip(m ? origStatus(m) : status, from, what);
}
function keyTip(td: TableDiff, section: 'primary_key' | 'props', from: SideId) {
  if (lit(keyMark(td, section), from)) return t('compare:arrow.revert');
  return t(`compare:arrow.${section === 'primary_key' ? 'pk' : 'props'}.${from === 'right' ? 'copyLeft' : 'copyRight'}`);
}

/** What an arrow does, for its tooltip. */
function arrowTip(status: Status, from: SideId, what: 'table' | 'object' | 'item' = 'table') {
  const to = from === 'left' ? 'Right' : 'Left';
  const srcMissing = (status === 'only_right' && from === 'left') || (status === 'only_left' && from === 'right');
  const dstMissing = (status === 'only_left' && from === 'left') || (status === 'only_right' && from === 'right');
  const action = srcMissing ? 'delete' : dstMissing ? 'create' : 'copy';
  return t(`compare:arrow.${what}.${action}${to}`);
}

// -- "Eliminar" -------------------------------------------------------------------------------
// The trash next to the arrows: drop the element on the left, on the right or
// on both. One small menu, shared by every row (rows only hold a button), and
// built when it opens.
interface DropOption { to: DropTo; label: string; on: boolean; reason: string | null }
interface DropMenu { title: string; options: DropOption[]; info: { text: string; tip?: string; warn?: boolean }[]; run: (to: DropTo) => unknown }
const DROP_TOS: DropTo[] = ['left', 'right', 'both'];
const sideLabel = (s: SideId) => (s === 'left' ? t('compare:left') : t('compare:right'));
/** Whether side `s` has the element in that model. */
function has(model: DbModel | null | undefined, s: SideId, m: Pick<Mark, 'type' | 'ck' | 'okind' | 'section' | 'probe'>): boolean {
  if (m.type === 'object') return !!findObject(model, s, m.okind!, m.ck);
  const b = findTable(model, s, m.ck);
  if (!b || m.type === 'table') return !!b;
  if (m.section === 'primary_key') return !!b.primary_key?.columns.length;
  if (m.section === 'props') return !!b.comment;
  return findItem(m.section as Section, b[m.section as Section] as Item[] | undefined, m.probe ?? []) >= 0;
}
/**
 * Whether side `s` has the element on its server (only that can be dropped)
 * and still in its work copy (another change, like dropping its table or the
 * column an index uses, may have taken it already).
 */
const origHas = (s: SideId, m: Pick<Mark, 'type' | 'ck' | 'okind' | 'section' | 'probe'>) => has(sides[s].orig, s, m) && has(sides[s].work, s, m);
/** Left / right / both, each with why it can't be picked; the lit one undoes the drop. */
function dropOptions(m: Mark | null, what: Pick<Mark, 'type' | 'ck' | 'okind' | 'section' | 'probe'>, extra: (s: SideId) => string | null = () => null, labels = 'drop'): DropOption[] {
  const why = (s: SideId) => {
    if (struck(m, s)) return syncBlocked(s);
    if (!origHas(s, what)) return t('compare:drop.missing');
    return syncBlocked(s) ?? extra(s);
  };
  return DROP_TOS.map((to) => {
    const on = m?.drop === to;
    return { to, on, label: t(`compare:${labels}.${to}`), reason: on ? null : to === 'both' ? why('left') ?? why('right') : why(to) };
  });
}
/** Other tables' foreign keys to that table, per side: they go with it. */
function cascadeInfo(ck: string): DropMenu['info'] {
  const info: DropMenu['info'] = [];
  const m = applied.get(`t:${ck}`);
  for (const s of SIDES) {
    const cur = findTable(sides[s].work, s, ck);
    // Already dropped there: what the drop took along.
    const taken = !cur && struck(m, s) ? (m!.cascade ?? []).filter((c) => c.side === s) : [];
    const refs = cur
      ? referencing(s, cur).map((r) => ({ table: r.table.name, fks: r.fks }))
      : [...new Set(taken.map((c) => c.ck))].map((k) => ({ table: k.slice(k.indexOf('.') + 1), fks: taken.filter((c) => c.ck === k).map((c) => c.item as ForeignKeyDef) }));
    const count = refs.reduce((n, r) => n + r.fks.length, 0);
    if (count) {
      info.push({
        text: t('compare:drop.cascade', { side: sideLabel(s), count, tables: refs.map((r) => r.table).join(', ') }),
        tip: refs.flatMap((r) => r.fks.map((fk) => `${r.table}.${fk.name ?? fk.columns.join(',')}`)).join('\n'),
      });
    }
  }
  return info;
}
/**
 * The table as side `s` has it on its server (what a drop is about, even once
 * the drop took it out of the work copy), or as the work copy has it.
 */
const baseTable = (s: SideId, ck: string) => findTable(sides[s].orig, s, ck) ?? findTable(sides[s].work, s, ck);
/**
 * A unique index, the primary key or a column (`column`) that foreign keys
 * point at (other tables' or its own table's): the engine may refuse to drop it.
 */
function referencedInfo(ck: string, cols: (s: SideId) => string[] | null, column = false): DropMenu['info'] {
  const info: DropMenu['info'] = [];
  for (const s of SIDES) {
    const cur = findTable(sides[s].work, s, ck);
    const base = baseTable(s, ck);
    const c = cols(s);
    if (!cur || !base || !c?.length) continue;
    const own = cur.foreign_keys.filter((fk) => refersTo(fk, cur, cur));
    for (const r of referencing(s, base).concat(own.length ? [{ table: cur, fks: own }] : [])) {
      for (const fk of r.fks) {
        const hit = column ? fk.ref_columns.some((x) => lc(x) === lc(c[0])) : colsKey(fk.ref_columns) === colsKey(c);
        if (hit) info.push({ warn: true, text: t(column ? 'compare:drop.referencedColumn' : 'compare:drop.referenced', { side: sideLabel(s), fk: fk.name ?? fk.columns.join(', '), table: r.table.name }) });
      }
    }
  }
  return info;
}
/** What goes with a column (its indexes, foreign keys and CHECKs), and the computed columns that use it. */
function columnInfo(ck: string, col: string): DropMenu['info'] {
  const info: DropMenu['info'] = [];
  const label: Record<Section, string> = { columns: '', indexes: t('compare:drop.what.index'), foreign_keys: t('compare:drop.what.fk'), checks: t('compare:drop.what.check') };
  for (const s of SIDES) {
    const base = baseTable(s, ck);
    if (!base || !base.columns.some((c) => lc(c.name) === lc(col))) continue;
    if (cascadesColumns(s)) {
      const name = (section: Section, x: Item) =>
        section === 'foreign_keys' ? (x as ForeignKeyDef).name ?? `(${(x as ForeignKeyDef).columns.join(', ')})` : section === 'checks' ? (x as CheckDef).name ?? (x as CheckDef).expression : (x as IndexDef).name;
      const items = usersOf(base, col).flatMap(([section, xs]) => xs.map((x) => `${label[section]} ${name(section, x)}`));
      if (items.length) info.push({ text: t('compare:drop.withColumn', { side: sideLabel(s), count: items.length, items: items.join(', ') }) });
    }
    // A computed column's expression is in its type (`AS (…)`, `GENERATED ALWAYS AS (…)`).
    for (const c of base.columns) {
      if (lc(c.name) !== lc(col) && /\bAS\s*\(|^\s*AS\s/i.test(c.data_type) && mentionsName(c.data_type, col)) {
        info.push({ warn: true, text: t('compare:drop.computed', { side: sideLabel(s), column: c.name }) });
      }
    }
  }
  return info;
}
/** MySQL needs an index to back each foreign key: dropping the only one that does fails. */
function backsFkInfo(ck: string, ix: (s: SideId) => IndexDef | null): DropMenu['info'] {
  const info: DropMenu['info'] = [];
  const prefix = (cols: string[], of: string[]) => cols.length <= of.length && cols.every((c, i) => lc(c) === lc(of[i]));
  for (const s of SIDES) {
    const cur = findTable(sides[s].work, s, ck);
    const x = ix(s);
    if (!cur || !x || driverOf(sides[s].connectionId)?.dialect !== 'mysql') continue;
    for (const fk of cur.foreign_keys) {
      if (!prefix(fk.columns, x.columns)) continue;
      const others = [...cur.indexes.filter((o) => lc(o.name) !== lc(x.name)).map((o) => o.columns), cur.primary_key?.columns ?? []];
      if (!others.some((o) => prefix(fk.columns, o))) info.push({ warn: true, text: t('compare:drop.backsFk', { side: sideLabel(s), fk: fk.name ?? fk.columns.join(', ') }) });
    }
  }
  return info;
}
/** Each side's index usage, to decide (from the badges already loaded). */
function usageInfo(name: (s: SideId) => string | null, pk: boolean): DropMenu['info'] {
  return SIDES.flatMap((s) => {
    const n = name(s);
    const b = n !== null || pk ? ixBadge(s, n, pk) : null;
    return b ? [{ text: t('compare:drop.usage', { side: sideLabel(s), usage: b.text }), tip: ixBadgeTip(s, n, pk) }] : [];
  });
}

function tableDropMenu(td: TableDiff): DropMenu {
  const ck = tableCk(td);
  return { title: td.key, options: dropOptions(tableMark(td), { type: 'table', ck }), info: cascadeInfo(ck), run: (to) => arrowTable(td, 'left', to) };
}
function objectDropMenu(o: ObjectDiff): DropMenu {
  return { title: o.key, options: dropOptions(objectMark(o), { type: 'object', ck: objectCk(o), okind: o.kind }), info: [], run: (to) => arrowObject(o, 'left', to) };
}
/** A table or object a change removed from both sides: its trash undoes (or moves) the drop. */
function ghostDropMenu(key: string, m: Mark): DropMenu {
  return { title: m.label, options: dropOptions(m, m), info: m.type === 'table' ? cascadeInfo(m.ck) : [], run: (to) => arrowGhost(key, 'left', to) };
}
function rowDropMenu(it: { table?: TableDiff; object?: ObjectDiff; ghost?: string; mark: Mark | null }): DropMenu {
  if (it.ghost && it.mark) return ghostDropMenu(it.ghost, it.mark);
  return it.table ? tableDropMenu(it.table) : objectDropMenu(it.object!);
}
function itemDropMenu(td: TableDiff, section: Section, d: ItemDiff): DropMenu {
  const ck = tableCk(td);
  const what = { type: 'item' as const, ck, section, probe: itemProbe(td, section, d) };
  let extra: (s: SideId) => string | null = () => null;
  let info: DropMenu['info'] = [];
  if (section === 'columns') {
    extra = (s) => {
      const cur = findTable(sides[s].work, s, ck);
      if (cur?.primary_key?.columns.some((c) => lc(c) === lc(d.name))) return t('compare:drop.inPk');
      return cur && cur.columns.length <= 1 ? t('compare:drop.lastColumn') : null;
    };
    info = [...columnInfo(ck, d.name), ...referencedInfo(ck, () => [d.name], true)];
  } else if (section === 'indexes') {
    // As the side's server has it: a drop already took it out of the work copy.
    const ix = (s: SideId) => {
      const b = baseTable(s, ck);
      const i = b ? findItem('indexes', b.indexes, what.probe) : -1;
      return i >= 0 ? b!.indexes[i] : null;
    };
    info = [
      ...usageInfo((s) => ix(s)?.name ?? null, false),
      ...referencedInfo(ck, (s) => (ix(s)?.unique ? ix(s)!.columns : null)),
      ...backsFkInfo(ck, ix),
    ];
  }
  return { title: d.name, options: dropOptions(itemMark(td, section, d), what, extra), info, run: (to) => arrowItem(td, section, d, 'left', to) };
}
function ghostItemDropMenu(key: string, m: Mark): DropMenu {
  const info = m.section === 'columns' ? [...columnInfo(m.ck, m.label), ...referencedInfo(m.ck, () => [m.label], true)] : [];
  return { title: m.label, options: dropOptions(m, m), info, run: (to) => arrowGhost(key, 'left', to) };
}
function keyDropMenu(td: TableDiff, section: 'primary_key' | 'props'): DropMenu {
  const ck = tableCk(td);
  const pk = section === 'primary_key';
  return {
    title: pk ? t('compare:primaryKey') : t('compare:drop.clearComment'),
    options: dropOptions(keyMark(td, section), { type: 'item', ck, section }, (s) => (pk && driverOf(sides[s].connectionId)?.id === 'cockroachdb' ? t('compare:drop.pkRequired', { engine: driverOf(sides[s].connectionId)!.name }) : null), pk ? 'drop' : 'dropComment'),
    info: pk ? [...usageInfo(() => null, true), ...referencedInfo(ck, (s) => baseTable(s, ck)?.primary_key?.columns ?? null)] : [],
    run: (to) => arrowKey(td, section, 'left', to),
  };
}

const dropMenu = reactive<{ menu: DropMenu | null; style: Record<string, string> }>({ menu: null, style: {} });
const dropEl = ref<HTMLElement | null>(null);
function openDrop(e: MouseEvent, build: () => DropMenu) {
  const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
  const up = r.bottom > window.innerHeight - 240;
  dropMenu.style = {
    ...(r.right < 380 ? { left: `${r.left}px` } : { right: `${window.innerWidth - r.right}px` }),
    ...(up ? { bottom: `${window.innerHeight - r.top + 2}px` } : { top: `${r.bottom + 2}px` }),
  };
  dropMenu.menu = build();
  window.addEventListener('pointerdown', outsideDrop, true);
  window.addEventListener('keydown', keyDrop, true);
  window.addEventListener('wheel', closeDrop, true);
  window.addEventListener('resize', closeDrop);
}
function closeDrop() {
  dropMenu.menu = null;
  window.removeEventListener('pointerdown', outsideDrop, true);
  window.removeEventListener('keydown', keyDrop, true);
  window.removeEventListener('wheel', closeDrop, true);
  window.removeEventListener('resize', closeDrop);
}
function outsideDrop(e: PointerEvent) {
  if (!dropEl.value?.contains(e.target as Node)) closeDrop();
}
function keyDrop(e: KeyboardEvent) {
  if (e.key === 'Escape') { e.stopPropagation(); closeDrop(); }
}
function pickDrop(o: DropOption) {
  const m = dropMenu.menu;
  closeDrop();
  if (m && !o.reason) m.run(o.to);
}
onBeforeUnmount(closeDrop);
/** Equal items "Solo diferencias" hides: they can be dropped once shown. */
const hiddenEqual = (td: TableDiff, section: Section) =>
  onlyDiff.value && section !== 'columns' ? (td[section] ?? []).filter((x) => x.status === 'equal' && !touched.has(itemKey(td, section, x)) && !itemMarkKey(td, section, x)).length : 0;

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
/** The selected table as side `s` has it on its server. */
const origTable = (s: SideId) => (selTable.value ? findTable(sides[s].orig, s, tableCk(selTable.value)) : null);
/** What a drop took from side `s`, shown struck through there. */
function droppedItem(m: Mark | null, s: SideId): Item | null {
  if (!m || !struck(m, s)) return null;
  const b = m.beforeBy?.[s] as { item: Item | null } | undefined;
  return b?.item ?? m.probe?.[0] ?? null;
}
const codeDiff = computed(() => (selObject.value ? lineDiff(objectOf('left')?.definition ?? '', objectOf('right')?.definition ?? '') : []));

// -- index usage: each side's numbers, read on that side's own connection -------------------
// Only on drivers that report it; cached per connection, database and table
// (composables/indexUsage), so going back to a table doesn't read it again.
function usageTable(s: SideId) {
  const tbl = tableOf(s);
  return tbl && driverOf(sides[s].connectionId)?.supports_index_usage ? { kind: 'table', schema: tbl.schema, name: tbl.name } : null;
}
watch(
  () => (['left', 'right'] as SideId[]).map((s) => { const x = usageTable(s); return x ? `${sides[s].connectionId}|${sides[s].database}|${x.schema ?? ''}|${x.name}` : ''; }).join('\n'),
  () => {
    for (const s of ['left', 'right'] as SideId[]) {
      const x = usageTable(s);
      if (x) loadIndexUsage(sides[s].connectionId, sides[s].database, x);
    }
  },
  { immediate: true },
);
/** The usage of `name` on side `s` (the primary key's with `pk`). */
function ixUsage(s: SideId, name: string | null, pk = false): IndexUsage | null {
  const x = usageTable(s);
  const report = x ? indexUsageEntry(sides[s].connectionId, sides[s].database, x)?.report : null;
  if (!report) return null;
  return report.indexes.find((i) => (pk ? i.primary_key : i.name.toLowerCase() === (name ?? '').toLowerCase())) ?? null;
}
function ixReport(s: SideId) {
  const x = usageTable(s);
  return x ? indexUsageEntry(sides[s].connectionId, sides[s].database, x) : undefined;
}
function ixBadge(s: SideId, name: string | null, pk = false) {
  // The read failed on that side: say so instead of showing nothing.
  if (ixReport(s)?.status === 'error') return { text: '—', unused: false, health: null, healthTip: ixReport(s)?.error ?? null };
  const u = ixUsage(s, name, pk);
  return u ? usageBadge(u, ixReport(s)?.report) : null;
}
function ixBadgeTip(s: SideId, name: string | null, pk = false) {
  const err = ixReport(s);
  if (err?.status === 'error') return err.error ?? '';
  const u = ixUsage(s, name, pk);
  if (!u) return '';
  const report = err?.report;
  if (report && !report.stats_available) return usageBadge(u, report)?.healthTip ?? '';
  if (u.read_share == null && !u.unused) return t('explorer:indexes.noReadsTip');
  if (u.unused) return t('compare:indexUsage.unused', { updates: u.updates.toLocaleString() });
  return [t('compare:indexUsage.share', { pct: sharePct(u.read_share ?? 0), reads: u.reads.toLocaleString() }), seekTip(u)].filter(Boolean).join('\n');
}

// -- sync --------------------------------------------------------------------------------------
// One "Sincronizar" for both sides: a script per side with pending changes,
// one tab each; "Ejecutar" runs left then right, each on its own connection,
// and stops at the first failure.
interface SyncTab { side: SideId; script: SyncScript | null; error: string | null; done: boolean; deps: DepCheck[] }
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

// -- what gets destroyed, and what depends on it ------------------------------------------------
/** What a side's script drops for good: tables (with their data), columns, code objects. */
function destructive(s: SideId) {
  const c = pending.value[s];
  const name = (x: { schema: string | null; name: string }) => (x.schema ? `${x.schema}.${x.name}` : x.name);
  const tables = c.tables.flatMap((x) => (x.op === 'drop' ? [name(x.table)] : []));
  const columns = c.tables.flatMap((x) => {
    if (x.op !== 'alter') return [];
    const kept = new Set(x.new.columns.map((col) => lc(col.name)));
    return x.old.columns.filter((col) => !kept.has(lc(col.name))).map((col) => `${name(x.old)}.${col.name}`);
  });
  const objects = c.objects.flatMap((x) => (x.op === 'drop' ? [name(x.object)] : []));
  return { tables, columns, objects, any: tables.length + columns.length + objects.length > 0 };
}
function destructiveLines(s: SideId): string[] {
  const d = destructive(s);
  const list = (xs: string[]) => (xs.length > 8 ? `${xs.slice(0, 8).join(', ')}…` : xs.join(', '));
  return [
    d.tables.length ? t('compare:sync.destructiveTables', { count: d.tables.length, names: list(d.tables) }) : '',
    d.columns.length ? t('compare:sync.destructiveColumns', { count: d.columns.length, names: list(d.columns) }) : '',
    d.objects.length ? t('compare:sync.destructiveObjects', { count: d.objects.length, names: list(d.objects) }) : '',
  ].filter(Boolean);
}

/**
 * Before a drop runs, what depends on each dropped table, column, view or
 * routine (`get_dependents`, asked on the side that drops it). It can take
 * long and queues the side's explorer reads behind it, so it's read in the
 * background, one at a time, once per database load (cached); it never
 * holds the "Ejecutar" button, and a failure only says it couldn't check.
 */
interface DepCheck {
  key: string;
  label: string;
  state: 'loading' | 'error' | 'done';
  error: string | null;
  /** Confirmed or probable: they break. */
  breaks: Dependent[];
  /** Only named inside dynamic SQL: to check by hand. */
  review: Dependent[];
  /** Definitions that couldn't be read: the list may be incomplete. */
  unreadable: string[];
  note: string | null;
}
const depCache = new Map<string, Promise<DependencyReport>>();
const depKey = (connectionId: string, database: string, x: DependencyTarget) =>
  [connectionId, database, x.object.kind, x.object.schema ?? '', x.object.name, x.column ?? ''].join('\u0001');
/** That database was read again: what depends on what may have changed. */
function forgetDependents(connectionId: string, database: string) {
  const prefix = `${connectionId}\u0001${database}\u0001`;
  for (const k of [...depCache.keys()]) if (k.startsWith(prefix)) depCache.delete(k);
}
function dependentsOf(s: SideId, target: DependencyTarget): Promise<DependencyReport> {
  const { connectionId, database } = sides[s];
  const k = depKey(connectionId, database, target);
  let p = depCache.get(k);
  if (!p) {
    p = api.getDependents(connectionId, database, target);
    depCache.set(k, p);
    // A failed check can be tried again next time.
    p.catch(() => { if (depCache.get(k) === p) depCache.delete(k); });
  }
  return p;
}
const qual = (x: { schema: string | null; name: string }) => `${lc(x.schema)}.${lc(x.name)}`;
/** The tables, columns and code objects side `s` drops, as dependency targets. */
function dropTargets(s: SideId): { label: string; target: DependencyTarget }[] {
  const c = pending.value[s];
  const name = (x: { schema: string | null; name: string }) => (x.schema ? `${x.schema}.${x.name}` : x.name);
  const ref = (x: TableSchema | CodeObject) => ({ kind: x.kind || 'table', schema: x.schema, name: x.name });
  const out: { label: string; target: DependencyTarget }[] = [];
  for (const x of c.tables) {
    if (x.op === 'drop') out.push({ label: name(x.table), target: { object: ref(x.table) } });
    if (x.op !== 'alter') continue;
    const kept = new Set(x.new.columns.map((col) => lc(col.name)));
    for (const col of x.old.columns) if (!kept.has(lc(col.name))) out.push({ label: `${name(x.old)}.${col.name}`, target: { object: ref(x.old), column: col.name } });
  }
  // Nothing refers to a trigger: it goes with its table or alone.
  for (const x of c.objects) if (x.op === 'drop' && x.object.kind !== 'trigger') out.push({ label: name(x.object), target: { object: ref(x.object) } });
  return out;
}
/** Split a report into what breaks and what to review, leaving out what this same script also takes away. */
function classify(s: SideId, target: DependencyTarget, report: DependencyReport): Pick<DepCheck, 'breaks' | 'review'> {
  const c = pending.value[s];
  const droppedTables = new Set(c.tables.flatMap((x) => (x.op === 'drop' ? [qual(x.table)] : [])));
  const droppedObjects = new Set(c.objects.flatMap((x) => (x.op === 'drop' ? [`${x.object.kind}:${qual(x.object)}`] : [])));
  const alters = new Map(c.tables.flatMap((x) => (x.op === 'alter' ? [[qual(x.old), x] as const] : [])));
  const self = qual(target.object);
  const handled = (d: Dependent) => {
    const dk = qual(d);
    if (d.relation === 'code') {
      if (droppedObjects.has(`${d.kind}:${dk}`)) return true;
      // The table's own triggers go with it.
      return !target.column && d.kind === 'trigger' && lc(d.parent) === lc(target.object.name) && droppedTables.has(self);
    }
    // A foreign key, index or check: `d` is the table holding it, `d.detail`
    // names the constraint as `schema_dependents` writes it.
    if (droppedTables.has(dk)) return true;
    const a = alters.get(dk);
    if (!a || a.op !== 'alter') return false;
    const detail = lc(d.detail);
    const gone = <T extends Item>(section: Section, list: T[] | undefined) => (list ?? []).filter((x) => findItem(section, a.new[section] as Item[] | undefined, [x]) < 0);
    if (d.relation === 'foreign_key') {
      return gone('foreign_keys', a.old.foreign_keys).some((fk) => (fk.name ? detail.startsWith(`${lc(fk.name)} (`) : detail.startsWith(`(${fk.columns.map(lc).join(', ')})`)));
    }
    if (d.relation === 'check') return gone('checks', a.old.checks).some((c) => (c.name ? detail.startsWith(`${lc(c.name)}: `) : detail === lc(c.expression)));
    const pk = a.old.primary_key;
    if (pk?.columns.length && detail.startsWith(`${lc(pk.name ?? 'PRIMARY KEY')} (`)) {
      // The primary key on that column: fine once it's dropped or no longer has the column.
      return sj(pk) !== sj(a.new.primary_key) && !a.new.primary_key?.columns.some((c) => lc(c) === lc(target.column));
    }
    return gone('indexes', a.old.indexes).some((x) => detail.startsWith(`${lc(x.name)} (`));
  };
  const breaks: Dependent[] = [];
  const review: Dependent[] = [];
  for (const d of report.items) {
    // A whole table: other tables' foreign keys and the code that uses it; its own indexes and checks go with it.
    if (!target.column && (d.relation === 'index' || d.relation === 'check')) continue;
    if (!target.column && d.relation === 'foreign_key' && qual(d) === self) continue;
    if (handled(d)) continue;
    (d.confidence === 'review' ? review : breaks).push(d);
  }
  return { breaks, review };
}
let depRun = 0;
/**
 * At most this many checks per side: each may take minutes on a slow engine
 * and holds the side's metadata session meanwhile. The rest say they weren't
 * checked.
 */
const DEP_LIMIT = 20;
/** Fill a sync tab's checks, one target at a time, while that dialog is the one open. */
async function checkDependents(tab: SyncTab) {
  const s = tab.side;
  if (!driverOf(sides[s].connectionId)?.supports_dependencies) return;
  const run = depRun;
  const all = dropTargets(s);
  const todo = all.slice(0, DEP_LIMIT);
  tab.deps = all.map(({ label, target }, i) => ({
    key: depKey('', '', target), label, breaks: [], review: [], unreadable: [], note: null,
    ...(i < DEP_LIMIT ? { state: 'loading' as const, error: null } : { state: 'error' as const, error: t('compare:deps.tooMany', { limit: DEP_LIMIT }) }),
  }));
  for (const [i, { target }] of todo.entries()) {
    // Closed or regenerated: what's left isn't asked (and doesn't hold the session).
    if (run !== depRun || !sync.open) {
      for (const d of tab.deps.slice(i)) if (d.state === 'loading') Object.assign(d, { state: 'error', error: t('compare:deps.stopped') });
      return;
    }
    const check = tab.deps[i];
    try {
      const report = await dependentsOf(s, target);
      Object.assign(check, classify(s, target, report), { state: 'done', unreadable: report.unreadable, note: report.note });
    } catch (e) {
      Object.assign(check, { state: 'error', error: errorMessage(e) });
    }
  }
}
const depText = (d: Dependent) => `${d.kind} ${d.schema ? `${d.schema}.` : ''}${d.name}${d.detail ? ` — ${d.detail}` : d.mentions[0] ? ` — ${d.mentions[0].line}: ${d.mentions[0].text}` : ''}`;

// A run is a task (stores/tasks.ts): "Seguir en segundo plano" closes the
// dialog and the run goes on; closing the tab doesn't stop it either (its
// session `sync:<runId>` isn't tied to the tab). Once this view is gone the
// loop still runs the remaining sides, but skips reloading them.
const tasks = useTasksStore();
const syncTask = shallowRef<TaskHandle | null>(null);
let syncRunId = '';
let alive = true;
onBeforeUnmount(() => {
  alive = false;
  // The dialog is gone with the view: the run goes on in the background (a
  // notice comes at the end) and "Ver detalle" falls back to the panel's log.
  if (sync.running) syncTask.value?.background();
  syncTask.value?.setReopen(undefined);
});
const syncCancelling = computed(() => !!syncTask.value && !!tasks.byId(syncTask.value.id)?.cancelling);
function reopenSync() {
  tabs.activate(props.tab.id);
  sync.open = true;
}
/** Closing the dialog while it runs sends it to the background (a notice comes at the end). */
function closeSync() {
  if (sync.running) syncTask.value?.background();
  sync.open = false;
}
function cancelSync() {
  if (syncTask.value) tasks.cancel(syncTask.value.id);
}

async function openSync() {
  // A run in progress: show it instead of generating new scripts over it.
  if (sync.running) { sync.open = true; return; }
  const list = sidesWithChanges.value;
  if (!list.length) return;
  sync.tabs = list.map((side) => ({ side, script: null, error: null, done: false, deps: [] }));
  sync.active = list[0];
  sync.error = null;
  sync.open = true;
  sync.loading = true;
  depRun++;
  for (const tab of sync.tabs) checkDependents(tab);
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
  if (todo.some((x) => destructive(x.side).any)) lines.push(t('compare:sync.confirmDestructive'));
  const breaks = todo.reduce((n, x) => n + x.deps.reduce((m, d) => m + d.breaks.length, 0), 0);
  if (breaks) lines.push(t('compare:sync.confirmDependents', { count: breaks }));
  if (todo.some((x) => x.deps.some((d) => d.state === 'loading'))) lines.push(t('compare:sync.confirmDepsPending'));
  const unchecked = todo.reduce((n, x) => n + x.deps.filter((d) => d.state === 'error').length, 0);
  if (unchecked) lines.push(t('compare:sync.confirmDepsFailed', { count: unchecked }));
  const incomplete = todo.reduce((n, x) => n + x.deps.filter((d) => d.state === 'done' && d.unreadable.length).length, 0);
  if (incomplete) lines.push(t('compare:sync.confirmDepsIncomplete', { count: incomplete }));
  try {
    await ElMessageBox.confirm(lines.join(' '), t('compare:sync.button'), {
      confirmButtonText: t('common:run'), cancelButtonText: t('common:cancel'), type: 'warning',
    });
  } catch {
    return;
  }
  sync.running = true;
  sync.error = null;
  const total = todo.reduce((n, x) => n + x.script!.statements.length, 0);
  const first = sides[todo[0].side];
  // An older run's task must not reopen this dialog: it now shows this run.
  syncTask.value?.setReopen(undefined);
  let settled = false;
  // The run whose session is registered (its first `schema-sync-progress`
  // came), and the run a cancel was sent to after that: that one landed.
  let readyRun = '';
  let cancelledRun = '';
  // The run in flight; a late event of a returned one would count it twice.
  let liveRun = '';
  const task = startTask({
    kind: 'sync',
    title: t('tasks:compareSync.title', { target: todo.map((x) => sides[x.side].database || connName(sides[x.side].connectionId)).join(', ') }),
    connectionId: first.connectionId,
    database: first.database,
    // The run's dedicated session: interrupts the statement running and stops the rest.
    // That session only exists once the side has connected (tunnel, login), and
    // a cancel for an unknown key is a no-op, so it's re-sent every 500 ms until
    // one goes out after the run's first progress event (the session is
    // registered by then, and its backend flag stays set), however long
    // connecting takes. Before the first run starts nothing is sent: the loop
    // sees `isCancelling` and never starts it.
    cancel: () => {
      const send = () => {
        if (!syncRunId) return undefined;
        if (readyRun === syncRunId) cancelledRun = syncRunId;
        return api.cancelQuery(`sync:${syncRunId}`).catch(() => {});
      };
      const timer = setInterval(() => {
        if (settled || (syncRunId && cancelledRun === syncRunId)) clearInterval(timer);
        else send();
      }, 500);
      return send();
    },
    reopen: reopenSync,
  });
  syncTask.value = task;
  task.progress({ done: 0, total, unit: 'statements' });
  let ran = 0;
  // Cancelled for real: a side stopped short, or a side never started.
  let stopped = false;
  try {
    // Per statement, as the backend runs them: `ran` counts the sides already done.
    await task.listen<{ run_id: string; done: number }>('schema-sync-progress', ({ payload }) => {
      if (payload.run_id !== liveRun) return;
      readyRun = payload.run_id;
      task.progress({ done: ran + payload.done });
    });
    for (const tab of todo) {
      if (task.isCancelling) { stopped = true; break; }
      const s = tab.side;
      const side = sides[s];
      const statements = tab.script!.statements;
      if (alive) sync.active = s;
      task.progress({ phase: tabLabel(s) });
      task.log(t('tasks:compareSync.sideStart', { side: tabLabel(s), count: statements.length }));
      syncRunId = crypto.randomUUID();
      let r: Awaited<ReturnType<typeof compareApi.run>>;
      liveRun = syncRunId;
      try {
        r = await compareApi.run(side.connectionId, side.database, statements, syncRunId);
      } catch (e) {
        sync.error = t('compare:sync.failedSide', { side: tabLabel(s), message: errorMessage(e) });
        break;
      } finally {
        liveRun = '';
      }
      ran += r.done;
      task.progress({ done: ran });
      // What's on this side's server now; on success its pending changes are done.
      const keep = side.work;
      if (alive) {
        try {
          await load(s, generation);
        } catch {
          /* shown next to the side */
        }
      }
      if (r.failed) {
        side.work = keep;
        const [i, msg] = r.failed;
        // Cancelled between statements, or the interrupted statement failed because of it.
        if (msg === 'cancelado' || task.isCancelling) stopped = true;
        sync.error = t('compare:sync.failedSide', {
          side: tabLabel(s),
          message: t('compare:sync.failed', { n: i + 1, total: statements.length, done: r.done, message: tb(msg) }),
        });
        break;
      }
      tab.done = true;
      task.log(t('compare:sync.done', { where: where(s) }));
      if (alive && sync.open) ElMessage.success(t('compare:sync.done', { where: where(s) }));
    }
    const summary = ran === total ? t('tasks:compareSync.summaryAll', { count: ran }) : t('tasks:compareSync.summary', { count: ran, total });
    // A cancel that arrived after every statement ran changes nothing: the task is done.
    if (stopped) task.cancelled(summary);
    else if (sync.error) { task.log(sync.error, 'error'); task.fail(sync.error); }
    else task.finish(undefined, summary);
    task.setReopen(undefined);
    if (!alive) return;
    history.length = 0;
    canUndo.value = false;
    const finished = sync.tabs.every((x) => x.done || !x.script?.statements.length);
    if (finished) {
      sync.open = false;
      touched.clear();
      applied.clear();
    } else {
      prune();
    }
    await recompare();
    // What the sync made equal leaves "Solo diferencias", the selected row too.
    if (finished && onlyDiff.value && selected.value?.status === 'equal') selectedId.value = null;
  } catch (e) {
    // Something after the run (reloading, recomparing) failed: the task may already have ended.
    if (tasks.byId(task.id)?.state === 'running') task.fail(e);
    ElMessage.error(errorMessage(e));
  } finally {
    settled = true;
    sync.running = false;
    syncRunId = '';
    // Ended: "Ver detalle" shows the panel's own detail, not this dialog (which
    // may show newer scripts by then).
    task.setReopen(undefined);
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
              <span class="cv-name" :class="{ drop: !!it.mark?.drop }" :title="it.mark?.drop ? `${it.key} · ${$t(`compare:drop.pending.${it.mark.drop}`)}` : it.key">{{ it.key }}</span>
              <span v-if="isPending(it)" class="cv-dot" :title="$t('compare:pendingChanges')" />
              <span class="cv-arrows" :class="{ keep: !!it.mark }" @click.stop>
                <template v-if="it.status !== 'equal' || it.mark">
                  <button :class="{ on: lit(it.mark, 'right') }" :title="tip(it.mark, it.status, 'right', it.table || it.mark?.type === 'table' ? 'table' : 'object')" @click="arrowRow(it, 'right')">←</button>
                  <button :class="{ on: lit(it.mark, 'left') }" :title="tip(it.mark, it.status, 'left', it.table || it.mark?.type === 'table' ? 'table' : 'object')" @click="arrowRow(it, 'left')">→</button>
                </template>
                <button class="cv-trash" :class="{ on: !!it.mark?.drop }" :title="$t('compare:drop.title')" @click="openDrop($event, () => rowDropMenu(it))"><el-icon><ei-delete /></el-icon></button>
              </span>
            </div>
          </template>
        </div>
      </div>

      <div class="cv-detail">
        <div v-if="!selected" class="cv-empty small">{{ $t('compare:pickItem') }}</div>

        <template v-else-if="selTable">
          <div class="cv-dhead">
            <div v-for="s in (['left', 'right'] as SideId[])" :key="s" class="cv-dside" :style="{ order: s === 'left' ? 0 : 2 }">
              <template v-if="tableOf(s)">{{ sides[s].database }} · {{ selTable.key }}</template>
              <span v-else-if="struck(tableMark(selTable), s)" class="cv-ghost drop" :title="$t(`compare:drop.pending.${s}`)">{{ sides[s].database }} · {{ selTable.key }}</span>
              <template v-else>—</template>
            </div>
            <div class="cv-mid" style="order: 1">
              <template v-if="selTable.status !== 'equal' || tableMark(selTable)">
                <button :class="{ on: lit(tableMark(selTable), 'right') }" :title="tip(tableMark(selTable), selTable.status, 'right', 'table')" @click="arrowTable(selTable, 'right')">←</button>
                <button :class="{ on: lit(tableMark(selTable), 'left') }" :title="tip(tableMark(selTable), selTable.status, 'left', 'table')" @click="arrowTable(selTable, 'left')">→</button>
              </template>
              <button class="cv-trash" :class="{ on: !!tableMark(selTable)?.drop }" :title="$t('compare:drop.title')" @click="openDrop($event, () => tableDropMenu(selTable!))"><el-icon><ei-delete /></el-icon></button>
            </div>
          </div>
          <div class="cv-grid">
            <template v-if="tableOf('left') && tableOf('right')">
              <template v-for="sec in SECTIONS" :key="sec.id">
                <div v-if="(selTable[sec.id] ?? []).length || ghostItems(selTable, sec.id).length" class="cv-sec">
                  {{ sec.label }}
                  <span v-if="hiddenEqual(selTable, sec.id)" class="cv-hidden" :title="$t('compare:drop.hiddenEqualTip')">{{ $t('compare:drop.hiddenEqual', { count: hiddenEqual(selTable, sec.id) }) }}</span>
                </div>
                <div v-for="d in (selTable[sec.id] ?? []).filter((x) => !onlyDiff || x.status !== 'equal' || sec.id === 'columns' || touched.has(itemKey(selTable!, sec.id, x)) || !!itemMarkKey(selTable!, sec.id, x))" :key="sec.id + d.name + d.left + d.right" class="cv-row" :class="d.status">
                  <div class="cv-cell" :class="{ none: d.left === null }">
                    <template v-if="itemOf('left', sec.id, d)">
                      <b>{{ titleOf(sec.id, itemOf('left', sec.id, d)!) }}</b>
                      <span v-for="p in partsOf(sec.id, itemOf('left', sec.id, d)!)" :key="p.f" :class="{ hl: d.fields.includes(p.f) }">{{ p.t }}</span>
                      <span
                        v-if="sec.id === 'indexes' && ixBadge('left', d.name)" class="cv-ixbadge" :class="badgeClass(ixBadge('left', d.name)!)"
                        :title="ixBadgeTip('left', d.name)"
                      >{{ ixBadge('left', d.name)!.text }}</span>
                    </template>
                    <template v-else-if="droppedItem(itemMark(selTable, sec.id, d), 'left')">
                      <b class="cv-ghost drop" :title="$t('compare:drop.pending.left')">{{ titleOf(sec.id, droppedItem(itemMark(selTable, sec.id, d), 'left')!) }}</b>
                      <span v-for="p in partsOf(sec.id, droppedItem(itemMark(selTable, sec.id, d), 'left')!)" :key="p.f" class="cv-ghost drop">{{ p.t }}</span>
                    </template>
                  </div>
                  <div class="cv-mid">
                    <template v-if="d.status !== 'equal' || itemMark(selTable, sec.id, d)">
                      <button :class="{ on: lit(itemMark(selTable, sec.id, d), 'right') }" :title="tip(itemMark(selTable, sec.id, d), d.status, 'right', 'item')" @click="arrowItem(selTable, sec.id, d, 'right')">←</button>
                      <button :class="{ on: lit(itemMark(selTable, sec.id, d), 'left') }" :title="tip(itemMark(selTable, sec.id, d), d.status, 'left', 'item')" @click="arrowItem(selTable, sec.id, d, 'left')">→</button>
                    </template>
                    <span v-else-if="rowMark(itemKey(selTable, sec.id, d))" class="cv-dot" :title="$t('compare:pendingChanges')" />
                    <button class="cv-trash" :class="{ on: !!itemMark(selTable, sec.id, d)?.drop }" :title="$t('compare:drop.title')" @click="openDrop($event, () => itemDropMenu(selTable!, sec.id, d))"><el-icon><ei-delete /></el-icon></button>
                  </div>
                  <div class="cv-cell" :class="{ none: d.right === null }">
                    <template v-if="itemOf('right', sec.id, d)">
                      <b>{{ titleOf(sec.id, itemOf('right', sec.id, d)!) }}</b>
                      <span v-for="p in partsOf(sec.id, itemOf('right', sec.id, d)!)" :key="p.f" :class="{ hl: d.fields.includes(p.f) }">{{ p.t }}</span>
                      <span
                        v-if="sec.id === 'indexes' && ixBadge('right', d.name)" class="cv-ixbadge" :class="badgeClass(ixBadge('right', d.name)!)"
                        :title="ixBadgeTip('right', d.name)"
                      >{{ ixBadge('right', d.name)!.text }}</span>
                    </template>
                    <template v-else-if="droppedItem(itemMark(selTable, sec.id, d), 'right')">
                      <b class="cv-ghost drop" :title="$t('compare:drop.pending.right')">{{ titleOf(sec.id, droppedItem(itemMark(selTable, sec.id, d), 'right')!) }}</b>
                      <span v-for="p in partsOf(sec.id, droppedItem(itemMark(selTable, sec.id, d), 'right')!)" :key="p.f" class="cv-ghost drop">{{ p.t }}</span>
                    </template>
                  </div>
                </div>
                <!-- Removed from both sides by an arrow or a drop: its name where undoing it brings it back. -->
                <div v-for="[gk, gm] in ghostItems(selTable, sec.id)" :key="gk" class="cv-row equal">
                  <div class="cv-cell none"><b v-if="targets(gm).includes('left')" class="cv-ghost" :class="{ drop: !!gm.drop }">{{ gm.label }}</b></div>
                  <div class="cv-mid">
                    <button :class="{ on: lit(gm, 'right') }" :title="tip(gm, 'equal', 'right', 'item')" @click="arrowGhost(gk, 'right')">←</button>
                    <button :class="{ on: lit(gm, 'left') }" :title="tip(gm, 'equal', 'left', 'item')" @click="arrowGhost(gk, 'left')">→</button>
                    <button class="cv-trash" :class="{ on: !!gm.drop }" :title="$t('compare:drop.title')" @click="openDrop($event, () => ghostItemDropMenu(gk, gm))"><el-icon><ei-delete /></el-icon></button>
                  </div>
                  <div class="cv-cell none"><b v-if="targets(gm).includes('right')" class="cv-ghost" :class="{ drop: !!gm.drop }">{{ gm.label }}</b></div>
                </div>
              </template>
              <template v-if="propParts(tableOf('left')).length || propParts(tableOf('right')).length || keyMark(selTable, 'props')">
                <div class="cv-sec">{{ $t('compare:tableProps') }}</div>
                <div class="cv-row" :class="selTable.fields.length ? 'changed' : 'equal'">
                  <div v-for="s in (['left', 'right'] as SideId[])" :key="s" class="cv-cell" :style="{ order: s === 'left' ? 0 : 2 }">
                    <span v-for="p in propParts(tableOf(s))" :key="p.f" :class="{ hl: selTable.fields.includes(p.f) }">{{ p.t }}</span>
                    <span v-if="struck(keyMark(selTable, 'props'), s) && origTable(s)?.comment" class="cv-ghost drop" :title="$t(`compare:drop.pending.${s}`)">— {{ origTable(s)!.comment }}</span>
                  </div>
                  <div class="cv-mid" style="order: 1">
                    <template v-if="selTable.fields.length || (keyMark(selTable, 'props') && !keyMark(selTable, 'props')!.drop)">
                      <button :class="{ on: lit(keyMark(selTable, 'props'), 'right') }" :title="keyTip(selTable, 'props', 'right')" @click="arrowKey(selTable, 'props', 'right')">←</button>
                      <button :class="{ on: lit(keyMark(selTable, 'props'), 'left') }" :title="keyTip(selTable, 'props', 'left')" @click="arrowKey(selTable, 'props', 'left')">→</button>
                    </template>
                    <span v-else-if="rowMark(itemKey(selTable, 'props', null))" class="cv-dot" :title="$t('compare:pendingChanges')" />
                    <button v-if="origTable('left')?.comment || origTable('right')?.comment" class="cv-trash" :class="{ on: !!keyMark(selTable, 'props')?.drop }" :title="$t('compare:drop.clearComment')" @click="openDrop($event, () => keyDropMenu(selTable!, 'props'))"><el-icon><ei-delete /></el-icon></button>
                  </div>
                </div>
              </template>
              <div v-if="pkText(tableOf('left')) || pkText(tableOf('right')) || keyMark(selTable, 'primary_key')" class="cv-sec">{{ $t('compare:primaryKey') }}</div>
              <div v-if="pkText(tableOf('left')) || pkText(tableOf('right')) || keyMark(selTable, 'primary_key')" class="cv-row" :class="selTable.primary_key">
                <div class="cv-cell" :class="{ none: !pkText(tableOf('left')) }">
                  <span v-if="!pkText(tableOf('left')) && struck(keyMark(selTable, 'primary_key'), 'left')" class="cv-ghost drop" :title="$t('compare:drop.pending.left')">{{ pkText(origTable('left')) }}</span>
                  <span :class="{ hl: selTable.primary_key !== 'equal' }">{{ pkText(tableOf('left')) }}</span>
                  <span v-if="pkText(tableOf('left')) && ixBadge('left', null, true)" class="cv-ixbadge" :class="badgeClass(ixBadge('left', null, true)!)" :title="ixBadgeTip('left', null, true)">{{ ixBadge('left', null, true)!.text }}</span>
                </div>
                <div class="cv-mid">
                  <template v-if="selTable.primary_key !== 'equal' || keyMark(selTable, 'primary_key')">
                    <button :class="{ on: lit(keyMark(selTable, 'primary_key'), 'right') }" :title="keyTip(selTable, 'primary_key', 'right')" @click="arrowKey(selTable, 'primary_key', 'right')">←</button>
                    <button :class="{ on: lit(keyMark(selTable, 'primary_key'), 'left') }" :title="keyTip(selTable, 'primary_key', 'left')" @click="arrowKey(selTable, 'primary_key', 'left')">→</button>
                  </template>
                  <span v-else-if="rowMark(itemKey(selTable, 'primary_key', null))" class="cv-dot" :title="$t('compare:pendingChanges')" />
                  <button class="cv-trash" :class="{ on: !!keyMark(selTable, 'primary_key')?.drop }" :title="$t('compare:drop.title')" @click="openDrop($event, () => keyDropMenu(selTable!, 'primary_key'))"><el-icon><ei-delete /></el-icon></button>
                </div>
                <div class="cv-cell" :class="{ none: !pkText(tableOf('right')) }">
                  <span v-if="!pkText(tableOf('right')) && struck(keyMark(selTable, 'primary_key'), 'right')" class="cv-ghost drop" :title="$t('compare:drop.pending.right')">{{ pkText(origTable('right')) }}</span>
                  <span :class="{ hl: selTable.primary_key !== 'equal' }">{{ pkText(tableOf('right')) }}</span>
                  <span v-if="pkText(tableOf('right')) && ixBadge('right', null, true)" class="cv-ixbadge" :class="badgeClass(ixBadge('right', null, true)!)" :title="ixBadgeTip('right', null, true)">{{ ixBadge('right', null, true)!.text }}</span>
                </div>
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
            <div v-for="s in (['left', 'right'] as SideId[])" :key="s" class="cv-dside" :style="{ order: s === 'left' ? 0 : 2 }">
              <template v-if="objectOf(s)">{{ sides[s].database }} · {{ selObject.key }}</template>
              <span v-else-if="struck(objectMark(selObject), s)" class="cv-ghost drop" :title="$t(`compare:drop.pending.${s}`)">{{ sides[s].database }} · {{ selObject.key }}</span>
              <template v-else>—</template>
            </div>
            <div class="cv-mid" style="order: 1">
              <template v-if="selObject.status !== 'equal' || objectMark(selObject)">
                <button :class="{ on: lit(objectMark(selObject), 'right') }" :title="tip(objectMark(selObject), selObject.status, 'right', 'object')" @click="arrowObject(selObject, 'right')">←</button>
                <button :class="{ on: lit(objectMark(selObject), 'left') }" :title="tip(objectMark(selObject), selObject.status, 'left', 'object')" @click="arrowObject(selObject, 'left')">→</button>
              </template>
              <button class="cv-trash" :class="{ on: !!objectMark(selObject)?.drop }" :title="$t('compare:drop.title')" @click="openDrop($event, () => objectDropMenu(selObject!))"><el-icon><ei-delete /></el-icon></button>
            </div>
          </div>
          <CodeDiff :lines="codeDiff" />
        </template>

        <!-- A table or object an arrow or a drop removed from both sides: only its arrows and trash, to undo it. -->
        <template v-else-if="selected.ghost && selected.mark">
          <div class="cv-dhead">
            <div class="cv-dside"><span v-if="targets(selected.mark).includes('left')" class="cv-ghost" :class="{ drop: !!selected.mark.drop }">{{ selected.key }}</span><template v-else>—</template></div>
            <div class="cv-mid">
              <button :class="{ on: lit(selected.mark, 'right') }" :title="tip(selected.mark, 'equal', 'right', selected.mark.type === 'table' ? 'table' : 'object')" @click="arrowGhost(selected.ghost, 'right')">←</button>
              <button :class="{ on: lit(selected.mark, 'left') }" :title="tip(selected.mark, 'equal', 'left', selected.mark.type === 'table' ? 'table' : 'object')" @click="arrowGhost(selected.ghost, 'left')">→</button>
              <button class="cv-trash" :class="{ on: !!selected.mark.drop }" :title="$t('compare:drop.title')" @click="openDrop($event, () => ghostDropMenu(selected!.ghost!, selected!.mark!))"><el-icon><ei-delete /></el-icon></button>
            </div>
            <div class="cv-dside"><span v-if="targets(selected.mark).includes('right')" class="cv-ghost" :class="{ drop: !!selected.mark.drop }">{{ selected.key }}</span><template v-else>—</template></div>
          </div>
          <p v-if="selected.mark.drop" class="cv-ghost-note">{{ $t(`compare:drop.pending.${selected.mark.drop}`) }}</p>
        </template>
      </div>
    </div>

    <el-dialog
      :model-value="sync.open"
      :title="$t('compare:sync.button')"
      width="860px"
      top="6vh"
      append-to-body
      @close="closeSync"
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
          <el-alert v-if="!activeTab.done && destructiveLines(activeTab.side).length" type="error" :closable="false" show-icon style="margin-bottom: 6px">
            <template #title><div v-for="l in destructiveLines(activeTab.side)" :key="l">{{ l }}</div></template>
          </el-alert>
          <div v-if="!activeTab.done && activeTab.deps.length" class="cv-deps">
            <div class="cv-deps-h">{{ $t('compare:deps.title') }}</div>
            <div v-for="d in activeTab.deps" :key="d.key" class="cv-dep">
              <div class="cv-dep-line">
                <b>{{ d.label }}</b>
                <span v-if="d.state === 'loading'" class="cv-dep-muted"><el-icon class="is-loading"><ei-loading /></el-icon> {{ $t('compare:deps.loading') }}</span>
                <span v-else-if="d.state === 'error'" class="cv-dep-warn" :title="d.error ?? ''">{{ $t('compare:deps.failed') }}</span>
                <template v-else>
                  <span v-if="d.breaks.length" class="cv-dep-bad">{{ $t('compare:deps.breaks', { count: d.breaks.length }) }}</span>
                  <span v-if="d.review.length" class="cv-dep-warn">{{ $t('compare:deps.review', { count: d.review.length }) }}</span>
                  <span v-if="d.unreadable.length" class="cv-dep-warn" :title="d.unreadable.join('\n')">{{ $t('compare:deps.incomplete', { count: d.unreadable.length }) }}</span>
                  <span v-if="!d.breaks.length && !d.review.length" class="cv-dep-muted">{{ d.unreadable.length ? $t('compare:deps.noneFound') : $t('compare:deps.none') }}</span>
                </template>
              </div>
              <div v-for="(x, i) in d.breaks" :key="'b' + i" class="cv-dep-item bad">{{ depText(x) }}</div>
              <div v-for="(x, i) in d.review" :key="'r' + i" class="cv-dep-item warn">{{ depText(x) }}</div>
              <div v-if="d.note" class="cv-dep-item">{{ tb(d.note) }}</div>
            </div>
          </div>
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
        <template v-if="sync.running">
          <el-button :disabled="syncCancelling" @click="cancelSync">{{ syncCancelling ? $t('tasks:panel.cancelling') : $t('tasks:panel.cancel') }}</el-button>
          <el-button type="primary" @click="closeSync">{{ $t('tasks:panel.background') }}</el-button>
        </template>
        <el-button v-else @click="closeSync">{{ $t('common:close') }}</el-button>
        <el-button type="danger" :disabled="!canRun" :loading="sync.running" @click="runSync">{{ $t('common:run') }}</el-button>
      </template>
    </el-dialog>

    <Teleport to="body">
      <div v-if="dropMenu.menu" ref="dropEl" class="cv-menu" :style="dropMenu.style" role="menu">
        <div class="cv-menu-title">{{ dropMenu.menu.title }}</div>
        <button
          v-for="o in dropMenu.menu.options" :key="o.to" class="cv-menu-item" :class="{ on: o.on }" role="menuitem"
          :disabled="!!o.reason" :title="o.on ? $t('compare:drop.undo') : o.reason ?? ''" @click="pickDrop(o)"
        >
          <el-icon class="cv-menu-check"><ei-check v-if="o.on" /></el-icon>
          <span class="cv-menu-label">{{ o.label }}</span>
          <small v-if="o.reason">{{ o.reason }}</small>
        </button>
        <div v-for="(l, i) in dropMenu.menu.info" :key="i" class="cv-menu-info" :class="{ warn: l.warn }" :title="l.tip">{{ l.text }}</div>
      </div>
    </Teleport>
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
/* The arrow already used on an element: lit, and kept visible in the list. */
.cv-arrows.keep { display: inline-flex; }
.cv-arrows button.on, .cv-mid button.on { background: var(--el-color-primary); border-color: var(--el-color-primary); color: #fff; }
.cv-ghost { color: var(--nm-text-muted); text-decoration: line-through; }
/* "Eliminar": the trash, lit red once used, and what it drops struck through in red. */
.cv-arrows button.cv-trash, .cv-mid button.cv-trash { display: inline-flex; align-items: center; justify-content: center; color: var(--nm-text-muted); font-size: 12px; }
.cv-arrows button.cv-trash:hover, .cv-mid button.cv-trash:hover { color: var(--nm-danger); border-color: var(--nm-danger); background: color-mix(in srgb, var(--nm-danger) 14%, transparent); }
.cv-arrows button.cv-trash.on, .cv-mid button.cv-trash.on { background: var(--nm-danger); border-color: var(--nm-danger); color: #fff; }
/* In the detail it stays faint until its row is pointed at (the list shows it on hover already). */
.cv-row .cv-trash:not(.on), .cv-dhead .cv-trash:not(.on) { opacity: 0.45; }
.cv-row:hover .cv-trash, .cv-dhead:hover .cv-trash { opacity: 1; }
.cv-ghost.drop, .cv-name.drop { color: var(--nm-danger); text-decoration: line-through; }
.cv-ghost-note { margin: 0; padding: 10px 12px; font-size: 12.5px; color: var(--nm-text-muted); }
.cv-hidden { margin-left: 8px; font-weight: 400; text-transform: none; letter-spacing: 0; }
.cv-menu {
  position: fixed; z-index: 3000; min-width: 220px; max-width: 380px; padding: 4px 0; border: 1px solid var(--nm-border);
  border-radius: 6px; background: var(--ide-editor); color: var(--nm-text); box-shadow: 0 6px 20px rgba(0, 0, 0, 0.35); font-size: 12.5px;
}
.cv-menu-title { padding: 4px 12px 6px; font-weight: 600; color: var(--nm-text-strong); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; border-bottom: 1px solid var(--nm-border-soft); margin-bottom: 4px; }
.cv-menu-item { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; width: 100%; padding: 4px 12px 4px 6px; border: 0; background: none; color: inherit; font: inherit; text-align: left; cursor: pointer; }
.cv-menu-item:hover:not(:disabled) { background: var(--ide-hover); color: var(--nm-danger); }
.cv-menu-item.on { color: var(--nm-danger); font-weight: 600; }
.cv-menu-item:disabled { cursor: default; color: var(--nm-text-muted); }
.cv-menu-item small { flex-basis: 100%; padding-left: 22px; font-size: 11px; color: var(--nm-text-muted); }
.cv-menu-check { width: 16px; flex: none; }
.cv-menu-info { padding: 4px 12px; font-size: 11.5px; color: var(--nm-text-muted); border-top: 1px solid var(--nm-border-soft); }
.cv-menu-info.warn { color: var(--nm-warning); }
.cv-deps { max-height: 170px; overflow: auto; margin-bottom: 8px; padding: 6px 10px; border: 1px solid var(--nm-border); border-radius: 4px; font-size: 12.5px; }
.cv-deps-h { font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.04em; color: var(--nm-text-muted); margin-bottom: 4px; }
.cv-dep + .cv-dep { margin-top: 4px; }
.cv-dep-line { display: flex; flex-wrap: wrap; align-items: center; gap: 4px 10px; }
.cv-dep-muted { color: var(--nm-text-muted); display: inline-flex; align-items: center; gap: 4px; }
.cv-dep-bad { color: var(--nm-danger); font-weight: 600; }
.cv-dep-warn { color: var(--nm-warning); }
.cv-dep-item { padding-left: 14px; font-family: var(--nm-mono); font-size: 11.5px; color: var(--nm-text-muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.cv-dep-item.bad { color: var(--nm-text); }
.cv-dep-item.warn { color: var(--nm-warning); }
.changed { --st: var(--nm-warning); }
.only_left { --st: #3794ff; }
.only_right { --st: #89d185; }
.equal { --st: var(--nm-text-muted); }
.cv-st, .cv-chip { color: var(--st); }
.cv-detail { flex: 1; min-width: 0; display: flex; flex-direction: column; min-height: 0; }
.cv-dhead { display: grid; grid-template-columns: 1fr 92px 1fr; align-items: center; border-bottom: 1px solid var(--nm-border); background: var(--ide-sidebar); }
.cv-dside { padding: 8px 12px; font-weight: 600; color: var(--nm-text-strong); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.cv-mid { display: flex; justify-content: center; gap: 4px; }
.cv-grid { flex: 1; overflow: auto; font-size: 12.5px; }
.cv-sec { padding: 10px 12px 4px; font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.04em; color: var(--nm-text-muted); }
.cv-row { display: grid; grid-template-columns: 1fr 92px 1fr; align-items: stretch; border-bottom: 1px solid var(--nm-border-soft); }
.cv-row:not(.equal) { background: color-mix(in srgb, var(--st) 7%, transparent); }
.cv-row:not(.equal) .cv-cell:first-child { box-shadow: inset 3px 0 0 var(--st); }
.cv-mid { align-items: center; }
.cv-cell { display: flex; flex-wrap: wrap; align-items: center; gap: 2px 8px; padding: 4px 12px; min-width: 0; font-family: var(--nm-mono); font-size: 12px; }
.cv-cell b { font-family: var(--nm-font); color: var(--nm-text-strong); }
.cv-cell.none { background: repeating-linear-gradient(135deg, transparent 0 6px, color-mix(in srgb, var(--nm-text-muted) 10%, transparent) 6px 7px); }
.cv-cell .hl { color: var(--nm-warning); font-weight: 600; }
.cv-cell .cv-ixbadge { font-family: var(--nm-font); font-size: 10.5px; padding: 0 6px; border-radius: 8px; background: color-mix(in srgb, var(--nm-accent) 18%, transparent); color: var(--nm-text); }
.cv-cell .cv-ixbadge.unused { background: color-mix(in srgb, var(--nm-danger) 22%, transparent); color: var(--nm-danger); }
.cv-cell .cv-ixbadge.h-good { background: color-mix(in srgb, var(--nm-success) 20%, transparent); color: var(--nm-success); }
.cv-cell .cv-ixbadge.h-warn { background: color-mix(in srgb, var(--nm-warning) 22%, transparent); color: var(--nm-warning); }
.cv-cell .cv-ixbadge.h-bad { background: color-mix(in srgb, var(--nm-danger) 22%, transparent); color: var(--nm-danger); }
.cv-script { height: 52vh; border: 1px solid var(--nm-border); border-radius: 4px; overflow: hidden; display: flex; flex-direction: column; }
</style>
