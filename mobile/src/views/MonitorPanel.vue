<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { tb } from '../backend';
import type { MetricRow, MonitorView } from '../api';

// The Monitor on a phone: the engine's figures grouped as on the desktop
// (CPU, memory, connections…), a bar where there's a ceiling, counters as a
// rate per second between two snapshots, then the server's facts and what
// the engine can't report.

const props = defineProps<{ view: MonitorView | null; at: number }>();

/** The previous snapshot's counters, for their rate. */
const prev = ref<{ at: number; values: Map<string, number> } | null>(null);
const rates = ref(new Map<string, number>());

watch(
  () => props.at,
  () => {
    const v = props.view;
    if (!v) return;
    const now = new Map(v.metrics.filter((m) => m.counter && m.value != null).map((m) => [m.key, m.value!]));
    const next = new Map<string, number>();
    if (prev.value) {
      const secs = (props.at - prev.value.at) / 1000;
      for (const [k, val] of now) {
        const before = prev.value.values.get(k);
        if (before != null && secs > 0 && val >= before) next.set(k, (val - before) / secs);
      }
    }
    rates.value = next;
    prev.value = { at: props.at, values: now };
  },
  { immediate: true },
);

const groups = computed(() => {
  const out = new Map<string, MetricRow[]>();
  for (const m of props.view?.metrics ?? []) {
    if (!out.has(m.group)) out.set(m.group, []);
    out.get(m.group)!.push(m);
  }
  return [...out.entries()];
});

const nf = (n: number, d = 0) => n.toLocaleString(undefined, { maximumFractionDigits: d });

function bytes(n: number): string {
  const u = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
  let i = 0;
  while (Math.abs(n) >= 1024 && i < u.length - 1) {
    n /= 1024;
    i++;
  }
  return `${nf(n, i ? 1 : 0)} ${u[i]}`;
}

function show(m: MetricRow, v: number): string {
  switch (m.unit) {
    case 'percent': return `${nf(v, 1)} %`;
    case 'bytes': return bytes(v);
    case 'millis': return v >= 1000 ? `${nf(v / 1000, 1)} s` : `${nf(v)} ms`;
    case 'seconds': return v >= 3600 ? `${nf(v / 3600, 1)} h` : v >= 60 ? `${nf(v / 60, 1)} min` : `${nf(v, 1)} s`;
    default: return nf(v, v < 10 ? 1 : 0);
  }
}

/** What a metric reads now: its rate for counters (waits for a second snapshot). */
function reading(m: MetricRow): string {
  if (m.counter) {
    const r = rates.value.get(m.key);
    if (r == null) return '…';
    return m.unit === 'percent' ? `${nf(r, 1)} %` : `${m.unit === 'bytes' ? bytes(r) : nf(r, r < 10 ? 1 : 0)}/s`;
  }
  return m.value == null ? '—' : show(m, m.value);
}

/** 0–1 for a bar, when there's one. */
function share(m: MetricRow): number | null {
  if (m.counter) {
    const r = rates.value.get(m.key);
    return m.unit === 'percent' && r != null ? Math.min(1, r / 100) : null;
  }
  if (m.value == null) return null;
  if (m.max && m.max > 0) return Math.min(1, m.value / m.max);
  if (m.unit === 'percent') return Math.min(1, m.value / 100);
  return null;
}

/** Amber and red only where full is bad: CPU, or a figure against its
 * ceiling (memory, connections, disk). A high cache hit rate is good news. */
function level(m: MetricRow, s: number): string {
  const usage = (m.max != null && m.max > 0) || m.key.includes('cpu');
  return !usage ? '' : s >= 0.9 ? 'hot' : s >= 0.75 ? 'warm' : '';
}
</script>

<template>
  <div class="mon">
    <div v-for="[g, list] in groups" :key="g" class="card grp">
      <h3>{{ tb(g) }}</h3>
      <div v-for="m in list" :key="m.key" class="row">
        <div class="line">
          <span class="lbl">{{ tb(m.label) }}</span>
          <span class="val">{{ reading(m) }}<span v-if="m.max && !m.counter && m.value != null" class="muted">&nbsp;/&nbsp;{{ show(m, m.max) }}</span></span>
        </div>
        <div v-if="share(m) != null" class="meter"><span :class="level(m, share(m)!)" :style="{ width: `${share(m)! * 100}%` }" /></div>
      </div>
    </div>
    <div v-if="view?.info.length" class="card grp">
      <div v-for="([k, v], i) in view.info" :key="i" class="line info">
        <span class="lbl">{{ tb(k) }}</span>
        <span class="val">{{ v }}</span>
      </div>
    </div>
    <p v-for="(n, i) in view?.notes ?? []" :key="i" class="muted">{{ tb(n) }}</p>
  </div>
</template>

<style scoped>
.mon { display: flex; flex-direction: column; gap: 12px; }
.grp h3 { margin: 0 0 8px; font-size: 13px; text-transform: uppercase; letter-spacing: .5px; color: var(--text-dim); }
.row { padding: 6px 0; }
.line { display: flex; justify-content: space-between; gap: 12px; align-items: baseline; }
.lbl { color: var(--text); min-width: 0; }
.val { color: var(--text-strong); font-variant-numeric: tabular-nums; text-align: right; white-space: nowrap; }
.info { padding: 4px 0; }
.info .val { white-space: normal; word-break: break-word; max-width: 60%; }
.meter { height: 6px; border-radius: 3px; background: var(--border); margin-top: 6px; overflow: hidden; }
.meter span { display: block; height: 100%; background: var(--accent); border-radius: 3px; transition: width .4s; }
.meter span.warm { background: var(--warning); }
.meter span.hot { background: var(--danger); }
</style>
