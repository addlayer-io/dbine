<script setup lang="ts">
import { computed, reactive, ref, watch } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import { dataCompareApi, type DataCompareResult, type DataScript, type DataSide, type Dir } from '../api/dataCompare';
import type { Cell, ObjectRef } from '../api/types';
import { newQuery } from '../composables/actions';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import { dbKey, objKey, useConnectionsStore } from '../stores/connections';
import { useTabsStore, type DataCompareTab } from '../stores/tabs';

// Data compare (docs/comparacion-de-datos.md): a table's rows (left) against
// another table's (right), by key; then a script, in the target engine's
// language, that makes one side like the other. Nothing runs without the
// user's click.

const props = defineProps<{ tab: DataCompareTab }>();
const conns = useConnectionsStore();
const { t } = useTranslation();

interface Pick { connectionId: string; database: string; table: string }
const tableId = (o: ObjectRef | null) => (o ? `${o.schema ?? ''}\u0001${o.name}` : '');
/** Last time's picks (restored with the app), when their connections still exist. */
const kept = (s: 'left' | 'right') => {
  const p = props.tab.picks?.[s];
  return p && conns.byId(p.connectionId) ? { connectionId: p.connectionId, database: p.database, table: p.table ?? '' } : null;
};
const left = reactive<Pick>(kept('left') ?? { connectionId: props.tab.connectionId, database: props.tab.database, table: tableId(props.tab.object) });
const right = reactive<Pick>(kept('right') ?? { connectionId: props.tab.connectionId, database: props.tab.database, table: '' });
const limit = ref(200000);

/** The databases and tables to choose from, per side. */
function dbsOf(p: Pick): string[] {
  return conns.live[p.connectionId]?.databases ?? [];
}
function tablesOf(p: Pick) {
  const d = conns.driverOf(p.connectionId);
  const browsable = new Set(d?.object_kinds.filter((k) => k.browsable ?? true).map((k) => k.id) ?? []);
  return (conns.objects[dbKey(p.connectionId, p.database)]?.items ?? []).filter((o) => browsable.has(o.kind));
}
async function prepare(p: Pick) {
  if (!p.connectionId) return;
  if (!(await conns.ensureConnected(p.connectionId))) return;
  const live = conns.live[p.connectionId];
  // Another connection keeps the database if it has one by that name.
  if (live && live.databases.length && !live.databases.includes(p.database)) p.database = live.defaultDatabase || live.databases[0];
  await conns.loadObjects(p.connectionId, p.database);
}
watch(() => [left.connectionId, left.database], () => prepare(left), { immediate: true });
watch(() => [right.connectionId, right.database], () => prepare(right), { immediate: true });

/** The table in `list` that is `id` (schema and name; engines' default
 *  schemas count as the same, so dbo.X finds public.x). */
function sameTable(list: ReturnType<typeof tablesOf>, id: string) {
  const [schema, name] = id.split('\u0001');
  return list.find((o) => tableId(o) === id)
    ?? list.find((o) => o.name.toLowerCase() === (name ?? '').toLowerCase() && normSchema(o.schema) === normSchema(schema || null));
}
// Changing the connection or database keeps the chosen table when the new
// one has it; only a table that isn't there is cleared. Waits for the real
// list (a cached one may lack new tables).
for (const p of [left, right]) {
  watch(() => [p.connectionId, p.database, p.table, conns.objects[dbKey(p.connectionId, p.database)]?.status], () => {
    if (!p.table || conns.objects[dbKey(p.connectionId, p.database)]?.status !== 'ready') return;
    const o = sameTable(tablesOf(p), p.table);
    p.table = o ? tableId(o) : '';
  });
}
// Same table on the right by default (same schema first).
watch(() => [left.table, tablesOf(right).length], () => {
  if (right.table || !left.table) return;
  if (right.connectionId === left.connectionId && right.database === left.database) return;
  const list = tablesOf(right);
  const name = left.table.split('\u0001')[1] ?? '';
  const same = sameTable(list, left.table) ?? list.find((o) => o.name.toLowerCase() === name.toLowerCase());
  if (same) right.table = tableId(same);
});

