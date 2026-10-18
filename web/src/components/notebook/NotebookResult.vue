<script setup lang="ts">
import { computed } from 'vue';
import { useTranslation } from 'i18next-vue';
import type { CellResult, LoadedTable, QueryOutcome } from '../../api/types';
import { locale } from '../../i18n';
import { tb } from '../../i18n/backend';
import { BYTES_DEFAULT, BYTES_RANGE, BYTES_SETTING, ROWS_DEFAULT, ROWS_RANGE, ROWS_SETTING, clamp, rowCounts } from '../../composables/notebookModel';
import {
  readCellCharts, sameJson, storableCharts, writeCellCharts, type CellCharts, type ChartState, type MapSettings, type StoredChart,
} from '../../composables/chartSettings';
import { useSettingsStore } from '../../stores/settings';
import ResultsPane, { type View } from '../ResultsPane.vue';
import ChartTabs from './ChartTabs.vue';

// A SQL cell's result: the summary line ("100 de 100.000.000 filas · 18/10
// 14:32 · 41 s · Ventas prod › ventas") and, under it, the editor's results
// pane — with every row of a run made here, or the preview the file kept.
// The pane opens in the view the cell keeps (`chart`), with its charts (one
// or more, each with its title and settings) and its map source.

const props = defineProps<{
  result: CellResult | null;
  /** The run made in this session (all its rows); null: only what the file kept. */
  live: QueryOutcome | null;
  fallback: QueryOutcome | null;
  running: boolean;
  /** "No guardar el resultado" is on. */
  dropped: boolean;
  /** Only for a run made here: a full export re-runs exactly that query
   *  (a stored preview exports just its rows). */
  source: { connectionId: string; database: string; sql: string } | null;
  dialect: string;
  title: string;
  /** The cell's stored charts: { view, active, charts: [{ title, type, x, y, agg, sort, split, points… }], map }. */
  chart: unknown | null;
  /** A "Resultados del notebook" cell's run here: the tables it loaded. */
  loaded?: LoadedTable[] | null;
}>();
const emit = defineEmits<{ group: []; explain: []; chart: [chart: Record<string, unknown>] }>();
const { t } = useTranslation();

const counts = computed(() => (props.result ? rowCounts(props.result) : null));
const n = (x: number) => x.toLocaleString(locale());

function duration(ms: number): string {
  if (ms < 1000) return t('notebook:summary.ms', { ms: n(ms) });
  const s = ms / 1000;
  if (s < 60) return t('notebook:summary.seconds', { s: s < 10 ? s.toLocaleString(locale(), { maximumFractionDigits: 1 }) : n(Math.round(s)) });
  return t('notebook:summary.minutes', { m: n(Math.floor(s / 60)), s: n(Math.round(s % 60)) });
}

const summary = computed(() => {
  const r = props.result;
  if (!r) return '';
  const c = counts.value!;
  const parts: string[] = [];
  const affected = r.results.filter((s) => !s.columns.length && s.rows_affected != null).reduce((x, s) => x + (s.rows_affected ?? 0), 0);
  // What the screen shows: every row of a live run, else what the file kept.
  const shown = props.live ? props.live.results.reduce((x, s) => x + (s.columns.length ? s.rows.length : 0), 0) : c.kept;
  if (c.sets) parts.push(shown < c.total ? t('notebook:summary.rowsOf', { count: c.total, kept: n(shown), total: n(c.total) }) : t('notebook:summary.rows', { count: c.total, rows: n(c.total) }));
  else if (affected || r.results.length) parts.push(t('notebook:summary.affected', { count: affected, rows: n(affected) }));
  const at = new Date(r.ran_at);
  if (!Number.isNaN(at.getTime())) parts.push(at.toLocaleString(locale(), { day: '2-digit', month: '2-digit', hour: '2-digit', minute: '2-digit' }));
  parts.push(duration(r.duration_ms));
  if (r.target_label) parts.push(r.target_label);
  return parts.join(' · ');
});

