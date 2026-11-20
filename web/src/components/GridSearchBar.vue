<script setup lang="ts">
// The search bar over a result's grid (issue #142): the text, "Aa", the
// place among the matches, "Solo filas / columnas con coincidencias" and,
// under it, the columns with matches and how many (a click limits the
// search to that column). The pane owns the state (useGridSearch).
import { computed, ref, type UnwrapNestedRefs } from 'vue';
import { locale, t } from '../i18n';
import type { GridSearch } from '../composables/useGridSearch';

const props = defineProps<{
  s: UnwrapNestedRefs<GridSearch>;
  /** The result's columns. */
  columns: number;
  /** The result was cut: the search sees these rows only. */
  loaded?: number | null;
}>();
const emit = defineEmits<{ go: [dir: 1 | -1]; close: [] }>();

const input = ref<HTMLInputElement | null>(null);
function focus() {
  input.value?.focus();
  input.value?.select();
}
defineExpose({ focus });

const fmt = (n: number) => n.toLocaleString(locale());
const status = computed(() => {
  const s = props.s;
  if (s.busy) return t('results:search.searching', { percent: s.progress.toLocaleString(locale(), { style: 'percent' }) });
  if (!s.needle || !s.hits) return '';
  if (!s.total) return t('results:tree.noMatches');
  if (s.place) return t('results:search.place', { at: fmt(s.place), total: fmt(s.total) });
  return t('results:search.count', { count: s.total, n: fmt(s.total) });
});

function onKey(e: KeyboardEvent) {
  if (e.key === 'Enter') {
    e.preventDefault();
    const dir = e.shiftKey ? -1 : 1;
    props.s.afterSearch(() => emit('go', dir));
  } else if (e.key === 'Escape') {
    e.preventDefault();
    emit('close');
  }
}

function pickColumn(c: number) {
  props.s.setScope(props.s.scope === c ? null : c);
  props.s.afterSearch(() => emit('go', 1));
}
</script>

<template>
  <div class="gs" role="search">
    <div class="gs-bar">
      <input
        ref="input"
        :value="s.text"
        class="gs-input"
        :placeholder="$t('results:search.placeholder')"
        :aria-label="$t('results:search.placeholder')"
        spellcheck="false"
        @input="s.setText(($event.target as HTMLInputElement).value)"
        @keydown="onKey"
      />
      <button
        class="gs-btn gs-case"
        :class="{ on: s.caseSensitive }"
        :aria-pressed="s.caseSensitive"
        :aria-label="$t('results:search.caseSensitive')"
        :title="$t('results:search.caseSensitive')"
        @click="s.setCaseSensitive(!s.caseSensitive)"
      >Aa</button>
      <span class="gs-status" aria-live="polite">{{ status }}</span>
      <button :disabled="!s.total || s.busy" :aria-label="$t('results:tree.prevMatch')" class="gs-btn" :title="$t('results:tree.prevMatch')" @click="emit('go', -1)"><el-icon><ei-arrow-up /></el-icon></button>
      <button :disabled="!s.total || s.busy" :aria-label="$t('results:tree.nextMatch')" class="gs-btn" :title="$t('results:tree.nextMatch')" @click="emit('go', 1)"><el-icon><ei-arrow-down /></el-icon></button>
      <el-checkbox size="small" class="gs-check" :model-value="s.onlyRows" @update:model-value="(v: unknown) => s.setOnlyRows(!!v)">{{ $t('results:search.onlyRows') }}</el-checkbox>
      <el-checkbox size="small" class="gs-check" :model-value="s.onlyColumns" @update:model-value="(v: unknown) => s.setOnlyColumns(!!v)">{{ $t('results:search.onlyColumns') }}</el-checkbox>
      <div class="nm-spacer" />
      <span v-if="loaded != null" class="gs-note" :title="$t('results:search.loadedHelp')">{{ $t('results:search.loaded', { count: loaded, rows: fmt(loaded) }) }}</span>
      <button class="gs-btn gs-close" :aria-label="$t('results:search.close')" :title="$t('results:search.close')" @click="emit('close')"><el-icon><ei-close /></el-icon></button>
    </div>
    <div v-if="s.needle && s.columnHits.length" class="gs-cols">
      <span class="gs-label">{{ $t('results:search.inColumns', { n: fmt(s.columnHits.length), count: columns }) }}</span>
      <button
        v-for="x in s.columnHits"
        :key="x.c"
        class="gs-chip"
        :class="{ on: s.scope === x.c }"
        :aria-pressed="s.scope === x.c"
        :title="s.scope === x.c ? $t('results:search.allColumns') : $t('results:search.onlyIn', { name: x.name })"
        @click="pickColumn(x.c)"
      >{{ x.name }} <span class="gs-n">{{ fmt(x.count) }}</span></button>
    </div>
  </div>
</template>

<style scoped>
.gs { border-bottom: 1px solid var(--nm-border-soft); font-size: 11.5px; background: var(--ide-sidebar); }
.gs-bar { display: flex; align-items: center; gap: 4px; padding: 3px 8px; }
.gs-input {
  width: 240px; padding: 2px 6px; font: inherit; color: var(--nm-text); background: var(--ide-input);
  border: 1px solid var(--nm-border); border-radius: 3px; outline: none;
}
.gs-input:focus { border-color: var(--ide-focus); }
.gs-status { color: var(--nm-text-dim); padding: 0 4px; white-space: nowrap; font-variant-numeric: tabular-nums; }
.gs-btn {
  display: inline-flex; align-items: center; justify-content: center; min-width: 22px; height: 20px; padding: 0 5px;
  font: inherit; color: var(--nm-text); background: transparent; border: 1px solid var(--nm-border); border-radius: 3px; cursor: pointer;
}
.gs-btn:hover:not(:disabled) { color: var(--nm-text-strong); border-color: var(--nm-border-hover); }
.gs-btn:disabled { opacity: 0.45; cursor: default; }
.gs-case { font-weight: 600; }
.gs-case.on { color: var(--nm-text-strong); border-color: var(--ide-focus); background: color-mix(in srgb, var(--ide-focus) 18%, transparent); }
.gs-close { border-color: transparent; }
.gs-check { margin: 0 0 0 8px; height: 20px; }
.gs-check :deep(.el-checkbox__label) { font-size: 11.5px; padding-left: 5px; }
.gs-note { color: var(--nm-text-dim); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.gs-cols { display: flex; align-items: center; gap: 4px; padding: 0 8px 4px; overflow-x: auto; white-space: nowrap; }
.gs-label { color: var(--nm-text-dim); margin-right: 2px; }
.gs-chip {
  display: inline-flex; align-items: center; gap: 5px; height: 19px; padding: 0 7px; font: inherit; color: var(--nm-text);
  background: transparent; border: 1px solid var(--nm-border); border-radius: 10px; cursor: pointer; flex-shrink: 0;
}
.gs-chip:hover { border-color: var(--nm-border-hover); color: var(--nm-text-strong); }
.gs-chip.on { border-color: var(--ide-focus); background: color-mix(in srgb, var(--ide-focus) 18%, transparent); color: var(--nm-text-strong); }
.gs-n { color: var(--nm-text-dim); font-variant-numeric: tabular-nums; }
.gs-chip.on .gs-n { color: inherit; }
</style>
