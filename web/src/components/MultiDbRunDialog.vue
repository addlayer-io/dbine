<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import { MAX_PARALLEL, recallSelection, wildcardMatcher, type MultiDbLive } from '../composables/multiDb';

// "Ejecutar en varias bases…": pick the connection's databases (a filter with
// `*` wildcards, Todas / Ninguna over what the filter shows). The last pick
// is remembered per connection; the first time, the tab's database is
// picked. "Ejecutar" hands the pick to the editor, which asks before a
// script that writes and starts the run; while it runs the list shows each
// database's state and the run can go to the background or be cancelled.

const props = defineProps<{
  connectionId: string;
  currentDatabase: string;
  databases: string[];
  loading?: boolean;
  readOnly?: boolean;
  live?: MultiDbLive | null;
}>();
const emit = defineEmits<{ run: [databases: string[]]; close: []; background: []; cancel: [] }>();
const { t } = useTranslation();

const filter = ref('');
const picked = ref<Set<string>>(new Set());
// Once the list is there (the connection may still be opening): the last
// pick that still exists, else the tab's database.
let initialized = false;
watch(() => props.databases, (dbs) => {
  if (initialized || !dbs.length) return;
  initialized = true;
  const remembered = recallSelection(props.connectionId)?.filter((d) => dbs.includes(d)) ?? [];
  picked.value = new Set(remembered.length ? remembered : dbs.includes(props.currentDatabase) ? [props.currentDatabase] : []);
}, { immediate: true });

const running = computed(() => !!props.live?.running);
/** While a run goes (or after it), only its databases. */
const shown = computed(() => {
  if (props.live) return props.live.databases;
  const m = wildcardMatcher(filter.value);
  return props.databases.filter(m);
});
/** In the order the server lists them. */
const selected = computed(() => props.databases.filter((d) => picked.value.has(d)));

function toggle(db: string, on: boolean) {
  const next = new Set(picked.value);
  if (on) next.add(db); else next.delete(db);
  picked.value = next;
}
function pickShown(on: boolean) {
  const next = new Set(picked.value);
  for (const d of shown.value) if (on) next.add(d); else next.delete(d);
  picked.value = next;
}

const num = (n: number) => n.toLocaleString(locale());
function stateOf(db: string) {
  return props.live?.states[db] ?? null;
}
function stateText(db: string): string {
  const s = stateOf(db);
  if (!s) return '';
  if (s.status === 'ok') return `${t('results:messages.rows', { count: s.rows, rows: num(s.rows) })} · ${t('results:messages.elapsed', { ms: num(s.elapsed_ms) })}`;
  if (s.status === 'error') return tb(s.error ?? '');
  return t(`multiDb:status.${s.status}`);
}
</script>