function side(p: Pick): DataSide | null {
  const o = tablesOf(p).find((x) => tableId(x) === p.table);
  return o ? { connection_id: p.connectionId, database: p.database, object: { kind: o.kind, schema: o.schema, name: o.name } } : null;
}

// -- not the same table on both sides ------------------------------------------------
// Two tables with the same name in different schemas (dbo.X against sos.X)
// look alike in the pickers, and syncing them overwrites the wrong table.
// Engines' default schemas count as the same (dbo against public is fine).
const DEFAULT_SCHEMAS = new Set(['dbo', 'public', 'main']);
const normSchema = (s: string | null) => (!s || DEFAULT_SCHEMAS.has(s.toLowerCase()) ? '' : s.toLowerCase());
const qualified = (o: ObjectRef) => (o.schema ? `${o.schema}.${o.name}` : o.name);
const mismatch = computed<{ kind: 'name' | 'schema'; left: string; right: string } | null>(() => {
  const l = side(left);
  const r = side(right);
  if (!l || !r) return null;
  const names = { left: qualified(l.object), right: qualified(r.object) };
  if (l.object.name.toLowerCase() !== r.object.name.toLowerCase()) return { kind: 'name', ...names };
  if (normSchema(l.object.schema) !== normSchema(r.object.schema)) return { kind: 'schema', ...names };
  return null;
});
const mismatchText = computed(() => (mismatch.value ? t(`dataCompare:mismatch.${mismatch.value.kind}`, { left: mismatch.value.left, right: mismatch.value.right }) : ''));

const running = ref(false);
const result = ref<DataCompareResult | null>(null);
const error = ref<string | null>(null);
const view = ref<'changed' | 'only_left' | 'only_right'>('changed');
const key = ref<string[]>(props.tab.picks?.key ?? []);
const tabs = useTabsStore();
watch(
  () => [left.connectionId, left.database, left.table, right.connectionId, right.database, right.table, key.value.join('\u0001')],
  () => tabs.setPicks(props.tab.id, { left: { ...left }, right: { ...right }, key: [...key.value] }),
);

/** The left table's columns, to choose the key before comparing (a table
 *  without a primary key needs one). */
const leftColumns = computed(() => {
  const o = tablesOf(left).find((x) => tableId(x) === left.table);
  if (!o) return [] as string[];
  return (conns.columns[objKey(left.connectionId, left.database, o.schema, o.name)]?.items ?? []).map((c) => c.name);
});
watch(() => [left.connectionId, left.database, left.table], () => { key.value = []; });
// Load its columns once the table (and the database's object list) is there.
watch(() => [left.connectionId, left.database, left.table, tablesOf(left).length], () => {
  const o = tablesOf(left).find((x) => tableId(x) === left.table);
  if (o) conns.loadColumns(left.connectionId, left.database, o);
}, { immediate: true });

async function compare() {
  const l = side(left);
  const r = side(right);
  if (!l || !r) { ElMessage.warning(t('dataCompare:pickBoth')); return; }
  running.value = true;
  error.value = null;
  try {
    result.value = await dataCompareApi.compare(l, r, key.value, limit.value);
    key.value = result.value.key;
    view.value = result.value.counts.changed ? 'changed' : result.value.counts.only_left ? 'only_left' : 'only_right';
  } catch (e) {
    error.value = errorMessage(e);
    result.value = null;
  } finally {
    running.value = false;
  }
}

const fmtN = (n: number) => n.toLocaleString(locale());
function show(v: Cell): string {
  if (v === null) return 'NULL';
  return typeof v === 'string' ? v : String(v);
}
const rows = computed(() => {
  const r = result.value;
  if (!r) return [];
  if (view.value === 'only_left') return r.only_left;
  if (view.value === 'only_right') return r.only_right;
  return [];
});
const shownOf = (n: number, list: unknown[]) => (n > list.length ? t('dataCompare:showingFirst', { shown: fmtN(list.length), total: fmtN(n) }) : '');

// -- what goes where: arrows per row, as in the schema compare ------------------

type Kind = 'changed' | 'only_left' | 'only_right';
/** Per kind: the direction of every row, and the rows set apart from it
 *  (by index, including rows past the ones shown). */
