<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import { api, errorMessage } from '../api/client';
import type { ColumnInfo, QueryOutcome, SortKey } from '../api/types';
import CodeEditor from '../components/CodeEditor.vue';
import ResultsPane from '../components/ResultsPane.vue';
import ConnectionState from '../components/ConnectionState.vue';
import { useConnectionGate } from '../composables/connectionGate';
import { newQuery } from '../composables/actions';
import { matchesLocal, type ColumnFilter, type FilterState } from '../composables/gridFilter';
import { pendingJumps } from '../composables/fkJump';
import { sortRows } from '../composables/gridSort';
import { objKey, useConnectionsStore } from '../stores/connections';
import { useOutputStore } from '../stores/output';
import { useTabsStore, type ObjectTab, type ObjectView } from '../stores/tabs';

// A table, view, collection, index… : its data (the driver's browse query),
// its structure (columns / fields) and its source.

const props = defineProps<{ tab: ObjectTab }>();

const conns = useConnectionsStore();
const tabs = useTabsStore();
const output = useOutputStore();
const { t } = useTranslation();

const driver = computed(() => conns.driverOf(props.tab.connectionId));
// Every view connects first: "Conectando…", or the connection's error with
// "Ver qué pasa…" and "Reintentar" — never an empty pane.
const { blocked: connBlocked, ensure: ensureConnection } = useConnectionGate(() => props.tab.connectionId);
const kind = computed(() => driver.value?.object_kinds.find((k) => k.id === props.tab.object.kind));
const views = computed(() => {
  const v: { id: ObjectView; label: string }[] = [];
  if (kind.value?.browsable ?? true) v.push({ id: 'data', label: t('query:object.data') });
  if (kind.value?.has_columns ?? true) v.push({ id: 'structure', label: driver.value?.language === 'sql' ? t('query:object.columns') : t('query:object.fields') });
  if (kind.value?.has_definition ?? true) v.push({ id: 'definition', label: t('query:object.definition') });
  return v;
});
const view = computed(() => (views.value.some((v) => v.id === props.tab.view) ? props.tab.view : views.value[0]?.id ?? 'definition'));

const title = computed(() => {
  const o = props.tab.object;
  return o.schema ? `${o.schema}.${o.name}` : o.name;
});

// -- data --------------------------------------------------------------------------
const limit = ref(200);
const browseText = ref('');
/** The same browse without a practical row limit, for "export every row". */
const exportText = ref('');
const outcome = ref<QueryOutcome | null>(null);
const running = ref(false);

// Column filters (the row under the headers). The driver adds them to its
// browse query; engines that can't get them applied to the loaded rows.
const filters = ref<Record<string, FilterState>>({});
const filterList = computed<ColumnFilter[]>(() => Object.values(filters.value).flatMap((f) => f.filters));
/** Why the filters only apply to the loaded rows, when they do ('' = the
 *  engine gave no reason; null = they're applied by the server). */
const localReason = ref<string | null>(null);

// Column sort (a click on a header). The driver adds an ORDER BY to its
// browse query; engines that can't get the loaded rows sorted.
const sort = ref<SortKey[]>([]);
/** Why the sort only applies to the loaded rows, when it does ('' = the
 *  engine gave no reason; null = the server sorts). */
const localSortReason = ref<string | null>(null);

function filteredBrowse(max: number, withSort = true) {
  return api.filteredBrowseQuery({
    connectionId: props.tab.connectionId, database: props.tab.database, object: props.tab.object,
    limit: max, filters: filterList.value, sort: withSort ? sort.value : [],
  });
}

function onSort(keys: SortKey[]) {
  sort.value = keys;
  loadData();
}
function clearSort() {
  sort.value = [];
  loadData();
}

function onFilter(column: string, state: FilterState | null) {
  const next = { ...filters.value };
  if (state && state.filters.length) next[column] = state;
  else delete next[column];
  filters.value = next;
  loadData();
}
function clearFilters() {
  filters.value = {};
  loadData();
}

