<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import type { Cell, ChartAggregate, ResultColumn } from '../api/types';
import { chartApi } from '../api/chart';
import { errorKind, errorMessage } from '../api/client';
import { locale } from '../i18n';
import { saveBinaryFile } from '../composables/files';
import { resolvedTheme } from '../composables/appearance';
import {
  CHART_KINDS, HISTOGRAM_BINS, NEEDS_X, ONE_VALUE, SPLITS, resolveChartSettings, sameJson,
  type AggregateInfo, type ChartAgg, type ChartKind, type ChartPoints, type ChartSettings, type ChartSort, type ChartState,
} from '../composables/chartSettings';
import { dateColumns, num, numericColumns, shapeFromPoints, shapePoints, shapeRows, MAX_SERIES, type ResolvedChart } from '../composables/chartShape';
import { chartOption, INKS } from '../composables/chartOption';
import { chartLabels, echarts, fmtNumber, kpiCaption } from '../composables/chartRender';
import { aggregateRequest, shapeFromAggregate } from '../composables/chartWhole';

// A chart of a result set, for any engine (it works on the grid's rows).
// The form comes from the data: a date column → lines, a text column →
// bars, two numeric columns → dispersion, one row → a KPI, one numeric
// column alone → a histogram; the user can change any choice. The shapes
// are in composables/chartShape.ts, the ECharts option in chartOption.ts.

const props = defineProps<{
  columns: ResultColumn[];
  rows: Cell[][];
  /** Settings to start from (by column name); defaults where they don't fit.
   *  Absent: changes are emitted only once the user makes one. */
  settings?: ChartSettings | null;
  /** Points kept with the settings (a notebook's whole-result aggregate):
   *  shown while "Calcular sobre todo el resultado" is on and not recomputed. */
  storedPoints?: ChartPoints | null;
  storedAggregate?: AggregateInfo | null;
  /** What produced the rows (a run made here): "Calcular sobre todo el
   *  resultado" runs that query again, aggregated. Absent: not offered
   *  (stored whole-result points still show). */
  source?: { connectionId: string; database: string; sql: string } | null;
}>();
/** The user changed the chart (or its data changed after they did). */
const emit = defineEmits<{ 'update:settings': [state: ChartState] }>();

type Kind = ChartKind;
type Agg = ChartAgg;

const { t } = useTranslation();
const KINDS = computed<{ id: Kind; label: string }[]>(() => CHART_KINDS.map((id) => ({ id, label: t(`results:chart.kind.${id}`) })));
const AGGS = computed<{ id: Agg; label: string }[]>(() =>
  (['none', 'sum', 'avg', 'count', 'min', 'max'] as Agg[]).filter((id) => id !== 'none' || !['heatmap', 'kpi'].includes(kind.value))
    .map((id) => ({ id, label: t(`results:chart.agg.${id}`) })));

// -- column typing -------------------------------------------------------------------
const numericCols = computed(() => numericColumns(props.rows, props.columns.length));
const dateCols = computed(() => dateColumns(props.rows, props.columns.length));
const textCols = computed(() => props.columns.map((_, i) => i).filter((i) => !numericCols.value.includes(i)));

// -- settings (defaults picked from the data) -------------------------------------------
const kind = ref<Kind>('bar');
const xCol = ref<number>(0);
const yCols = ref<number[]>([]);
const agg = ref<Agg>('none');
const sortBy = ref<ChartSort>('none');
/** One series per distinct value of this column (a pivot); a heatmap's Y. */
const splitCol = ref<number | null>(null);
const bins = ref<number>(0);
/** "Calcular sobre todo el resultado". */
const whole = ref(false);

