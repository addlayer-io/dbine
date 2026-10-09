<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { locksApi, type ServerProcess } from '../api/locks';
import { newQuery } from '../composables/actions';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';

// The Monitor's process list (docs/processes.md): the server's sessions and
// running requests, polled on their own while the tab shows them. Filters,
// sorting, blockers highlighted, and per row: the whole statement, open it
// in a query, cancel the statement or end the session.

const props = defineProps<{
  connectionId: string;
  /** Poll now (the tab and this view are showing, not paused). */
  polling: boolean;
  every: number;
  canKill: boolean;
  canCancel: boolean;
  /** Why the login can't end or cancel other sessions ('' when it can). */
  killDenied: string;
}>();
const emit = defineEmits<{ loading: [busy: boolean]; polled: [at: number] }>();
const { t } = useTranslation();

const list = ref<ServerProcess[] | null>(null);
const error = ref<string | null>(null);
const loading = ref(false);

async function refresh() {
  if (loading.value) return;
  loading.value = true;
  emit('loading', true);
  try {
    list.value = await locksApi.processes(props.connectionId);
    error.value = null;
    emit('polled', Date.now());
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    loading.value = false;
    emit('loading', false);
  }
}

let timer: ReturnType<typeof setInterval> | undefined;
watch(
  () => [props.polling, props.every] as const,
  ([polling, every]) => {
    clearInterval(timer);
    timer = undefined;
    if (!polling) return;
    refresh();
    timer = setInterval(refresh, every * 1000);
  },
  { immediate: true },
);
onUnmounted(() => clearInterval(timer));

// -- filters -------------------------------------------------------------------------------

const onlyActive = ref(false);
const hideSystem = ref(true);
const database = ref('');
const user = ref('');
const host = ref('');
const search = ref('');

function distinct(pick: (p: ServerProcess) => string | null): string[] {
  const out = new Set<string>();
  for (const p of list.value ?? []) {
    const v = pick(p);
    if (v) out.add(v);
  }
  return [...out].sort((a, b) => a.localeCompare(b));
}
const databases = computed(() => distinct((p) => p.database));
const users = computed(() => distinct((p) => p.user));
const hosts = computed(() => distinct((p) => p.host));
const hasSystem = computed(() => (list.value ?? []).some((p) => p.system));

/** id → how many sessions wait for it, directly or through others. */
const blocks = computed(() => {
  const children = new Map<string, string[]>();
  for (const p of list.value ?? []) {
    if (p.blocked_by) (children.get(p.blocked_by) ?? children.set(p.blocked_by, []).get(p.blocked_by)!).push(p.id);
  }
  const out = new Map<string, number>();
  const count = (id: string, seen: Set<string>): number => {
    if (seen.has(id)) return 0;
    seen.add(id);
    return (children.get(id) ?? []).reduce((n, c) => n + 1 + count(c, seen), 0);
  };
  for (const id of children.keys()) out.set(id, count(id, new Set()));
  return out;
});

const filtered = computed(() => {
  const q = search.value.trim().toLowerCase();
  return (list.value ?? []).filter((p) =>
    (!onlyActive.value || p.active || blocks.value.has(p.id))
    && (!hideSystem.value || !p.system)
    && (!database.value || p.database === database.value)
    && (!user.value || p.user === user.value)
    && (!host.value || p.host === host.value)
    && (!q || [p.id, p.status, p.user, p.host, p.program, p.database, p.command, p.wait, p.sql].some((v) => v?.toLowerCase().includes(q))));
});

// -- sorting -------------------------------------------------------------------------------

type Col = 'id' | 'status' | 'user' | 'host' | 'program' | 'database' | 'command' | 'elapsed_ms' | 'cpu_ms' | 'reads' | 'writes' | 'wait' | 'blocked_by' | 'sql';
const COLUMNS: { key: Col; label: string; num?: boolean }[] = [
  { key: 'id', label: 'processes:col.id' },
  { key: 'status', label: 'processes:col.status' },
  { key: 'user', label: 'processes:col.user' },
  { key: 'host', label: 'processes:col.host' },
  { key: 'program', label: 'processes:col.program' },
  { key: 'database', label: 'processes:col.database' },
  { key: 'command', label: 'processes:col.command' },
  { key: 'elapsed_ms', label: 'processes:col.elapsed', num: true },
  { key: 'cpu_ms', label: 'processes:col.cpu', num: true },
  { key: 'reads', label: 'processes:col.reads', num: true },
  { key: 'writes', label: 'processes:col.writes', num: true },
  { key: 'wait', label: 'processes:col.wait' },
  { key: 'blocked_by', label: 'processes:col.blockedBy' },
  { key: 'sql', label: 'processes:col.sql' },
];
/** The columns some row fills: an engine that doesn't report CPU shows no CPU column. */
const columns = computed(() => COLUMNS.filter((c) => c.key === 'id' || (list.value ?? []).some((p) => p[c.key] !== null && p[c.key] !== '')));

