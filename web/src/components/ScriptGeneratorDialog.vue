<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { save } from '@tauri-apps/plugin-dialog';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { locale } from '../i18n';
import type { ObjectRef } from '../api/types';
import { migrationApi, type MigrationTarget } from '../api/migration';
import { FAMILY_LABELS, type Family } from '../api/types';
import { useConnectionsStore } from '../stores/connections';

// Script of a whole database (or part of it): pick the objects on the left,
// what to emit on the right, then open it in an editor or stream it to a
// file with progress. Backend: `generate_script` (docs/api-comandos.md).

interface ScriptObject { kind: string; schema: string | null; name: string }
interface ScriptOptions {
  drop: boolean; if_exists: boolean; create: boolean; indexes: boolean;
  foreign_keys: boolean; definitions: boolean; data: boolean; data_limit: number | null;
}
interface ScriptResult { script: string | null; objects: number; rows: number }
interface ScriptProgress { id: string; done: number; total: number; current: string }

const props = withDefaults(defineProps<{
  connectionId: string;
  database: string;
  objects: ScriptObject[];
  /** kind id → plural label ("Tablas", "Vistas"…). */
  kindLabels: Record<string, string>;
  language: string;
  dialect: string;
  supportsData: boolean;
  title?: string;
  /** The source's driver id (the script's engine by default). */
  driverId?: string;
}>(), { title: '', driverId: '' });
const emit = defineEmits<{ close: []; 'open-script': [script: string, connectionId: string | null] }>();
const { t } = useTranslation();

// ---- engine: the same one, or another (the tables converted) -------------------
const conns = useConnectionsStore();
const targets = ref<MigrationTarget[]>([]);
const engine = ref(props.driverId);
onMounted(async () => { try { targets.value = await migrationApi.targets(); } catch { /* outside Tauri */ } });
const otherEngine = computed(() => !!engine.value && engine.value !== props.driverId);
const engineGroups = computed(() => {
  const by = new Map<string, MigrationTarget[]>();
  for (const t of targets.value.filter((x) => x.supported && x.id !== props.driverId)) by.set(t.family, [...(by.get(t.family) ?? []), t]);
  return [...by.entries()].map(([f, list]) => ({ label: FAMILY_LABELS[f as Family] ?? f, list: list.sort((a, b) => a.name.localeCompare(b.name)) }));
});
const engineName = computed(() => conns.drivers.find((d) => d.id === engine.value)?.name ?? engine.value);
/** Where a converted script opens: a connection of that engine. */
const openConns = computed(() => conns.list.filter((c) => c.config.driver === engine.value));
const openConnection = ref('');
watch(engine, () => {
  openConnection.value = openConns.value[0]?.id ?? '';
  if (otherEngine.value && !openConns.value.length) destination.value = 'file';
});

const TABLE_KINDS = ['table', 'collection'];
const key = (o: ScriptObject) => `${o.kind}\u0000${o.schema ?? ''}\u0000${o.name}`;
const display = (o: ScriptObject) => (o.schema ? `${o.schema}.${o.name}` : o.name);

// ---- object selection ------------------------------------------------------
const selected = reactive(new Set<string>(props.objects.map(key)));
const collapsed = reactive(new Set<string>());
const filter = ref('');

const groups = computed(() => {
  const order = Object.keys(props.kindLabels);
  const byKind = new Map<string, ScriptObject[]>();
  for (const o of props.objects) {
    if (!byKind.has(o.kind)) byKind.set(o.kind, []);
    byKind.get(o.kind)!.push(o);
  }
  const q = filter.value.trim().toLowerCase();
  return [...byKind.entries()]
    .sort(([a], [b]) => {
      const ia = order.indexOf(a), ib = order.indexOf(b);
      return (ia < 0 ? 999 : ia) - (ib < 0 ? 999 : ib) || a.localeCompare(b);
    })
    .map(([kind, all]) => {
      const items = (q ? all.filter((o) => display(o).toLowerCase().includes(q)) : all)
        .slice().sort((a, b) => display(a).localeCompare(display(b)));
      const checked = all.filter((o) => selected.has(key(o))).length;
      return { kind, label: props.kindLabels[kind] ?? kind, all, items, checked };
    })
    .filter((g) => g.items.length > 0);
});