let pending = false;
async function loadData() {
  // A filter applied while loading reloads when it ends.
  if (running.value) { pending = true; return; }
  if (!(await ensureConnection())) return;
  running.value = true;
  try {
    const [page, all] = await Promise.all([filteredBrowse(limit.value), filteredBrowse(1_000_000_000)]);
    browseText.value = page.query;
    exportText.value = all.query;
    localReason.value = filterList.value.length && !page.server_side ? page.reason ?? '' : null;
    localSortReason.value = sort.value.length && !page.sort_server_side ? page.sort_reason ?? '' : null;
    // Key columns for "Copiar como SQL UPDATEs" (cached; no refetch).
    conns.loadColumns(props.tab.connectionId, props.tab.database, { ...props.tab.object, parent: null });
    const run = () => api.executeQuery({
      sessionId: props.tab.id, connectionId: props.tab.connectionId, database: props.tab.database,
      sql: browseText.value, maxRows: limit.value,
    });
    let out = await run();
    // The server refused the order it was asked for (a text field in
    // Elasticsearch, a multivalued one in Solr…): load without it and sort
    // the loaded rows, saying why.
    if (out.error && sort.value.length && localSortReason.value === null) {
      const [plain, plainAll] = await Promise.all([filteredBrowse(limit.value, false), filteredBrowse(1_000_000_000, false)]);
      browseText.value = plain.query;
      exportText.value = plainAll.query;
      const why = out.error;
      out = await run();
      // Failing without the order too: it wasn't the order. Show that error.
      localSortReason.value = out.error ? null : why;
    }
    if (localReason.value !== null) {
      out.results = out.results.map((r) => {
        const names = r.columns.map((c) => c.name);
        return { ...r, rows: r.rows.filter((row) => matchesLocal(row, names, filterList.value)) };
      });
    }
    if (localSortReason.value !== null) {
      out.results = out.results.map((r) => ({ ...r, rows: sortRows(r.rows, r.columns, sort.value) }));
    }
    outcome.value = out;
    if (outcome.value.error) output.add('error', outcome.value.error, { where: title.value });
  } catch (e) {
    outcome.value = { results: [], messages: [], error: errorMessage(e), elapsed_ms: 0, plans: [] };
  } finally {
    running.value = false;
    if (pending) { pending = false; loadData(); }
  }
}

// -- structure ------------------------------------------------------------------------
const columns = ref<ColumnInfo[]>([]);
/** Primary key columns, known once the structure loaded (copy as UPDATEs). */
const keyColumns = computed(() => {
  const cached = conns.columns[objKey(props.tab.connectionId, props.tab.database, props.tab.object.schema, props.tab.object.name)]?.items ?? columns.value;
  return cached.filter((c) => c.primary_key).map((c) => c.name);
});
const colError = ref<string | null>(null);
async function loadColumns() {
  colError.value = null;
  if (!(await ensureConnection())) return;
  try {
    columns.value = await conns.loadColumns(props.tab.connectionId, props.tab.database, { ...props.tab.object, parent: null }, true);
    // The store keeps a failed read's error instead of throwing.
    const read = conns.columns[objKey(props.tab.connectionId, props.tab.database, props.tab.object.schema, props.tab.object.name)];
    if (read?.status === 'error') colError.value = read.error ?? t('common:error');
  } catch (e) {
    colError.value = errorMessage(e);
  }
}

// -- definition --------------------------------------------------------------------------
const definition = ref('');
const defError = ref<string | null>(null);
const defLoading = ref(false);
async function loadDefinition() {
  defError.value = null;
  if (!(await ensureConnection())) return;
  defLoading.value = true;
  try {
    definition.value = await api.getDefinition(props.tab.connectionId, props.tab.database, props.tab.object);
  } catch (e) {
    defError.value = errorMessage(e);
  } finally {
    defLoading.value = false;
  }
}

// -- a jump by foreign key (composables/fkJump): the filters it brings -------------------
function takeJump(): boolean {
  const f = pendingJumps.get(props.tab.id);
  if (!f) return false;
  pendingJumps.delete(props.tab.id);
  filters.value = { ...f };
  return true;
}
takeJump();

const loaded = new Set<ObjectView>();
// A jump to this tab while it's open: its filters replace the current ones.
watch(() => pendingJumps.has(props.tab.id), (has) => { if (has && takeJump() && loaded.has('data')) loadData(); });
function ensure(v: ObjectView, force = false) {
  if (loaded.has(v) && !force) return;
  loaded.add(v);
  if (v === 'data') loadData();
  if (v === 'structure') loadColumns();
  if (v === 'definition') loadDefinition();
}
watch(view, (v) => ensure(v), { immediate: true });
watch(() => props.tab.object, () => { loaded.clear(); ensure(view.value); });

function refresh() {
  // After a failed connect every view loads again, not only this one.
  if (connBlocked.value) loaded.clear();
  ensure(view.value, true);
}

function openAsQuery() {
  const text = view.value === 'definition' ? definition.value : browseText.value;
  newQuery(props.tab.connectionId, props.tab.database, text, title.value);
}
</script>

