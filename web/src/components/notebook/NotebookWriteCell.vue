<script setup lang="ts">
import { computed } from 'vue';
import { useTranslation } from 'i18next-vue';
import type { ColumnPair, NotebookCell, WriteCell, WriteMode, WritePlan, WriteTargetInfo } from '../../api/types';
import { locale } from '../../i18n';
import { tb } from '../../i18n/backend';
import { needsKey } from '../../composables/notebookModel';

// A write cell (docs/notebooks.md › Writing a DataFrame to a table): a
// small form, not code. Which DataFrame (a cell above), where (a
// connection or environment, a table: existing or new), how (insert,
// update by key, upsert, replace), the key, and which column goes where.
// "Vista previa" shows what would be written; running it always asks
// first (the view's confirmation), and its result is the counts.

/** A DataFrame column, as its cell's run (or stored preview) says. */
export interface FrameColumn { name: string; type: string }

const props = defineProps<{
  cell: NotebookCell;
  index: number;
  count: number;
  active: boolean;
  running: boolean;
  /** Some cell is running. */
  busy: boolean;
  /** The DataFrames it can write: the named SQL cells above. */
  frames: string[];
  /** The chosen DataFrame's columns (null: its cell hasn't run). */
  frameColumns: FrameColumn[] | null;
  /** "Ventas prod › ventas", or what's missing. */
  targetText: string;
  targetUnresolved: boolean;
  /** A production connection: shown in the danger color. */
  prod: boolean;
  /** The target engine's modes and the table's columns. */
  info: WriteTargetInfo | null;
  infoError: string | null;
  /** The target database's tables, for the picker. */
  tables: string[];
  /** The engine has schemas. */
  hasSchemas: boolean;
  /** The last preview (this session). */
  plan: WritePlan | null;
  planning: boolean;
  planError: string | null;
}>();
const emit = defineEmits<{
  activate: [];
  run: [];
  cancel: [];
  move: [by: -1 | 1];
  remove: [];
  change: [patch: Partial<WriteCell>];
  target: [];
  preview: [];
}>();
const { t } = useTranslation();

const w = computed<WriteCell>(() => props.cell.write ?? { source: null, target: null, schema: null, table: null, create: false, mode: 'insert', keys: [], mapping: null });
const MODES: WriteMode[] = ['insert', 'update', 'upsert', 'replace'];
/** A mode the target's engine writes (all of them until it's known). */
const modeOk = (m: WriteMode) => !props.info || props.info.modes.includes(m);

/** The table's columns, or the DataFrame's own names for a new table. */
const targetColumns = computed<string[]>(() => {
  if (w.value.create) return (props.frameColumns ?? []).map((c) => c.name);
  return props.info?.columns?.map((c) => c.name) ?? [];
});
const targetType = (name: string) => props.info?.columns?.find((c) => c.name === name)?.data_type ?? '';
/** Writes a field of any name (documents, keys…). */
const free = computed(() => !!props.info?.schemaless || w.value.create);

/** Each DataFrame column's target: as mapped, else by name (as the backend pairs them). */
const pairs = computed<ColumnPair[]>(() => {
  const cols = props.frameColumns ?? [];
  if (w.value.mapping) return w.value.mapping;
  const targets = targetColumns.value;
  return cols.flatMap((c) => {
    const tc = targets.find((x) => x.toLowerCase() === c.name.toLowerCase());
    return tc ? [{ source: c.name, target: tc }] : free.value || !targets.length ? [{ source: c.name, target: c.name }] : [];
  });
});
const targetOf = (source: string) => pairs.value.find((p) => p.source === source)?.target ?? '';
const unmatchedTargets = computed(() => (w.value.create ? [] : targetColumns.value.filter((c) => !pairs.value.some((p) => p.target === c))));
/** A target column picked in the mapping: `''` leaves the DataFrame column out. */
function mapColumn(source: string, target: string) {
  const next = (props.frameColumns ?? []).flatMap((c) => {
    const to = c.name === source ? target : targetOf(c.name);
    return to ? [{ source: c.name, target: to }] : [];
  });
  emit('change', { mapping: next });
}
/** Back to pairing by name. */
const byName = () => emit('change', { mapping: null });