const choices = reactive<Record<Kind, { all: Dir; rows: Map<number, Dir> }>>({
  changed: { all: 'none', rows: new Map() },
  only_left: { all: 'none', rows: new Map() },
  only_right: { all: 'none', rows: new Map() },
});
function resetChoices() {
  for (const k of ['changed', 'only_left', 'only_right'] as Kind[]) {
    choices[k].all = 'none';
    choices[k].rows = new Map();
  }
}
watch(() => result.value?.id, resetChoices);
function dirOf(kind: Kind, i: number): Dir {
  return choices[kind].rows.get(i) ?? choices[kind].all;
}
/** A click on an arrow sends the row that way; a second click leaves it alone. */
function toggle(kind: Kind, i: number, d: Dir) {
  const c = choices[kind];
  const next = dirOf(kind, i) === d ? 'none' : d;
  if (next === c.all) c.rows.delete(i);
  else c.rows.set(i, next);
}
/** Every row of a kind (also those not shown) one way. */
function setAll(kind: Kind, d: Dir) {
  choices[kind].all = d;
  choices[kind].rows = new Map();
}
/** How many rows of a kind will change. */
function chosen(kind: Kind): number {
  const r = result.value;
  if (!r) return 0;
  const c = choices[kind];
  let n = c.all !== 'none' ? r.counts[kind] : 0;
  for (const d of c.rows.values()) {
    if (d === c.all) continue;
    if (c.all !== 'none') n--;
    if (d !== 'none') n++;
  }
  return n;
}
const totalChosen = computed(() => chosen('changed') + chosen('only_left') + chosen('only_right'));
/** What an arrow does on a row of this kind, for its tooltip. */
function arrowTip(kind: Kind, d: 'left' | 'right') {
  const what = kind === 'changed' ? 'update' : (kind === 'only_left') === (d === 'right') ? 'insert' : 'delete';
  return t(`dataCompare:arrow.${what}.${d}`);
}
/** "Make the right (left) match": changed and missing rows that way.
 *  Deleting is never picked for you: those rows are chosen one by one or
 *  with "all" in their own view. */
function matchSide(d: 'left' | 'right') {
  setAll('changed', d);
  setAll(d === 'right' ? 'only_left' : 'only_right', d);
}

// -- the scripts -----------------------------------------------------------------

const sync = reactive({ open: false, tab: '' });
const scripts = ref<DataScript[]>([]);
const scriptError = ref<string | null>(null);
const building = ref(false);
const applying = ref(false);
function pickOf(kind: Kind): { all: Dir; rows: [number, Dir][] } {
  return { all: choices[kind].all, rows: [...choices[kind].rows.entries()] };
}
async function openSync() {
  if (!result.value) return;
  sync.open = true;
  building.value = true;
  scripts.value = [];
  scriptError.value = null;
  try {
    scripts.value = await dataCompareApi.scripts(result.value.id, { changed: pickOf('changed'), only_left: pickOf('only_left'), only_right: pickOf('only_right') });
    sync.tab = scripts.value[0]?.side ?? '';
  } catch (e) {
    scriptError.value = errorMessage(e);
  } finally {
    building.value = false;
  }
}
function sideName(side: 'left' | 'right') {
  const p = side === 'right' ? right : left;
  return `${conns.byId(p.connectionId)?.name ?? ''} · ${p.table.split('\u0001')[1] ?? ''}`;
}
/** "1 inserción, 3 actualizaciones y 2 borrados" (only what the script does). */
function summaryOf(sc: DataScript) {
  const parts = ([['insert', sc.inserts], ['update', sc.updates], ['delete', sc.deletes]] as const)
    .filter(([, n]) => n > 0)
    .map(([k, n]) => t(`dataCompare:count.${k}`, { count: n, n: fmtN(n) }));
  return parts.length > 1 ? `${parts.slice(0, -1).join(', ')} ${t('dataCompare:count.and')} ${parts[parts.length - 1]}` : parts[0] ?? '';
}
const currentScript = computed(() => scripts.value.find((s) => s.side === sync.tab) ?? null);
async function copyScript() {
  if (!currentScript.value) return;
  await navigator.clipboard.writeText(currentScript.value.script);
  ElMessage.success(t('dataCompare:copied'));
}
async function openInQuery() {
  const s = currentScript.value;
  if (!s) return;
  await newQuery(s.connection_id, s.database, s.script, t('dataCompare:scriptName'));
}
/** Runs every script, one side after the other; stops at the first error. */
async function apply() {
  if (!scripts.value.length) return;
  if (mismatch.value) {
    try {
      await ElMessageBox.confirm(mismatchText.value, t('dataCompare:mismatch.title'), {
        confirmButtonText: t('dataCompare:mismatch.runAnyway'), cancelButtonText: t('common:cancel'), type: 'warning',
      });
    } catch {
      return;
    }
  }
  applying.value = true;
  scriptError.value = null;
  try {
    for (const s of scripts.value) {
      if (!s.script.trim()) continue;
      const sessionId = `dsync:${s.side}:${Date.now()}`;
      try {
        const o = await api.executeQuery({ sessionId, connectionId: s.connection_id, database: s.database, sql: s.script, maxRows: 10, record: true });
        if (o.error) {
          sync.tab = s.side;
          scriptError.value = t('dataCompare:failedOn', { side: sideName(s.side), error: tb(o.error) });
          return;
        }
      } finally {
        api.closeSession(sessionId).catch(() => {});
      }
    }
    sync.open = false;
    ElMessage.success(t('dataCompare:applied'));
    await compare();
  } catch (e) {
    scriptError.value = errorMessage(e);
  } finally {
    applying.value = false;
  }
}
</script>