const sortBy = ref<Col | null>(null);
const sortDesc = ref(false);
function sortOn(c: Col) {
  if (sortBy.value === c) {
    if (sortDesc.value) sortBy.value = null;
    sortDesc.value = !sortDesc.value;
  } else {
    sortBy.value = c;
    sortDesc.value = !!COLUMNS.find((x) => x.key === c)?.num;
  }
}
const ID_NUMERIC = /^\d+$/;
const rows = computed(() => {
  const c = sortBy.value;
  // By default: blockers first, then active, then the rest by id.
  const base = [...filtered.value].sort((a, b) =>
    (blocks.value.get(b.id) ?? 0) - (blocks.value.get(a.id) ?? 0)
    || Number(b.active) - Number(a.active)
    || compare(a.id, b.id));
  if (!c) return base;
  return base.sort((a, b) => {
    const r = compare(a[c], b[c]);
    return sortDesc.value ? -r : r;
  });
});
function compare(a: unknown, b: unknown): number {
  if (a === b) return 0;
  if (a === null || a === undefined) return 1;
  if (b === null || b === undefined) return -1;
  if (typeof a === 'number' && typeof b === 'number') return a - b;
  const x = String(a), y = String(b);
  if (ID_NUMERIC.test(x) && ID_NUMERIC.test(y)) return Number(x) - Number(y);
  return x.localeCompare(y);
}

// -- formatting ----------------------------------------------------------------------------

function ms(v: number | null): string {
  if (v === null) return '';
  if (v < 1000) return `${v} ms`;
  const s = v / 1000;
  if (s < 60) return t('monitor:duration.s', { s: s.toLocaleString(locale(), { maximumFractionDigits: s < 10 ? 1 : 0 }) });
  const d = Math.floor(s / 86400), h = Math.floor((s % 86400) / 3600), m = Math.floor((s % 3600) / 60);
  return d ? t('monitor:duration.dh', { d, h }) : h ? t('monitor:duration.hm', { h, m }) : t('monitor:duration.ms', { m, s: Math.floor(s % 60) });
}
function cell(p: ServerProcess, c: Col): string {
  const v = p[c];
  if (v === null || v === undefined) return '';
  if (c === 'elapsed_ms' || c === 'cpu_ms') return ms(v as number);
  if (typeof v === 'number') return v.toLocaleString(locale());
  if (c === 'status' || c === 'wait') return tb(String(v));
  return String(v);
}

// -- selection and actions -----------------------------------------------------------------

const selectedId = ref<string | null>(null);
const selected = computed(() => list.value?.find((p) => p.id === selectedId.value) ?? null);
const waitingOn = computed(() => (selected.value ? (list.value ?? []).filter((p) => p.blocked_by === selected.value!.id) : []));

function select(p: ServerProcess) {
  selectedId.value = selectedId.value === p.id ? null : p.id;
}

async function openInQuery(p: ServerProcess) {
  if (!p.sql) return;
  await newQuery(props.connectionId, p.database ?? '', p.sql, t('processes:queryName', { id: p.id }));
}
async function copySql(p: ServerProcess) {
  if (!p.sql) return;
  await navigator.clipboard.writeText(p.sql);
  ElMessage.success(t('processes:copied'));
}