function pickDefaults() {
  const nums = numericCols.value;
  const dates = dateCols.value;
  const text = textCols.value[0];
  bins.value = 0;
  whole.value = false;
  if (props.rows.length === 1 && nums.length && !dates.length) {
    // One row: its first number, big.
    kind.value = 'kpi';
    xCol.value = text ?? 0;
    yCols.value = [nums[0]];
    agg.value = 'sum';
    sortBy.value = 'none';
    splitCol.value = null;
    return;
  }
  if (nums.length === 1 && text === undefined) {
    // One numeric column alone: how its values spread.
    kind.value = 'histogram';
    xCol.value = nums[0];
    yCols.value = [nums[0]];
    agg.value = 'none';
    sortBy.value = 'none';
    splitCol.value = null;
    return;
  }
  if (dates.length) {
    xCol.value = dates[0];
    kind.value = 'line';
  } else if (text !== undefined) {
    xCol.value = text;
    kind.value = 'bar';
  } else if (nums.length >= 2) {
    xCol.value = nums[0];
    kind.value = 'scatter';
  } else {
    xCol.value = 0;
    kind.value = 'bar';
  }
  // Only measures of comparable size share the axis (never a second axis):
  // the first numeric column, plus others within 10× of it.
  const candidates = nums.filter((i) => i !== xCol.value);
  const peak = (i: number) => Math.max(1e-9, ...props.rows.slice(0, 500).map((r) => Math.abs(num(r[i]) ?? 0)));
  const first = candidates[0];
  yCols.value = first === undefined ? [] : candidates.filter((i) => {
    const ratio = peak(i) / peak(first);
    return ratio <= 10 && ratio >= 0.1;
  }).slice(0, 3);
  // A second text column with a few values: one series per value.
  const splitCandidate = textCols.value.find((i) => i !== xCol.value &&
    new Set(props.rows.slice(0, 1000).map((r) => r[i])).size <= MAX_SERIES + 1);
  splitCol.value = kind.value !== 'scatter' && splitCandidate !== undefined && yCols.value.length ? splitCandidate : null;
  if (splitCol.value !== null) yCols.value = yCols.value.slice(0, 1);
  // Many rows per category: group them.
  const distinct = new Set(props.rows.map((r) => String(r[xCol.value]))).size;
  agg.value = kind.value !== 'scatter' && distinct < props.rows.length * 0.8 ? 'sum' : 'none';
  sortBy.value = 'none';
}
/** Group charts, histograms, box plots and KPIs can be computed over the
 *  whole result (not scatters, nor a value per row). */
const wholeApplies = computed(() => ['histogram', 'box', 'kpi'].includes(kind.value) || (kind.value !== 'scatter' && agg.value !== 'none'));
const name = (i: number | null) => (i === null || i < 0 ? null : props.columns[i]?.name ?? null);
const settingsNow = computed<ChartSettings>(() => {
  const s: ChartSettings = {
    kind: kind.value,
    x: kind.value === 'histogram' || kind.value === 'kpi' ? null : name(xCol.value),
    y: yCols.value.map((i) => props.columns[i]?.name).filter((n): n is string => n !== undefined),
    agg: agg.value,
    sort: sortBy.value,
    split: SPLITS.includes(kind.value) || kind.value === 'heatmap' ? name(splitCol.value) : null,
  };
  if (kind.value === 'histogram' && bins.value > 0) s.bins = bins.value;
  if (whole.value && wholeApplies.value) s.whole = true;
  return s;
});
/** The defaults' settings, and whether the chart is emitted: once the user
 *  changes it, or always when the parent keeps settings (passes the prop,
 *  even null: it then gets the defaults and their points on mount). */