<template>
  <el-dialog
    :model-value="true" width="620px" append-to-body :title="$t('multiDb:dialog.title')"
    :close-on-click-modal="false" :close-on-press-escape="!running" :show-close="!running" @close="emit('close')"
  >
    <p class="md-p">{{ $t('multiDb:dialog.intro', { max: MAX_PARALLEL }) }}</p>
    <el-alert v-if="readOnly" type="info" :title="$t('multiDb:dialog.readOnly')" :closable="false" show-icon class="md-alert" />

    <div class="md-bar">
      <el-input v-model="filter" clearable :disabled="!!live" :placeholder="$t('multiDb:dialog.filter')" class="md-filter">
        <template #prefix><el-icon><ei-search /></el-icon></template>
      </el-input>
      <el-button :disabled="!!live || !shown.length" @click="pickShown(true)">{{ $t('multiDb:dialog.all') }}</el-button>
      <el-button :disabled="!!live || !shown.length" @click="pickShown(false)">{{ $t('multiDb:dialog.none') }}</el-button>
    </div>
    <div class="md-count">
      {{ $t('multiDb:dialog.count', { selected: num(selected.length), total: num(databases.length) }) }}
      <span v-if="filter.trim()"> · {{ $t('multiDb:dialog.shown', { shown: num(shown.length) }) }}</span>
    </div>

    <div v-loading="loading" class="md-list" role="group" :aria-label="$t('multiDb:dialog.title')">
      <div v-if="!loading && !databases.length" class="md-empty">{{ $t('multiDb:dialog.noDatabases') }}</div>
      <div v-else-if="!loading && !shown.length" class="md-empty">{{ $t('multiDb:dialog.noMatch') }}</div>
      <div v-for="d in shown" :key="d" class="md-row" :class="stateOf(d)?.status">
        <el-checkbox :model-value="live ? true : picked.has(d)" :disabled="!!live" @update:model-value="(v: string | number | boolean) => toggle(d, !!v)">
          <span class="md-name" :title="d">{{ d }}</span>
        </el-checkbox>
        <span v-if="d === currentDatabase" class="md-tag">{{ $t('multiDb:dialog.current') }}</span>
        <span class="nm-spacer" />
        <template v-if="live && stateOf(d)">
          <el-icon v-if="stateOf(d)!.status === 'pending'" class="md-icon"><ei-clock /></el-icon>
          <el-icon v-else-if="stateOf(d)!.status === 'ok'" class="md-icon ok"><ei-circle-check-filled /></el-icon>
          <el-icon v-else-if="stateOf(d)!.status === 'error'" class="md-icon err"><ei-circle-close-filled /></el-icon>
          <el-icon v-else class="md-icon warn"><ei-warning-filled /></el-icon>
          <span class="md-state" :title="stateText(d)">{{ stateText(d) }}</span>
        </template>
      </div>
    </div>

    <template #footer>
      <div class="md-foot">
        <span v-if="live" class="md-progress">{{ $t('multiDb:dialog.progress', { done: num(live.done), total: num(live.databases.length) }) }}</span>
        <span class="nm-spacer" />
        <template v-if="running">
          <el-button @click="emit('background')">{{ $t('tasks:panel.background') }}</el-button>
          <el-button type="danger" @click="emit('cancel')">{{ $t('common:cancel') }}</el-button>
        </template>
        <template v-else>
          <el-button @click="emit('close')">{{ $t('common:cancel') }}</el-button>
          <el-button type="primary" :disabled="!selected.length" @click="emit('run', selected)">
            {{ $t('multiDb:dialog.run', { count: selected.length }) }}
          </el-button>
        </template>
      </div>
    </template>
  </el-dialog>
</template>

<style scoped lang="scss">
.md-p { margin: 0 0 10px; color: var(--nm-text); }
.md-alert { margin-bottom: 10px; }
.md-bar { display: flex; gap: 6px; align-items: center; }
.md-filter { flex: 1; }
.md-bar .el-button + .el-button { margin-left: 0; }
.md-count { margin: 6px 0; font-size: 12px; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; }
.md-list {
  height: 300px; overflow: auto; border: 1px solid var(--nm-border); border-radius: 4px; padding: 2px 0;
  background: var(--ide-editor, var(--nm-bg-elev));
}
.md-row { display: flex; align-items: center; gap: 6px; padding: 0 10px; min-height: 26px; }
.md-row:hover { background: var(--ide-hover, color-mix(in srgb, var(--nm-text) 6%, transparent)); }
.md-row :deep(.el-checkbox) { min-width: 0; max-width: 60%; height: 26px; }
.md-row :deep(.el-checkbox__label) { min-width: 0; overflow: hidden; }
.md-name { display: block; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.md-tag {
  flex-shrink: 0; font-size: 10.5px; padding: 0 6px; border-radius: 8px; color: var(--nm-text-dim);
  border: 1px solid var(--nm-border);
}
.md-icon { flex-shrink: 0; color: var(--nm-text-dim); }
.md-icon.ok { color: var(--nm-success); }
.md-icon.err { color: var(--nm-danger); }
.md-icon.warn { color: var(--nm-warning); }
.md-state {
  max-width: 45%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 12px; color: var(--nm-text-dim);
  font-variant-numeric: tabular-nums;
}
.md-row.error .md-state { color: var(--nm-danger); }
.md-empty { padding: 16px; text-align: center; color: var(--nm-text-dim); }
.md-foot { display: flex; align-items: center; gap: 8px; }
.md-foot .el-button + .el-button { margin-left: 0; }
.md-progress { font-size: 12px; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; }
</style>