async function confirmAndRun(p: ServerProcess, kind: 'kill' | 'cancel') {
  const k = `processes:${kind}`;
  try {
    await ElMessageBox.confirm(
      t(`${k}Confirm`, { id: p.id, user: p.user ?? '?' }) + (p.sql ? `\n\n${p.sql.slice(0, 400)}` : ''),
      t(`${k}Title`),
      { confirmButtonText: t(`${k}Action`), cancelButtonText: t('common:cancel'), type: 'warning', customStyle: { whiteSpace: 'pre-wrap' } },
    );
  } catch {
    return;
  }
  try {
    await (kind === 'kill' ? locksApi.kill : locksApi.cancel)(props.connectionId, p.id);
    ElMessage.success(t(`${k}Done`, { id: p.id }));
    refresh();
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

defineExpose({ refresh });
</script>

<template>
  <section class="pp">
    <div class="pp-bar">
      <el-checkbox v-model="onlyActive" size="small">{{ $t('processes:onlyActive') }}</el-checkbox>
      <el-checkbox v-if="hasSystem" v-model="hideSystem" size="small">{{ $t('processes:hideSystem') }}</el-checkbox>
      <el-select v-if="databases.length > 1" v-model="database" size="small" clearable :placeholder="$t('processes:anyDatabase')" style="width: 150px">
        <el-option v-for="d in databases" :key="d" :label="d" :value="d" />
      </el-select>
      <el-select v-if="users.length > 1" v-model="user" size="small" clearable :placeholder="$t('processes:anyUser')" style="width: 150px">
        <el-option v-for="u in users" :key="u" :label="u" :value="u" />
      </el-select>
      <el-select v-if="hosts.length > 1" v-model="host" size="small" clearable :placeholder="$t('processes:anyHost')" style="width: 150px">
        <el-option v-for="h in hosts" :key="h" :label="h" :value="h" />
      </el-select>
      <div class="pp-spacer" />
      <span v-if="list" class="pp-count">{{ $t('processes:count', { shown: rows.length, total: list.length }) }}</span>
      <el-input v-model="search" size="small" :placeholder="$t('monitor:filter')" clearable style="width: 200px">
        <template #prefix><el-icon><ei-search /></el-icon></template>
      </el-input>
    </div>

    <el-alert v-if="error" type="error" :title="error" :closable="false" show-icon class="pp-alert" />
    <div v-if="!list && !error" class="pp-empty"><el-icon class="is-loading" :size="22"><ei-loading /></el-icon></div>

    <div v-if="list" class="pp-wrap">
      <table class="pp-table">
        <thead>
          <tr>
            <th v-for="c in columns" :key="c.key" :class="{ num: c.num, sorted: sortBy === c.key }" @click="sortOn(c.key)">
              {{ $t(c.label) }}<span v-if="sortBy === c.key" class="pp-sort">{{ sortDesc ? '↓' : '↑' }}</span>
            </th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="p in rows"
            :key="p.id"
            :class="{ sel: p.id === selectedId, blocker: blocks.has(p.id), blocked: !!p.blocked_by, idle: !p.active, own: p.own }"
            @click="select(p)"
          >
            <td v-for="c in columns" :key="c.key" :class="{ num: c.num, sql: c.key === 'sql' }" :title="c.key === 'sql' ? p.sql ?? '' : undefined">
              <template v-if="c.key === 'id'">
                {{ p.id }}
                <span v-if="blocks.has(p.id)" class="pp-badge" :title="$t('locks:blocksTitle')">{{ $t('locks:blocks', { count: blocks.get(p.id) }) }}</span>
                <span v-if="p.own" class="pp-own">{{ $t('processes:own') }}</span>
              </template>
              <template v-else>{{ cell(p, c.key) }}</template>
            </td>
          </tr>
          <tr v-if="!rows.length">
            <td :colspan="columns.length" class="nm-muted">{{ list.length ? $t('monitor:noMatch') : $t('monitor:noRows') }}</td>
          </tr>
        </tbody>
      </table>
    </div>

    <div v-if="selected" class="pp-detail">
      <div class="pp-detail-head">
        <strong>{{ $t('processes:session', { id: selected.id }) }}</strong>
        <span class="pp-detail-meta">{{ [selected.user, selected.host, selected.program, selected.database].filter(Boolean).join(' · ') }}</span>
        <div class="pp-spacer" />
        <el-button size="small" :disabled="!selected.sql" @click="openInQuery(selected)">{{ $t('processes:openInQuery') }}</el-button>
        <el-button size="small" :disabled="!selected.sql" @click="copySql(selected)">{{ $t('processes:copy') }}</el-button>
        <span v-if="canCancel" :title="selected.own ? $t('processes:ownTitle') : killDenied">
          <el-button size="small" type="warning" plain :disabled="selected.own || !!killDenied || !selected.active" @click="confirmAndRun(selected, 'cancel')">
            {{ $t('processes:cancelAction') }}
          </el-button>
        </span>
        <span v-if="canKill" :title="selected.own ? $t('processes:ownTitle') : killDenied">
          <el-button size="small" type="danger" plain :disabled="selected.own || !!killDenied" @click="confirmAndRun(selected, 'kill')">
            {{ $t('processes:killAction') }}
          </el-button>
        </span>
        <el-button size="small" text @click="selectedId = null"><el-icon><ei-close /></el-icon></el-button>
      </div>
      <div v-if="selected.blocked_by || waitingOn.length" class="pp-chain">
        <span v-if="selected.blocked_by">
          {{ $t('processes:waitsFor') }}
          <a href="#" @click.prevent="selectedId = selected.blocked_by">{{ selected.blocked_by }}</a>
          <template v-if="selected.wait"> ({{ tb(selected.wait) }})</template>
        </span>
        <span v-if="waitingOn.length">
          {{ $t('processes:waitedBy') }}
          <template v-for="(w, i) in waitingOn" :key="w.id"><template v-if="i">, </template><a href="#" @click.prevent="selectedId = w.id">{{ w.id }}</a></template>
        </span>
      </div>
      <pre v-if="selected.sql" class="pp-sql">{{ selected.sql }}</pre>
      <p v-else class="nm-muted pp-nosql">{{ $t('processes:noSql') }}</p>
    </div>
  </section>
</template>

<style scoped>
.pp { display: flex; flex-direction: column; min-height: 0; border: 1px solid var(--nm-border-soft); border-radius: 4px; }
.pp-bar {
  display: flex; align-items: center; flex-wrap: wrap; gap: 8px 12px; padding: 6px 8px;
  border-bottom: 1px solid var(--nm-border-soft); background: var(--nm-bg-elev);
}
.pp-bar .el-checkbox { margin-right: 0; }
.pp-spacer { flex: 1; }
.pp-count { font-size: 11.5px; color: var(--nm-text-dim); }
.pp-alert { margin: 8px; width: auto; }
.pp-empty { display: flex; justify-content: center; padding: 40px; color: var(--nm-text-dim); }
.pp-wrap { flex: 1; min-height: 120px; max-height: 520px; overflow: auto; }
.pp-table { width: 100%; border-collapse: collapse; font-size: 12px; }
.pp-table th {
  position: sticky; top: 0; z-index: 1; text-align: left; font-weight: 600; padding: 5px 10px;
  background: var(--nm-bg-elev); color: var(--nm-text-dim); border-bottom: 1px solid var(--nm-border);
  white-space: nowrap; cursor: pointer; user-select: none;
}
.pp-table th:hover, .pp-table th.sorted { color: var(--nm-text-strong); }
.pp-sort { margin-left: 3px; }
.pp-table td {
  padding: 4px 10px; border-bottom: 1px solid var(--nm-border-soft); color: var(--nm-text);
  max-width: 260px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  font-variant-numeric: tabular-nums;
}
.pp-table .num { text-align: right; }
.pp-table td.sql { max-width: 480px; font-family: var(--nm-mono); }
.pp-table tbody tr { cursor: pointer; }
.pp-table tbody tr:hover td { background: var(--ide-hover); }
.pp-table tr.idle td { color: var(--nm-text-dim); }
.pp-table tr.blocked td { background: color-mix(in srgb, var(--nm-warning) 6%, transparent); }
.pp-table tr.blocker td { background: color-mix(in srgb, var(--nm-warning) 14%, transparent); color: var(--nm-text); }
.pp-table tr.sel td { background: color-mix(in srgb, var(--nm-accent) 18%, transparent); }
.pp-badge {
  margin-left: 6px; padding: 0 6px; border-radius: 8px; font-size: 10.5px;
  background: color-mix(in srgb, var(--nm-warning) 30%, transparent); color: var(--nm-text-strong);
}
.pp-own { margin-left: 6px; font-size: 10.5px; color: var(--nm-text-muted); }

.pp-detail { border-top: 1px solid var(--nm-border); padding: 8px 10px 10px; background: var(--nm-bg-elev); }
.pp-detail-head { display: flex; align-items: center; flex-wrap: wrap; gap: 6px 10px; }
.pp-detail-head .el-button { margin: 0; }
.pp-detail-head strong { color: var(--nm-text-strong); }
.pp-detail-meta { font-size: 11.5px; color: var(--nm-text-dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0; }
.pp-chain { display: flex; gap: 16px; margin-top: 6px; font-size: 12px; color: var(--nm-text-dim); }
.pp-chain a { color: var(--nm-accent); }
.pp-sql {
  margin: 8px 0 0; padding: 8px 10px; max-height: 220px; overflow: auto; white-space: pre-wrap; word-break: break-word;
  font-family: var(--nm-mono); font-size: 12px; color: var(--nm-text); background: var(--nm-bg);
  border: 1px solid var(--nm-border-soft); border-radius: 3px;
}
.pp-nosql { margin: 8px 0 0; font-size: 12px; }
</style>