/** The key's options: the target columns that are written. */
const keyOptions = computed(() => pairs.value.map((p) => p.target));

function setMode(m: WriteMode) {
  emit('change', needsKey(m) ? { mode: m } : { mode: m, keys: [] });
}

const counter = computed(() => (props.running ? '*' : props.cell.execution_count ?? ' '));
const n = (x: number) => x.toLocaleString(locale());
function duration(ms: number): string {
  if (ms < 1000) return t('notebook:summary.ms', { ms: n(ms) });
  const s = ms / 1000;
  if (s < 60) return t('notebook:summary.seconds', { s: s < 10 ? s.toLocaleString(locale(), { maximumFractionDigits: 1 }) : n(Math.round(s)) });
  return t('notebook:summary.minutes', { m: n(Math.floor(s / 60)), s: n(Math.round(s % 60)) });
}
const result = computed(() => props.cell.write_result ?? null);
const resultLine = computed(() => {
  const r = result.value;
  if (!r) return '';
  const parts: string[] = [];
  if (!r.error) {
    parts.push(t('notebook:write.result.inserted', { count: r.inserted, rows: n(r.inserted) }));
    parts.push(t('notebook:write.result.updated', { count: r.updated, rows: n(r.updated) }));
    if (r.deleted) parts.push(t('notebook:write.result.deleted', { count: r.deleted, rows: n(r.deleted) }));
  }
  const at = new Date(r.ran_at);
  if (!Number.isNaN(at.getTime())) parts.push(at.toLocaleString(locale(), { day: '2-digit', month: '2-digit', hour: '2-digit', minute: '2-digit' }));
  parts.push(duration(r.duration_ms));
  if (r.target_label) parts.push(r.target_label);
  return parts.join(' · ');
});
const headline = computed(() => {
  const where = [props.targetText, [w.value.schema, w.value.table].filter(Boolean).join('.')].filter(Boolean).join(' › ');
  return `${w.value.source || '?'} → ${where || '?'}`;
});
const previewRows = computed(() => props.plan?.preview.rows.slice(0, 8) ?? []);
const cellText = (v: unknown) => (v === null || v === undefined ? 'NULL' : typeof v === 'object' ? JSON.stringify(v) : String(v));
</script>

