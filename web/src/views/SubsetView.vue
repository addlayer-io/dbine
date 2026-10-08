<script setup lang="ts">
import { computed, onUnmounted, reactive, ref, watch } from 'vue';
import { listen } from '@tauri-apps/api/event';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import {
  subsetApi, type FakeKind, type FilterOp, type MaskRule, type PlanColumn, type PlanTable, type SubsetArgs, type SubsetPlan,
  type SubsetProgress, type SubsetReport, type SubsetTableMask,
} from '../api/subset';
import { tb } from '../i18n/backend';
import { objKey, useConnectionsStore } from '../stores/connections';
import type { SubsetTab } from '../stores/tabs';
import { runTask } from '../stores/tasks';

// "Copiar un subconjunto" of a table (docs/subconjunto-de-datos.md): which
// rows, how far to follow the foreign keys, where they go; then a plan
// (tables, rows, order, what's created) with a masking rule per column,
// and the copy, confirmed, in the background. The source is only read.

const props = defineProps<{ tab: SubsetTab }>();
const { t } = useTranslation();
const conns = useConnectionsStore();

// -- what to copy -----------------------------------------------------------------------

const filterKind = ref<'none' | 'expression' | 'columns'>('none');
const expression = ref('');
interface ColFilter { column: string; op: FilterOp; value: string }
const colFilters = ref<ColFilter[]>([]);
const OPS: FilterOp[] = ['eq', 'ne', 'gt', 'ge', 'lt', 'le', 'contains', 'starts_with', 'in', 'is_null', 'not_null'];
const limitKind = ref<'all' | 'rows' | 'percent'>('rows');
const limitRows = ref(1000);
const limitPercent = ref(10);
const withChildren = ref(false);
const depth = ref(1);
const maxRows = ref(1000);
const target = reactive({ connectionId: '', database: '' });

const driver = computed(() => conns.driverOf(props.tab.connectionId));
/** Engines with a condition language (SQL, CQL, N1QL…); the rest filter by column. */
const hasExpression = computed(() => ['sql', 'cql'].includes(driver.value?.language ?? ''));
const startColumns = computed(() => {
  const o = props.tab.object;
  return (conns.columns[objKey(props.tab.connectionId, props.tab.database, o.schema, o.name)]?.items ?? []).map((c) => c.name);
});
conns.loadColumns(props.tab.connectionId, props.tab.database, { kind: props.tab.object.kind, schema: props.tab.object.schema, name: props.tab.object.name, parent: null });

const targetDbs = computed(() => conns.live[target.connectionId]?.databases ?? []);
watch(() => target.connectionId, async (id) => {
  if (!id || !(await conns.ensureConnected(id))) return;
  const live = conns.live[id];
  if (live && live.databases.length && !live.databases.includes(target.database)) target.database = live.defaultDatabase || live.databases[0];
});
const targetName = computed(() => [conns.byId(target.connectionId)?.name, target.database].filter(Boolean).join(' · '));

function typed(v: string): unknown {
  return /^-?\d+(\.\d+)?$/.test(v.trim()) ? Number(v) : v;
}

function args(runId: string): SubsetArgs {
  const limit = limitKind.value === 'rows' ? { kind: 'rows' as const, count: limitRows.value }
    : limitKind.value === 'percent' ? { kind: 'percent' as const, percent: limitPercent.value } : { kind: 'all' as const };
  return {
    run_id: runId,
    connection_id: props.tab.connectionId,
    database: props.tab.database,
    table: props.tab.object,
    filter: {
      expression: filterKind.value === 'expression' && expression.value.trim() ? expression.value : null,
      columns: filterKind.value === 'columns'
        ? colFilters.value.filter((f) => f.column).map((f) => ({
          column: f.column, op: f.op,
          values: ['is_null', 'not_null'].includes(f.op) ? [] : f.op === 'in' ? f.value.split(',').map((x) => typed(x.trim())) : [typed(f.value)],
        }))
        : [],
      limit,
    },
    children: withChildren.value ? { depth: depth.value, max_rows: maxRows.value } : null,
    target_connection_id: target.connectionId,
    target_database: target.database,
  };
}