<template>
  <div class="dc">
    <div class="dc-pick">
      <div v-for="(p, i) in [left, right]" :key="i" class="dc-side">
        <span class="dc-side-label">{{ i === 0 ? $t('dataCompare:left') : $t('dataCompare:right') }}</span>
        <el-select v-model="p.connectionId" filterable size="small" class="dc-conn">
          <el-option v-for="c in conns.list" :key="c.id" :label="c.name" :value="c.id" />
        </el-select>
        <el-select v-if="dbsOf(p).length" v-model="p.database" filterable size="small" class="dc-db">
          <el-option v-for="d in dbsOf(p)" :key="d" :label="d" :value="d" />
        </el-select>
        <el-select v-model="p.table" filterable size="small" class="dc-table" :placeholder="$t('dataCompare:table')">
          <el-option v-for="o in tablesOf(p)" :key="tableId(o)" :label="o.schema ? `${o.schema}.${o.name}` : o.name" :value="tableId(o)" />
        </el-select>
      </div>
      <div class="dc-actions">
        <span class="dc-side-label">{{ $t('dataCompare:keyLabel') }}</span>
        <el-select v-model="key" multiple filterable collapse-tags size="small" class="dc-key" :placeholder="$t('dataCompare:key')">
          <el-option v-for="c in (result?.columns.length ? result.columns : leftColumns)" :key="c" :label="c" :value="c" />
        </el-select>
        <el-button :type="result ? 'default' : 'primary'" size="small" :loading="running" @click="compare">{{ $t('dataCompare:compare') }}</el-button>
        <!-- Same as the schema compare: choose with the arrows, then sync. -->
        <el-button v-if="result" size="small" :type="totalChosen ? 'primary' : 'default'" :disabled="!totalChosen" @click="openSync">
          {{ $t('compare:sync.button') }}<span v-if="totalChosen" class="dc-count">{{ fmtN(totalChosen) }}</span>
        </el-button>
      </div>
    </div>

    <div v-if="mismatch" class="dc-mismatch" role="alert"><el-icon><ei-warning-filled /></el-icon>{{ mismatchText }}</div>
    <div v-if="error" class="dc-error" role="alert">{{ error }}</div>

    <template v-if="result">
      <div class="dc-summary">
        <span class="dc-same"><b>{{ fmtN(result.counts.same) }}</b> {{ $t('dataCompare:same') }}</span>
        <button :class="{ on: view === 'changed' }" @click="view = 'changed'"><b>{{ fmtN(result.counts.changed) }}</b> {{ $t('dataCompare:changed') }}</button>
        <button :class="{ on: view === 'only_left' }" @click="view = 'only_left'"><b>{{ fmtN(result.counts.only_left) }}</b> {{ $t('dataCompare:onlyLeft') }}</button>
        <button :class="{ on: view === 'only_right' }" @click="view = 'only_right'"><b>{{ fmtN(result.counts.only_right) }}</b> {{ $t('dataCompare:onlyRight') }}</button>
        <span style="flex: 1" />
        <el-button size="small" :disabled="!result.counts.changed && !result.counts.only_left" :title="$t('dataCompare:matchHint')" @click="matchSide('right')">{{ $t('dataCompare:syncRight') }}</el-button>
        <el-button size="small" :disabled="!result.counts.changed && !result.counts.only_right" :title="$t('dataCompare:matchHint')" @click="matchSide('left')">{{ $t('dataCompare:syncLeft') }}</el-button>
      </div>
      <div v-if="view === 'changed' ? result.counts.changed : view === 'only_left' ? result.counts.only_left : result.counts.only_right" class="dc-bulk">
        <span>{{ $t('dataCompare:bulk', { count: chosen(view), n: fmtN(chosen(view)) }) }}</span>
        <button :class="{ on: choices[view].all === 'left' && !choices[view].rows.size }" :title="arrowTip(view, 'left')" @click="setAll(view, 'left')">{{ $t('dataCompare:allLeft') }}</button>
        <button :class="{ on: choices[view].all === 'right' && !choices[view].rows.size }" :title="arrowTip(view, 'right')" @click="setAll(view, 'right')">{{ $t('dataCompare:allRight') }}</button>
        <button :class="{ on: choices[view].all === 'none' && !choices[view].rows.size }" @click="setAll(view, 'none')">{{ $t('dataCompare:allNone') }}</button>
      </div>
      <div v-if="result.truncated_left || result.truncated_right || result.duplicate_keys || result.only_left_columns.length || result.only_right_columns.length" class="dc-notes">
        <div v-if="result.truncated_left || result.truncated_right">{{ $t('dataCompare:truncated', { limit: fmtN(limit) }) }}</div>
        <div v-if="result.duplicate_keys">{{ $t('dataCompare:duplicates', { count: result.duplicate_keys }) }}</div>
        <div v-if="result.only_left_columns.length">{{ $t('dataCompare:onlyLeftColumns', { list: result.only_left_columns.join(', ') }) }}</div>
        <div v-if="result.only_right_columns.length">{{ $t('dataCompare:onlyRightColumns', { list: result.only_right_columns.join(', ') }) }}</div>
      </div>
      <div class="dc-grid-wrap">
        <table class="dc-grid">
          <thead><tr><th class="dc-arrows-h" /><th v-for="(c, i) in result.columns" :key="c" :class="{ key: i < result.key.length }">{{ c }}</th></tr></thead>
          <tbody v-if="view === 'changed'">
            <tr v-for="(r, i) in result.changed" :key="i" :class="`go-${dirOf('changed', i)}`">
              <td class="dc-arrows">
                <button :class="{ on: dirOf('changed', i) === 'left' }" :title="arrowTip('changed', 'left')" @click="toggle('changed', i, 'left')">←</button>
                <button :class="{ on: dirOf('changed', i) === 'right' }" :title="arrowTip('changed', 'right')" @click="toggle('changed', i, 'right')">→</button>
              </td>
              <td v-for="(c, j) in result.columns" :key="c" :class="{ diff: r.diff.includes(j), key: j < result.key.length }">
                <template v-if="r.diff.includes(j)">
                  <div class="dc-l" :title="$t('dataCompare:left')">{{ show(r.left[j]) }}</div>
                  <div class="dc-r" :title="$t('dataCompare:right')">{{ show(r.right[j]) }}</div>
                </template>
                <template v-else>{{ show(r.left[j]) }}</template>
              </td>
            </tr>
          </tbody>
          <tbody v-else>
            <tr v-for="(r, i) in rows" :key="i" :class="`go-${dirOf(view, i)}`">
              <td class="dc-arrows">
                <button :class="{ on: dirOf(view, i) === 'left' }" :title="arrowTip(view, 'left')" @click="toggle(view, i, 'left')">←</button>
                <button :class="{ on: dirOf(view, i) === 'right' }" :title="arrowTip(view, 'right')" @click="toggle(view, i, 'right')">→</button>
              </td>
              <td v-for="(c, j) in result.columns" :key="c" :class="{ key: j < result.key.length }">{{ show(r[j]) }}</td>
            </tr>
          </tbody>
        </table>
        <div v-if="view === 'changed' ? !result.changed.length : !rows.length" class="dc-empty">{{ $t('dataCompare:nothing') }}</div>
        <div class="dc-more">
          {{ view === 'changed' ? shownOf(result.counts.changed, result.changed) : view === 'only_left' ? shownOf(result.counts.only_left, result.only_left) : shownOf(result.counts.only_right, result.only_right) }}
        </div>
      </div>
    </template>
    <div v-else-if="!running && !error" class="dc-hint">{{ $t('dataCompare:hint') }}</div>

    <el-dialog v-model="sync.open" :title="$t('dataCompare:syncTitle')" width="760px" append-to-body>
      <div v-if="building" v-loading="true" class="dc-building" />
      <el-tabs v-if="scripts.length" v-model="sync.tab">
        <el-tab-pane v-for="sc in scripts" :key="sc.side" :name="sc.side" :label="`${sc.side === 'left' ? $t('dataCompare:left') : $t('dataCompare:right')} · ${sideName(sc.side)}`">
          <p class="dc-hint2">{{ $t('dataCompare:scriptSummary', { parts: summaryOf(sc) }) }}</p>
          <pre class="dc-script nm-selectable">{{ sc.script || $t('dataCompare:noChanges') }}</pre>
        </el-tab-pane>
      </el-tabs>
      <p v-if="scripts.length > 1" class="dc-hint2">{{ $t('dataCompare:bothSides') }}</p>
      <div v-if="mismatch" class="dc-mismatch in-dialog" role="alert"><el-icon><ei-warning-filled /></el-icon>{{ mismatchText }}</div>
      <div v-if="scriptError" class="dc-error" role="alert">{{ scriptError }}</div>
      <template #footer>
        <div class="dc-foot">
          <el-button :disabled="!currentScript?.script" @click="copyScript">{{ $t('common:copy') }}</el-button>
          <el-button :disabled="!currentScript?.script" @click="openInQuery">{{ $t('dataCompare:openInQuery') }}</el-button>
          <span style="flex: 1" />
          <el-button @click="sync.open = false">{{ $t('common:cancel') }}</el-button>
          <el-button type="primary" :loading="applying" :disabled="!scripts.some((x) => x.script.trim())" @click="apply">{{ $t('dataCompare:run') }}</el-button>
        </div>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.dc { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--ide-editor); }