let defaultsKey = '';
let chosen = props.settings !== undefined;
let applying = false;
function applyColumns() {
  applying = true;
  pickDefaults();
  defaultsKey = JSON.stringify(settingsNow.value);
  const r = props.settings ? resolveChartSettings(props.settings, props.columns.map((c) => c.name), numericCols.value) : null;
  if (r) {
    kind.value = r.kind;
    if (r.x >= 0) xCol.value = r.x;
    else if (r.kind === 'box') xCol.value = -1;
    yCols.value = r.y;
    agg.value = r.agg;
    sortBy.value = r.sort;
    splitCol.value = r.split;
    bins.value = r.bins;
    whole.value = !!props.settings?.whole;
    chosen = true;
  }
  queueMicrotask(() => { applying = false; });
}
watch(() => props.columns, applyColumns, { immediate: true });
// Splitting shows one measure (one series per value).
watch(splitCol, (v) => { if (v !== null && SPLITS.includes(kind.value) && yCols.value.length > 1) yCols.value = yCols.value.slice(0, 1); });
// A new kind: what it needs from the current choices.
watch(kind, (k) => {
  if (applying) return;
  if (ONE_VALUE.includes(k) && yCols.value.length > 1) yCols.value = yCols.value.slice(0, 1);
  if (!yCols.value.length && numericCols.value.length) yCols.value = [numericCols.value.find((i) => i !== xCol.value) ?? numericCols.value[0]];
  if ((k === 'heatmap' || k === 'kpi') && agg.value === 'none') agg.value = k === 'kpi' ? 'sum' : 'count';
  if (NEEDS_X.includes(k) && (xCol.value < 0 || xCol.value >= props.columns.length)) xCol.value = textCols.value[0] ?? 0;
  if (k === 'box' && xCol.value >= 0 && numericCols.value.includes(xCol.value)) xCol.value = textCols.value[0] ?? -1;
  if (k === 'heatmap' && (splitCol.value === null || splitCol.value === xCol.value)) {
    splitCol.value = textCols.value.find((i) => i !== xCol.value) ?? null;
  }
  if (!SPLITS.includes(k) && k !== 'heatmap') splitCol.value = null;
});

// -- data shaping ------------------------------------------------------------------------
const resolved = computed<ResolvedChart>(() => ({
  kind: kind.value,
  x: kind.value === 'histogram' || kind.value === 'kpi' ? -1 : xCol.value,
  y: yCols.value,
  agg: agg.value,
  sort: sortBy.value,
  split: SPLITS.includes(kind.value) || kind.value === 'heatmap' ? splitCol.value : null,
  bins: bins.value,
}));
const labels = computed(() => { void locale(); return chartLabels(); });
const fromRows = computed(() => shapeRows(props.rows, resolved.value, props.columns.map((c) => c.name), labels.value));

/** The whole-result points in use (stored, for these exact settings). */
const wholePoints = computed(() => {
  if (!whole.value || !wholeApplies.value || !props.storedPoints || !props.storedAggregate || !props.settings) return null;
  return sameJson(settingsNow.value, { ...props.settings, whole: true }) ? props.storedPoints : null;
});

// -- the whole result (chart_aggregate) ------------------------------------------------------
/** The request for the current settings (null: not computable). */
const request = computed(() => (wholeApplies.value ? aggregateRequest({ ...settingsNow.value, whole: undefined }) : null));
const canWhole = computed(() => request.value !== null && !!props.source);
const wholeKey = computed(() => (whole.value && request.value && props.source ? JSON.stringify([request.value, props.source]) : ''));
const fetched = shallowRef<{ key: string; settings: ChartSettings; data: ChartAggregate } | null>(null);
const wholeLoading = ref(false);
const wholeError = ref<string | null>(null);
const wholeCancelled = ref(false);
const baseId = `chart-${Math.random().toString(36).slice(2, 10)}`;
let runId: string | null = null;
let seq = 0;
let timer: ReturnType<typeof setTimeout> | null = null;
function stopRun() {
  if (timer) { clearTimeout(timer); timer = null; }
  if (runId) { const id = runId; runId = null; chartApi.cancel(id).catch(() => {}); }
}
async function runWhole() {
  const key = wholeKey.value;
  const src = props.source;
  const req = request.value;
  if (!key || !src || !req) return;
  stopRun();
  const n = ++seq;
  const id = `${baseId}-${n}`;
  runId = id;
  const settings = { ...settingsNow.value };
  wholeLoading.value = true;
  wholeError.value = null;
  wholeCancelled.value = false;
  try {
    const data = await chartApi.aggregate({ chartId: id, connectionId: src.connectionId, database: src.database, query: src.sql, chart: req });
    if (n === seq) fetched.value = { key, settings, data };
  } catch (e) {
    if (n !== seq) return;
    if (errorKind(e) === 'cancelled') { wholeCancelled.value = true; whole.value = false; } else wholeError.value = errorMessage(e);
  } finally {
    if (n === seq) { wholeLoading.value = false; if (runId === id) runId = null; }
  }
}
function cancelWhole() {
  seq++;
  stopRun();
  wholeLoading.value = false;
  wholeCancelled.value = true;
  whole.value = false;
}
// A change of settings or of the query: computed again (shortly after the
// last change). On mount, points kept for these settings are shown as they are.
let mountedKey = true;
watch(wholeKey, (k) => {
  const first = mountedKey;
  mountedKey = false;
  wholeError.value = null;
  if (!k) { seq++; stopRun(); wholeLoading.value = false; return; }
  if (first && wholePoints.value) return;
  if (fetched.value?.key === k) return;
  if (timer) clearTimeout(timer);
  timer = setTimeout(() => { timer = null; void runWhole(); }, 250);
}, { immediate: true });
watch(whole, (on) => { if (on) wholeCancelled.value = false; });
onBeforeUnmount(() => { seq++; stopRun(); });

