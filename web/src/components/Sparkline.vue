<script setup lang="ts">
import { computed, ref } from 'vue';
import { useTranslation } from 'i18next-vue';
import { locale } from '../i18n';

// A single-series line of the recent history of one metric, with a
// crosshair + tooltip on hover. `null` points are gaps (the server didn't
// report the value that time).

const props = defineProps<{
  points: { t: number; v: number | null }[];
  format: (v: number) => string;
  /** Fixed ceiling (a gauge's max, 100 for percentages); else the data's max. */
  max?: number | null;
}>();

const { t } = useTranslation();

const W = 240;
const H = 44;
const PAD = 2;

const top = computed(() => {
  const vals = props.points.map((p) => p.v).filter((v): v is number => v !== null);
  const m = Math.max(props.max ?? 0, ...vals, 0);
  return m > 0 ? m : 1;
});

function x(i: number) {
  const n = props.points.length;
  return n <= 1 ? W : PAD + (i / (n - 1)) * (W - PAD * 2);
}
function y(v: number) {
  return H - PAD - (v / top.value) * (H - PAD * 2);
}

/** One path per run of non-null points. */
const paths = computed(() => {
  const out: string[] = [];
  let d = '';
  props.points.forEach((p, i) => {
    if (p.v === null) {
      if (d) out.push(d);
      d = '';
      return;
    }
    d += `${d ? 'L' : 'M'}${x(i).toFixed(1)},${y(p.v).toFixed(1)}`;
  });
  if (d) out.push(d);
  return out;
});

const area = computed(() => {
  const pts = props.points.map((p, i) => ({ i, v: p.v })).filter((p) => p.v !== null);
  if (pts.length < 2) return '';
  const line = pts.map((p) => `${x(p.i).toFixed(1)},${y(p.v!).toFixed(1)}`).join('L');
  return `M${x(pts[0].i).toFixed(1)},${H - PAD}L${line}L${x(pts[pts.length - 1].i).toFixed(1)},${H - PAD}Z`;
});

const hover = ref<number | null>(null);
const host = ref<SVGSVGElement>();
function onMove(e: PointerEvent) {
  const r = host.value!.getBoundingClientRect();
  const n = props.points.length;
  if (!n) return;
  const rel = ((e.clientX - r.left) / r.width) * W;
  hover.value = Math.max(0, Math.min(n - 1, Math.round(((rel - PAD) / (W - PAD * 2)) * (n - 1))));
}

const tip = computed(() => {
  if (hover.value === null) return null;
  const p = props.points[hover.value];
  if (!p) return null;
  return {
    left: `${(x(hover.value) / W) * 100}%`,
    value: p.v === null ? t('monitor:sparkline.noData') : props.format(p.v),
    time: new Date(p.t).toLocaleTimeString(locale()),
  };
});
</script>

<template>
  <div class="sl">
    <svg
      ref="host"
      :viewBox="`0 0 ${W} ${H}`"
      preserveAspectRatio="none"
      class="sl-svg"
      @pointermove="onMove"
      @pointerleave="hover = null"
    >
      <path v-if="area" :d="area" class="sl-area" />
      <path v-for="(d, i) in paths" :key="i" :d="d" class="sl-line" vector-effect="non-scaling-stroke" />
      <line
        v-if="hover !== null"
        :x1="x(hover)" :x2="x(hover)" y1="0" :y2="H"
        class="sl-cross" vector-effect="non-scaling-stroke"
      />
    </svg>
    <div v-if="tip" class="sl-tip" :style="{ left: tip.left }">
      <strong>{{ tip.value }}</strong>
      <span>{{ tip.time }}</span>
    </div>
  </div>
</template>

<style scoped>
.sl { position: relative; height: 44px; }
.sl-svg { width: 100%; height: 100%; display: block; cursor: crosshair; }
.sl-line { fill: none; stroke: var(--nm-accent); stroke-width: 2; stroke-linejoin: round; stroke-linecap: round; }
.sl-area { fill: var(--nm-accent); opacity: 0.08; }
.sl-cross { stroke: var(--nm-text-dim); stroke-width: 1; }
.sl-tip {
  position: absolute; bottom: 100%; transform: translate(-50%, -4px); pointer-events: none; z-index: 2;
  display: flex; flex-direction: column; align-items: center; gap: 1px; white-space: nowrap;
  padding: 3px 7px; border: 1px solid var(--nm-border); background: var(--ide-sidebar);
  color: var(--nm-text); font-size: 11px; border-radius: 3px;
}
.sl-tip strong { color: var(--nm-text-strong); font-weight: 600; }
.sl-tip span { color: var(--nm-text-dim); }
</style>
