// The results pane's search (issue #142): what's typed, the options, the
// search over the loaded rows (gridSearch.ts) and the match the user is on.
// The pane maps rows to the grid (with "Solo filas con coincidencias" the
// grid shows only some of them) and moves the grid to a match.

import { computed, onBeforeUnmount, ref, shallowRef, triggerRef, watch } from 'vue';
import type { Cell, ResultColumn } from '../api/types';
import { shownVectorMeta } from './vector';
import { lowerBound, matcher, placeOf, rowColumns, rowsWithColumn, searchRows, step, type Hits, type Spot } from './gridSearch';

export interface SearchSource {
  rows: Cell[][];
  columns: ResultColumn[];
}

/** A column with matches, for the chips under the search box. */
export interface ColumnHits {
  c: number;
  name: string;
  count: number;
}

export function useGridSearch(source: () => SearchSource | null) {
  const open = ref(false);
  /** What's typed; `needle` follows it after a pause. */
  const text = ref('');
  const needle = ref('');
  const caseSensitive = ref(false);
  /** "Solo filas con coincidencias". */
  const onlyRows = ref(false);
  /** "Solo columnas con coincidencias". */
  const onlyColumns = ref(false);
  /** The search is limited to this column (a chip was clicked). */
  const scope = ref<number | null>(null);
  /** The hits as the search goes (the counts fill in). */
  const live = shallowRef<Hits | null>(null);
  /** The last finished search: the row filter, the marks and moving between
   *  matches use it. */
  const hits = shallowRef<Hits | null>(null);
  const busy = ref(false);
  /** The match the user is on. */
  const at = ref<Spot | null>(null);
  let gen = 0;
  let whenDone: (() => void) | null = null;

  let timer: ReturnType<typeof setTimeout> | undefined;
  watch(text, (q) => {
    clearTimeout(timer);
    timer = setTimeout(() => { needle.value = q; }, 200);
  });
  onBeforeUnmount(() => { clearTimeout(timer); gen++; });
  /** Searches what's typed now, without waiting for the pause. */
  function flush() {
    clearTimeout(timer);
    needle.value = text.value;
  }

  const re = computed(() => (open.value ? matcher({ text: needle.value, caseSensitive: caseSensitive.value }) : null));
  /** The columns searched: all but vectors (their cells show a preview). */
  const searched = computed(() => source()?.columns.flatMap((c, i) => (shownVectorMeta(c) ? [] : [i])) ?? []);
  const rowsNow = () => source()?.rows ?? null;

  watch(() => source()?.columns, () => { scope.value = null; });
  watch([re, rowsNow, searched], run, { immediate: true });

  async function run() {
    const g = ++gen;
    at.value = null;
    const src = source();
    const r = re.value;
    if (!src || !r) {
      live.value = null;
      hits.value = null;
      busy.value = false;
      whenDone = null;
      return;
    }
    busy.value = true;
    const h = await searchRows(src.rows, src.columns.length, searched.value, r, {
      cancelled: () => g !== gen,
      onSlice: (x) => {
        if (g !== gen) return;
        if (live.value === x) triggerRef(live);
        else live.value = x;
      },
    });
    if (g !== gen || !h) return;
    hits.value = h;
    triggerRef(live);
    busy.value = false;
    const f = whenDone;
    whenDone = null;
    f?.();
  }

  /** Runs `f` once the search for what's typed has finished. */
  function afterSearch(f: () => void) {
    if (needle.value !== text.value) flush();
    // The watcher runs the new search on the next tick; until then `busy`
    // still says false.
    queueMicrotask(() => {
      if (busy.value) whenDone = f;
      else f();
    });
  }

  const progress = computed(() => {
    const h = live.value;
    return h && h.total ? h.scanned / h.total : 0;
  });

  /** A row's matching columns (within the chosen column, if any). */
  function colsOf(k: number): number[] {
    const src = source();
    const r = re.value;
    const h = hits.value;
    if (!src || !r || !h) return [];
    const cols = rowColumns(src.rows[h.rows[k]], searched.value, r);
    return scope.value === null ? cols : cols.filter((c) => c === scope.value);
  }

  /** With a column chosen: the places in `hits.rows` of the rows it matches in. */
  const scoped = computed(() => {
    const h = hits.value;
    const src = source();
    const r = re.value;
    if (scope.value === null || !h || !src || !r) return null;
    return rowsWithColumn(src.rows, h, scope.value, r);
  });

  /** Moves to the next (1) or previous (-1) match; returns it. */
  function go(dir: 1 | -1): Spot | null {
    const h = hits.value;
    if (!h || !h.n) return null;
    const ks = scoped.value;
    if (ks) {
      if (!ks.length) return null;
      const from = at.value;
      let i: number;
      if (!from) i = dir > 0 ? 0 : ks.length - 1;
      else {
        const p = lowerBound(ks, from.k);
        const on = p < ks.length && ks[p] === from.k;
        i = on ? p + dir : dir > 0 ? p : p - 1;
        i = ((i % ks.length) + ks.length) % ks.length;
      }
      at.value = { k: ks[i], c: scope.value! };
    } else {
      at.value = step(h.n, colsOf, at.value, dir);
    }
    return at.value;
  }

  /** The searched row a match is in. */
  const rowOf = (s: Spot) => hits.value?.rows[s.k] ?? -1;

  /** Matching cells (within the chosen column). */
  const total = computed(() => {
    const h = hits.value;
    if (!h) return 0;
    return scope.value === null ? h.cells : h.perColumn[scope.value] ?? 0;
  });
  /** 1-based place of the current match, or 0. */
  const place = computed(() => {
    const h = hits.value;
    const s = at.value;
    if (!h || !s) return 0;
    const ks = scoped.value;
    if (ks) return lowerBound(ks, s.k) + 1;
    return placeOf(h, s, colsOf);
  });

  /** The columns with matches, most matches first (the counts fill in as
   *  the search goes). */
  const columnHits = computed<ColumnHits[]>(() => {
    const h = live.value;
    const src = source();
    if (!h || !src) return [];
    return h.perColumn
      .map((count, c) => ({ c, name: src.columns[c]?.name ?? '', count }))
      .filter((x) => x.count > 0)
      .sort((a, b) => b.count - a.count || a.c - b.c);
  });

  /** The searched rows "Solo filas" leaves (ascending), or null for all. */
  const rowsShown = computed<Uint32Array | null>(() => {
    const h = hits.value;
    if (!onlyRows.value || !h || !re.value) return null;
    const ks = scoped.value;
    if (ks) return ks.map((k) => h.rows[k]);
    return h.rows.slice(0, h.n);
  });

  /** The columns "Solo columnas" leaves, or null for all (also when nothing
   *  matches: the grid keeps its columns). */
  const columnsShown = computed<Set<number> | null>(() => {
    const h = hits.value;
    if (!onlyColumns.value || !h || !h.cells || !re.value) return null;
    return new Set(h.perColumn.flatMap((n, c) => (n > 0 ? [c] : [])));
  });

  /** The matching rows, for the marks beside the scroll bar. */
  const markRows = computed<Uint32Array | null>(() => {
    const h = hits.value;
    if (!h || !h.n || !re.value) return null;
    const ks = scoped.value;
    return ks ? ks.map((k) => h.rows[k]) : h.rows.subarray(0, h.n);
  });

  function setScope(c: number | null) {
    scope.value = c;
    at.value = null;
  }
  // Setters for the search bar (its props aren't written to).
  const setText = (v: string) => { text.value = v; };
  const setCaseSensitive = (v: boolean) => { caseSensitive.value = v; };
  const setOnlyRows = (v: boolean) => { onlyRows.value = v; };
  const setOnlyColumns = (v: boolean) => { onlyColumns.value = v; };

  function show() {
    open.value = true;
  }
  function close() {
    open.value = false;
    at.value = null;
  }

  return {
    open, text, needle, caseSensitive, onlyRows, onlyColumns, scope, busy, at, re,
    live, hits, progress, total, place, columnHits, rowsShown, columnsShown, markRows,
    flush, afterSearch, go, rowOf, setScope, setText, setCaseSensitive, setOnlyRows, setOnlyColumns, show, close,
  };
}

export type GridSearch = ReturnType<typeof useGridSearch>;
