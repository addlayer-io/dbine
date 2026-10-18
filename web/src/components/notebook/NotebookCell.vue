<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue';
import type { Language, LoadedTable, NotebookCell, QueryOutcome } from '../../api/types';
import type { Snippet } from '../../composables/snippets';
import { renderMarkdown } from '../../composables/markdown';
import { onMarkdownClick } from '../../composables/externalLink';
import CodeEditor from '../CodeEditor.vue';
import NotebookResult from './NotebookResult.vue';

// One cell of a notebook: SQL (the query editor, its result under it),
// text (Markdown, rendered; double click to edit) or a cell from another
// tool (Python, R…: read-only, greyed, with what it printed). The toolbar
// shows on hover and on the active cell.

/** What the cell's editor needs from its target (the view resolves it). */
export interface CellContext {
  language?: Language;
  dialect?: string;
  schema: Record<string, string[]>;
  snippets: Snippet[];
  /** "Ventas prod › ventas", or what's missing. */
  target: string;
  /** The cell has a target of its own. */
  overridden: boolean;
  /** The target can't be resolved yet (shown in warning color). */
  unresolved: boolean;
}

const props = defineProps<{
  cell: NotebookCell;
  index: number;
  count: number;
  active: boolean;
  running: boolean;
  /** Some cell is running (actions that run wait). */
  busy: boolean;
  editing: boolean;
  ctx: CellContext;
  live: QueryOutcome | null;
  fallback: QueryOutcome | null;
  source: { connectionId: string; database: string; sql: string } | null;
  title: string;
  /** A "Resultados del notebook" cell's run here: the tables it loaded. */
  loaded?: LoadedTable[] | null;
}>();
const emit = defineEmits<{
  activate: [];
  source: [text: string];
  /** `sql`: the selection (or null: the whole cell); `next`: then go to the next cell. */
  run: [sql: string | null, next: boolean];
  runFrom: [];
  cancel: [];
  move: [by: -1 | 1];
  remove: [];
  kind: [kind: 'sql' | 'markdown'];
  target: [];
  keep: [];
  explain: [];
  /** The result's view or chart changed (the cell's `chart`). */
  chart: [chart: Record<string, unknown>];
  propose: [];
  group: [];
  edit: [on: boolean];
  /** Shift+Enter in a text cell: render it and go to the next one. */
  next: [];
  save: [];
  needColumns: [path: string[]];
  needNames: [];
  /** A new name for the cell (the view checks it). */
  rename: [name: string];
}>();

const editor = ref<InstanceType<typeof CodeEditor> | null>(null);
const textarea = ref<{ focus: () => void } | null>(null);
const root = ref<HTMLElement | null>(null);

defineExpose({
  focus: () => {
    if (props.cell.kind === 'sql') editor.value?.focus();
    else if (props.cell.kind === 'markdown') { if (!props.editing) emit('edit', true); void nextTick(() => textarea.value?.focus()); }
  },
  selectionText: () => editor.value?.selectionText() ?? '',
  replaceAll: (code: string) => { if (editor.value) editor.value.replaceAll(code); else emit('source', code); },
  /** Scrolls just enough for the cell's result to show (its top first when taller than the view). */
  revealResult: () => {
    const el = root.value?.querySelector<HTMLElement>('.nr');
    if (!el) return;
    const view = el.closest<HTMLElement>('.nb-body');
    // Taller than the view: its top; else just enough for all of it.
    el.scrollIntoView({ block: view && el.offsetHeight > view.clientHeight ? 'start' : 'nearest' });
  },
});

const html = computed(() => (props.cell.kind === 'markdown' ? renderMarkdown(props.cell.source) : ''));
watch(() => props.editing, (on) => { if (on) void nextTick(() => textarea.value?.focus()); });

/** Shift+Enter: run (or render) and go on; before CodeMirror sees it. */
function onKeyCapture(e: KeyboardEvent) {
  if (e.key !== 'Enter' || !e.shiftKey || e.metaKey || e.ctrlKey || e.altKey || e.isComposing) return;
  if (props.cell.kind === 'foreign') return;
  e.preventDefault();
  e.stopPropagation();
  if (props.cell.kind === 'sql') emit('run', null, true);
  else emit('next');
}
/** A text cell: Ctrl/⌘+Enter renders it. */
function onTextKey(e: KeyboardEvent) {
  if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) { e.preventDefault(); emit('edit', false); }
  else if (e.key === 'Escape') { e.preventDefault(); emit('edit', false); }
  else if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 's') { e.preventDefault(); emit('save'); }
}

