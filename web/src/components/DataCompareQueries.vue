<script setup lang="ts">
import { computed, watch } from 'vue';
import { ensureConnectedOrTell } from '../composables/connectionGate';
import { unpairedRight, type PairRow, type ToleranceForm } from '../composables/queryCompare';
import { useConnectionsStore } from '../stores/connections';
import type { QueryComparePick } from '../stores/tabs';
import type { ResultColumn } from '../api/types';

// Data compare of two queries (docs/data-compare.md, "Comparing two
// queries"): each side's connection, database and query; the columns of
// both results paired, the key among them; and how values compare (a
// tolerance on numbers, NULL as BLANK…). DataCompareView reads the columns
// and runs the compare.

const left = defineModel<QueryComparePick>('left', { required: true });
const right = defineModel<QueryComparePick>('right', { required: true });
const pairs = defineModel<PairRow[]>('pairs', { required: true });
const form = defineModel<ToleranceForm>('form', { required: true });
const props = defineProps<{
  /** Each result's columns, as last read. */
  columns: { left: ResultColumn[]; right: ResultColumn[] };
  /** A side changed since: its columns are read again before comparing. */
  stale: boolean;
  busy: boolean;
}>();
const emit = defineEmits<{ read: [] }>();
const conns = useConnectionsStore();

const sides = computed(() => [
  { name: 'left' as const, pick: left.value },
  { name: 'right' as const, pick: right.value },
]);
const dbsOf = (p: QueryComparePick) => conns.live[p.connectionId]?.databases ?? [];
/** The engine has databases: the picker stays, even before the list arrives. */
const hasDatabases = (p: QueryComparePick) => !!p.connectionId && (!!conns.driverOf(p.connectionId)?.databases_label || dbsOf(p).length > 0);
const connecting = (p: QueryComparePick) => conns.live[p.connectionId]?.status === 'connecting';
/** The language its query is written in ("SQL", "DAX"…). */
const languageOf = (p: QueryComparePick) => {
  const l = conns.driverOf(p.connectionId)?.language;
  return l && l !== 'unknown' ? l.toUpperCase() : '';
};

/** Another connection: connect, list its databases and keep the database
 *  when it has one by that name. */
async function prepare(p: QueryComparePick) {
  if (!p.connectionId) return;
  if (!(await ensureConnectedOrTell(p.connectionId))) return;
  if (!dbsOf(p).length) await conns.refreshDatabases(p.connectionId).catch(() => {});
  const live = conns.live[p.connectionId];
  if (!live) return;
  if (!p.database && !live.databases.length) p.database = live.defaultDatabase;
  else if (live.databases.length && !live.databases.includes(p.database)) p.database = live.defaultDatabase || live.databases[0];
}
watch(() => left.value.connectionId, () => prepare(left.value), { immediate: true });
watch(() => right.value.connectionId, () => prepare(right.value), { immediate: true });

const rightNames = computed(() => props.columns.right.map((c) => c.name));
const typeOf = (cols: ResultColumn[], name: string) => cols.find((c) => c.name === name)?.type_name ?? '';
const unpaired = computed(() => unpairedRight(pairs.value, props.columns.right));
/** A key column is compared: choosing "none" on the right takes it out of the key. */
function setRight(row: PairRow, name: string | undefined) {
  row.right = name ?? '';
  if (!row.right) row.key = false;
}
</script>

