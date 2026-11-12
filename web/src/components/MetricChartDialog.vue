<script setup lang="ts">
import { nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import { echarts } from '../composables/chartRender';
import { locale } from '../i18n';

// One monitor metric enlarged: its whole history kept by the tab (not only the
// sparkline's last points) on a time axis, with the exact value and time on
// hover and a zoom over the timeline. It follows new points as they arrive.

const props = defineProps<{
  title: string;
  points: { t: number; v: number | null }[];
  format: (v: number) => string;
  /** Fixed ceiling (100 for a share); else the data's own range. */
  max: number | null;
  /** What it measures and where it comes from, when the driver says it. */
  help?: string | null;
  source?: string | null;
}>();
const emit = defineEmits<{ close: [] }>();
const { t } = useTranslation();

const el = ref<HTMLDivElement | null>(null);
let chart: echarts.ECharts | null = null;
let ro: ResizeObserver | null = null;

function css(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

const time = (ms: number) => new Date(ms).toLocaleTimeString(locale(), { hour: '2-digit', minute: '2-digit', second: '2-digit' });

function render() {
  if (!chart) return;
  const accent = css('--nm-accent') || '#3794ff';
  const dim = css('--nm-text-dim') || '#999';
  const line = css('--nm-border-soft') || '#333';
  chart.setOption({
    animation: false,
    grid: { left: 70, right: 20, top: 16, bottom: 70 },
    tooltip: {
      trigger: 'axis',
      valueFormatter: (v: unknown) => (typeof v === 'number' ? props.format(v) : '—'),
      axisPointer: { type: 'line' },
    },
    xAxis: {
      type: 'time',
      axisLabel: { color: dim, formatter: (v: number) => time(v), hideOverlap: true },
      axisLine: { lineStyle: { color: line } },
      splitLine: { show: false },
    },
    yAxis: {
      type: 'value',
      min: 0,
      max: props.max ?? undefined,
      axisLabel: { color: dim, formatter: (v: number) => props.format(v) },
      splitLine: { lineStyle: { color: line } },
    },
    dataZoom: [
      { type: 'inside', filterMode: 'none' },
      { type: 'slider', height: 22, bottom: 16, filterMode: 'none', labelFormatter: (v: number) => time(v) },
    ],
    series: [{
      type: 'line',
      name: props.title,
      showSymbol: false,
      connectNulls: false,
      lineStyle: { width: 1.6, color: accent },
      areaStyle: { color: accent, opacity: 0.12 },
      data: props.points.map((p) => [p.t, p.v]),
    }],
  }, { replaceMerge: ['series'] });
}

onMounted(async () => {
  await nextTick();
  if (!el.value) return;
  chart = echarts.init(el.value);
  render();
  ro = new ResizeObserver(() => chart?.resize());
  ro.observe(el.value);
});
watch(() => props.points.length && props.points[props.points.length - 1], render);
onBeforeUnmount(() => {
  ro?.disconnect();
  chart?.dispose();
  chart = null;
});
</script>

<template>
  <el-dialog :model-value="true" width="min(960px, 92vw)" append-to-body :title="title" @close="emit('close')">
    <div class="mc">
      <div ref="el" class="mc-chart" role="img" :aria-label="t('monitor:chart.aria', { name: title })" />
      <p class="mc-span">{{ points.length > 1 ? t('monitor:chart.span', { from: time(points[0].t), to: time(points[points.length - 1].t), count: points.length }) : t('monitor:measuring') }}</p>
      <p v-if="help" class="mc-help">{{ help }}</p>
      <div v-if="source" class="mc-source">
        <span>{{ t('monitor:metric.source') }}</span>
        <pre class="nm-selectable">{{ source }}</pre>
      </div>
    </div>
  </el-dialog>
</template>

<style scoped>
.mc { display: flex; flex-direction: column; gap: 8px; }
.mc-chart { width: 100%; height: 360px; }
.mc-span { margin: 0; font-size: 11.5px; color: var(--nm-text-dim); }
.mc-help { margin: 0; font-size: 12.5px; color: var(--nm-text); }
.mc-source span { font-size: 11px; color: var(--nm-text-dim); }
.mc-source pre {
  margin: 4px 0 0; padding: 8px 10px; max-height: 160px; overflow: auto; white-space: pre-wrap;
  font-family: var(--nm-mono); font-size: 11.5px; background: var(--nm-bg); border: 1px solid var(--nm-border-soft); border-radius: 3px;
}
</style>