/** A foreign cell's stored output, as text (streams, plain results, errors). */
const foreignOutput = computed(() => {
  if (props.cell.kind !== 'foreign') return '';
  const raw = props.cell.raw as { outputs?: Record<string, unknown>[] } | null;
  const join = (v: unknown) => (Array.isArray(v) ? v.join('') : typeof v === 'string' ? v : '');
  return (raw?.outputs ?? []).map((o) => {
    if (o.output_type === 'stream') return join(o.text);
    if (o.output_type === 'error') return `${o.ename ?? ''}: ${o.evalue ?? ''}`;
    const data = o.data as Record<string, unknown> | undefined;
    return data ? join(data['text/plain']) : '';
  }).filter(Boolean).join('\n');
});

const counter = computed(() => (props.running ? '*' : props.cell.execution_count ?? ' '));

// The cell's name (its table for "Resultados del notebook" cells): click to edit.
const naming = ref(false);
const nameDraft = ref('');
const nameInput = ref<{ focus: () => void; select?: () => void } | null>(null);
function startRename() {
  nameDraft.value = props.cell.name ?? '';
  naming.value = true;
  void nextTick(() => { nameInput.value?.focus(); nameInput.value?.select?.(); });
}
function endRename(save: boolean) {
  if (!naming.value) return;
  naming.value = false;
  const v = nameDraft.value.trim();
  if (save && v && v !== props.cell.name) emit('rename', v);
}
</script>