const fresh = computed(() => (fetched.value && fetched.value.key === wholeKey.value ? fetched.value : null));
/** How the shown whole-result points were computed. */
const aggregateInfo = computed<AggregateInfo | null>(() => {
  const f = fresh.value;
  if (f) return { method: f.data.method, rows_scanned: f.data.rows_scanned, rows_capped: f.data.rows_capped, groups_truncated: f.data.groups_truncated };
  return wholePoints.value ? props.storedAggregate ?? null : null;
});
const shaped = computed(() => {
  const f = fresh.value;
  if (f) return shapeFromAggregate(f.settings, f.data, labels.value);
  return wholePoints.value ? shapeFromPoints(kind.value, wholePoints.value) : fromRows.value;
});
const usingWhole = computed(() => !!fresh.value || !!wholePoints.value);
const n = (v: number) => v.toLocaleString(locale());
const wholeNote = computed(() => {
  const a = aggregateInfo.value;
  if (!a) return '';
  const parts = [a.method === 'server' ? t('results:chart.whole.server')
    : a.rows_scanned === null ? t('results:chart.whole.clientNoCount')
    : a.rows_capped ? t('results:chart.whole.capped', { rows: n(a.rows_scanned) })
    : t('results:chart.whole.client', { count: a.rows_scanned, rows: n(a.rows_scanned) })];
  if (a.groups_truncated) parts.push(t('results:chart.whole.truncated'));
  if (!fresh.value) parts.push(t('results:chart.whole.stored'));
  return parts.join(' · ');
});
const wholeTitle = computed(() => (!request.value ? t('results:chart.whole.unsupported')
  : !props.source ? t('results:chart.whole.needsRun') : t('results:chart.whole.title')));

// -- ECharts option -------------------------------------------------------------------------
const option = computed(() => chartOption({
  kind: kind.value,
  shaped: shaped.value,
  theme: resolvedTheme.value,
  fontFamily: getComputedStyle(document.body).fontFamily,
  fmt: fmtNumber,
  others: labels.value.others,
  xName: props.columns[xCol.value]?.name ?? '',
  kpiCaption: kpiCaption(shaped.value),
}));

/** What the chart plots (aggregated, capped): a notebook keeps it. */
const points = computed(() => shapePoints(shaped.value));
defineExpose({ points, settings: settingsNow, option });
watch([settingsNow, points, aggregateInfo], ([s, p]) => {
  if (!chosen && JSON.stringify(s) === defaultsKey) return;
  chosen = true;
  emit('update:settings', { ...s, points: p, aggregate: usingWhole.value ? aggregateInfo.value : null });
}, { immediate: true });

// -- rendering ----------------------------------------------------------------------------
const host = ref<HTMLDivElement | null>(null);
const chart = shallowRef<echarts.ECharts | null>(null);
let ro: ResizeObserver | null = null;

onMounted(() => {
  chart.value = echarts.init(host.value!, undefined, { renderer: 'canvas' });
  chart.value.setOption(option.value, true);
  ro = new ResizeObserver(() => chart.value?.resize());
  ro.observe(host.value!);
});
onBeforeUnmount(() => { ro?.disconnect(); chart.value?.dispose(); });
watch(option, (o) => chart.value?.setOption(o, true));

