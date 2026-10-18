// A chart's ECharts option, built in code from its shape (chartShape.ts):
// every option is declarative and comes from here, never from a file (a
// formatter from a foreign .ipynb would run its code). ChartView shows it;
// a notebook export renders it to PNG (chartRender.ts).
//
// Colors: the reference categorical palette, dark steps, validated against
// the editor surface (#1e1e1e); assigned in fixed order, never cycled. The
// ink (text, axes, tooltip) follows the theme. A heatmap uses one hue,
// light to dark (dark to light on the dark theme).

import type { ChartKind } from './chartSettings';
import type { Shaped } from './chartShape';

export const PALETTE = ['#3987e5', '#d95926', '#199e70', '#c98500', '#d55181', '#008300', '#9085e9', '#e66767'];
export const INKS = {
  dark: { primary: '#e7e7e7', secondary: '#9d9d9d', muted: '#6f6f6f', grid: '#2b2b2b', axis: '#3c3c3c', surface: '#1e1e1e', tip: '#252526', tipBorder: '#454545', other: '#5a5a5a', otherLine: '#6f6f6f', shadow: 'rgba(255,255,255,0.04)' },
  light: { primary: '#1f1f1f', secondary: '#646464', muted: '#8a8a8a', grid: '#e5e5e5', axis: '#d4d4d4', surface: '#ffffff', tip: '#f8f8f8', tipBorder: '#c8c8c8', other: '#b4b4b4', otherLine: '#8a8a8a', shadow: 'rgba(0,0,0,0.04)' },
};
const HEAT = {
  light: ['#eaf2fc', '#a9caf2', '#3987e5', '#1d4f91'],
  dark: ['#1c2633', '#22477a', '#3987e5', '#a6c9f4'],
};
/** Pie and scatter compare every pair of colors: past three, fold to "Otros". */
const ALL_PAIRS_CAP = 3;

export type Theme = keyof typeof INKS;

export interface OptionInput {
  kind: ChartKind;
  shaped: Shaped;
  theme: Theme;
  fontFamily: string;
  fmt: (v: number) => string;
  /** The "Otros" label (its slice / series is grey). */
  others: string;
  /** The X column's name (a scatter's axis). */
  xName: string;
  /** A KPI's caption lines ("Suma de total", "1.234 filas"). */
  kpiCaption?: string[];
  /** Exports render without animation. */
  animation?: boolean;
}