<template>
  <div
    ref="root"
    class="nc"
    :class="[cell.kind, { active, running }]"
    @focusin="emit('activate')"
    @mousedown="emit('activate')"
    @keydown.capture="onKeyCapture"
  >
    <div class="nc-gutter">
      <span v-if="cell.kind === 'sql'" class="nc-count" :title="$t('notebook:cell.countTip')">[{{ counter }}]</span>
      <button v-if="cell.kind === 'sql' && !running" class="nc-play" :disabled="busy" :title="$t('notebook:cell.runTip')" :aria-label="$t('notebook:cell.run')" @click="emit('run', null, false)">
        <el-icon><ei-video-play /></el-icon>
      </button>
      <button v-else-if="running" class="nc-play stop" :title="$t('common:cancel')" :aria-label="$t('common:cancel')" @click="emit('cancel')">
        <el-icon><ei-video-pause /></el-icon>
      </button>
    </div>

    <div class="nc-main">
      <!-- The cell's actions: on hover and on the active cell. -->
      <div class="nc-bar">
        <template v-if="cell.kind === 'sql'">
          <el-input
            v-if="naming"
            ref="nameInput"
            v-model="nameDraft"
            size="small"
            class="nc-name-input"
            :aria-label="$t('notebook:name.label')"
            @keydown.enter.prevent="endRename(true)"
            @keydown.esc.prevent="endRename(false)"
            @blur="endRename(true)"
          />
          <button v-else class="nc-name" :title="$t('notebook:name.tip')" @click="startRename">{{ cell.name || '—' }}</button>
        </template>
        <button v-if="cell.kind === 'sql'" class="nc-target" :class="{ own: ctx.overridden, warn: ctx.unresolved, results: !!cell.target?.results }" :title="$t('notebook:cell.targetTip')" @click="emit('target')">
          <el-icon><ei-coin /></el-icon>{{ ctx.target }}
        </button>
        <span v-if="cell.kind === 'foreign'" class="nc-lang">{{ cell.language || '?' }}</span>
        <button v-if="cell.kind === 'sql'" class="nc-btn" :disabled="busy" :title="$t('notebook:cell.runFrom')" @click="emit('runFrom')"><el-icon><ei-d-arrow-right /></el-icon></button>
        <button v-if="cell.kind === 'markdown' && !editing" class="nc-btn" :title="$t('notebook:cell.edit')" @click="emit('edit', true)"><el-icon><ei-edit /></el-icon></button>
        <button v-if="cell.kind === 'markdown' && editing" class="nc-btn" :title="$t('notebook:cell.render')" @click="emit('edit', false)"><el-icon><ei-check /></el-icon></button>
        <button class="nc-btn" :disabled="index === 0" :title="$t('notebook:cell.moveUp')" @click="emit('move', -1)"><el-icon><ei-arrow-up /></el-icon></button>
        <button class="nc-btn" :disabled="index === count - 1" :title="$t('notebook:cell.moveDown')" @click="emit('move', 1)"><el-icon><ei-arrow-down /></el-icon></button>
        <el-dropdown trigger="click" placement="bottom-end" @command="(c: string) => c === 'sql' || c === 'markdown' ? emit('kind', c) : c === 'keep' ? emit('keep') : c === 'explain' ? emit('explain') : c === 'propose' ? emit('propose') : c === 'target' ? emit('target') : emit('remove')">
          <button class="nc-btn" :title="$t('notebook:cell.more')" :aria-label="$t('notebook:cell.more')"><el-icon><ei-more-filled /></el-icon></button>
          <template #dropdown>
            <el-dropdown-menu>
              <el-dropdown-item v-if="cell.kind === 'markdown'" command="sql">{{ $t('notebook:cell.toSql') }}</el-dropdown-item>
              <el-dropdown-item v-if="cell.kind === 'sql'" command="markdown">{{ $t('notebook:cell.toText') }}</el-dropdown-item>
              <el-dropdown-item v-if="cell.kind === 'sql'" command="target">{{ $t('notebook:cell.target') }}</el-dropdown-item>
              <el-dropdown-item v-if="cell.kind === 'sql'" command="keep">
                <span class="nc-check"><el-icon v-if="!cell.keep_result"><ei-check /></el-icon></span>{{ $t('notebook:cell.dontKeep') }}
              </el-dropdown-item>
              <el-dropdown-item v-if="cell.kind === 'sql'" command="explain" divided :disabled="!cell.result">{{ $t('notebook:ai.explain') }}</el-dropdown-item>
              <el-dropdown-item command="propose" :divided="cell.kind !== 'sql'">{{ $t('notebook:ai.propose') }}</el-dropdown-item>
              <el-dropdown-item command="remove" divided>{{ $t('notebook:cell.delete') }}</el-dropdown-item>
            </el-dropdown-menu>
          </template>
        </el-dropdown>
      </div>

      <!-- SQL -->
      <template v-if="cell.kind === 'sql'">
        <div class="nc-editor">
          <CodeEditor
            ref="editor"
            :model-value="cell.source"
            :language="ctx.language"
            :dialect="ctx.dialect"
            :schema="ctx.schema"
            :snippets="ctx.snippets"
            :placeholder="$t('notebook:cell.sqlPlaceholder')"
            run-actions
            @update:model-value="(v: string) => emit('source', v)"
            @run="(text: string) => emit('run', text.trim() === cell.source.trim() ? null : text, false)"
            @run-statement="() => emit('run', null, false)"
            @save="emit('save')"
            @need-columns="(p: string[]) => emit('needColumns', p)"
            @need-names="emit('needNames')"
          />
        </div>
        <NotebookResult
          v-if="cell.result || running"
          :result="cell.result"
          :live="live"
          :fallback="fallback"
          :running="running"
          :dropped="!cell.keep_result"
          :source="source"
          :dialect="ctx.dialect ?? ''"
          :title="title"
          :chart="cell.chart"
          :loaded="loaded ?? null"
          @group="emit('group')"
          @explain="emit('explain')"
          @chart="(c: Record<string, unknown>) => emit('chart', c)"
        />
      </template>

      <!-- Text (Markdown) -->
      <template v-else-if="cell.kind === 'markdown'">
        <el-input
          v-if="editing"
          ref="textarea"
          class="nc-text"
          type="textarea"
          :model-value="cell.source"
          :autosize="{ minRows: 2 }"
          resize="none"
          :placeholder="$t('notebook:cell.textPlaceholder')"
          @update:model-value="(v: string) => emit('source', v)"
          @keydown="onTextKey"
          @blur="emit('edit', false)"
        />
        <!-- Escaped first (composables/markdown.ts): no raw HTML. -->
        <div v-else-if="cell.source.trim()" class="nc-md nm-selectable" :title="$t('notebook:cell.dblClickEdit')" @dblclick="emit('edit', true)" @click="onMarkdownClick" v-html="html" />
        <div v-else class="nc-md empty" @dblclick="emit('edit', true)">{{ $t('notebook:cell.emptyText') }}</div>
      </template>

      <!-- From another tool: kept as it is, never run -->
      <template v-else>
        <pre class="nc-foreign nm-selectable" :title="$t('notebook:cell.foreignTip', { language: cell.language || '?' })">{{ cell.source }}</pre>
        <pre v-if="foreignOutput" class="nc-foreign-out nm-selectable">{{ foreignOutput }}</pre>
      </template>
    </div>
  </div>
</template>

