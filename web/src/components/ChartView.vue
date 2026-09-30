<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, watch } from 'vue';
import * as echarts from 'echarts/core';
import { BarChart, LineChart, PieChart, ScatterChart } from 'echarts/charts';
import { DataZoomComponent, GridComponent, LegendComponent, TooltipComponent } from 'echarts/components';
import { CanvasRenderer } from 'echarts/renderers';
import { useTranslation } from 'i18next-vue';
import type { Cell, ResultColumn } from '../api/types';
import { locale } from '../i18n';
import { saveBinaryFile } from '../composables/files';

// A chart of a result set, for any engine (it works on the grid's rows).
// The form comes from the data: a date column → lines, a text column →
// bars, two numeric columns → dispersion; the user can change any choice.
// Colors: the reference categorical palette, dark steps, validated against
// the editor surface (#1e1e1e); assigned in fixed order, never cycled.

echarts.use([BarChart, LineChart, PieChart, ScatterChart, GridComponent, TooltipComponent, LegendComponent, DataZoomComponent, CanvasRenderer]);

const props = defineProps<{ columns: ResultColumn[]; rows: Cell[][] }>();

const PALETTE = ['#3987e5', '#d95926', '#199e70', '#c98500', '#d55181', '#008300', '#9085e9', '#e66767'];
const INK = { primary: '#e7e7e7', secondary: '#9d9d9d', muted: '#6f6f6f', grid: '#2b2b2b', axis: '#3c3c3c', surface: '#1e1e1e' };
/** Pie and scatter compare every pair of colors: past three, fold to "Otros". */
const ALL_PAIRS_CAP = 3;

type Kind = 'bar' | 'hbar' | 'stacked' | 'line' | 'area' | 'pie' | 'scatter';
type Agg = 'none' | 'sum' | 'avg' | 'count' | 'min' | 'max';

const { t } = useTranslation();
const KINDS = computed<{ id: Kind; label: string }[]>(() =>
  (['bar', 'hbar', 'stacked', 'line', 'area', 'pie', 'scatter'] as Kind[]).map((id) => ({ id, label: t(`results:chart.kind.${id}`) })));
const AGGS = computed<{ id: Agg; label: string }[]>(() =>
  (['none', 'sum', 'avg', 'count', 'min', 'max'] as Agg[]).map((id) => ({ id, label: t(`results:chart.agg.${id}`) })));
/** The "Otros" slice / series (past the palette's cap). */
const others = () => t('results:chart.others');

// -- column typing -------------------------------------------------------------------
function num(v: Cell): number | null {
  if (typeof v === 'number') return v;
  if (typeof v === 'boolean') return v ? 1 : 0;
  if (typeof v === 'string' && v.trim() !== '' && /^-?\d+(\.\d+)?(e[+-]?\d+)?$/i.test(v.trim())) return Number(v);
  return null;
}
const sample = computed(() => props.rows.slice(0, 200));
function share(i: number, test: (v: Cell) => boolean) {
  const vals = sample.value.map((r) => r[i]).filter((v) => v !== null);
  return vals.length ? vals.filter(test).length / vals.length : 0;
}
const numericCols = computed(() => props.columns.map((_, i) => i).filter((i) => share(i, (v) => num(v) !== null) >= 0.8));
const dateCols = computed(() =>
  props.columns.map((_, i) => i).filter((i) => share(i, (v) => typeof v === 'string' && /^\d{4}-\d{2}(-\d{2})?([ T]|$)/.test(v)) >= 0.8));

// -- settings (defaults picked from the data) -------------------------------------------
const kind = ref<Kind>('bar');
const xCol = ref<number>(0);
const yCols = ref<number[]>([]);
const agg = ref<Agg>('none');
const sortBy = ref<'none' | 'value' | 'label'>('none');
/** One series per distinct value of this column (a pivot), or none. */
const splitCol = ref<number | null>(null);
/** Series past this fold into "Otros" (fixed palette order, never cycled). */
const MAX_SERIES = 7;

function pickDefaults() {
  const nums = numericCols.value;
  const dates = dateCols.value;
  const text = props.columns.map((_, i) => i).find((i) => !nums.includes(i));
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
  const splitCandidate = props.columns.map((_, i) => i).find((i) => i !== xCol.value && !nums.includes(i) &&
    new Set(props.rows.slice(0, 1000).map((r) => r[i])).size <= MAX_SERIES + 1);
  splitCol.value = kind.value !== 'scatter' && splitCandidate !== undefined && yCols.value.length ? splitCandidate : null;
  if (splitCol.value !== null) yCols.value = yCols.value.slice(0, 1);
  // Many rows per category: group them.
  const distinct = new Set(props.rows.map((r) => String(r[xCol.value]))).size;
  agg.value = kind.value !== 'scatter' && distinct < props.rows.length * 0.8 ? 'sum' : 'none';
  sortBy.value = 'none';
}
watch(() => props.columns, pickDefaults, { immediate: true });
// Splitting shows one measure (one series per value).
watch(splitCol, (v) => { if (v !== null && yCols.value.length > 1) yCols.value = yCols.value.slice(0, 1); });