const selectedCount = computed(() => props.objects.filter((o) => selected.has(key(o))).length);

function groupState(g: { items: ScriptObject[] }) {
  const n = g.items.filter((o) => selected.has(key(o))).length;
  return { all: n > 0 && n === g.items.length, some: n > 0 && n < g.items.length };
}
function setGroup(items: ScriptObject[], on: boolean) {
  for (const o of items) (on ? selected.add(key(o)) : selected.delete(key(o)));
}
function setAll(on: boolean) {
  for (const g of groups.value) setGroup(g.items, on);
}
function toggle(o: ScriptObject) {
  const k = key(o);
  if (selected.has(k)) selected.delete(k); else selected.add(k);
}
function toggleCollapse(kind: string) {
  if (collapsed.has(kind)) collapsed.delete(kind); else collapsed.add(kind);
}

// ---- options -----------------------------------------------------------------
const opts = reactive<ScriptOptions>({
  drop: false, if_exists: true, create: true, indexes: true,
  foreign_keys: true, definitions: true, data: false, data_limit: null,
});
const limitRows = ref(false);
const limitValue = ref<number>(1000);
const destination = ref<'editor' | 'file'>('editor');

const hasTables = computed(() => props.objects.some((o) => TABLE_KINDS.includes(o.kind)));
const hasDefinitions = computed(() => props.objects.some((o) => !TABLE_KINDS.includes(o.kind)));
const nothingToEmit = computed(() =>
  !opts.drop && !opts.create && !opts.indexes && !opts.foreign_keys && !opts.definitions && !opts.data);

// ---- run ---------------------------------------------------------------------
const running = ref(false);
const cancelling = ref(false);
const progress = reactive({ done: 0, total: 0, current: '' });
const scriptId = crypto.randomUUID();
let unlisten: UnlistenFn | null = null;

const percent = computed(() => (progress.total ? Math.min(100, Math.round((progress.done / progress.total) * 100)) : 0));

function extension(): string {
  const lang = otherEngine.value ? conns.drivers.find((d) => d.id === engine.value)?.language ?? 'sql' : props.language;
  switch (lang) {
    case 'sql': return 'sql';
    case 'cql': return 'cql';
    case 'json': return 'js';
    default: return 'txt';
  }
}

async function run() {
  if (!selectedCount.value) {
    ElMessage.warning(t('scripts:generator.selectAtLeastOne'));
    return;
  }
  let path: string | null = null;
  if (destination.value === 'file') {
    const ext = extension();
    try {
      path = await save({
        defaultPath: `${props.database || 'script'}.${ext}`,
        filters: [{ name: 'Script', extensions: [ext] }, { name: t('scripts:run.allFiles'), extensions: ['*'] }],
      });
    } catch { /* no dialog outside Tauri */ }
    if (!path) return;
  }
  const objects: ObjectRef[] = props.objects
    .filter((o) => selected.has(key(o)))
    .map((o) => ({ kind: o.kind, schema: o.schema, name: o.name }));
  running.value = true;
  cancelling.value = false;
  Object.assign(progress, { done: 0, total: objects.length, current: '' });
  try {
    unlisten = await listen<ScriptProgress>('script-progress', (e) => {
      if (e.payload.id !== scriptId) return;
      Object.assign(progress, { done: e.payload.done, total: e.payload.total, current: e.payload.current });
    });
    const result = await invoke<ScriptResult>('generate_script', {
      args: {
        script_id: scriptId,
        connection_id: props.connectionId,
        database: props.database,
        objects,
        options: {
          ...opts,
          data: props.supportsData && opts.data,
          data_limit: props.supportsData && opts.data && limitRows.value ? limitValue.value : null,
        },
        path,
        target_driver: otherEngine.value ? engine.value : null,
      },
    });
    if (path) {
      const objects = t('scripts:generator.savedObjects', { count: result.objects, n: result.objects.toLocaleString(locale()) });
      const message = result.rows
        ? t('scripts:generator.savedWithRows', { objects, count: result.rows, n: result.rows.toLocaleString(locale()) })
        : t('scripts:generator.saved', { objects });
      ElMessage.success({ message, duration: 4000 });
    } else if (result.script != null) {
      emit('open-script', result.script, otherEngine.value ? openConnection.value || null : null);
    }
    emit('close');
  } catch (e) {
    if (cancelling.value) ElMessage.info(t('scripts:generator.cancelled'));
    else ElMessage.error({ message: errorMessage(e), duration: 6000 });
  } finally {
    running.value = false;
    cancelling.value = false;
    unlisten?.();
    unlisten = null;
  }
}