/** The ECharts option for a chart (no functions from outside: formatters are these). */
export function chartOption(o: OptionInput): Record<string, unknown> {
  const INK = INKS[o.theme];
  const { cats, series, names } = o.shaped;
  const fmt = o.fmt;
  const many = cats.length > 40;
  const axisLabel = { color: INK.secondary, fontSize: 11, hideOverlap: true };
  const axisLine = { lineStyle: { color: INK.axis } };
  const splitLine = { lineStyle: { color: INK.grid } };
  const legendOn = series.length >= 2 || o.kind === 'pie';
  const base = {
    backgroundColor: 'transparent',
    color: PALETTE,
    animation: o.animation !== false,
    animationDuration: 250,
    textStyle: { fontFamily: o.fontFamily, color: INK.primary },
    tooltip: {
      backgroundColor: INK.tip, borderColor: INK.tipBorder, textStyle: { color: INK.primary, fontSize: 12 },
      valueFormatter: (v: unknown) => (typeof v === 'number' ? fmt(v) : String(v)),
    },
    legend: legendOn ? { top: 0, textStyle: { color: INK.secondary }, icon: 'roundRect', itemWidth: 12, itemHeight: 8, type: 'scroll', pageTextStyle: { color: INK.secondary } } : undefined,
  };
  const zoom = (horizontal: boolean) => (many
    ? [{ type: 'inside', [horizontal ? 'yAxisIndex' : 'xAxisIndex']: 0 }, { type: 'slider', height: 18, bottom: 8, borderColor: INK.axis, textStyle: { color: INK.muted }, [horizontal ? 'yAxisIndex' : 'xAxisIndex']: 0 }]
    : undefined);
  const catAxis = (rotate: boolean) => ({ type: 'category', data: cats, axisLabel: { ...axisLabel, rotate: rotate && cats.length > 12 ? 35 : 0 }, axisLine, axisTick: { show: false } });
  const valAxis = { type: 'value', axisLabel: { ...axisLabel, formatter: (v: number) => fmt(v) }, axisLine: { show: false }, splitLine };
  const clean = (s: number[]) => s.map((v) => (Number.isFinite(v) ? v : null));

  if (o.kind === 'kpi') {
    const v = series[0]?.[0];
    const lines = o.kpiCaption ?? [];
    return {
      ...base,
      legend: undefined,
      tooltip: undefined,
      graphic: [{
        type: 'group', left: 'center', top: 'middle',
        children: [
          { type: 'text', x: 0, y: 0, style: { text: v !== undefined && Number.isFinite(v) ? fmt(v) : '—', fontSize: 46, fontWeight: 600, fill: INK.primary, fontFamily: o.fontFamily, align: 'center' } },
          ...lines.map((text, i) => ({ type: 'text', x: 0, y: 64 + i * 20, style: { text, fontSize: i === 0 ? 13 : 12, fill: i === 0 ? INK.secondary : INK.muted, fontFamily: o.fontFamily, align: 'center' } })),
        ],
      }],
    };
  }

  if (o.kind === 'pie') {
    // One measure; slices past the cap fold into "Otros" (all-pairs palette limit).
    const pairs = cats.map((c, i) => ({ name: c, value: series[0]?.[i] ?? 0 }))
      .filter((p) => Number.isFinite(p.value) && p.value > 0)
      .sort((a, b) => b.value - a.value);
    const cap = Math.max(ALL_PAIRS_CAP, 7);
    const top = pairs.slice(0, cap);
    const rest = pairs.slice(cap).reduce((s, p) => s + p.value, 0);
    if (rest > 0) top.push({ name: o.others, value: rest });
    return {
      ...base,
      tooltip: { ...base.tooltip, trigger: 'item', formatter: (p: { name: string; value: number; percent: number }) => `${esc(p.name)}<br/><b>${fmt(p.value)}</b> (${p.percent}%)` },
      series: [{
        type: 'pie', radius: ['45%', '72%'], center: ['50%', '55%'],
        itemStyle: { borderColor: INK.surface, borderWidth: 2, borderRadius: 4 },
        label: { color: INK.secondary, formatter: '{b}: {d}%' },
        labelLine: { lineStyle: { color: INK.axis } },
        data: top.map((d, i) => (d.name === o.others ? { ...d, itemStyle: { color: INK.other } } : { ...d, itemStyle: { color: PALETTE[i % PALETTE.length] } })),
      }],
    };
  }

  if (o.kind === 'scatter') {
    const pairs = o.shaped.pairs ?? [];
    return {
      ...base,
      tooltip: { ...base.tooltip, trigger: 'item' },
      grid: { left: 56, right: 24, top: legendOn ? 36 : 16, bottom: 40 },
      xAxis: { type: 'value', name: o.xName, nameLocation: 'middle', nameGap: 28, nameTextStyle: { color: INK.secondary }, axisLabel, axisLine, splitLine },
      yAxis: { type: 'value', axisLabel, axisLine, splitLine },
      series: pairs.slice(0, ALL_PAIRS_CAP).map((d, j) => ({
        type: 'scatter', name: names[j], symbolSize: 8,
        itemStyle: { borderColor: INK.surface, borderWidth: 1, opacity: 0.9 },
        data: d,
      })),
    };
  }

  if (o.kind === 'histogram') {
    return {
      ...base,
      tooltip: { ...base.tooltip, trigger: 'axis', axisPointer: { type: 'shadow', shadowStyle: { color: INK.shadow } } },
      grid: { left: 56, right: 24, top: 16, bottom: cats.length > 8 ? 64 : 40 },
      xAxis: { ...catAxis(false), axisLabel: { ...axisLabel, rotate: cats.length > 8 ? 30 : 0 } },
      yAxis: valAxis,
      series: [{
        type: 'bar', name: names[0], data: clean(series[0] ?? []), color: PALETTE[0],
        barCategoryGap: '6%', itemStyle: { borderRadius: [2, 2, 0, 0] }, emphasis: { focus: 'series' },
      }],
    };
  }

  if (o.kind === 'box') {
    const data = cats.map((_, i) => [0, 1, 2, 3, 4].map((k) => series[k]?.[i] ?? NaN));
    const statNames = o.shaped.names;
    return {
      ...base,
      legend: undefined,
      tooltip: {
        ...base.tooltip,
        trigger: 'item',
        formatter: (p: { seriesType: string; name: string; data: number[] }) => {
          if (p.seriesType === 'scatter') return `${esc(cats[p.data[0]] ?? '')}<br/><b>${fmt(p.data[1])}</b>`;
          const v = p.data.slice(-5);
          return `${esc(p.name)}<br/>${statNames.map((n, k) => `${esc(n)}: <b>${fmt(v[k])}</b>`).join('<br/>')}`;
        },
      },
      grid: { left: 56, right: 24, top: 16, bottom: many ? 64 : 40 },
      xAxis: catAxis(true),
      yAxis: { ...valAxis, scale: true },
      dataZoom: zoom(false),
      series: [
        { type: 'boxplot', name: names[0], data, itemStyle: { color: `${PALETTE[0]}33`, borderColor: PALETTE[0], borderWidth: 1.5 }, boxWidth: [8, 40] },
        { type: 'scatter', name: 'outliers', data: o.shaped.outliers ?? [], symbolSize: 6, itemStyle: { color: PALETTE[1], opacity: 0.8 } },
      ],
    };
  }

  if (o.kind === 'heatmap') {
    const vals = series.flat().filter(Number.isFinite);
    const lo = vals.length ? Math.min(...vals) : 0;
    const hi = vals.length ? Math.max(...vals) : 1;
    const span = hi === lo ? 1 : hi - lo;
    // Labels in the ink that reads on the cell: the ramp's far end is dark on
    // the light theme and light on the dark one.
    const onFar = o.theme === 'light' ? '#ffffff' : '#10161f';
    const data: { value: [number, number, number]; label: { color: string } }[] = [];
    series.forEach((row, j) => row.forEach((v, i) => {
      if (Number.isFinite(v)) data.push({ value: [i, j, v], label: { color: (v - lo) / span > 0.55 ? onFar : INK.primary } });
    }));
    const small = cats.length * names.length <= 225;
    return {
      ...base,
      legend: undefined,
      tooltip: {
        ...base.tooltip, trigger: 'item',
        formatter: (p: { value: [number, number, number] }) => `${esc(cats[p.value[0]] ?? '')} · ${esc(names[p.value[1]] ?? '')}<br/><b>${fmt(p.value[2])}</b>`,
      },
      grid: { left: 96, right: 24, top: 12, bottom: 72, containLabel: false },
      xAxis: { ...catAxis(true), splitArea: { show: false } },
      yAxis: { type: 'category', data: names, axisLabel: { ...axisLabel, width: 84, overflow: 'truncate' }, axisLine, axisTick: { show: false } },
      visualMap: {
        min: lo, max: hi === lo ? lo + 1 : hi, calculable: false, orient: 'horizontal', left: 'center', bottom: 4, itemHeight: 160, itemWidth: 10,
        text: [fmt(hi), fmt(lo)], textGap: 8, textStyle: { color: INK.secondary, fontSize: 11 }, inRange: { color: HEAT[o.theme] },
      },
      series: [{
        type: 'heatmap', data, label: { show: small, fontSize: 10, formatter: (p: { value: [number, number, number] }) => fmt(p.value[2]) },
        itemStyle: { borderColor: INK.surface, borderWidth: 1 }, emphasis: { itemStyle: { borderColor: INK.primary } },
      }],
    };
  }

  const horizontal = o.kind === 'hbar';
  const isBar = ['bar', 'hbar', 'stacked'].includes(o.kind);
  if (o.kind === 'combo') {
    // The first measure as bars on the left axis, the rest as lines on the right.
    return {
      ...base,
      tooltip: { ...base.tooltip, trigger: 'axis', axisPointer: { type: 'shadow', shadowStyle: { color: INK.shadow } } },
      grid: { left: 56, right: series.length > 1 ? 56 : 24, top: legendOn ? 36 : 16, bottom: many ? 64 : 40 },
      xAxis: catAxis(true),
      yAxis: [
        { ...valAxis, name: names[0], nameTextStyle: { color: INK.muted, fontSize: 11 } },
        ...(series.length > 1 ? [{ ...valAxis, splitLine: { show: false }, name: names.slice(1).join(', '), nameTextStyle: { color: INK.muted, fontSize: 11 } }] : []),
      ],
      dataZoom: zoom(false),
      series: series.map((s, j) => (j === 0
        ? { type: 'bar', name: names[0], data: clean(s), color: PALETTE[0], barMaxWidth: 36, itemStyle: { borderRadius: [4, 4, 0, 0] }, emphasis: { focus: 'series' } }
        : {
          type: 'line', name: names[j], data: clean(s), yAxisIndex: 1, color: names[j] === o.others ? INK.otherLine : PALETTE[j % PALETTE.length],
          lineStyle: { width: 2 }, symbol: 'circle', symbolSize: 7, showSymbol: cats.length <= 30, emphasis: { focus: 'series' },
        })),
    };
  }

  return {
    ...base,
    tooltip: { ...base.tooltip, trigger: 'axis', axisPointer: { type: isBar ? 'shadow' : 'line', lineStyle: { color: INK.muted }, shadowStyle: { color: INK.shadow } } },
    grid: { left: horizontal ? 120 : 56, right: 24, top: legendOn ? 36 : 16, bottom: many ? 64 : 40, containLabel: horizontal },
    xAxis: horizontal ? valAxis : catAxis(true),
    yAxis: horizontal ? { ...catAxis(false), inverse: true } : valAxis,
    dataZoom: zoom(horizontal),
    series: series.map((s, j) => ({
      color: names[j] === o.others ? INK.otherLine : PALETTE[j % PALETTE.length],
      type: isBar ? 'bar' : 'line',
      name: names[j],
      data: clean(s),
      stack: o.kind === 'stacked' ? 'total' : undefined,
      barMaxWidth: 36,
      barGap: '8%',
      itemStyle: isBar
        ? { borderRadius: o.kind === 'stacked' ? 0 : horizontal ? [0, 4, 4, 0] : [4, 4, 0, 0], borderColor: o.kind === 'stacked' ? INK.surface : undefined, borderWidth: o.kind === 'stacked' ? 1 : 0 }
        : undefined,
      lineStyle: isBar ? undefined : { width: 2 },
      symbol: 'circle',
      symbolSize: 8,
      showSymbol: !isBar && cats.length <= 30,
      smooth: false,
      areaStyle: o.kind === 'area' ? { opacity: 0.18 } : undefined,
      emphasis: { focus: 'series' },
    })),
  };
}

/** Tooltip HTML: names come from the data. */
function esc(s: string): string {
  return s.replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]!);
}