<style scoped>
.nc { position: relative; display: flex; gap: 0; border: 1px solid transparent; border-radius: 4px; }
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
.nc-main { flex: 1; min-width: 0; display: flex; flex-direction: column; padding: 4px 6px 4px 0; }
.nc-bar {
  display: flex; align-items: center; gap: 2px; min-height: 22px; padding: 0 0 2px; font-size: 11.5px;
  visibility: hidden;
}
.nc:hover .nc-bar, .nc.active .nc-bar, .nc-bar:focus-within { visibility: visible; }
.nc-bar > .nc-target { margin-right: auto; }
.nc-target {
  display: inline-flex; align-items: center; gap: 4px; max-width: 60%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  border: 0; padding: 1px 6px; border-radius: 3px; background: none; font: inherit; color: var(--nm-text-dim); cursor: pointer;
}
.nc-target:hover { background: var(--ide-hover); color: var(--nm-text-strong); }
.nc-target.results { color: var(--nm-accent); }
.nc-name {
  flex-shrink: 0; max-width: 180px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; margin-right: 2px;
  border: 1px solid var(--nm-border-soft); padding: 0 6px; border-radius: 3px; line-height: 16px; background: none;
  font-family: var(--nm-mono); font-size: 11px; color: var(--nm-text-dim); cursor: text;
}
.nc-name:hover { border-color: var(--nm-border-hover); color: var(--nm-text-strong); }
.nc-name-input { width: 160px; margin-right: 2px; }
.nc-name-input :deep(input) { font-family: var(--nm-mono); font-size: 11px; }
.nc-target.own { color: var(--nm-text-strong); }
.nc-target.warn { color: var(--nm-warning); }
.nc.sql .nc-bar { visibility: visible; }
.nc.sql:not(:hover):not(.active) .nc-bar > :not(.nc-target):not(.nc-name) { visibility: hidden; }
.nc-lang { margin-right: auto; padding: 0 6px; border-radius: 8px; line-height: 16px; border: 1px solid var(--nm-border); color: var(--nm-text-dim); text-transform: lowercase; }
.nc-btn { display: inline-flex; align-items: center; justify-content: center; width: 22px; height: 20px; border: 0; border-radius: 3px; background: none; color: var(--nm-text-dim); cursor: pointer; }
.nc-btn:hover:not(:disabled) { background: var(--ide-hover); color: var(--nm-text-strong); }
.nc-btn:disabled { opacity: 0.35; cursor: default; }
.nc-check { display: inline-flex; width: 16px; }
.nc-editor { border: 1px solid var(--nm-border-soft); border-radius: 3px; overflow: hidden; }
.nc-editor :deep(.ce) { height: auto; }
.nc-editor :deep(.cm-editor) { height: auto; min-height: 38px; max-height: 520px; }
.nc-editor :deep(.cm-scroller) { overflow: auto; }
.nc-text :deep(textarea) { font-family: var(--nm-mono); font-size: var(--nm-editor-font-size); line-height: 1.5; }
.nc-md { padding: 2px 4px; min-height: 22px; line-height: 1.55; color: var(--nm-text); overflow-wrap: anywhere; }
.nc-md.empty { color: var(--nm-text-muted); font-style: italic; cursor: text; }
.nc-md :deep(h1), .nc-md :deep(h2), .nc-md :deep(h3), .nc-md :deep(h4), .nc-md :deep(h5), .nc-md :deep(h6) { margin: 6px 0 6px; color: var(--nm-text-strong); font-weight: 600; line-height: 1.3; }
.nc-md :deep(h1) { font-size: 1.6em; }
.nc-md :deep(h2) { font-size: 1.35em; }
.nc-md :deep(h3) { font-size: 1.15em; }
.nc-md :deep(h4), .nc-md :deep(h5), .nc-md :deep(h6) { font-size: 1em; }
.nc-md :deep(p) { margin: 0 0 8px; }
.nc-md :deep(ul), .nc-md :deep(ol) { margin: 0 0 8px; padding-left: 22px; }
.nc-md :deep(code) { font-family: var(--nm-mono); font-size: 0.92em; padding: 0 4px; border-radius: 3px; background: var(--nm-overlay-code); }
.nc-md :deep(pre) { margin: 0 0 8px; padding: 8px 10px; overflow: auto; border-radius: 4px; background: var(--nm-overlay-code); }
.nc-md :deep(pre code) { padding: 0; background: none; }
.nc-md :deep(blockquote) { margin: 0 0 8px; padding: 2px 10px; border-left: 3px solid var(--nm-border); color: var(--nm-text-dim); }
.nc-md :deep(hr) { border: 0; border-top: 1px solid var(--nm-border); margin: 10px 0; }
.nc-md :deep(table) { border-collapse: collapse; margin: 0 0 8px; font-size: 0.95em; }
.nc-md :deep(th), .nc-md :deep(td) { padding: 3px 8px; border: 1px solid var(--nm-border); }
.nc-md :deep(th) { background: var(--nm-overlay-code); font-weight: 600; }
.nc-md :deep(a) { color: var(--nm-syn-link, var(--nm-accent)); }
.nc-foreign, .nc-foreign-out {
  margin: 0; padding: 6px 10px; overflow: auto; max-height: 320px; white-space: pre-wrap; font-family: var(--nm-mono); font-size: 12.5px;
  border-radius: 3px; color: var(--nm-text-dim); background: var(--nm-overlay-code); opacity: 0.75;
}
.nc-foreign-out { margin-top: 4px; background: none; border: 1px dashed var(--nm-border); }
</style>