// -- the plan ---------------------------------------------------------------------------

const plan = ref<SubsetPlan | null>(null);
const planning = ref(false);
const planError = ref<string | null>(null);
const progress = ref('');
const report = ref<SubsetReport | null>(null);
const expanded = ref<Set<string>>(new Set());
let planId = '';

const tableKey = (p: { schema: string | null; name: string }) => `${p.schema ?? ''}\u0001${p.name}`;
/** The rule chosen per column (`<schema>\u0001<table>\u0001<column>`). */
const rules = reactive<Record<string, MaskRule>>({});
const colKey = (tbl: PlanTable, c: PlanColumn) => `${tableKey(tbl)}\u0001${c.name}`;

function progressText(p: SubsetProgress): string {
  if (p.phase === 'collect') return t('subset:progress.collect', { table: p.table ?? '', rows: p.rows.toLocaleString() });
  return t(`subset:progress.${p.phase}`, { table: p.table ?? '' });
}

async function makePlan() {
  if (planning.value || !target.connectionId) return;
  if (!(await conns.ensureConnected(props.tab.connectionId))) return;
  planning.value = true;
  planError.value = null;
  report.value = null;
  planId = `${props.tab.id}-plan-${Date.now()}`;
  const id = planId;
  const unlisten = await listen<SubsetProgress>('subset-progress', (e) => {
    if (e.payload.runId === id) progress.value = progressText(e.payload);
  });
  try {
    const p = await subsetApi.plan(args(id));
    for (const tbl of p.tables) for (const c of tbl.columns) {
      const k = colKey(tbl, c);
      if (!rules[k]) rules[k] = { ...c.suggested };
    }
    plan.value = p;
    expanded.value = new Set(p.tables.filter((x) => x.columns.some((c) => !c.key && c.suggested.rule !== 'keep')).map(tableKey));
  } catch (e) {
    planError.value = errorMessage(e);
    plan.value = null;
  } finally {
    unlisten();
    planning.value = false;
    progress.value = '';
  }
}
function cancelPlan() {
  if (planning.value) subsetApi.cancel(planId);
}
onUnmounted(cancelPlan);

const blocked = computed(() => plan.value?.tables.filter((x) => x.error) ?? []);
const created = computed(() => plan.value?.tables.filter((x) => !x.exists && !x.error).length ?? 0);
const maskedKeys = computed(() => plan.value?.tables.flatMap((tbl) => tbl.columns.filter((c) => c.key && rules[colKey(tbl, c)]?.rule !== 'keep').map((c) => `${tbl.name}.${c.name}`)) ?? []);

function toggle(tbl: PlanTable) {
  const s = new Set(expanded.value);
  const k = tableKey(tbl);
  if (s.has(k)) s.delete(k);
  else s.add(k);
  expanded.value = s;
}

// -- masking rules ----------------------------------------------------------------------

const FAKES: FakeKind[] = ['name', 'first_name', 'last_name', 'email', 'phone', 'document', 'address', 'city', 'company'];
const RULE_IDS = ['keep', ...FAKES.map((f) => `fake:${f}`), 'shift_date', 'noise', 'fixed', 'null', 'hash'];