function cancel() {
  if (running.value) {
    cancelling.value = true;
    invoke('cancel_query', { args: { session_id: `script:${scriptId}` } }).catch(() => {});
  } else emit('close');
}
onBeforeUnmount(() => unlisten?.());
defineExpose({ run });
</script>

<template>
  <el-dialog
    :model-value="true" :title="title || $t('scripts:generator.title')" width="880px" append-to-body align-center
    :close-on-click-modal="false" :close-on-press-escape="!running" :show-close="!running"
    class="sg-dialog" @close="cancel"
  >
    <div class="sg-sub nm-muted">
      <i18next :translation="$t('scripts:generator.subtitle', { count: objects.length, n: objects.length.toLocaleString(locale()) })"><template #database><b>{{ database }}</b></template></i18next>
    </div>
    <div class="sg-body">
      <!-- objects -->
      <section class="sg-objects">
        <div class="sg-objects-head">
          <el-input v-model="filter" :placeholder="$t('scripts:generator.filterPlaceholder')" clearable :disabled="running" class="sg-filter">
            <template #prefix><el-icon><ei-search /></el-icon></template>
          </el-input>
          <el-button link type="primary" :disabled="running" @click="setAll(true)">{{ $t('common:all') }}</el-button>
          <el-button link :disabled="running" @click="setAll(false)">{{ $t('common:none') }}</el-button>
        </div>
        <div class="sg-list" :class="{ disabled: running }">
          <template v-for="g in groups" :key="g.kind">
            <div class="sg-group">
              <button class="sg-caret" type="button" @click="toggleCollapse(g.kind)">
                <el-icon><ei-arrow-right v-if="collapsed.has(g.kind) && !filter" /><ei-arrow-down v-else /></el-icon>
              </button>
              <el-checkbox
                :model-value="groupState(g).all" :indeterminate="groupState(g).some" :disabled="running"
                @change="(v: string | number | boolean) => setGroup(g.items, !!v)"
              >
                <span class="sg-group-label">{{ g.label }}</span>
              </el-checkbox>
              <span class="sg-count">{{ g.checked }}/{{ g.all.length }}</span>
            </div>
            <template v-if="!collapsed.has(g.kind) || filter">
              <div
                v-for="o in g.items" :key="key(o)" class="sg-item" :class="{ on: selected.has(key(o)) }"
                @click="!running && toggle(o)"
              >
                <el-checkbox :model-value="selected.has(key(o))" :disabled="running" @click.stop @change="toggle(o)" />
                <span class="sg-name" :title="display(o)">
                  <span v-if="o.schema" class="sg-schema">{{ o.schema }}.</span>{{ o.name }}
                </span>
              </div>
            </template>
          </template>
          <div v-if="!groups.length" class="sg-empty nm-muted">
            {{ objects.length ? $t('scripts:generator.noMatch') : $t('scripts:generator.noObjects') }}
          </div>
        </div>
        <div class="sg-objects-foot nm-muted">
          {{ $t('scripts:generator.selectedOf', { selected: selectedCount.toLocaleString(locale()), total: objects.length.toLocaleString(locale()) }) }}
        </div>
      </section>

      <!-- options -->
      <section class="sg-options">
        <div class="nm-section-title">{{ $t('scripts:generator.dropSection') }}</div>
        <el-checkbox v-model="opts.drop" :disabled="running">{{ $t('scripts:generator.drop') }}</el-checkbox>
        <el-checkbox v-model="opts.if_exists" :disabled="running">{{ $t('scripts:generator.ifExists') }}</el-checkbox>

        <div class="nm-section-title sg-gap">{{ $t('scripts:generator.structure') }}</div>
        <el-checkbox v-model="opts.create" :disabled="running || !hasTables">{{ $t('scripts:generator.createTables') }}</el-checkbox>
        <el-checkbox v-model="opts.indexes" :disabled="running || !hasTables" class="sg-indent">{{ $t('scripts:generator.indexes') }}</el-checkbox>
        <el-checkbox v-model="opts.foreign_keys" :disabled="running || !hasTables" class="sg-indent">{{ $t('scripts:generator.foreignKeys') }}</el-checkbox>
        <el-checkbox v-model="opts.definitions" :disabled="running || !hasDefinitions">
          {{ $t('scripts:generator.definitions') }}
        </el-checkbox>

        <div class="nm-section-title sg-gap">{{ $t('scripts:generator.dataSection') }}</div>
        <el-checkbox v-model="opts.data" :disabled="running || !supportsData || !hasTables">{{ $t('scripts:generator.data') }}</el-checkbox>
        <div v-if="!supportsData" class="sg-help">{{ $t('scripts:generator.noDataSupport') }}</div>
        <div class="sg-limit sg-indent">
          <el-checkbox v-model="limitRows" :disabled="running || !opts.data || !supportsData">{{ $t('scripts:generator.maxRows') }}</el-checkbox>
          <el-input-number
            v-model="limitValue" :min="1" :max="100000000" :step="1000" :controls="false"
            :disabled="running || !opts.data || !limitRows || !supportsData" class="sg-limit-input"
          />
        </div>

        <div class="nm-section-title sg-gap">{{ $t('scripts:generator.engine') }}</div>
        <el-select v-model="engine" filterable :disabled="running" size="small" style="width: 100%">
          <el-option :value="driverId" :label="$t('scripts:generator.sameEngine', { name: conns.drivers.find((d) => d.id === driverId)?.name ?? $t('scripts:generator.sameEngineFallback') })" />
          <el-option-group v-for="g in engineGroups" :key="g.label" :label="g.label">
            <el-option v-for="t in g.list" :key="t.id" :value="t.id" :label="t.name" />
          </el-option-group>
        </el-select>
        <div v-if="otherEngine" class="sg-help">
          <i18next :translation="$t('scripts:generator.otherEngineHelp', { engine: engineName })"><template #migrate><b>{{ $t('scripts:generator.migrate') }}</b></template></i18next>
        </div>

        <div class="nm-section-title sg-gap">{{ $t('scripts:generator.destination') }}</div>
        <el-radio-group v-model="destination" :disabled="running" class="sg-dest">
          <el-radio value="editor" :disabled="otherEngine && !openConns.length">{{ $t('scripts:generator.openInEditor') }}</el-radio>
          <el-radio value="file">{{ $t('scripts:generator.saveToFile') }}</el-radio>
        </el-radio-group>
        <el-select v-if="otherEngine && destination === 'editor' && openConns.length" v-model="openConnection" size="small" style="width: 100%; margin-top: 4px" :placeholder="$t('scripts:generator.openConnection')">
          <el-option v-for="c in openConns" :key="c.id" :label="c.name" :value="c.id" />
        </el-select>
        <div v-if="otherEngine && !openConns.length" class="sg-help">{{ $t('scripts:generator.noConnections', { engine: engineName }) }}</div>
        <div class="sg-help">
          {{ destination === 'editor'
            ? $t('scripts:generator.editorHelp')
            : $t('scripts:generator.fileHelp') }}
        </div>
      </section>
    </div>

    <div v-if="running" class="sg-progress">
      <div class="sg-progress-text">
        <el-icon class="is-loading"><ei-loading /></el-icon>
        <span v-if="cancelling">{{ $t('scripts:run.cancelling') }}</span>
        <span v-else class="sg-current">{{ progress.current ? $t('scripts:generator.generating', { name: progress.current }) : $t('scripts:generator.preparing') }}</span>
        <span class="nm-spacer" />
        <span class="sg-counter">{{ progress.done.toLocaleString(locale()) }} / {{ progress.total.toLocaleString(locale()) }}</span>
      </div>
      <el-progress :percentage="percent" :show-text="false" :stroke-width="4" />
    </div>

    <template #footer>
      <div class="sg-footer">
        <span v-if="!running && nothingToEmit" class="sg-warn">{{ $t('scripts:generator.pickOption') }}</span>
        <span class="nm-spacer" />
        <el-button :disabled="cancelling" @click="cancel">{{ running ? $t('scripts:generator.cancelGeneration') : $t('common:cancel') }}</el-button>
        <el-button type="primary" :loading="running" :disabled="!selectedCount || nothingToEmit" @click="run">
          {{ destination === 'file' ? $t('scripts:generator.saveEllipsis') : $t('scripts:generator.generate') }}
        </el-button>
      </div>
    </template>
  </el-dialog>