<template>
  <div class="nc write" :class="{ active, running }" @focusin="emit('activate')" @mousedown="emit('activate')">
    <div class="nc-gutter">
      <span class="nc-count" :title="$t('notebook:cell.countTip')">[{{ counter }}]</span>
      <button v-if="!running" class="nc-play" :disabled="busy" :title="$t('notebook:write.runTip')" :aria-label="$t('notebook:write.run')" @click="emit('run')">
        <el-icon><ei-video-play /></el-icon>
      </button>
      <button v-else class="nc-play stop" :title="$t('common:cancel')" :aria-label="$t('common:cancel')" @click="emit('cancel')">
        <el-icon><ei-video-pause /></el-icon>
      </button>
    </div>

    <div class="nc-main">
      <div class="nc-bar nw-bar">
        <span class="nw-badge"><el-icon><ei-upload /></el-icon>{{ $t('notebook:write.badge') }}</span>
        <span class="nw-head" :title="headline">{{ headline }}</span>
        <button class="nc-btn nw-preview-btn" :disabled="busy || planning" :title="$t('notebook:write.previewTip')" @click="emit('preview')">
          <el-icon><ei-view /></el-icon><span>{{ $t('notebook:write.preview') }}</span>
        </button>
        <button class="nc-btn" :disabled="index === 0" :title="$t('notebook:cell.moveUp')" @click="emit('move', -1)"><el-icon><ei-arrow-up /></el-icon></button>
        <button class="nc-btn" :disabled="index === count - 1" :title="$t('notebook:cell.moveDown')" @click="emit('move', 1)"><el-icon><ei-arrow-down /></el-icon></button>
        <el-dropdown trigger="click" placement="bottom-end" @command="() => emit('remove')">
          <button class="nc-btn" :title="$t('notebook:cell.more')" :aria-label="$t('notebook:cell.more')"><el-icon><ei-more-filled /></el-icon></button>
          <template #dropdown>
            <el-dropdown-menu><el-dropdown-item command="remove">{{ $t('notebook:cell.delete') }}</el-dropdown-item></el-dropdown-menu>
          </template>
        </el-dropdown>
      </div>

      <div class="nw-form">
        <label class="nw-lbl" for="nw-source">{{ $t('notebook:write.source') }}</label>
        <div class="nw-val">
          <el-select id="nw-source" :model-value="w.source ?? ''" size="small" class="nw-source" :placeholder="$t('notebook:write.sourcePlaceholder')" :no-data-text="$t('notebook:write.noFrames')" @update:model-value="(v: string) => emit('change', { source: v || null, mapping: null, keys: [] })">
            <el-option v-for="f in frames" :key="f" :value="f" :label="f"><code>{{ f }}</code></el-option>
          </el-select>
          <span v-if="w.source && !frameColumns" class="nw-dim">{{ $t('notebook:write.frameNotRun') }}</span>
        </div>

        <span class="nw-lbl">{{ $t('notebook:write.target') }}</span>
        <div class="nw-val">
          <button class="nw-target" :class="{ warn: targetUnresolved, prod }" :title="$t('notebook:write.targetTip')" @click="emit('target')">
            <el-icon><ei-coin /></el-icon><span>{{ targetText }}</span><span v-if="prod" class="nw-prod">PROD</span>
          </button>
          <el-input v-if="hasSchemas" :model-value="w.schema ?? ''" size="small" class="nw-schema" :placeholder="$t('notebook:write.schema')" :aria-label="$t('notebook:write.schema')" @change="(v: string) => emit('change', { schema: v.trim() || null })" />
          <el-select
            :model-value="w.table ?? ''"
            size="small"
            filterable
            allow-create
            default-first-option
            class="nw-table"
            :placeholder="w.create ? $t('notebook:write.newTablePlaceholder') : $t('notebook:write.tablePlaceholder')"
            :aria-label="$t('notebook:write.table')"
            @update:model-value="(v: string) => emit('change', { table: v.trim() || null, mapping: null, keys: [] })"
          >
            <el-option v-for="tbl in tables" :key="tbl" :value="tbl" :label="tbl" />
          </el-select>
          <el-checkbox :model-value="w.create" size="small" @update:model-value="(v: boolean | string | number) => emit('change', { create: !!v, mapping: null })">{{ $t('notebook:write.create') }}</el-checkbox>
        </div>

        <span class="nw-lbl">{{ $t('notebook:write.mode') }}</span>
        <div class="nw-val">
          <el-radio-group :model-value="w.mode" size="small" @update:model-value="(v: string | number | boolean | undefined) => setMode(v as WriteMode)">
            <el-tooltip v-for="m in MODES" :key="m" :content="modeOk(m) ? $t(`notebook:write.modes.${m}Tip`) : $t('notebook:write.modeUnsupported')" placement="top" :show-after="400">
              <el-radio-button :value="m" :disabled="!modeOk(m)">{{ $t(`notebook:write.modes.${m}`) }}</el-radio-button>
            </el-tooltip>
          </el-radio-group>
        </div>

        <template v-if="needsKey(w.mode)">
          <label class="nw-lbl" for="nw-keys">{{ $t('notebook:write.keys') }}</label>
          <div class="nw-val">
            <el-select id="nw-keys" :model-value="w.keys" multiple size="small" class="nw-keys" :placeholder="$t('notebook:write.keysPlaceholder')" @update:model-value="(v: string[]) => emit('change', { keys: v })">
              <el-option v-for="k in keyOptions" :key="k" :value="k" :label="k" />
            </el-select>
            <span class="nw-dim">{{ $t('notebook:write.keysHelp') }}</span>
          </div>
        </template>

        <span class="nw-lbl top">{{ $t('notebook:write.columns') }}</span>
        <div class="nw-val nw-cols">
          <div v-if="!frameColumns" class="nw-dim">{{ $t('notebook:write.columnsUnknown') }}</div>
          <table v-else class="nw-map">
            <thead><tr><th>{{ $t('notebook:write.frameColumn') }}</th><th /><th>{{ w.create ? $t('notebook:write.newColumn') : $t('notebook:write.tableColumn') }}</th></tr></thead>
            <tbody>
              <tr v-for="c in frameColumns" :key="c.name" :class="{ skip: !targetOf(c.name) }">
                <td><code>{{ c.name }}</code> <span class="nw-type">{{ c.type }}</span></td>
                <td class="nw-arrow">→</td>
                <td>
                  <el-select :model-value="targetOf(c.name)" size="small" class="nw-col" filterable :allow-create="free" :placeholder="$t('notebook:write.skip')" :aria-label="c.name" @update:model-value="(v: string) => mapColumn(c.name, v)">
                    <el-option value="" :label="$t('notebook:write.skip')" />
                    <el-option v-for="tc in (w.create ? [c.name] : targetColumns)" :key="tc" :value="tc" :label="tc">
                      <span>{{ tc }}</span><span class="nw-type"> {{ targetType(tc) }}</span>
                    </el-option>
                  </el-select>
                </td>
              </tr>
            </tbody>
          </table>
          <div v-if="unmatchedTargets.length" class="nw-dim">{{ $t('notebook:write.unmatchedTarget', { columns: unmatchedTargets.join(', ') }) }}</div>
          <button v-if="w.mapping" class="nw-link" @click="byName">{{ $t('notebook:write.byName') }}</button>
        </div>
      </div>

      <div v-if="infoError" class="nw-error nm-selectable">{{ tb(infoError) }}</div>

      <!-- What would be written (nothing is, until it runs and is confirmed). -->
      <div v-if="planning" class="nw-box nm-muted"><el-icon class="is-loading"><ei-loading /></el-icon> {{ $t('notebook:write.planning') }}</div>
      <div v-else-if="planError" class="nw-error nm-selectable">{{ tb(planError) }}</div>
      <div v-else-if="plan" class="nw-box nw-plan">
        <div class="nw-plan-head">
          <strong>{{ $t('notebook:write.previewTitle', { count: plan.rows, rows: n(plan.rows) }) }}</strong>
          <span class="nw-dim">{{ plan.target_label }}</span>
        </div>
        <div class="nw-conv">
          <span v-for="c in plan.columns" :key="c.source" class="nw-chip" :class="{ conv: c.converted }" :title="c.converted ? $t('notebook:write.convertedTip') : undefined">
            <code>{{ c.source }}</code> <span class="nw-type">{{ c.source_type }}</span> → <code>{{ c.target }}</code> <span class="nw-type">{{ c.target_type || '—' }}</span>
          </span>
        </div>
        <ul v-if="plan.warnings.length" class="nw-warn">
          <li v-for="(x, i) in plan.warnings" :key="i">{{ tb(x) }}</li>
        </ul>
        <pre v-if="plan.create" class="nw-ddl nm-selectable">{{ plan.create }}</pre>
        <div v-if="previewRows.length" class="nw-scroll">
          <table class="nw-rows">
            <thead><tr><th v-for="c in plan.preview.columns" :key="c">{{ c }}</th></tr></thead>
            <tbody><tr v-for="(r, i) in previewRows" :key="i"><td v-for="(v, j) in r" :key="j" :class="{ null: v === null }">{{ cellText(v) }}</td></tr></tbody>
          </table>
        </div>
        <div v-if="plan.rows > previewRows.length" class="nw-dim">{{ $t('notebook:write.previewMore', { shown: n(previewRows.length), total: n(plan.rows) }) }}</div>
      </div>

      <!-- Its last run. -->
      <div v-if="running" class="nw-box nm-muted"><el-icon class="is-loading"><ei-loading /></el-icon> {{ $t('notebook:write.writing') }}</div>
      <div v-else-if="result" class="nw-result" :class="{ err: !!result.error }">
        <div class="nw-result-line">
          <el-icon v-if="result.error"><ei-circle-close-filled /></el-icon>
          <el-icon v-else class="ok"><ei-circle-check-filled /></el-icon>
          <span class="nm-selectable">{{ resultLine }}</span>
        </div>
        <div v-if="result.error" class="nw-error nm-selectable">{{ tb(result.error) }}</div>
        <ul v-if="result.warnings?.length" class="nw-warn">
          <li v-for="(x, i) in result.warnings" :key="i">{{ tb(x) }}</li>
        </ul>
      </div>
    </div>
  </div>