// -- data shaping ------------------------------------------------------------------------
const label = (v: Cell) => (v === null ? t('results:chart.null') : String(v));

const shaped = computed((): { cats: string[]; series: number[][]; names: string[] } => {
  if (splitCol.value !== null && yCols.value.length) return pivot(splitCol.value, yCols.value[0]);
  const x = xCol.value;
  const ys = yCols.value;
  let cats: string[] = [];
  let series: number[][] = ys.map(() => []);
  if (agg.value === 'none') {
    cats = props.rows.map((r) => label(r[x]));
    series = ys.map((y) => props.rows.map((r) => num(r[y]) ?? NaN));
  } else {
    const groups = new Map<string, number[][]>();
    for (const r of props.rows) {
      const k = label(r[x]);
      const g = groups.get(k) ?? ys.map(() => []);
      // "Cantidad" counts rows per category; the others reduce the values.
      ys.forEach((y, j) => { const v = agg.value === 'count' ? 1 : num(r[y]); if (v !== null) g[j].push(v); });
      groups.set(k, g);
    }
    cats = [...groups.keys()];
    const reduce = (a: number[]): number => {
      const v = a.filter((n) => !Number.isNaN(n));
      switch (agg.value) {
        case 'sum': return v.reduce((s, n) => s + n, 0);
        case 'avg': return v.length ? v.reduce((s, n) => s + n, 0) / v.length : NaN;
        case 'min': return v.length ? Math.min(...v) : NaN;
        case 'max': return v.length ? Math.max(...v) : NaN;
        default: return a.length;
      }
    };
    series = ys.map((_, j) => cats.map((c) => reduce(groups.get(c)![j])));
  }
  if (sortBy.value !== 'none' && series.length) {
    const idx = cats.map((_, i) => i);
    if (sortBy.value === 'value') idx.sort((a, b) => (series[0][b] || 0) - (series[0][a] || 0));
    else idx.sort((a, b) => cats[a].localeCompare(cats[b], undefined, { numeric: true }));
    cats = idx.map((i) => cats[i]);
    series = series.map((s) => idx.map((i) => s[i]));
  }
  return { cats, series, names: ys.map((_, j) => seriesName(j)) };
});

/** Rows → one series per value of `split` (top values; the rest "Otros"). */
function pivot(split: number, y: number) {
  const x = xCol.value;
  const reduceAgg: Agg = agg.value === 'none' ? 'sum' : agg.value;
  const totals = new Map<string, number>();
  for (const r of props.rows) totals.set(label(r[split]), (totals.get(label(r[split])) ?? 0) + Math.abs(num(r[y]) ?? 0));
  const keep = [...totals.entries()].sort((a, b) => b[1] - a[1]).slice(0, MAX_SERIES).map(([k]) => k);
  const names = totals.size > keep.length ? [...keep, others()] : keep;
  const cells = new Map<string, Map<string, number[]>>();
  const cats: string[] = [];
  for (const r of props.rows) {
    const c = label(r[x]);
    if (!cells.has(c)) { cells.set(c, new Map()); cats.push(c); }
    const sKey = keep.includes(label(r[split])) ? label(r[split]) : others();
    const v = reduceAgg === 'count' ? 1 : num(r[y]);
    if (v === null) continue;
    const m = cells.get(c)!;
    (m.get(sKey) ?? m.set(sKey, []).get(sKey)!).push(v);
  }
  const reduce = (a: number[] | undefined) => {
    if (!a?.length) return NaN;
    switch (reduceAgg) {
      case 'avg': return a.reduce((s, n) => s + n, 0) / a.length;
      case 'min': return Math.min(...a);
      case 'max': return Math.max(...a);
      case 'count': return a.length;
      default: return a.reduce((s, n) => s + n, 0);
    }
  };
  let order = cats.map((_, i) => i);
  if (sortBy.value === 'label') order.sort((a, b) => cats[a].localeCompare(cats[b], undefined, { numeric: true }));
  const series = names.map((nm) => order.map((i) => reduce(cells.get(cats[i])!.get(nm))));
  if (sortBy.value === 'value') {
    const tot = order.map((_, k) => series.reduce((s, ser) => s + (Number.isFinite(ser[k]) ? ser[k] : 0), 0));
    const idx = order.map((_, k) => k).sort((a, b) => tot[b] - tot[a]);
    order = idx.map((k) => order[k]);
    return { cats: order.map((i) => cats[i]), series: series.map((ser) => idx.map((k) => ser[k])), names };
  }
  return { cats: order.map((i) => cats[i]), series, names };
}