const canChart = computed(() => yCols.value.length > 0 && shaped.value.cats.length > 0);
const tooMany = computed(() => ['pie', 'scatter', 'kpi', 'heatmap', 'box', 'histogram'].includes(kind.value) ? false : shaped.value.cats.length > 5000);
const needsYCat = computed(() => kind.value === 'heatmap' && splitCol.value === null);

async function exportPng() {
  const url = chart.value?.getDataURL({ type: 'png', pixelRatio: 2, backgroundColor: INKS[resolvedTheme.value].surface });
  if (url) await saveBinaryFile(url.split(',', 2)[1], `${t('results:chart.fileName')}.png`, [{ name: 'PNG', extensions: ['png'] }]);
}

const showX = computed(() => NEEDS_X.includes(kind.value) || kind.value === 'box');
const xLabel = computed(() => (kind.value === 'pie' || kind.value === 'box' ? t('results:chart.category') : t('results:chart.xAxis')));
const valueLimit = computed(() => (ONE_VALUE.includes(kind.value) || (splitCol.value !== null && SPLITS.includes(kind.value)) ? 1 : kind.value === 'scatter' ? 3 : 8));
const showAgg = computed(() => !['scatter', 'histogram', 'box'].includes(kind.value));
const showSort = computed(() => !['scatter', 'pie', 'kpi', 'histogram', 'heatmap'].includes(kind.value));
const BIN_OPTIONS = computed(() => HISTOGRAM_BINS.map((b) => ({ value: b, label: b === 0 ? t('results:chart.binsAuto') : String(b) })));
</script>