</template>

<style scoped>
.nc { position: relative; display: flex; border: 1px solid transparent; border-radius: 4px; }
.nc:hover { border-color: var(--nm-border-soft); }
.nc.active { border-color: var(--nm-border); }
.nc.active::before { content: ''; position: absolute; left: -1px; top: -1px; bottom: -1px; width: 3px; border-radius: 3px 0 0 3px; background: var(--ide-focus, var(--nm-accent)); }
.nc-gutter { flex: none; width: 52px; display: flex; flex-direction: column; align-items: center; gap: 4px; padding-top: 8px; }
.nc-count { font-family: var(--nm-mono); font-size: 11.5px; color: var(--nm-text-dim); white-space: pre; }
.nc.running .nc-count { color: var(--nm-accent); }
.nc-play { display: inline-flex; align-items: center; justify-content: center; width: 22px; height: 22px; border: 0; border-radius: 50%; background: none; color: var(--nm-text-dim); cursor: pointer; }
.nc-play:hover:not(:disabled) { background: var(--ide-hover); color: var(--nm-success); }
.nc-play.stop { color: var(--nm-danger); }
.nc-play:disabled { opacity: 0.4; cursor: default; }
.nc-main { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 6px; padding: 4px 6px 6px 0; }
.nw-bar { display: flex; align-items: center; gap: 4px; min-height: 22px; font-size: 11.5px; }
.nw-badge {
  display: inline-flex; align-items: center; gap: 4px; flex-shrink: 0; padding: 0 7px; line-height: 18px; border-radius: 3px;
  font-size: 11px; font-weight: 600; color: var(--nm-accent); border: 1px solid color-mix(in srgb, var(--nm-accent) 45%, transparent);
  background: color-mix(in srgb, var(--nm-accent) 10%, transparent);
}
.nw-head { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--nm-text-dim); font-family: var(--nm-mono); font-size: 11px; }
.nc-btn { display: inline-flex; align-items: center; justify-content: center; gap: 3px; min-width: 22px; height: 20px; padding: 0 3px; border: 0; border-radius: 3px; background: none; font: inherit; color: var(--nm-text-dim); cursor: pointer; }
.nc-btn:hover:not(:disabled) { background: var(--ide-hover); color: var(--nm-text-strong); }
.nc-btn:disabled { opacity: 0.35; cursor: default; }
.nw-preview-btn { padding: 0 6px; }
.nw-form {
  display: grid; grid-template-columns: max-content minmax(0, 1fr); gap: 7px 12px; align-items: center; padding: 10px 12px;
  border: 1px solid var(--nm-border-soft); border-radius: 4px; background: var(--nm-bg-elev, var(--nm-bg));
}
.nw-lbl { font-size: 12px; color: var(--nm-text-dim); }
.nw-lbl.top { align-self: start; padding-top: 2px; }
.nw-val { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; min-width: 0; }
.nw-source { width: 220px; }
.nw-schema { width: 110px; }
.nw-table { width: 230px; }
.nw-keys { min-width: 220px; max-width: 420px; }
.nw-target {
  display: inline-flex; align-items: center; gap: 5px; max-width: 340px; height: 24px; padding: 0 8px; overflow: hidden;
  border: 1px solid var(--nm-border); border-radius: 4px; background: none; font: inherit; font-size: 12px; color: var(--nm-text); cursor: pointer;
}
.nw-target > span:first-of-type { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.nw-target:hover { border-color: var(--nm-border-hover); color: var(--nm-text-strong); }
.nw-target.warn { color: var(--nm-warning); }
.nw-target.prod { border-color: color-mix(in srgb, var(--nm-danger) 60%, transparent); background: color-mix(in srgb, var(--nm-danger) 12%, transparent); }
.nw-prod { font-size: 9.5px; font-weight: 700; padding: 0 4px; border-radius: 3px; color: #fff; background: var(--nm-danger); }
.nw-dim { font-size: 12px; color: var(--nm-text-dim); }
.nw-type { font-size: 11px; color: var(--nm-text-muted); }
.nw-cols { flex-direction: column; align-items: flex-start; gap: 6px; }
.nw-map { border-collapse: collapse; font-size: 12px; }
.nw-map th { padding: 0 8px 4px 0; text-align: left; font-weight: 500; color: var(--nm-text-dim); }
.nw-map td { padding: 2px 8px 2px 0; }
.nw-map code { font-family: var(--nm-mono); font-size: 11.5px; }
.nw-map tr.skip td:first-child { opacity: 0.55; text-decoration: line-through; }
.nw-arrow { color: var(--nm-text-muted); }
.nw-col { width: 200px; }
.nw-link { border: 0; padding: 0; background: none; font: inherit; font-size: 12px; color: var(--nm-accent); cursor: pointer; }
.nw-link:hover { text-decoration: underline; }
.nw-box { padding: 8px 12px; border: 1px solid var(--nm-border-soft); border-radius: 4px; font-size: 12px; }
.nw-plan { display: flex; flex-direction: column; gap: 6px; }
.nw-plan-head { display: flex; align-items: baseline; gap: 10px; flex-wrap: wrap; color: var(--nm-text-strong); }
.nw-conv { display: flex; flex-wrap: wrap; gap: 4px 6px; }
.nw-chip { padding: 1px 6px; border-radius: 3px; border: 1px solid var(--nm-border-soft); font-size: 11.5px; }
.nw-chip code { font-family: var(--nm-mono); font-size: 11px; }
.nw-chip.conv { border-color: color-mix(in srgb, var(--nm-warning) 55%, transparent); background: color-mix(in srgb, var(--nm-warning) 10%, transparent); }
.nw-warn { margin: 0; padding-left: 18px; color: var(--nm-warning); font-size: 12px; }
.nw-ddl { margin: 0; padding: 6px 10px; max-height: 160px; overflow: auto; border-radius: 3px; font-family: var(--nm-mono); font-size: 11.5px; background: var(--nm-overlay-code); white-space: pre-wrap; }
.nw-scroll { overflow: auto; max-height: 220px; border: 1px solid var(--nm-border-soft); border-radius: 3px; }
.nw-rows { border-collapse: collapse; font-size: 12px; font-family: var(--nm-mono); }
.nw-rows th, .nw-rows td { padding: 2px 10px; border-bottom: 1px solid var(--nm-border-soft); text-align: left; white-space: nowrap; }
.nw-rows th { position: sticky; top: 0; background: var(--nm-bg-elev, var(--nm-bg)); font-weight: 600; color: var(--nm-text-strong); }
.nw-rows td.null { color: var(--nm-text-muted); font-style: italic; }
.nw-result { display: flex; flex-direction: column; gap: 4px; padding: 6px 10px; border-radius: 4px; font-size: 12px; border: 1px solid var(--nm-border-soft); }
.nw-result-line { display: flex; align-items: center; gap: 6px; color: var(--nm-text); }
.nw-result-line .ok { color: var(--nm-success); }
.nw-result.err { border-color: color-mix(in srgb, var(--nm-danger) 50%, transparent); }
.nw-result.err .nw-result-line .el-icon { color: var(--nm-danger); }
.nw-error { padding: 6px 10px; border-radius: 3px; font-size: 12px; white-space: pre-wrap; color: var(--nm-text); border: 1px solid color-mix(in srgb, var(--nm-danger) 50%, transparent); background: color-mix(in srgb, var(--nm-danger) 10%, transparent); }
</style>