// -- ECharts option -------------------------------------------------------------------------
const fmt = (v: number) => (Number.isFinite(v) ? v.toLocaleString(locale(), { maximumFractionDigits: 2 }) : '—');
const seriesName = (j: number) => {
  const c = props.columns[yCols.value[j]]?.name ?? '';
  return agg.value === 'none' ? c : t('results:chart.seriesName', { agg: AGGS.value.find((a) => a.id === agg.value)!.label, column: c });
};

const option = computed(() => {
  const { cats, series, names } = shaped.value;
  const many = cats.length > 40;
  const axisLabel = { color: INK.secondary, fontSize: 11, hideOverlap: true };
  const axisLine = { lineStyle: { color: INK.axis } };
  const splitLine = { lineStyle: { color: INK.grid } };
  const base = {
    backgroundColor: 'transparent',
    color: PALETTE,
    animationDuration: 250,
    textStyle: { fontFamily: getComputedStyle(document.body).fontFamily, color: INK.primary },
    tooltip: {
      backgroundColor: '#252526', borderColor: '#454545', textStyle: { color: INK.primary, fontSize: 12 },
      valueFormatter: (v: unknown) => (typeof v === 'number' ? fmt(v) : String(v)),
    },
    legend: series.length >= 2 || kind.value === 'pie'
      ? { top: 0, textStyle: { color: INK.secondary }, icon: 'roundRect', itemWidth: 12, itemHeight: 8 }
      : undefined,
  };

  if (kind.value === 'pie') {
    // One measure; slices past the cap fold into "Otros" (all-pairs palette limit).
    const pairs = cats.map((c, i) => ({ name: c, value: series[0]?.[i] ?? 0 }))
      .filter((p) => Number.isFinite(p.value) && p.value > 0)
      .sort((a, b) => b.value - a.value);
    const cap = Math.max(ALL_PAIRS_CAP, 7);
    const top = pairs.slice(0, cap);
    const rest = pairs.slice(cap).reduce((s, p) => s + p.value, 0);
    if (rest > 0) top.push({ name: others(), value: rest });
    return {
      ...base,
      tooltip: { ...base.tooltip, trigger: 'item', formatter: (p: { name: string; value: number; percent: number }) => `${p.name}<br/><b>${fmt(p.value)}</b> (${p.percent}%)` },
      series: [{
        type: 'pie', radius: ['45%', '72%'], center: ['50%', '55%'],
        itemStyle: { borderColor: INK.surface, borderWidth: 2, borderRadius: 4 },
        label: { color: INK.secondary, formatter: '{b}: {d}%' },
        labelLine: { lineStyle: { color: INK.axis } },
        data: top.map((d, i) => (d.name === others() ? { ...d, itemStyle: { color: '#5a5a5a' } } : { ...d, itemStyle: { color: PALETTE[i % PALETTE.length] } })),
      }],
    };
  }

  if (kind.value === 'scatter') {
    const xs = props.rows.map((r) => num(r[xCol.value]));
    return {
      ...base,
      tooltip: { ...base.tooltip, trigger: 'item' },
      grid: { left: 56, right: 24, top: series.length >= 2 ? 36 : 16, bottom: 40 },
      xAxis: { type: 'value', name: props.columns[xCol.value]?.name, nameLocation: 'middle', nameGap: 28, nameTextStyle: { color: INK.secondary }, axisLabel, axisLine, splitLine },
      yAxis: { type: 'value', axisLabel, axisLine, splitLine },
      series: series.slice(0, ALL_PAIRS_CAP).map((s, j) => ({
        type: 'scatter', name: names[j], symbolSize: 8,
        itemStyle: { borderColor: INK.surface, borderWidth: 1, opacity: 0.9 },
        data: s.map((v, i) => [xs[i], v]).filter(([a, b]) => a !== null && Number.isFinite(b as number)),
      })),
    };
  }

  const horizontal = kind.value === 'hbar';
  const catAxis = { type: 'category', data: cats, axisLabel: { ...axisLabel, rotate: !horizontal && cats.length > 12 ? 35 : 0 }, axisLine, axisTick: { show: false } };
  const valAxis = { type: 'value', axisLabel: { ...axisLabel, formatter: (v: number) => fmt(v) }, axisLine: { show: false }, splitLine };
  const isBar = ['bar', 'hbar', 'stacked'].includes(kind.value);
  return {
    ...base,
    tooltip: { ...base.tooltip, trigger: 'axis', axisPointer: { type: isBar ? 'shadow' : 'line', lineStyle: { color: INK.muted }, shadowStyle: { color: 'rgba(255,255,255,0.04)' } } },
    grid: { left: horizontal ? 120 : 56, right: 24, top: series.length >= 2 ? 36 : 16, bottom: many ? 64 : 40, containLabel: horizontal },
    xAxis: horizontal ? valAxis : catAxis,
    yAxis: horizontal ? { ...catAxis, inverse: true } : valAxis,
    dataZoom: many ? [{ type: 'inside', [horizontal ? 'yAxisIndex' : 'xAxisIndex']: 0 }, { type: 'slider', height: 18, bottom: 8, borderColor: INK.axis, textStyle: { color: INK.muted }, [horizontal ? 'yAxisIndex' : 'xAxisIndex']: 0 }] : undefined,
    series: series.map((s, j) => ({
      color: names[j] === others() ? '#6f6f6f' : PALETTE[j % PALETTE.length],
      type: isBar ? 'bar' : 'line',
      name: names[j],
      data: s.map((v) => (Number.isFinite(v) ? v : null)),
      stack: kind.value === 'stacked' ? 'total' : undefined,
      barMaxWidth: 36,
      barGap: '8%',
      itemStyle: isBar
        ? { borderRadius: kind.value === 'stacked' ? 0 : horizontal ? [0, 4, 4, 0] : [4, 4, 0, 0], borderColor: kind.value === 'stacked' ? INK.surface : undefined, borderWidth: kind.value === 'stacked' ? 1 : 0 }
        : undefined,
      lineStyle: isBar ? undefined : { width: 2 },
      symbol: 'circle',
      symbolSize: 8,
      showSymbol: !isBar && cats.length <= 30,
      smooth: false,
      areaStyle: kind.value === 'area' ? { opacity: 0.18 } : undefined,
      emphasis: { focus: 'series' },
    })),
  };
});

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
const tooMany = computed(() => ['pie', 'scatter'].includes(kind.value) ? false : shaped.value.cats.length > 5000);