const outcome = computed(() => props.live ?? props.fallback);
/** The pane only when there's something in it (rows, messages, an error). */
const hasPane = computed(() => {
  const o = outcome.value;
  if (!o || props.running) return false;
  return o.results.some((r) => r.columns.length && r.rows.length) || !!o.error || (o.log?.length ?? 0) > 0 || o.messages.length > 0;
});
/** Taller with more rows, up to a limit (the cell can be resized). */
const height = computed(() => {
  const rows = Math.max(0, ...(outcome.value?.results ?? []).map((r) => r.rows.length));
  return Math.min(380, Math.max(150, 96 + rows * 24));
});
const stored = computed(() => readCellCharts(props.chart));
const VIEWS: View[] = ['table', 'chart', 'tree', 'map'];
const initialView = computed(() => VIEWS.find((v) => v === stored.value.view));
const EMPTY: StoredChart = { title: '', settings: null, points: null, aggregate: null };
/** At least one chart (a cell that never showed one gets it on first view). */
const charts = computed(() => (stored.value.charts.length ? stored.value.charts : [EMPTY]));
const active = computed(() => Math.min(stored.value.active, charts.value.length - 1));
const activeChart = computed(() => charts.value[active.value]);
/** Changes when another chart is shown (ChartView starts over with its settings). */
const chartKey = computed(() => `${active.value}/${charts.value.length}`);
const settingsStore = useSettingsStore();
/** The file keeps at most the cell's rows of points, and all the charts'
 *  points together within the cell's bytes. */
const maxPoints = () => clamp(settingsStore.get<number>(ROWS_SETTING, ROWS_DEFAULT), ROWS_RANGE);
const maxBytes = () => clamp(settingsStore.get<number>(BYTES_SETTING, BYTES_DEFAULT), BYTES_RANGE);
/** Store `next` when it differs from what the cell has (an older file's
 *  one-chart format isn't rewritten until something changes). */
function commit(next: CellCharts) {
  const json = writeCellCharts({ ...next, charts: storableCharts(next.charts, maxPoints(), maxBytes()) });
  if (!sameJson(json, writeCellCharts(stored.value))) emit('chart', json);
}
function onView(v: View) {
  commit({ ...stored.value, view: v });
}
/** The chart changed. Its points come from a run made here, or from the
 *  whole result (computed on the server); on a stored preview they'd be
 *  from cut rows: the stored ones stay while the settings are the same, and
 *  go when they change (they'd no longer match). Only aggregated points are
 *  kept (storablePoints). */
function onChart({ points, aggregate, ...settings }: ChartState) {
  const cur = activeChart.value;
  const same = sameJson(settings, cur.settings);
  const pts = aggregate || props.live ? points : same ? cur.points : null;
  const agg = aggregate ?? (same && !props.live ? cur.aggregate : null);
  const list = charts.value.map((c, i) => (i === active.value ? { ...c, settings, points: pts, aggregate: agg } : c));
  commit({ ...stored.value, view: 'chart', active: active.value, charts: list });
}
function onMap(m: MapSettings) {
  commit({ ...stored.value, view: 'map', map: m });
}
const titles = computed(() => charts.value.map((c) => c.title));
function selectChart(i: number) {
  commit({ ...stored.value, charts: charts.value, active: i });
}
function addChart() {
  commit({ ...stored.value, view: 'chart', charts: [...charts.value, EMPTY], active: charts.value.length });
}
function renameChart(i: number, title: string) {
  commit({ ...stored.value, charts: charts.value.map((c, k) => (k === i ? { ...c, title } : c)), active: active.value });
}
function removeChart(i: number) {
  if (charts.value.length < 2) return;
  const list = charts.value.filter((_, k) => k !== i);
  commit({ ...stored.value, charts: list, active: Math.min(i <= active.value && active.value > 0 ? active.value - 1 : active.value, list.length - 1) });
}
/** "ventas_pg · 4 filas · 12 ms · de su ejecución" per loaded table. */
const tables = computed(() => (props.running ? [] : props.loaded ?? []).map((l) => {
  const parts = [t('notebook:loaded.rows', { count: l.rows, rows: n(l.rows) })];
  if (!l.cached) parts.push(duration(l.elapsed_ms));
  parts.push(l.cached ? t('notebook:loaded.cached') : t(`notebook:loaded.${l.origin}`));
  if (l.capped) parts.push(t('notebook:loaded.capped'));
  if (l.as_text) parts.push(t('notebook:loaded.asText'));
  return { name: l.name, text: parts.join(' · '), warn: l.capped || l.as_text };
}));
/** The preview was cut: the summary offers an aggregated version. */
const cut = computed(() => !!counts.value?.truncated && !props.running);
</script>