function ruleId(r: MaskRule | undefined): string {
  if (!r) return 'keep';
  return r.rule === 'fake' ? `fake:${r.kind}` : r.rule;
}
function ruleLabel(id: string): string {
  return id.startsWith('fake:') ? t('subset:rules.fake', { kind: t(`subset:fakes.${id.slice(5)}`) }) : t(`subset:rules.${id}`);
}
function setRule(tbl: PlanTable, c: PlanColumn, id: string) {
  const k = colKey(tbl, c);
  const prev = rules[k];
  if (id.startsWith('fake:')) rules[k] = { rule: 'fake', kind: id.slice(5) as FakeKind };
  else if (id === 'shift_date') rules[k] = { rule: 'shift_date', days: prev?.rule === 'shift_date' ? prev.days : 30 };
  else if (id === 'noise') rules[k] = { rule: 'noise', percent: prev?.rule === 'noise' ? prev.percent : 10 };
  else if (id === 'fixed') rules[k] = { rule: 'fixed', value: prev?.rule === 'fixed' ? prev.value : '' };
  else rules[k] = { rule: id } as MaskRule;
}
/** A setting of the column's rule (`days`, `percent`, `value`). */
function param(tbl: PlanTable, c: PlanColumn, k: 'days' | 'percent' | 'value'): unknown {
  return (rules[colKey(tbl, c)] as Record<string, unknown> | undefined)?.[k];
}
function setParam(tbl: PlanTable, c: PlanColumn, k: 'days' | 'percent' | 'value', v: unknown) {
  const r = rules[colKey(tbl, c)];
  if (r) rules[colKey(tbl, c)] = { ...r, [k]: v } as MaskRule;
}
function resetRules() {
  for (const tbl of plan.value?.tables ?? []) for (const c of tbl.columns) rules[colKey(tbl, c)] = { ...c.suggested };
}
const maskedCount = (tbl: PlanTable) => tbl.columns.filter((c) => !c.skipped && rules[colKey(tbl, c)]?.rule !== 'keep').length;

function masks(): SubsetTableMask[] {
  const out: SubsetTableMask[] = [];
  for (const tbl of plan.value?.tables ?? []) {
    const columns: Record<string, MaskRule> = {};
    for (const c of tbl.columns) {
      const r = rules[colKey(tbl, c)];
      if (r && r.rule !== 'keep' && !c.skipped) columns[c.name] = r;
    }
    if (Object.keys(columns).length) out.push({ schema: tbl.schema, name: tbl.name, columns });
  }
  return out;
}

// -- the copy ---------------------------------------------------------------------------

const copying = ref(false);

async function copy() {
  const p = plan.value;
  if (!p || copying.value || blocked.value.length) return;
  const summary = t('subset:confirm', {
    rows: p.total_rows.toLocaleString(), tables: p.tables.length, target: targetName.value, created,
    masked: p.tables.reduce((n, x) => n + maskedCount(x), 0),
  });
  try {
    if (p.confirm_label) {
      const word = p.confirm_label;
      await ElMessageBox.prompt(`${summary}\n\n${t('subset:confirmProd', { name: word })}`, t('subset:confirmTitle'), {
        confirmButtonText: t('subset:copy'), cancelButtonText: t('common:cancel'), type: 'warning', confirmButtonClass: 'el-button--danger',
        inputValidator: (v) => v.trim() === word || t('subset:typeName', { name: word }),
      });
    } else {
      await ElMessageBox.confirm(summary, t('subset:confirmTitle'), { confirmButtonText: t('subset:copy'), cancelButtonText: t('common:cancel'), type: 'warning' });
    }
  } catch {
    return;
  }
  const id = `${props.tab.id}-run-${Date.now()}`;
  const a = args(id);
  const m = masks();
  const confirm = p.confirm_label ?? '';
  copying.value = true;
  report.value = null;
  runTask<SubsetReport>({
    kind: 'subset',
    title: t('subset:task', { table: props.tab.object.name, target: targetName.value }),
    connectionId: target.connectionId, database: target.database, background: true,
    cancel: () => subsetApi.cancel(id),
    run: async (task) => {
      await task.listen<SubsetProgress>('subset-progress', (e) => {
        if (e.payload.runId !== id) return;
        const phase = progressText(e.payload);
        if (e.payload.phase === 'insert' && e.payload.total) task.progress({ phase, done: e.payload.rows, total: e.payload.total, unit: 'rows' });
        else task.progress({ phase });
      });
      try {
        const r = await subsetApi.run(a, m, confirm);
        report.value = r;
        return r;
      } finally {
        copying.value = false;
      }
    },
    summary: (r) => t('subset:done', { rows: r.tables.reduce((n, x) => n + x.written, 0).toLocaleString(), tables: r.tables.filter((x) => x.status === 'done').length }),
    outcome: (r) => (r.cancelled ? 'cancelled' : r.tables.some((x) => x.status === 'error') ? 'error' : 'done'),
  });
  ElMessage.info(t('subset:started'));
}
</script>