<template>
  <div class="ov">
    <div class="nm-toolbar">
      <span class="ov-title">{{ title }}</span>
      <div class="ov-views">
        <button
          v-for="v in views"
          :key="v.id"
          class="nm-subtab"
          :class="{ active: view === v.id }"
          @click="tabs.setView(tab.id, v.id)"
        >{{ v.label }}</button>
      </div>
      <div class="nm-spacer" />
      <template v-if="view === 'data'">
        <span class="nm-muted">{{ $t('query:object.rows') }}</span>
        <el-select v-model="limit" size="small" style="width: 90px" @change="loadData">
          <el-option v-for="n in [100, 200, 1000, 5000, 20000]" :key="n" :label="n.toLocaleString(locale())" :value="n" />
        </el-select>
      </template>
      <el-button v-if="view !== 'structure'" @click="openAsQuery">
        <el-icon><ei-document-add /></el-icon>&nbsp;{{ $t('query:object.openAsQuery') }}
      </el-button>
      <el-button @click="refresh"><el-icon><ei-refresh /></el-icon></el-button>
    </div>

    <div class="ov-body">
      <ConnectionState v-if="connBlocked" :connection-id="tab.connectionId" @retry="refresh" />
      <template v-else>
      <div v-if="view === 'data' && filterList.length" class="ov-filters">
        <el-icon><ei-filter /></el-icon>
        <span>
          {{ $t('query:object.filteredColumns', { count: Object.keys(filters).length }) }}
          <template v-if="localReason !== null"> · {{ $t('query:object.localFilter', { rows: limit.toLocaleString(locale()), reason: localReason ? tb(localReason) : $t('query:object.noServerFilter') }) }}</template>
        </span>
        <div class="nm-spacer" />
        <el-button size="small" link @click="clearFilters">{{ $t('query:object.clearFilters') }}</el-button>
      </div>
      <div v-if="view === 'data' && sort.length && localSortReason !== null" class="ov-filters">
        <el-icon><ei-sort /></el-icon>
        <span>{{ $t('query:object.localSort', { rows: limit.toLocaleString(locale()), reason: localSortReason ? tb(localSortReason) : $t('query:object.noServerSort') }) }}</span>
        <div class="nm-spacer" />
        <el-button size="small" link @click="clearSort">{{ $t('query:object.clearSort') }}</el-button>
      </div>
      <ResultsPane
        v-if="view === 'data'"
        :outcome="outcome"
        :running="running"
        :source="exportText ? { connectionId: tab.connectionId, database: tab.database, sql: exportText } : null"
        :title="tab.object.name"
        :dialect="driver?.dialect ?? ''"
        :table="{ schema: tab.object.schema, name: tab.object.name }"
        :edit-source="{ connectionId: tab.connectionId, database: tab.database, language: driver?.language ?? 'sql', target: { kind: tab.object.kind, schema: tab.object.schema, name: tab.object.name } }"
        @script="(code: string) => newQuery(tab.connectionId, tab.database, code, $t('query:object.changesIn', { name: tab.object.name }))"
        @applied="loadData"
        :key-columns="keyColumns"
        filterable
        :filters="filters"
        @filter="onFilter"
        :sort="sort"
        @sort="onSort"
      />

      <div v-else-if="view === 'structure'" class="nm-content">
        <el-alert v-if="colError" type="error" :title="colError" :closable="false" />
        <el-table v-else :data="columns" size="small" height="100%" stripe>
          <el-table-column type="index" label="#" width="48" />
          <el-table-column prop="name" :label="$t('query:object.name')" min-width="180">
            <template #default="{ row }">
              <span class="ov-col">
                <el-icon v-if="row.primary_key" color="var(--nm-syn-code)" :title="$t('query:object.primaryKey')"><ei-key /></el-icon>
                {{ row.name }}
              </span>
            </template>
          </el-table-column>
          <el-table-column prop="data_type" :label="$t('query:object.type')" min-width="160">
            <template #default="{ row }"><code>{{ row.data_type }}</code></template>
          </el-table-column>
          <el-table-column :label="$t('query:object.nullable')" width="70">
            <template #default="{ row }">{{ row.nullable ? $t('query:object.yes') : $t('query:object.no') }}</template>
          </el-table-column>
          <el-table-column :label="$t('query:object.autoIncrement')" width="80">
            <template #default="{ row }">{{ row.auto_increment ? $t('query:object.yes') : '' }}</template>
          </el-table-column>
          <el-table-column prop="default_value" :label="$t('query:object.default')" min-width="160" />
        </el-table>
      </div>

      <div v-else class="ov-def">
        <el-alert v-if="defError" type="error" :title="defError" :closable="false" style="margin: 12px" />
        <div v-else-if="defLoading" class="nm-content nm-muted">{{ $t('common:loading') }}</div>
        <CodeEditor v-else :model-value="definition" :language="driver?.language" :dialect="driver?.dialect" read-only />
      </div>
      </template>
    </div>
  </div>
</template>

<style scoped>
.ov { display: flex; flex-direction: column; height: 100%; min-height: 0; }
.ov-title { font-weight: 600; color: var(--nm-text-strong); margin-right: 8px; white-space: nowrap; }
.ov-views { display: flex; height: 100%; }
.ov-body { flex: 1; min-height: 0; display: flex; flex-direction: column; }
.ov-body > .nm-content { flex: 1; min-height: 0; }
.ov-filters {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 2px 10px;
  font-size: 12px;
  color: var(--nm-text);
  background: color-mix(in srgb, var(--ide-focus) 12%, transparent);
  border-bottom: 1px solid var(--nm-border);
}
.ov-def { flex: 1; min-height: 0; display: flex; flex-direction: column; }
.ov-def > :deep(.ce) { flex: 1; }
.ov-col { display: inline-flex; align-items: center; gap: 4px; }
</style>