async function exportPng() {
  const url = chart.value?.getDataURL({ type: 'png', pixelRatio: 2, backgroundColor: INK.surface });
  if (url) await saveBinaryFile(url.split(',', 2)[1], `${t('results:chart.fileName')}.png`, [{ name: 'PNG', extensions: ['png'] }]);
}
</script>

<template>
  <div class="ch">
    <div class="ch-controls">
      <el-select v-model="kind" size="small" style="width: 160px" :title="$t('results:chart.kindTitle')">
        <el-option v-for="k in KINDS" :key="k.id" :label="k.label" :value="k.id" />
      </el-select>
      <span class="ch-lbl">{{ kind === 'pie' ? $t('results:chart.category') : $t('results:chart.xAxis') }}</span>
      <el-select v-model="xCol" size="small" filterable style="width: 160px">
        <el-option v-for="(c, i) in columns" :key="i" :label="c.name" :value="i" />
      </el-select>
      <span class="ch-lbl">{{ $t('results:chart.values') }}</span>
      <el-select
        v-model="yCols"
        size="small"
        multiple
        collapse-tags
        collapse-tags-tooltip
        :multiple-limit="kind === 'pie' || splitCol !== null ? 1 : kind === 'scatter' ? 3 : 8"
        style="width: 220px"
        :placeholder="$t('results:chart.numericColumns')"
      >
        <el-option v-for="i in numericCols" :key="i" :label="columns[i].name" :value="i" />
      </el-select>
      <template v-if="kind !== 'scatter' && kind !== 'pie'">
        <span class="ch-lbl">{{ $t('results:chart.splitBy') }}</span>
        <el-select v-model="splitCol" size="small" clearable placeholder="—" style="width: 140px" :title="$t('results:chart.splitByTitle')">
          <el-option v-for="(c, i) in columns" v-show="i !== xCol && !numericCols.includes(i)" :key="i" :label="c.name" :value="i" />
        </el-select>
      </template>
      <el-select v-if="kind !== 'scatter'" v-model="agg" size="small" style="width: 130px" :title="$t('results:chart.aggTitle')">
        <el-option v-for="a in AGGS" :key="a.id" :label="a.label" :value="a.id" />
      </el-select>
      <el-select v-if="kind !== 'scatter' && kind !== 'pie'" v-model="sortBy" size="small" style="width: 150px">
        <el-option :label="$t('results:chart.sort.none')" value="none" />
        <el-option :label="$t('results:chart.sort.value')" value="value" />
        <el-option :label="$t('results:chart.sort.label')" value="label" />
      </el-select>
      <div class="nm-spacer" />
      <el-button size="small" :disabled="!canChart" @click="exportPng">
        <el-icon><ei-download /></el-icon>&nbsp;PNG
      </el-button>
    </div>
    <div v-if="!numericCols.length" class="ch-empty nm-muted">
      {{ $t('results:chart.noNumeric') }}
    </div>
    <div v-else-if="!yCols.length" class="ch-empty nm-muted">{{ $t('results:chart.pickValues') }}</div>
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
</style>