<template>
  <div class="sv">
    <header class="sv-head">
      <strong>{{ $t('subset:title', { name: tab.object.name }) }}</strong>
      <span class="nm-muted">{{ [conns.byId(tab.connectionId)?.name, tab.database].filter(Boolean).join(' · ') }}</span>
      <div class="sv-spacer" />
      <el-button v-if="planning" size="small" @click="cancelPlan">{{ $t('common:cancel') }}</el-button>
      <el-button size="small" :loading="planning" :disabled="!target.connectionId" @click="makePlan">{{ plan ? $t('subset:replan') : $t('subset:plan') }}</el-button>
      <el-button size="small" type="primary" :disabled="!plan || planning || copying || blocked.length > 0" :loading="copying" @click="copy">{{ $t('subset:copyEllipsis') }}</el-button>
    </header>

    <div class="sv-body">
      <section class="sv-form">
        <h3 class="nm-section-title">{{ $t('subset:rowsTitle') }}</h3>
        <div class="sv-row">
          <span class="sv-label">{{ $t('subset:filter') }}</span>
          <el-radio-group v-model="filterKind" size="small">
            <el-radio-button value="none">{{ $t('subset:filterNone') }}</el-radio-button>
            <el-radio-button v-if="hasExpression" value="expression">{{ $t('subset:filterExpression') }}</el-radio-button>
            <el-radio-button value="columns">{{ $t('subset:filterColumns') }}</el-radio-button>
          </el-radio-group>
        </div>
        <div v-if="filterKind === 'expression'" class="sv-row">
          <span class="sv-label" />
          <el-input v-model="expression" type="textarea" :rows="2" class="sv-mono" :placeholder="$t('subset:expressionPlaceholder')" />
        </div>
        <template v-if="filterKind === 'columns'">
          <div v-for="(f, i) in colFilters" :key="i" class="sv-row">
            <span class="sv-label" />
            <el-select v-model="f.column" size="small" filterable class="sv-col" :placeholder="$t('subset:column')">
              <el-option v-for="c in startColumns" :key="c" :label="c" :value="c" />
            </el-select>
            <el-select v-model="f.op" size="small" class="sv-op">
              <el-option v-for="o in OPS" :key="o" :label="$t(`subset:ops.${o}`)" :value="o" />
            </el-select>
            <el-input v-if="!['is_null', 'not_null'].includes(f.op)" v-model="f.value" size="small" class="sv-val" :placeholder="f.op === 'in' ? $t('subset:inPlaceholder') : ''" />
            <el-button size="small" text @click="colFilters.splice(i, 1)"><el-icon><ei-close /></el-icon></el-button>
          </div>
          <div class="sv-row">
            <span class="sv-label" />
            <el-button size="small" text @click="colFilters.push({ column: '', op: 'eq', value: '' })">{{ $t('subset:addFilter') }}</el-button>
          </div>
        </template>
        <div class="sv-row">
          <span class="sv-label">{{ $t('subset:limit') }}</span>
          <el-select v-model="limitKind" size="small" class="sv-op">
            <el-option value="all" :label="$t('subset:limitAll')" />
            <el-option value="rows" :label="$t('subset:limitRows')" />
            <el-option value="percent" :label="$t('subset:limitPercent')" />
          </el-select>
          <el-input-number v-if="limitKind === 'rows'" v-model="limitRows" :min="1" :max="1000000" :step="100" size="small" controls-position="right" />
          <el-input-number v-if="limitKind === 'percent'" v-model="limitPercent" :min="0.01" :max="100" :step="5" size="small" controls-position="right" />
        </div>

        <h3 class="nm-section-title">{{ $t('subset:relationsTitle') }}</h3>
        <p class="nm-muted sv-hint">{{ $t('subset:parentsHint') }}</p>
        <div class="sv-row">
          <el-checkbox v-model="withChildren" size="small">{{ $t('subset:children') }}</el-checkbox>
        </div>
        <div v-if="withChildren" class="sv-row">
          <span class="sv-label">{{ $t('subset:depth') }}</span>
          <el-input-number v-model="depth" :min="1" :max="10" size="small" controls-position="right" />
          <span class="sv-label sv-label-inline">{{ $t('subset:maxRows') }}</span>
          <el-input-number v-model="maxRows" :min="1" :max="1000000" :step="500" size="small" controls-position="right" />
        </div>

        <h3 class="nm-section-title">{{ $t('subset:targetTitle') }}</h3>
        <div class="sv-row">
          <span class="sv-label">{{ $t('subset:target') }}</span>
          <el-select v-model="target.connectionId" filterable size="small" class="sv-col" :placeholder="$t('subset:connection')">
            <el-option v-for="c in conns.list" :key="c.id" :label="c.name" :value="c.id" :disabled="c.config.read_only" />
          </el-select>
          <el-select v-if="targetDbs.length" v-model="target.database" filterable size="small" class="sv-col">
            <el-option v-for="d in targetDbs" :key="d" :label="d" :value="d" />
          </el-select>
        </div>
        <p class="nm-muted sv-hint">{{ $t('subset:targetHint') }}</p>
      </section>

      <el-alert v-if="planError" type="error" :title="tb(planError)" :closable="false" show-icon class="sv-alert" />
      <div v-if="planning" class="sv-empty"><el-icon class="is-loading" :size="18"><ei-loading /></el-icon><span>{{ progress || $t('subset:planning') }}</span></div>

      <section v-if="plan && !planning" class="sv-plan">
        <div class="sv-plan-head">
          <h3 class="nm-section-title">{{ $t('subset:planTitle') }}</h3>
          <span class="nm-muted">{{ $t('subset:planSummary', { rows: plan.total_rows.toLocaleString(), tables: plan.tables.length, created, engine: plan.target_engine }) }}</span>
          <div class="sv-spacer" />
          <el-button size="small" text @click="resetRules">{{ $t('subset:resetRules') }}</el-button>
        </div>
        <el-alert v-for="c in plan.cycles" :key="c" type="warning" :title="tb(c)" :closable="false" show-icon class="sv-alert" />
        <el-alert v-for="n in plan.notes" :key="n" type="info" :title="tb(n)" :closable="false" show-icon class="sv-alert" />
        <el-alert v-if="maskedKeys.length" type="warning" :title="$t('subset:maskedKeys', { list: maskedKeys.join(', ') })" :closable="false" show-icon class="sv-alert" />

        <div v-for="(tbl, i) in plan.tables" :key="tableKey(tbl)" class="sv-table" :class="{ error: tbl.error }">
          <button class="sv-table-row" @click="toggle(tbl)">
            <span class="sv-n">{{ i + 1 }}</span>
            <el-icon class="sv-chev" :class="{ open: expanded.has(tableKey(tbl)) }"><ei-arrow-right /></el-icon>
            <strong>{{ tbl.schema ? `${tbl.schema}.${tbl.name}` : tbl.name }}</strong>
            <span class="sv-badge" :class="tbl.role">{{ $t(`subset:roles.${tbl.role}`) }}</span>
            <span class="sv-rows">{{ $t('subset:rowsCount', { count: tbl.rows }) }}<span v-if="tbl.capped" class="sv-capped"> · {{ $t('subset:capped') }}</span></span>
            <span class="sv-spacer" />
            <span v-if="maskedCount(tbl)" class="sv-badge masked">{{ $t('subset:maskedCount', { count: maskedCount(tbl) }) }}</span>
            <span class="nm-muted sv-target">→ {{ tbl.target }} · {{ tbl.error ? $t('subset:cannot') : tbl.exists ? $t('subset:exists') : $t('subset:willCreate') }}</span>
          </button>
          <div v-if="expanded.has(tableKey(tbl))" class="sv-detail">
            <el-alert v-if="tbl.error" type="error" :title="tb(tbl.error)" :closable="false" show-icon class="sv-alert" />
            <table class="sv-cols">
              <thead><tr><th>{{ $t('subset:column') }}</th><th>{{ $t('subset:type') }}</th><th>{{ $t('subset:rule') }}</th><th /></tr></thead>
              <tbody>
                <tr v-for="c in tbl.columns" :key="c.name" :class="{ skipped: c.skipped }">
                  <td>
                    {{ c.name }}
                    <span v-if="c.key" class="sv-tag">{{ $t('subset:key') }}</span>
                    <span v-if="c.suggested.rule !== 'keep'" class="sv-tag pii" :title="$t('subset:piiTitle')">{{ $t('subset:pii') }}</span>
                  </td>
                  <td class="nm-muted">{{ c.data_type }}</td>
                  <td>
                    <span v-if="c.skipped" class="nm-muted">{{ tb(c.skipped) }}</span>
                    <div v-else class="sv-rule">
                      <el-select :model-value="ruleId(rules[colKey(tbl, c)])" size="small" class="sv-rule-select" @update:model-value="(v: string) => setRule(tbl, c, v)">
                        <el-option v-for="r in RULE_IDS" :key="r" :value="r" :label="ruleLabel(r)" :disabled="r === 'null' && !c.nullable" />
                      </el-select>
                      <template v-if="rules[colKey(tbl, c)]?.rule === 'shift_date'">
                        <span class="nm-muted">±</span>
                        <el-input-number :model-value="param(tbl, c, 'days') as number" @update:model-value="(v: number | undefined) => setParam(tbl, c, 'days', v ?? 1)" :min="1" :max="36500" size="small" controls-position="right" />
                        <span class="nm-muted">{{ $t('subset:days') }}</span>
                      </template>
                      <template v-else-if="rules[colKey(tbl, c)]?.rule === 'noise'">
                        <span class="nm-muted">±</span>
                        <el-input-number :model-value="param(tbl, c, 'percent') as number" @update:model-value="(v: number | undefined) => setParam(tbl, c, 'percent', v ?? 1)" :min="0.1" :max="100" size="small" controls-position="right" />
                        <span class="nm-muted">%</span>
                      </template>
                      <el-input v-else-if="rules[colKey(tbl, c)]?.rule === 'fixed'" :model-value="param(tbl, c, 'value') as string" @update:model-value="(v: string) => setParam(tbl, c, 'value', v)" size="small" class="sv-val" />
                    </div>
                  </td>
                  <td>
                    <span v-if="c.key && rules[colKey(tbl, c)]?.rule !== 'keep'" class="sv-warn" :title="$t('subset:keyWarning')">⚠ {{ $t('subset:keyMasked') }}</span>
                  </td>
                </tr>
              </tbody>
            </table>
            <details v-if="tbl.create_ddl" class="sv-ddl">
              <summary>{{ $t('subset:showDdl') }}</summary>
              <pre>{{ tbl.create_ddl }}</pre>
            </details>
          </div>
        </div>
      </section>

      <section v-if="report" class="sv-report">
        <h3 class="nm-section-title">{{ report.cancelled ? $t('subset:reportCancelled') : $t('subset:reportTitle') }}</h3>
        <table class="sv-cols">
          <thead><tr><th>{{ $t('subset:table') }}</th><th>{{ $t('subset:target') }}</th><th>{{ $t('subset:written') }}</th><th>{{ $t('subset:status') }}</th></tr></thead>
          <tbody>
            <tr v-for="r in report.tables" :key="r.table" :class="r.status">
              <td>{{ r.table }}<span v-if="r.masked.length" class="sv-tag pii" :title="r.masked.join(', ')">{{ $t('subset:maskedCount', { count: r.masked.length }) }}</span></td>
              <td class="nm-muted">{{ r.target }}<span v-if="r.created"> · {{ $t('subset:created') }}</span></td>
              <td>{{ r.written.toLocaleString() }} / {{ r.rows.toLocaleString() }}</td>
              <td>
                {{ $t(`subset:statuses.${r.status}`) }}
                <div v-if="r.error" class="sv-error">{{ tb(r.error) }}</div>
                <div v-for="n in r.notes" :key="n" class="nm-muted">{{ tb(n) }}</div>
              </td>
            </tr>
          </tbody>
        </table>
        <div v-for="n in report.notes" :key="n" class="nm-muted sv-hint">· {{ tb(n) }}</div>
      </section>
    </div>
  </div>