<template>
  <div class="nr">
    <div class="nr-sum" :class="{ err: !!result?.error && !running }">
      <template v-if="running">
        <el-icon class="is-loading"><ei-loading /></el-icon><span>{{ $t('results:running') }}</span>
      </template>
      <template v-else-if="result">
        <el-icon v-if="result.error"><ei-circle-close-filled /></el-icon>
        <el-icon v-else class="ok"><ei-circle-check-filled /></el-icon>
        <span class="nr-text nm-selectable">{{ summary }}</span>
        <span v-if="!live && !dropped" class="nr-badge" :title="$t('notebook:summary.storedTip')">{{ $t('notebook:summary.stored') }}</span>
        <span v-if="dropped" class="nr-badge" :title="$t('notebook:summary.droppedTip')">{{ $t('notebook:summary.dropped') }}</span>
        <button v-if="cut" class="nr-link" :title="$t('notebook:ai.groupTip')" @click="emit('group')">{{ $t('notebook:ai.group') }}</button>
        <div class="nm-spacer" />
        <button class="nr-link" :title="$t('notebook:ai.explainTip')" @click="emit('explain')">{{ $t('notebook:ai.explain') }}</button>
      </template>
    </div>
    <div v-if="tables.length" class="nr-tables" :title="$t('notebook:loaded.tip')">
      <span class="nr-tables-lbl">{{ $t('notebook:loaded.title') }}</span>
      <span v-for="tbl in tables" :key="tbl.name" class="nr-table" :class="{ warn: tbl.warn }"><code>{{ tbl.name }}</code> {{ tbl.text }}</span>
    </div>
    <div v-if="result?.error && !hasPane && !running" class="nr-error nm-selectable">{{ tb(result.error) }}</div>
    <div v-if="hasPane" class="nr-pane" :style="{ height: `${height}px` }">
      <ResultsPane
        :outcome="outcome"
        :running="false"
        :source="source"
        :title="title"
        :dialect="dialect"
        hide-status
        :initial-view="initialView"
        :chart-settings="activeChart.settings"
        :chart-points="activeChart.points"
        :chart-aggregate="activeChart.aggregate"
        :chart-key="chartKey"
        :map-settings="stored.map"
        @update:view="onView"
        @update:chart-settings="onChart"
        @update:map-settings="onMap"
      >
        <template #chartTabs>
          <ChartTabs :titles="titles" :active="active" @select="selectChart" @add="addChart" @rename="renameChart" @remove="removeChart" />
        </template>
      </ResultsPane>
    </div>
  </div>
</template>

<style scoped>
.nr { display: flex; flex-direction: column; min-width: 0; }
.nr-sum {
  display: flex; align-items: center; gap: 6px; min-height: 24px; padding: 2px 10px; font-size: 12px;
  color: var(--nm-text-dim); font-variant-numeric: tabular-nums;
}
.nr-sum .ok { color: var(--nm-success); }
.nr-sum.err > .el-icon { color: var(--nm-danger); }
.nr-text { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.nr-badge {
  flex-shrink: 0; padding: 0 6px; border-radius: 8px; font-size: 11px; line-height: 16px;
  border: 1px solid var(--nm-border); color: var(--nm-text-dim);
}
.nr-tables { display: flex; flex-wrap: wrap; align-items: center; gap: 4px 12px; padding: 0 10px 4px; font-size: 11.5px; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; }
.nr-tables-lbl { color: var(--nm-text-muted); }
.nr-table code { font-family: var(--nm-mono); font-size: 11px; color: var(--nm-text); }
.nr-table.warn { color: var(--nm-warning); }
.nr-link { flex-shrink: 0; border: 0; padding: 0; background: none; font: inherit; color: var(--nm-accent); cursor: pointer; }
.nr-link:hover { text-decoration: underline; }
.nr-error {
  margin: 0 10px 6px; padding: 6px 10px; border-radius: 3px; white-space: pre-wrap; font-family: var(--nm-mono); font-size: 12px; color: var(--nm-text);
  border: 1px solid color-mix(in srgb, var(--nm-danger) 50%, transparent); background: color-mix(in srgb, var(--nm-danger) 10%, transparent);
}
.nr-pane { display: flex; flex-direction: column; min-height: 110px; resize: vertical; overflow: hidden; border-top: 1px solid var(--nm-border-soft); }
.nr-pane > * { flex: 1; min-height: 0; }
</style>