.dc-pick { display: flex; flex-wrap: wrap; gap: 8px 18px; align-items: center; padding: 10px 14px; border-bottom: 1px solid var(--nm-border); }
/* The key and the button always on their own line, at the left. */
.dc-side { display: flex; align-items: center; gap: 6px; }
.dc-side-label { font-size: 11px; font-weight: 600; letter-spacing: .05em; text-transform: uppercase; color: var(--nm-text-dim); }
.dc-conn { width: 180px; }
.dc-db { width: 150px; }
.dc-table { width: 220px; }
.dc-actions { display: flex; gap: 6px; align-items: center; flex-basis: 100%; }
.dc-key { width: 320px; }
.dc-count { margin-left: 6px; padding: 0 6px; border-radius: 8px; font-size: 11px; line-height: 16px; background: rgba(255, 255, 255, 0.22); }
.dc-bulk { display: flex; align-items: center; gap: 6px; padding: 6px 14px; border-bottom: 1px solid var(--nm-border); font-size: 12px; color: var(--nm-text-dim); }
.dc-bulk button { border: 1px solid var(--nm-border); border-radius: 3px; background: none; color: var(--nm-text); padding: 2px 8px; font: inherit; cursor: pointer; }
.dc-bulk button.on { background: var(--ide-selection); border-color: var(--ide-focus, var(--el-color-primary)); }
.dc-grid th.dc-arrows-h { width: 52px; }
.dc-arrows { white-space: nowrap; padding: 0 4px !important; }
.dc-arrows button { width: 20px; height: 18px; padding: 0; border: 1px solid transparent; border-radius: 3px; background: none; color: var(--nm-text-dim); font: inherit; cursor: pointer; }
.dc-arrows button:hover { border-color: var(--nm-border); color: var(--nm-text-strong); }
.dc-arrows button.on { background: var(--el-color-primary); color: #fff; }
.dc-grid tr.go-left td:not(.dc-arrows), .dc-grid tr.go-right td:not(.dc-arrows) { background: color-mix(in srgb, var(--el-color-primary) 8%, transparent); }
.dc-building { height: 80px; }
.dc-mismatch { display: flex; align-items: center; gap: 8px; margin: 10px 14px 0; padding: 8px 10px; border-radius: 3px; border: 1px solid color-mix(in srgb, var(--nm-warning) 55%, transparent); background: color-mix(in srgb, var(--nm-warning) 12%, transparent); color: var(--nm-text-strong); font-size: 12.5px; }
.dc-mismatch .el-icon { color: var(--nm-warning); flex: none; }
.dc-mismatch.in-dialog { margin: 8px 0 0; }
.dc-error { margin: 10px 14px; padding: 8px 10px; border-radius: 3px; border: 1px solid color-mix(in srgb, var(--nm-danger) 50%, transparent); background: color-mix(in srgb, var(--nm-danger) 10%, transparent); color: var(--nm-text); white-space: pre-wrap; }
.dc-summary { display: flex; align-items: center; gap: 6px; padding: 8px 14px; border-bottom: 1px solid var(--nm-border); font-size: 12.5px; }
.dc-summary button { border: 1px solid var(--nm-border); border-radius: 12px; background: none; color: var(--nm-text); padding: 3px 10px; font: inherit; cursor: pointer; }
.dc-summary button.on { background: var(--ide-selection); border-color: var(--ide-focus, var(--el-color-primary)); color: var(--nm-text-strong); }
.dc-same { color: var(--nm-text-dim); margin-right: 6px; }
.dc-notes { padding: 6px 14px; font-size: 12px; color: var(--nm-warning); border-bottom: 1px solid var(--nm-border); }
.dc-grid-wrap { flex: 1; min-height: 0; overflow: auto; }
.dc-grid { border-collapse: collapse; font-size: 12px; font-family: var(--nm-mono); }
.dc-grid th { position: sticky; top: 0; background: var(--nm-bg-elev); color: var(--nm-text-dim); font-weight: 500; text-align: left; padding: 4px 10px; white-space: nowrap; border-bottom: 1px solid var(--nm-border); }
.dc-grid th.key { color: var(--nm-text-strong); }
.dc-grid td { padding: 3px 10px; border-bottom: 1px solid var(--nm-border-soft); white-space: nowrap; max-width: 320px; overflow: hidden; text-overflow: ellipsis; color: var(--nm-text); }
.dc-grid td.key { color: var(--nm-text-strong); }
.dc-grid td.diff { background: color-mix(in srgb, var(--nm-warning) 10%, transparent); }
.dc-l { color: var(--nm-danger); }
.dc-r { color: var(--nm-success); }
.dc-empty, .dc-hint { padding: 30px 14px; color: var(--nm-text-dim); font-size: 12.5px; }
.dc-more { padding: 6px 14px; color: var(--nm-text-dim); font-size: 11.5px; }
.dc-opts { display: flex; gap: 16px; margin-bottom: 8px; }
.dc-hint2 { margin: 0 0 6px; color: var(--nm-text-dim); font-size: 12.5px; }
.dc-script { margin: 0; padding: 10px 12px; max-height: 45vh; overflow: auto; border: 1px solid var(--nm-border); border-radius: 4px; background: var(--ide-editor, var(--nm-bg-elev)); font-family: var(--nm-mono); font-size: 12px; color: var(--nm-text-strong); white-space: pre-wrap; }
.dc-foot { display: flex; align-items: center; gap: 8px; }
.dc-foot .el-button { margin: 0; }
</style>