</template>

<style scoped>
.sv { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--nm-bg); }
.sv-head { display: flex; align-items: center; gap: 10px; padding: 10px 16px; border-bottom: 1px solid var(--nm-border-soft); }
.sv-head .el-button { margin: 0; }
.sv-head strong { color: var(--nm-text-strong); }
.sv-spacer { flex: 1; }
.sv-body { flex: 1; overflow: auto; padding: 8px 16px 24px; }
.sv-form { max-width: 860px; }
.sv-form h3 { margin: 14px 0 6px; }
.sv-row { display: flex; align-items: center; gap: 8px; margin: 6px 0; }
.sv-label { width: 110px; flex: none; font-size: 12.5px; color: var(--nm-text-dim); }
.sv-label-inline { width: auto; margin-left: 12px; }
.sv-col { width: 220px; }
.sv-op { width: 170px; }
.sv-val { width: 220px; }
.sv-mono :deep(textarea) { font-family: var(--nm-mono); font-size: 12px; }
.sv-hint { margin: 2px 0 6px; font-size: 12px; }
.sv-alert { margin: 6px 0; width: auto; }
.sv-empty { display: flex; align-items: center; gap: 8px; padding: 24px 0; color: var(--nm-text-dim); }
.sv-plan { margin-top: 18px; }
.sv-plan-head { display: flex; align-items: baseline; gap: 10px; margin-bottom: 6px; }
.sv-plan-head h3 { margin: 0; }
.sv-table { border: 1px solid var(--nm-border-soft); border-radius: 4px; margin-bottom: 6px; }
.sv-table.error { border-left: 3px solid var(--nm-danger); }
.sv-table-row { display: flex; align-items: center; gap: 8px; width: 100%; padding: 7px 10px; border: 0; background: none; cursor: pointer; font: inherit; color: var(--nm-text); text-align: left; }
.sv-table-row:hover { background: var(--ide-hover); }
.sv-n { width: 18px; color: var(--nm-text-dim); font-size: 11px; text-align: right; }
.sv-chev { transition: transform 0.15s; color: var(--nm-text-dim); }
.sv-chev.open { transform: rotate(90deg); }
.sv-badge { font-size: 10.5px; padding: 0 6px; border-radius: 8px; background: var(--ide-hover); color: var(--nm-text-dim); }
.sv-badge.start { color: var(--nm-info); }
.sv-badge.masked { color: var(--nm-warning); }
.sv-rows { font-size: 12px; color: var(--nm-text-dim); }
.sv-capped { color: var(--nm-warning); }
.sv-target { font-size: 12px; }
.sv-detail { padding: 0 12px 10px 36px; }
.sv-cols { width: 100%; border-collapse: collapse; font-size: 12.5px; }
.sv-cols th { text-align: left; font-weight: 500; color: var(--nm-text-dim); padding: 4px 6px; border-bottom: 1px solid var(--nm-border-soft); }
.sv-cols td { padding: 3px 6px; vertical-align: middle; }
.sv-cols tr.skipped td { color: var(--nm-text-dim); }
.sv-cols tr.error td { color: var(--nm-danger); }
.sv-tag { margin-left: 6px; font-size: 10px; padding: 0 5px; border-radius: 3px; background: var(--ide-hover); color: var(--nm-text-dim); }
.sv-tag.pii { color: var(--nm-warning); }
.sv-rule { display: flex; align-items: center; gap: 6px; }
.sv-rule-select { width: 220px; }
.sv-warn { font-size: 11.5px; color: var(--nm-warning); }
.sv-error { color: var(--nm-danger); font-size: 12px; }
.sv-ddl { margin-top: 8px; font-size: 12px; }
.sv-ddl pre {
  margin: 6px 0 0; padding: 8px 10px; max-height: 200px; overflow: auto; white-space: pre-wrap;
  font-family: var(--nm-mono); font-size: 12px; background: var(--nm-bg-elev); border: 1px solid var(--nm-border-soft); border-radius: 3px;
}
.sv-report { margin-top: 18px; }
</style>