<template>
  <div class="dq">
    <div class="dq-sides">
      <div v-for="s in sides" :key="s.name" class="dq-side">
        <div class="dq-head">
          <span class="dq-label">{{ s.name === 'left' ? $t('dataCompare:left') : $t('dataCompare:right') }}</span>
          <el-select v-model="s.pick.connectionId" filterable size="small" class="dq-conn" :placeholder="$t('dataCompare:query.connection')">
            <el-option v-for="c in conns.list" :key="c.id" :label="c.name" :value="c.id" />
          </el-select>
          <el-select
            v-if="hasDatabases(s.pick)" v-model="s.pick.database" filterable allow-create default-first-option size="small" class="dq-db"
            :loading="connecting(s.pick)" :disabled="connecting(s.pick)"
            @visible-change="(open: boolean) => open && conns.refreshDatabasesSoon(s.pick.connectionId)"
          >
            <el-option v-for="d in dbsOf(s.pick)" :key="d" :label="d" :value="d" />
          </el-select>
          <span v-if="languageOf(s.pick)" class="dq-lang">{{ languageOf(s.pick) }}</span>
        </div>
        <el-input
          v-model="s.pick.query" type="textarea" :autosize="{ minRows: 3, maxRows: 12 }" class="dq-query" spellcheck="false"
          :placeholder="$t('dataCompare:query.placeholder')" :aria-label="$t('dataCompare:query.queryOf', { side: s.name === 'left' ? $t('dataCompare:left') : $t('dataCompare:right') })"
        />
      </div>
    </div>

    <div class="dq-bar">
      <span class="dq-label">{{ $t('dataCompare:query.columns') }}</span>
      <el-button size="small" :loading="busy" :disabled="busy" @click="emit('read')">{{ $t('dataCompare:query.readColumns') }}</el-button>
      <span v-if="stale && pairs.length" class="dq-stale">{{ $t('dataCompare:query.stale') }}</span>
      <span v-else-if="!pairs.length" class="dq-hint">{{ $t('dataCompare:query.columnsHint') }}</span>
    </div>
    <table v-if="pairs.length" class="dq-pairs">
      <thead>
        <tr>
          <th class="dq-key">{{ $t('dataCompare:keyLabel') }}</th>
          <th>{{ $t('dataCompare:left') }}</th>
          <th>{{ $t('dataCompare:right') }}</th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="(row, i) in pairs" :key="i" :class="{ off: !row.right }">
          <td class="dq-key"><el-checkbox v-model="row.key" size="small" :disabled="!row.right" :aria-label="$t('dataCompare:query.keyOf', { column: row.left })" /></td>
          <td class="dq-name" :title="typeOf(columns.left, row.left)">{{ row.left }}</td>
          <td>
            <el-select
              :model-value="row.right" size="small" clearable filterable class="dq-right" :placeholder="$t('dataCompare:query.notCompared')"
              @update:model-value="(v: string | undefined) => setRight(row, v)"
            >
              <el-option v-for="n in rightNames" :key="n" :label="n" :value="n" />
            </el-select>
          </td>
        </tr>
      </tbody>
    </table>
    <div v-if="pairs.length && unpaired.length" class="dq-hint dq-unpaired">{{ $t('dataCompare:query.unpairedRight', { list: unpaired.join(', ') }) }}</div>

    <div class="dq-opts">
      <span class="dq-label">{{ $t('dataCompare:query.tolerance') }}</span>
      <span class="dq-opt" :title="$t('dataCompare:query.absoluteHint')">
        ±<el-input-number v-model="form.absolute" :min="0" :step="0.01" size="small" controls-position="right" class="dq-num" :aria-label="$t('dataCompare:query.absolute')" />
      </span>
      <span class="dq-opt" :title="$t('dataCompare:query.relativeHint')">
        {{ $t('dataCompare:query.or') }}
        <el-input-number v-model="form.relativePct" :min="0" :step="0.1" size="small" controls-position="right" class="dq-num" :aria-label="$t('dataCompare:query.relative')" />
        %
      </span>
      <el-checkbox v-model="form.nullAsBlank" size="small" :title="$t('dataCompare:query.nullAsBlankHint')">{{ $t('dataCompare:query.nullAsBlank') }}</el-checkbox>
      <el-checkbox v-model="form.ignoreCase" size="small">{{ $t('dataCompare:query.ignoreCase') }}</el-checkbox>
      <el-checkbox v-model="form.trim" size="small">{{ $t('dataCompare:query.trim') }}</el-checkbox>
    </div>
    <div class="dq-hint">{{ $t('dataCompare:query.readOnly') }}</div>
  </div>
</template>

<style scoped>
.dq { display: flex; flex-direction: column; gap: 8px; flex-basis: 100%; }
.dq-sides { display: grid; grid-template-columns: repeat(auto-fit, minmax(320px, 1fr)); gap: 10px 18px; }
.dq-side { display: flex; flex-direction: column; gap: 6px; min-width: 0; }
.dq-head { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
.dq-label { font-size: 11px; font-weight: 600; letter-spacing: .05em; text-transform: uppercase; color: var(--nm-text-dim); }
.dq-conn { width: 180px; }
.dq-db { width: 150px; }
.dq-lang { font-size: 10.5px; padding: 0 6px; border: 1px solid var(--nm-border); border-radius: 8px; color: var(--nm-text-dim); }
.dq-query :deep(textarea) { font-family: var(--nm-mono); font-size: 12px; }
.dq-bar { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.dq-stale { font-size: 12px; color: var(--nm-warning); }
.dq-hint { font-size: 12px; color: var(--nm-text-dim); }
.dq-pairs { border-collapse: collapse; font-size: 12px; max-width: 760px; }
.dq-pairs th { text-align: left; font-weight: 500; color: var(--nm-text-dim); padding: 2px 10px 2px 0; border-bottom: 1px solid var(--nm-border); }
.dq-pairs td { padding: 2px 10px 2px 0; border-bottom: 1px solid var(--nm-border-soft); }
.dq-pairs tr.off .dq-name { color: var(--nm-text-dim); }
.dq-key { width: 48px; text-align: center !important; }
.dq-name { font-family: var(--nm-mono); white-space: nowrap; color: var(--nm-text-strong); }
.dq-right { width: 240px; }
.dq-opts { display: flex; align-items: center; gap: 6px 14px; flex-wrap: wrap; font-size: 12.5px; }
.dq-opt { display: inline-flex; align-items: center; gap: 4px; color: var(--nm-text); }
.dq-num { width: 130px; }
</style>