</template>

<style scoped>
.sg-sub { margin: -6px 0 10px; }
.sg-sub b { color: var(--nm-text-strong); font-weight: 600; }
.sg-body { display: grid; grid-template-columns: minmax(0, 1fr) 290px; gap: 16px; height: 440px; }

.sg-objects { display: flex; flex-direction: column; min-height: 0; border: 1px solid var(--nm-border); border-radius: var(--nm-radius); background: var(--ide-editor); }
.sg-objects-head { display: flex; align-items: center; gap: 8px; padding: 6px 8px; border-bottom: 1px solid var(--nm-border-soft); }
.sg-filter { flex: 1; }
.sg-list { flex: 1; overflow: auto; padding: 2px 0; }
.sg-list.disabled { opacity: 0.7; }
.sg-group { display: flex; align-items: center; gap: 2px; height: 26px; padding: 0 8px 0 2px; position: sticky; top: 0; background: var(--ide-sidebar); border-bottom: 1px solid var(--nm-border-soft); z-index: 1; }
.sg-caret { display: inline-flex; align-items: center; justify-content: center; width: 18px; height: 18px; border: none; background: transparent; color: var(--nm-text-dim); cursor: pointer; padding: 0; }
.sg-group-label { font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.05em; color: var(--nm-text); }
.sg-count { margin-left: auto; font-size: 11px; color: var(--nm-text-muted); font-variant-numeric: tabular-nums; }
.sg-item { display: flex; align-items: center; gap: 6px; height: 22px; padding: 0 8px 0 28px; cursor: pointer; }
.sg-item:hover { background: var(--ide-hover); }
.sg-item :deep(.el-checkbox) { height: 22px; }
.sg-name { font-size: 12.5px; color: var(--nm-text-dim); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.sg-item.on .sg-name { color: var(--nm-text); }
.sg-schema { color: var(--nm-text-muted); }
.sg-empty { padding: 24px; text-align: center; }
.sg-objects-foot { padding: 5px 10px; border-top: 1px solid var(--nm-border-soft); font-size: 11.5px; }

.sg-options { display: flex; flex-direction: column; align-items: flex-start; overflow: auto; padding-right: 4px; }
.sg-options .el-checkbox { height: 24px; margin-right: 0; }
.sg-options .nm-section-title { margin-bottom: 4px; }
.sg-gap { margin-top: 14px; }
.sg-indent { margin-left: 22px; }
.sg-limit { display: flex; align-items: center; gap: 8px; }
.sg-limit-input { width: 90px; }
.sg-dest { display: flex; flex-direction: column; align-items: flex-start; gap: 0; }
.sg-dest .el-radio { height: 24px; margin-right: 0; }
.sg-help { font-size: 11.5px; color: var(--nm-text-dim); line-height: 1.45; margin-top: 4px; }

.sg-progress { margin-top: 12px; display: flex; flex-direction: column; gap: 6px; }
.sg-progress-text { display: flex; align-items: center; gap: 8px; font-size: 12.5px; }
.sg-current { font-family: var(--nm-mono); font-size: 12px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.sg-counter { color: var(--nm-text-dim); font-variant-numeric: tabular-nums; }
.sg-body :deep(.el-checkbox__input.is-checked + .el-checkbox__label),
.sg-body :deep(.el-radio__input.is-checked + .el-radio__label) { color: var(--nm-text); }
.sg-footer { display: flex; align-items: center; gap: 8px; }
.sg-warn { font-size: 12px; color: var(--nm-warning); }
</style>