<template>
  <div class="ch">
    <div class="ch-controls">
      <el-select v-model="kind" size="small" style="width: 170px" :title="$t('results:chart.kindTitle')">
        <el-option v-for="k in KINDS" :key="k.id" :label="k.label" :value="k.id" />
      </el-select>
      <template v-if="showX">
        <span class="ch-lbl">{{ xLabel }}</span>
        <el-select v-model="xCol" size="small" filterable style="width: 160px" :clearable="kind === 'box'" :placeholder="kind === 'box' ? $t('results:chart.noCategory') : undefined" @clear="xCol = -1">
          <el-option v-for="(c, i) in columns" :key="i" :label="c.name" :value="i" />
        </el-select>
      </template>
      <template v-if="kind === 'heatmap'">
        <span class="ch-lbl">{{ $t('results:chart.yAxis') }}</span>
        <el-select v-model="splitCol" size="small" filterable style="width: 150px">
          <el-option v-for="(c, i) in columns" v-show="i !== xCol" :key="i" :label="c.name" :value="i" />
        </el-select>
      </template>
      <span class="ch-lbl">{{ kind === 'histogram' || kind === 'box' || kind === 'kpi' || kind === 'heatmap' ? $t('results:chart.value') : $t('results:chart.values') }}</span>
      <el-select
        v-model="yCols"
        size="small"
        multiple
        collapse-tags
        collapse-tags-tooltip
        :multiple-limit="valueLimit"
        style="width: 200px"
        :placeholder="$t('results:chart.numericColumns')"
      >
        <el-option v-for="i in numericCols" :key="i" :label="columns[i].name" :value="i" />
      </el-select>
      <template v-if="SPLITS.includes(kind)">
        <span class="ch-lbl">{{ $t('results:chart.splitBy') }}</span>
        <el-select v-model="splitCol" size="small" clearable placeholder="—" style="width: 140px" :title="$t('results:chart.splitByTitle')">
          <el-option v-for="(c, i) in columns" v-show="i !== xCol && !numericCols.includes(i)" :key="i" :label="c.name" :value="i" />
        </el-select>
      </template>
      <el-select v-if="showAgg" v-model="agg" size="small" style="width: 130px" :title="$t('results:chart.aggTitle')">
        <el-option v-for="a in AGGS" :key="a.id" :label="a.label" :value="a.id" />
      </el-select>
      <template v-if="kind === 'histogram'">
        <span class="ch-lbl">{{ $t('results:chart.bins') }}</span>
        <el-select v-model="bins" size="small" style="width: 100px" :title="$t('results:chart.binsTitle')">
          <el-option v-for="b in BIN_OPTIONS" :key="b.value" :label="b.label" :value="b.value" />
        </el-select>
      </template>
      <el-select v-if="showSort" v-model="sortBy" size="small" style="width: 150px">
        <el-option :label="$t('results:chart.sort.none')" value="none" />
        <el-option :label="$t('results:chart.sort.value')" value="value" />
        <el-option :label="$t('results:chart.sort.label')" value="label" />
      </el-select>
      <el-checkbox
        v-if="wholeApplies && (canWhole || whole)"
        v-model="whole"
        size="small"
        class="ch-whole"
        :disabled="!canWhole && !whole"
        :title="wholeTitle"
      >
        {{ $t('results:chart.whole.label') }}
      </el-checkbox>
      <div class="nm-spacer" />
      <el-button size="small" :disabled="!canChart" @click="exportPng">
        <el-icon><ei-download /></el-icon>&nbsp;PNG
      </el-button>
    </div>
    <div v-if="whole && wholeApplies && (wholeLoading || wholeError || wholeNote)" class="ch-note" :class="{ err: !!wholeError }">
      <template v-if="wholeLoading">
        <el-icon class="is-loading"><ei-loading /></el-icon>
        <span>{{ $t('results:chart.whole.running') }}</span>
        <button class="ch-link" @click="cancelWhole">{{ $t('common:cancel') }}</button>
      </template>
      <template v-else-if="wholeError">
        <el-icon><ei-warning-filled /></el-icon>
        <span class="ch-note-text nm-selectable">{{ $t('results:chart.whole.failed', { error: wholeError }) }}</span>
        <button v-if="canWhole" class="ch-link" @click="runWhole">{{ $t('results:chart.whole.retry') }}</button>
      </template>
      <template v-else>
        <el-icon><ei-data-analysis /></el-icon>
        <span class="ch-note-text">{{ wholeNote }}</span>
        <button v-if="canWhole" class="ch-link" @click="runWhole">{{ $t('results:chart.whole.refresh') }}</button>
      </template>
    </div>
    <div v-else-if="wholeCancelled" class="ch-note">{{ $t('results:chart.whole.cancelled') }}</div>
    <div v-if="!numericCols.length" class="ch-empty nm-muted">
      {{ $t('results:chart.noNumeric') }}
    </div>
    <div v-else-if="!yCols.length" class="ch-empty nm-muted">{{ $t('results:chart.pickValues') }}</div>
    <div v-else-if="needsYCat" class="ch-empty nm-muted">{{ $t('results:chart.pickYAxis') }}</div>
    <div v-else-if="tooMany" class="ch-empty nm-muted">
      {{ $t('results:chart.tooMany', { n: shaped.cats.length.toLocaleString(locale()) }) }}
    </div>
    <div v-show="canChart && !tooMany" ref="host" class="ch-canvas" />
  </div>
</template>

<style scoped>
.ch { display: flex; flex-direction: column; flex: 1; min-height: 0; }
.ch-controls { display: flex; align-items: center; gap: 6px; padding: 6px 10px; border-bottom: 1px solid var(--nm-border-soft); flex-wrap: wrap; }
.ch-lbl { font-size: 11.5px; color: var(--nm-text-dim); margin-left: 4px; }
.ch-canvas { flex: 1; min-height: 160px; margin: 8px 12px 6px; }
.ch-empty { padding: 16px; }
.ch-whole { margin-left: 6px; height: 24px; }
.ch-whole :deep(.el-checkbox__label) { font-size: 11.5px; color: var(--nm-text-dim); }
.ch-note {
  display: flex; align-items: center; gap: 6px; padding: 3px 12px; font-size: 11.5px; color: var(--nm-text-dim);
  border-bottom: 1px solid var(--nm-border-soft); font-variant-numeric: tabular-nums;
}
.ch-note.err { color: var(--nm-warning); }
.ch-note-text { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.ch-link { flex-shrink: 0; border: 0; padding: 0; background: none; font: inherit; color: var(--nm-accent); cursor: pointer; }
.ch-link:hover { text-decoration: underline; }
</style>
