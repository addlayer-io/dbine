<script setup lang="ts">
import { computed, nextTick, onMounted, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import type { IndexUsage } from '../api/types';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import { badgeClass, indexTag, indexUsageEntry, loadIndexUsage, seekTip, sharePct, usageBadge } from '../composables/indexUsage';
import { dropIndexItem, type DropIndexTarget } from '../composables/dropIndex';
import ContextMenu, { type MenuItem } from '../components/ContextMenu.vue';
import DropIndexDialog from '../components/DropIndexDialog.vue';
import { useConnectionsStore } from '../stores/connections';
import type { IndexesTab } from '../stores/tabs';

// "Índices · <tabla>": every index of the table with its definition, size
// and usage counters since the server started them, sortable and copyable.
// The derived numbers (reads, share, unused) come from the backend.

const props = defineProps<{ tab: IndexesTab }>();
const conns = useConnectionsStore();
const { t } = useTranslation();

const entry = computed(() => indexUsageEntry(props.tab.connectionId, props.tab.database, props.tab.object));
const report = computed(() => entry.value?.report ?? null);
const loading = computed(() => entry.value?.status === 'loading');
const qualified = computed(() => (props.tab.object.schema ? `${props.tab.object.schema}.${props.tab.object.name}` : props.tab.object.name));

function load(force = false) {
  loadIndexUsage(props.tab.connectionId, props.tab.database, props.tab.object, force);
}
onMounted(() => load());

type Col = { id: string; label: string; num?: boolean; value: (i: IndexUsage) => string | number | null; text: (i: IndexUsage) => string; tip?: string };
const num = (n: number | null | undefined) => (n == null ? '' : n.toLocaleString(locale()));
const stats = computed(() => !!report.value?.stats_available);
function size(kb: number | null) {
  if (kb == null) return '';
  const units = ['KB', 'MB', 'GB', 'TB'];
  let v = kb;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(v < 10 && i ? 1 : 0)} ${units[i]}`;
}
const counter = (f: (i: IndexUsage) => number) => (i: IndexUsage) => (stats.value ? num(f(i)) : '');
// The engine doesn't count index writes: updates, writes per read and the
// last write are unknown, not 0, so they show a dash.
const noWrites = computed(() => stats.value && report.value?.writes_counted === false);
const writeCol = (c: Col): Col => (noWrites.value ? { ...c, value: () => null, text: () => '—', tip: t('explorer:indexes.noWritesTip') } : c);
const cols = computed<Col[]>(() => [
  { id: 'name', label: t('explorer:indexes.col.name'), value: (i) => i.name, text: (i) => i.name },
  { id: 'type', label: t('explorer:indexes.col.type'), value: (i) => indexTag(i), text: (i) => indexTag(i) },
  { id: 'keys', label: t('explorer:indexes.col.keys'), value: (i) => i.key_columns.join(', '), text: (i) => i.key_columns.join(', ') },
  { id: 'include', label: 'INCLUDE', value: (i) => i.included_columns.join(', '), text: (i) => i.included_columns.join(', ') },
  { id: 'filter', label: t('explorer:indexes.col.filter'), value: (i) => i.filter ?? '', text: (i) => i.filter ?? '' },
  { id: 'size', label: t('explorer:indexes.col.size'), num: true, value: (i) => i.size_kb, text: (i) => size(i.size_kb) },
  { id: 'seeks', label: 'Seeks', num: true, value: (i) => i.seeks, text: counter((i) => i.seeks) },
  { id: 'scans', label: 'Scans', num: true, value: (i) => i.scans, text: counter((i) => i.scans) },
  { id: 'lookups', label: 'Lookups', num: true, value: (i) => i.lookups, text: counter((i) => i.lookups) },
  writeCol({ id: 'updates', label: 'Updates', num: true, value: (i) => i.updates, text: counter((i) => i.updates) }),
  { id: 'seeks%', label: t('explorer:indexes.col.seekShare'), num: true, value: (i) => i.seek_ratio ?? null, text: (i) => (i.seek_ratio == null ? '' : sharePct(i.seek_ratio)) },
  { id: 'share', label: t('explorer:indexes.col.readShare'), num: true, value: (i) => i.read_share, text: (i) => (i.read_share == null ? '' : sharePct(i.read_share)) },
  writeCol({
    id: 'wpr', label: t('explorer:indexes.col.writesPerRead'), num: true, value: (i) => i.writes_per_read,
    text: (i) => (i.writes_per_read == null ? '' : i.writes_per_read.toLocaleString(locale(), { maximumFractionDigits: 2 })),
  }),
  { id: 'lastRead', label: t('explorer:indexes.col.lastRead'), value: (i) => i.last_read, text: (i) => i.last_read ?? '' },
  writeCol({ id: 'lastWrite', label: t('explorer:indexes.col.lastWrite'), value: (i) => i.last_write, text: (i) => i.last_write ?? '' }),
]);

// -- sorting: a click on a header sorts by it, a second click reverses ---------------------
const sort = ref<{ col: string; desc: boolean } | null>(null);
function sortBy(id: string) {
  sort.value = sort.value?.col === id ? { col: id, desc: !sort.value.desc } : { col: id, desc: !!cols.value.find((c) => c.id === id)?.num };
}
const rows = computed(() => {
  const list = [...(report.value?.indexes ?? [])];
  const s = sort.value;
  const col = s && cols.value.find((c) => c.id === s.col);
  if (!s || !col) return list;
  return list.sort((a, b) => {
    const x = col.value(a);
    const y = col.value(b);
    // Empty values last, whatever the direction.
    if (x == null || x === '') return y == null || y === '' ? 0 : 1;
    if (y == null || y === '') return -1;
    const c = typeof x === 'number' && typeof y === 'number' ? x - y : String(x).localeCompare(String(y), locale(), { numeric: true });
    return s.desc ? -c : c;
  });
});

// -- the index clicked in the explorer --------------------------------------------------
const body = ref<HTMLElement | null>(null);
watch(() => [props.tab.focus, rows.value.length] as const, async ([focus]) => {
  if (!focus) return;
  await nextTick();
  body.value?.querySelector<HTMLElement>('tr.focus')?.scrollIntoView({ block: 'nearest' });
}, { immediate: true });

// -- right click on a row: "Eliminar índice…" ------------------------------------------------
const menu = ref<{ x: number; y: number; items: MenuItem[] } | null>(null);
const dropping = ref<DropIndexTarget | null>(null);
function rowMenu(e: MouseEvent, i: IndexUsage) {
  const items: MenuItem[] = [{
    label: t('explorer:menu.copyName'),
    action: () => navigator.clipboard.writeText(i.name).then(() => ElMessage.success({ message: t('explorer:indexes.copied'), duration: 1200 })).catch(() => {}),
  }];
  const drop = dropIndexItem({ connectionId: props.tab.connectionId, database: props.tab.database, table: props.tab.object, index: i.name }, (x) => { dropping.value = x; });
  if (drop) items.push(drop);
  menu.value = { x: e.clientX, y: e.clientY, items };
}

async function copyGrid() {
  const lines = [cols.value.map((c) => c.label), ...rows.value.map((i) => cols.value.map((c) => c.text(i)))];
  try {
    await navigator.clipboard.writeText(lines.map((l) => l.map((v) => v.replace(/[\t\n]/g, ' ')).join('\t')).join('\n'));
    ElMessage.success({ message: t('explorer:indexes.copied'), duration: 1200 });
  } catch { /* no clipboard */ }
}
</script>

<template>
  <div class="iu" v-loading="loading && !report">
    <header class="iu-head">
      <h2>{{ $t('explorer:indexes.tab') }} · {{ qualified }}</h2>
      <span class="iu-dim">{{ tab.database || conns.byId(tab.connectionId)?.name }}</span>
      <span style="flex: 1" />
      <el-button size="small" :loading="loading" @click="load(true)">
        <el-icon><ei-refresh /></el-icon><span>{{ $t('explorer:indexes.refresh') }}</span>
      </el-button>
      <el-button size="small" :disabled="!rows.length" @click="copyGrid">
        <el-icon><ei-document-copy /></el-icon><span>{{ $t('explorer:indexes.copy') }}</span>
      </el-button>
    </header>
    <div v-if="entry?.status === 'error'" class="iu-error">{{ entry.error }}</div>
    <template v-else-if="entry?.status === 'ready' && !report">
      <p class="iu-dim">{{ $t('explorer:indexes.unsupported') }}</p>
    </template>
    <template v-else-if="report">
      <p v-if="report.stats_available" class="iu-note">
        {{ report.since ? $t('explorer:indexes.since', { since: report.since }) : $t('explorer:indexes.sinceRestart') }}
        <br>{{ $t('explorer:indexes.health.smallTables') }}
      </p>
      <p v-if="report.note" class="iu-note warn">{{ tb(report.note) }}</p>
      <div ref="body" class="iu-grid">
        <table class="iu-table">
          <thead>
            <tr>
              <th v-for="c in cols" :key="c.id" :class="{ n: c.num, sorted: sort?.col === c.id }" @click="sortBy(c.id)">
                {{ c.label }}<span v-if="sort?.col === c.id" class="iu-arrow">{{ sort.desc ? '▼' : '▲' }}</span>
              </th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="i in rows" :key="i.name" :class="{ focus: i.name === tab.focus }" @contextmenu.prevent="rowMenu($event, i)">
              <td class="iu-name nm-selectable">
                {{ i.name }}
                <span v-if="usageBadge(i, report)" class="iu-badge" :class="badgeClass(usageBadge(i, report)!)" :title="usageBadge(i, report)!.healthTip ?? undefined">{{ usageBadge(i, report)!.text }}</span>
              </td>
              <td :title="i.kind"><span class="iu-tag">{{ indexTag(i) }}</span></td>
              <td
                v-for="c in cols.slice(2)" :key="c.id" :class="[{ n: c.num }, c.id === 'seeks%' && i.seek_health ? `h-${i.seek_health}` : '']" class="nm-selectable"
                :title="c.id === 'seeks%' ? seekTip(i) ?? undefined : c.tip"
              >{{ c.text(i) }}</td>
            </tr>
            <tr v-if="!rows.length"><td :colspan="cols.length" class="iu-dim">{{ $t('explorer:indexes.none') }}</td></tr>
          </tbody>
        </table>
      </div>
    </template>
    <ContextMenu v-if="menu" :x="menu.x" :y="menu.y" :items="menu.items" @close="menu = null" />
    <DropIndexDialog v-if="dropping" :target="dropping" @close="dropping = null" />
  </div>
</template>

<style scoped>
.iu { height: 100%; min-height: 0; display: flex; flex-direction: column; padding: 16px 22px 16px; background: var(--ide-editor); }
.iu-head { display: flex; align-items: center; gap: 8px; }
.iu-head h2 { margin: 0; font-size: 17px; color: var(--nm-text-strong); }
.iu-head .el-button { margin: 0; }
.iu-head .el-button .el-icon + span { margin-left: 4px; }
.iu-dim { color: var(--nm-text-dim); font-size: 12px; }
.iu-note { margin: 10px 0 0; font-size: 12px; color: var(--nm-text-dim); }
.iu-note.warn { color: var(--nm-text); }
.iu-error { margin: 10px 0; padding: 8px 10px; border-radius: 3px; border: 1px solid color-mix(in srgb, var(--nm-danger) 50%, transparent); background: color-mix(in srgb, var(--nm-danger) 10%, transparent); color: var(--nm-text); white-space: pre-wrap; font-size: 12px; }
.iu-grid { flex: 1; min-height: 0; overflow: auto; margin-top: 12px; }
.iu-table { border-collapse: collapse; font-size: 12.5px; min-width: 100%; }
.iu-table th { position: sticky; top: 0; z-index: 1; background: var(--ide-editor); text-align: left; font-weight: 500; color: var(--nm-text-dim); padding: 5px 8px; border-bottom: 1px solid var(--nm-border); white-space: nowrap; cursor: pointer; user-select: none; }
.iu-table th:hover, .iu-table th.sorted { color: var(--nm-text-strong); }
.iu-table td { padding: 4px 8px; border-bottom: 1px solid var(--nm-border-soft); color: var(--nm-text); white-space: nowrap; }
.iu-table .n { text-align: right; font-variant-numeric: tabular-nums; }
.iu-table tr.focus td { background: color-mix(in srgb, var(--nm-accent) 14%, transparent); }
.iu-arrow { margin-left: 4px; font-size: 9px; }
.iu-name { font-weight: 500; color: var(--nm-text-strong); }
.iu-tag { font-size: 10.5px; padding: 0 5px; border-radius: 3px; border: 1px solid var(--nm-border); color: var(--nm-text-dim); }
.iu-badge { margin-left: 6px; font-size: 10.5px; padding: 0 5px; border-radius: 8px; background: color-mix(in srgb, var(--nm-accent) 18%, transparent); color: var(--nm-text); font-weight: 400; }
.iu-badge.unused { background: color-mix(in srgb, var(--nm-danger) 22%, transparent); color: var(--nm-danger); }
.iu-badge.h-good { background: color-mix(in srgb, var(--nm-success) 20%, transparent); color: var(--nm-success); }
.iu-badge.h-warn { background: color-mix(in srgb, var(--nm-warning) 22%, transparent); color: var(--nm-warning); }
.iu-badge.h-bad { background: color-mix(in srgb, var(--nm-danger) 22%, transparent); color: var(--nm-danger); }
.iu-table td.h-good { color: var(--nm-success); }
.iu-table td.h-warn { color: var(--nm-warning); }
.iu-table td.h-bad { color: var(--nm-danger); font-weight: 600; }
</style>
