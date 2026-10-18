<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { t } from '../i18n';
import { CaretBottom, CaretTop, Filter, MoreFilled } from '@element-plus/icons-vue';
import { coerce, coerceNew, sameValue, type Edits } from '../composables/gridEdit';
import { isMac, isSaveShortcut } from '../composables/shortcuts';
import type { FkLink, GridLinks } from '../composables/fkJump';
import type { Cell, ResultColumn, SortKey } from '../api/types';
import { nextSort } from '../composables/gridSort';
import ContextMenu, { type MenuItem } from './ContextMenu.vue';
import CellViewer from './CellViewer.vue';
import { columnKind, describe, menuFor, parseFilter, type ColumnFilter, type FilterMenuEntry, type FilterState } from '../composables/gridFilter';
import { gridFontSize } from '../composables/appearance';
import { COPY_FORMATS, defaultCopyFormat, formatRows, setDefaultCopyFormat, type CopyContext, type CopyFormat } from '../composables/copyFormats';

// Virtualized result grid: only the rows in view are in the DOM, so tens of
// thousands of rows scroll smoothly. Columns resize by dragging the header
// edge; a click selects a cell, a click on the row number selects rows
// (shift extends). ⌘C copies the selection as TSV; double-click opens the
// value in a viewer.

const props = defineProps<{
  columns: ResultColumn[];
  rows: Cell[][];
  /** Table, dialect and key columns for the SQL / Mongo copy formats. */
  copyContext?: CopyContext;
  /** Cells can be edited (the changes become UPDATE code, never run). */
  editable?: boolean;
  /** Why they can't, shown when trying. */
  noEditReason?: string | null;
  /** Columns that can't be edited even when the rows can, by index, with
   *  the reason (a value converted to be shown, such as Oracle's
   *  SDO_GEOMETRY as text). */
  readOnlyColumns?: Record<number, string>;
  /** Edited values, row → column → value. */
  edits?: Edits;
  /** Rows marked for deletion (their edits are ignored). */
  deleted?: Set<number>;
  /** Rows can be marked for deletion (the DELETE code is generated, never run). */
  deletable?: boolean;
  /** Why they can't, shown on the menu item. */
  noDeleteReason?: string | null;
  /** Show the filter row under the headers. */
  filterable?: boolean;
  /** The column filters, by column name. */
  filters?: Record<string, FilterState>;
  /** The last `added` rows are new (not in the table yet): a cell left
   *  alone takes the column's default, and removing the row drops it. */
  added?: number;
  /** New rows can be added (the INSERT code is generated, never run). */
  insertable?: boolean;
  /** A click on a header sorts by that column (Shift+click adds a key). */
  sortable?: boolean;
  /** The current sort, shown on the headers; the parent sorts the rows. */
  sort?: SortKey[];
  /** Foreign keys from and to the rows' table: the menu offers the jumps
   *  and ⌘/Ctrl+click on a key cell goes to the referenced row. */
  links?: GridLinks | null;
}>();
const emit = defineEmits<{
  /** A cell's new value; `undefined` reverts it. */
  edit: [row: number, col: number, value: Cell | undefined];
  /** Mark (`true`) or unmark rows for deletion. */
  delete: [rows: number[], mark: boolean];
  /** A column's filter changed; `null` clears it. */
  filter: [column: string, state: FilterState | null];
  /** "Agregar fila": the pane adds a new row at the end. */
  add: [];
  /** The sort after a header click (empty: none). */
  sort: [keys: SortKey[]];
  /** Open the rows `link` leads to from row `row` (its key isn't NULL). */
  follow: [link: FkLink, row: number];
}>();

/** Row height for the grid font size (Configuración › General): 22px at 12px. */
const ROW_H_AT_12 = 22;
const rowH = computed(() => Math.round((gridFontSize.value * ROW_H_AT_12) / 12));
const NUM_W = 52;
/** Header rows above the data: the names, and the filters. */
const HEAD = computed(() => (props.filterable ? 2 : 1));

const scroller = ref<HTMLDivElement | null>(null);
const scrollTop = ref(0);
const viewH = ref(400);
const widths = ref<number[]>([]);

// Declared before the column watcher below, which resets them (immediate).
const selection = ref<{ r: number; c: number } | null>(null);
/** The other corner of a block of cells (shift+click, drag, shift+arrows):
 *  the block runs from here to `selection`. Null: just the active cell. */
const anchor = ref<{ r: number; c: number } | null>(null);
const rowSel = ref<{ from: number; to: number } | null>(null);
type Block = { r0: number; r1: number; c0: number; c1: number };
/** Earlier blocks of a multiple selection (⌘/Ctrl+click adds one, like a
 *  spreadsheet); the current one is still anchor..selection. */
const cellExtra = ref<Block[]>([]);
/** Earlier row ranges of a multiple row selection (⌘/Ctrl+click on the numbers). */
const rowExtra = ref<{ from: number; to: number }[]>([]);
function clearExtras() {
  cellExtra.value = [];
  rowExtra.value = [];
}
/** Columns the user hid, by name. Hidden columns keep their index (edits,
 *  selection and filters use it) and are only left out of the layout. */
const hiddenNames = ref<Set<string>>(new Set());

function measure(text: string) {
  return Math.min(360 * (gridFontSize.value / 12), Math.max(64, text.length * 7.2 * (gridFontSize.value / 12) + 20));
}

watch(
  () => props.columns,
  (cols, old) => {
    // Same columns (a filtered reload): keep the widths and the scroll.
    const names = (x?: ResultColumn[]) => (x ?? []).map((c) => c.name).join('\u0000');
    if (old && names(cols) === names(old) && widths.value.length === cols.length) {
      selection.value = null;
      anchor.value = null;
      rowSel.value = null;
      clearExtras();
      return;
    }
    hiddenNames.value = new Set();
    widths.value = cols.map((c, i) => {
      let w = measure(c.name);
      for (const r of props.rows.slice(0, 100)) w = Math.max(w, measure(display(r[i])));
      return w;
    });
    selection.value = null;
    anchor.value = null;
    rowSel.value = null;
    clearExtras();
    scroller.value?.scrollTo({ top: 0, left: 0 });
  },
  { immediate: true },
);

// Another grid font size: the columns keep their proportions at the new size.
watch(gridFontSize, (n, o) => { widths.value = widths.value.map((w) => Math.round((w * n) / o)); });

const isHidden = (c: number) => hiddenNames.value.has(props.columns[c]?.name);
/** Width of the columns before `c` (or all of them), hidden ones left out. */
const widthUpTo = (c: number) => widths.value.slice(0, c).reduce((a, w, i) => (isHidden(i) ? a : a + w), 0);
const totalW = computed(() => NUM_W + widthUpTo(widths.value.length));
/** The next shown column from `c` in direction `d` (itself if none). */
function step(c: number, d: number) {
  for (let x = c + d; x >= 0 && x < props.columns.length; x += d) if (!isHidden(x)) return x;
  return c;
}
const first = computed(() => Math.max(0, Math.floor(scrollTop.value / rowH.value) - 10));
const last = computed(() => Math.min(props.rows.length, Math.ceil((scrollTop.value + viewH.value) / rowH.value) + 10));
const visible = computed(() => props.rows.slice(first.value, last.value).map((r, k) => ({ r, i: first.value + k })));

let ro: ResizeObserver | null = null;
onMounted(() => {
  ro = new ResizeObserver(() => { viewH.value = scroller.value?.clientHeight ?? 400; });
  if (scroller.value) ro.observe(scroller.value);
});
onBeforeUnmount(() => ro?.disconnect());

function display(v: Cell): string {
  if (v === null) return 'NULL';
  if (typeof v === 'string') return v.length > 500 ? v.slice(0, 500) + '…' : v;
  return String(v);
}
const isDeleted = (r: number) => !!props.deleted?.has(r);
/** Index of the first new row (rows from here on aren't in the table yet). */
const base = computed(() => props.rows.length - (props.added ?? 0));
const isAdded = (r: number) => r >= base.value;
/** A new row's cell the user hasn't set: the column's default goes. */
const isUnset = (r: number, c: number) => isAdded(r) && !(props.edits?.[r] && c in props.edits[r]);
/** A row's cells can be edited: the result's when editable, new rows when
 *  rows can be added (a table without a key still takes new rows). */
const canEditRow = (r: number) => (isAdded(r) ? !!props.insertable : !!props.editable);
const canEditCell = (r: number, c: number) => canEditRow(r) && !props.readOnlyColumns?.[c];
/** The value shown: the edit, if any (a row marked for deletion shows its
 *  original values: its edits are ignored). */
function valueAt(r: number, c: number): Cell {
  const e = isDeleted(r) ? undefined : props.edits?.[r];
  return e && c in e ? e[c] : props.rows[r]?.[c] ?? null;
}
const isEdited = (r: number, c: number) => !isDeleted(r) && !isAdded(r) && !!props.edits?.[r] && c in props.edits[r];

function cellClass(v: Cell) {
  if (v === null) return 'nm-null';
  if (typeof v === 'number') return 'nm-num';
  if (typeof v === 'boolean') return 'nm-bool';
  return '';
}

// -- selection ---------------------------------------------------------------

function selectCell(r: number, c: number) {
  selection.value = { r, c };
  anchor.value = null;
  rowSel.value = null;
  clearExtras();
  scroller.value?.focus();
}
/** A click on a cell: shift extends the block from the active cell, and
 *  dragging with the button down extends it to the cell under the pointer. */
let dragging = false;
function onCellDown(e: MouseEvent, r: number, c: number) {
  if (e.button !== 0) return;
  // ⌘/Ctrl on a foreign key cell: the referenced row (not another block).
  if ((e.metaKey || e.ctrlKey) && isLinkCell(r, c)) {
    e.preventDefault();
    const link = parentsAt(c).find((l) => keyOf(l, r));
    if (link) emit('follow', link, r);
    return;
  }
  // ⌘/Ctrl: keep what's selected and start another block here.
  if ((e.metaKey || e.ctrlKey) && selection.value && !rowSel.value) {
    const s = selection.value;
    cellExtra.value = [...cellExtra.value, block.value ?? { r0: s.r, r1: s.r, c0: s.c, c1: s.c }];
    selection.value = { r, c };
    anchor.value = null;
    scroller.value?.focus();
    e.preventDefault();
    dragging = true;
    window.addEventListener('mouseup', () => { dragging = false; }, { once: true });
    return;
  }
  if (e.shiftKey && selection.value) {
    anchor.value ??= selection.value;
    selection.value = { r, c };
    rowSel.value = null;
    scroller.value?.focus();
    e.preventDefault();
    return;
  }
  selectCell(r, c);
  dragging = true;
  window.addEventListener('mouseup', () => { dragging = false; }, { once: true });
}
function onCellEnter(r: number, c: number) {
  if (!dragging || !selection.value) return;
  if (selection.value.r === r && selection.value.c === c) return;
  anchor.value ??= selection.value;
  selection.value = { r, c };
}
/** The block of cells, when there's one (more than the active cell). */
const block = computed(() => {
  const a = anchor.value, s = selection.value;
  if (!a || !s || (a.r === s.r && a.c === s.c)) return null;
  return { r0: Math.min(a.r, s.r), r1: Math.max(a.r, s.r), c0: Math.min(a.c, s.c), c1: Math.max(a.c, s.c) };
});
const within = (b: Block, r: number, c: number) => r >= b.r0 && r <= b.r1 && c >= b.c0 && c <= b.c1;
/** Part of the selection beyond the active cell alone: the current block, or
 *  any block of a multiple selection (then the active cell too). */
const inBlock = (r: number, c: number) => {
  const b = block.value;
  if (b && within(b, r, c)) return true;
  if (!cellExtra.value.length) return false;
  const s = selection.value;
  return (!!s && s.r === r && s.c === c) || cellExtra.value.some((x) => within(x, r, c));
};
function selectRow(r: number, e: MouseEvent) {
  if ((e.metaKey || e.ctrlKey) && rowSel.value) {
    rowExtra.value = [...rowExtra.value, rowSel.value];
    rowSel.value = { from: r, to: r };
  } else if (e.shiftKey && rowSel.value) rowSel.value = { from: rowSel.value.from, to: r };
  else {
    rowSel.value = { from: r, to: r };
    rowExtra.value = [];
  }
  selection.value = null;
  anchor.value = null;
  cellExtra.value = [];
  scroller.value?.focus();
}
const inRange = (x: { from: number; to: number }, i: number) => i >= Math.min(x.from, x.to) && i <= Math.max(x.from, x.to);
const rowSelected = (i: number) => {
  const s = rowSel.value;
  return (!!s && inRange(s, i)) || rowExtra.value.some((x) => inRange(x, i));
};

function tsv(v: Cell) {
  return v === null ? '' : String(v).replace(/\t/g, ' ').replace(/\r?\n/g, ' ');
}
async function copy(text: string) {
  await navigator.clipboard.writeText(text);
  ElMessage.success({ message: t('common:copied'), duration: 1200 });
}

// -- copy formats -----------------------------------------------------------------------
const copyFormat = ref<CopyFormat>(defaultCopyFormat());

/** The rows a copy takes: the selected rows, the selected cell's row, or all. */
function rowsForCopy(): Cell[][] {
  if (rowSel.value) return selectedRows().map((i) => props.rows[i]);
  if (selection.value) return [props.rows[selection.value.r]];
  return props.rows;
}

function copyAs(format: CopyFormat) {
  // A block of cells: those columns of those rows, shown columns only, with
  // the values as shown (edits included).
  // A multiple selection: every row and column it touches, in grid order,
  // with the cells left out empty (as a spreadsheet pastes them).
  if (cellExtra.value.length && !rowSel.value && selection.value) {
    const s = selection.value;
    const blocks = [...cellExtra.value, block.value ?? { r0: s.r, r1: s.r, c0: s.c, c1: s.c }];
    const rs = new Set<number>(), cs = new Set<number>();
    for (const x of blocks) {
      for (let r = x.r0; r <= x.r1; r++) rs.add(r);
      for (let c = x.c0; c <= x.c1; c++) if (!isHidden(c)) cs.add(c);
    }
    const rowsIdx = [...rs].sort((a, b) => a - b), cols = [...cs].sort((a, b) => a - b);
    const rows = rowsIdx.map((r) => cols.map((c) => (blocks.some((x) => within(x, r, c)) ? valueAt(r, c) : null)));
    copy(formatRows(format, cols.map((c) => props.columns[c]), rows, props.copyContext));
    return;
  }
  const b = block.value;
  if (b && !rowSel.value) {
    const cols: number[] = [];
    for (let c = b.c0; c <= b.c1; c++) if (!isHidden(c)) cols.push(c);
    const rows: Cell[][] = [];
    for (let r = b.r0; r <= b.r1; r++) rows.push(cols.map((c) => valueAt(r, c)));
    copy(formatRows(format, cols.map((c) => props.columns[c]), rows, props.copyContext));
    return;
  }
  // A single cell with a plain format copies just its value.
  if (selection.value && !rowSel.value && (format === 'tsv' || format === 'tsv_headers')) {
    copy(tsv(props.rows[selection.value.r]?.[selection.value.c] ?? null));
    return;
  }
  copy(formatRows(format, props.columns, rowsForCopy(), props.copyContext));
}

function onKey(e: KeyboardEvent) {
  const mod = e.metaKey || e.ctrlKey;
  const sel = selection.value;
  if (sel && !mod && !e.altKey) {
    if (e.key === 'Enter' || e.key === 'F2') { e.preventDefault(); startEdit(sel.r, sel.c); return; }
    if (canEditCell(sel.r, sel.c) && (e.key === 'Backspace' || e.key === 'Delete')) { e.preventDefault(); startEdit(sel.r, sel.c, ''); return; }
    if (canEditCell(sel.r, sel.c) && e.key.length === 1) { e.preventDefault(); startEdit(sel.r, sel.c, e.key); return; }
  }
  if (rowSel.value && !mod && !e.altKey && (e.key === 'Backspace' || e.key === 'Delete')) { e.preventDefault(); toggleDelete(selectedRows()); return; }
  if (mod && e.key.toLowerCase() === 'c') { e.preventDefault(); copyAs(copyFormat.value); return; }
  if (mod && e.key.toLowerCase() === 'a') { e.preventDefault(); rowSel.value = { from: 0, to: props.rows.length - 1 }; selection.value = null; anchor.value = null; clearExtras(); return; }
  const s = selection.value;
  if (!s) return;
  const moves: Record<string, [number, number]> = { ArrowUp: [-1, 0], ArrowDown: [1, 0], ArrowLeft: [0, -1], ArrowRight: [0, 1] };
  const m = moves[e.key];
  if (!m) return;
  e.preventDefault();
  const r = Math.min(props.rows.length - 1, Math.max(0, s.r + m[0]));
  const c = m[1] ? step(s.c, m[1]) : s.c;
  // Shift extends the block from where it started; a plain arrow leaves it.
  if (e.shiftKey) anchor.value ??= s;
  else {
    anchor.value = null;
    cellExtra.value = [];
  }
  selection.value = { r, c };
  const el = scroller.value;
  if (el) {
    const top = r * rowH.value;
    const below = rowH.value * (HEAD.value + 1);
    if (top < el.scrollTop) el.scrollTop = top;
    else if (top + below > el.scrollTop + el.clientHeight) el.scrollTop = top + below - el.clientHeight;
  }
}

// -- editing -----------------------------------------------------------------------
const editing = ref<{ r: number; c: number; text: string } | null>(null);
const editInput = ref<HTMLInputElement | null>(null);
const editLeft = computed(() => (editing.value ? NUM_W + widthUpTo(editing.value.c) : 0));

function startEdit(r: number, c: number, text?: string) {
  if (!canEditRow(r)) {
    if (props.noEditReason) ElMessage.info({ message: t('results:grid.cantEdit', { reason: props.noEditReason }), duration: 3500 });
    return;
  }
  const readOnly = props.readOnlyColumns?.[c];
  if (readOnly) { ElMessage.info({ message: t('results:grid.cantEdit', { reason: readOnly }), duration: 3500 }); return; }
  if (isDeleted(r)) { ElMessage.info({ message: t('results:grid.rowDeleted'), duration: 3500 }); return; }
  const v = valueAt(r, c);
  selection.value = { r, c };
  anchor.value = null;
  cellExtra.value = [];
  editing.value = { r, c, text: text ?? (v === null || isUnset(r, c) ? '' : String(v)) };
  nextTick(() => {
    editInput.value?.focus();
    if (text === undefined) editInput.value?.select();
  });
}
function commitEdit(move: [number, number] | null = null) {
  const e = editing.value;
  if (!e) return;
  editing.value = null;
  if (isAdded(e.r)) {
    // A new row: an empty input leaves the cell to the column's default
    // (an explicit NULL comes from "Establecer NULL").
    const set = !isUnset(e.r, e.c);
    if (e.text === '') { if (set && valueAt(e.r, e.c) !== null) emit('edit', e.r, e.c, undefined); }
    else emit('edit', e.r, e.c, coerceNew(props.columns[e.c]?.type_name ?? '', e.text));
    afterCommit(e, move);
    return;
  }
  const original = props.rows[e.r]?.[e.c] ?? null;
  const prev = valueAt(e.r, e.c);
  // Leaving a NULL untouched keeps it NULL (the input shows it empty).
  if (!(prev === null && e.text === '')) {
    const v = coerce(original, e.text);
    emit('edit', e.r, e.c, sameValue(v, original) ? undefined : v);
  }
  afterCommit(e, move);
}
function afterCommit(e: { r: number; c: number }, move: [number, number] | null) {
  scroller.value?.focus();
  if (move) {
    selection.value = {
      r: Math.min(props.rows.length - 1, Math.max(0, e.r + move[0])),
      c: move[1] ? step(e.c, move[1]) : e.c,
    };
  }
}
function cancelEdit() {
  editing.value = null;
  scroller.value?.focus();
}
function onEditKey(ev: KeyboardEvent) {
  if (ev.key === 'Enter') { ev.preventDefault(); commitEdit([1, 0]); }
  else if (ev.key === 'Tab') { ev.preventDefault(); commitEdit([0, ev.shiftKey ? -1 : 1]); }
  else if (ev.key === 'Escape') { ev.preventDefault(); cancelEdit(); }
  // ⌘S keeps the value and goes on to the pane, which saves.
  else if (isSaveShortcut(ev)) { commitEdit(); return; }
  ev.stopPropagation();
}
function setNull(r: number, c: number) {
  if (!canEditCell(r, c) || isDeleted(r)) return startEdit(r, c);
  if (isAdded(r)) { emit('edit', r, c, !isUnset(r, c) && valueAt(r, c) === null ? undefined : null); return; }
  emit('edit', r, c, props.rows[r]?.[c] === null ? undefined : null);
}
watch(() => props.rows, () => { editing.value = null; });

/** Scrolls to a (new) row and starts editing its first shown cell. */
function editRow(r: number) {
  const el = scroller.value;
  if (el) {
    const top = r * rowH.value;
    const below = rowH.value * (HEAD.value + 1);
    if (top < el.scrollTop || top + below > el.scrollTop + el.clientHeight) el.scrollTop = Math.max(0, top + below - el.clientHeight);
  }
  const c = isHidden(0) ? step(0, 1) : 0;
  nextTick(() => startEdit(r, c));
}
/** Count, sum, average, minimum and maximum of the selected cells (two or
 *  more), as the status line shows them. Numbers only for sum/avg/min/max;
 *  decimals that arrive as text ("15000.00") count as numbers. */
const selectionStats = computed(() => {
  const cells: [number, number][] = [];
  if (rowSel.value) {
    const shown = props.columns.map((_, c) => c).filter((c) => !isHidden(c));
    for (const r of selectedRows()) for (const c of shown) cells.push([r, c]);
  } else if (selection.value) {
    const s = selection.value;
    const blocks = [...cellExtra.value, ...(block.value ? [block.value] : cellExtra.value.length ? [{ r0: s.r, r1: s.r, c0: s.c, c1: s.c }] : [])];
    const seen = new Set<string>();
    for (const b of blocks) for (let r = b.r0; r <= b.r1; r++) for (let c = b.c0; c <= b.c1; c++) {
      if (isHidden(c) || seen.has(`${r},${c}`)) continue;
      seen.add(`${r},${c}`);
      cells.push([r, c]);
    }
  }
  if (cells.length < 2) return null;
  let count = 0, n = 0, sum = 0, min = Infinity, max = -Infinity;
  for (const [r, c] of cells) {
    const v = valueAt(r, c);
    if (v === null) continue;
    count++;
    const x = typeof v === 'number' ? v : typeof v === 'string' && /^-?\d+(\.\d+)?$/.test(v.trim()) ? Number(v) : NaN;
    if (Number.isFinite(x)) { n++; sum += x; if (x < min) min = x; if (x > max) max = x; }
  }
  return { cells: cells.length, count, numbers: n, sum, avg: n ? sum / n : 0, min, max };
});

defineExpose({ editRow, selectionStats });

// -- deleting rows (marked here; the DELETE code comes with the UPDATEs) -----------------
function selectedRows(): number[] {
  const ranges = [...rowExtra.value, ...(rowSel.value ? [rowSel.value] : [])];
  const out = new Set<number>();
  for (const x of ranges) for (let i = Math.min(x.from, x.to); i <= Math.max(x.from, x.to); i++) out.add(i);
  return [...out].sort((a, b) => a - b);
}
/** Marks the rows, or unmarks them when they're all marked already. */
function toggleDelete(rows: number[]) {
  if (!rows.length) return;
  // New rows are just dropped: nothing to delete on the server.
  if (rows.every(isAdded)) {
    if (editing.value && rows.includes(editing.value.r)) editing.value = null;
    emit('delete', rows, true);
    return;
  }
  if (!props.deletable) {
    const reason = props.noDeleteReason ?? props.noEditReason;
    if (reason) ElMessage.info({ message: t('results:grid.cantDelete', { reason }), duration: 3500 });
    return;
  }
  if (editing.value && rows.includes(editing.value.r)) editing.value = null;
  emit('delete', rows, !rows.every(isDeleted));
}

// -- column resize -------------------------------------------------------------
/** A resize drag just ended: the click it ends with isn't a sort. */
let resized = false;
function startResize(e: PointerEvent, c: number) {
  const start = e.clientX;
  const from = widths.value[c];
  const move = (ev: PointerEvent) => { widths.value[c] = Math.max(40, from + ev.clientX - start); };
  const up = () => {
    window.removeEventListener('pointermove', move);
    window.removeEventListener('pointerup', up);
    resized = true;
    setTimeout(() => { resized = false; });
  };
  window.addEventListener('pointermove', move);
  window.addEventListener('pointerup', up);
}

// -- sort ----------------------------------------------------------------------
/** Where `name` is in the sort: its position (1-based) and direction. */
function sortOf(name: string) {
  const at = (props.sort ?? []).findIndex((k) => k.column === name);
  return at < 0 ? null : { n: at + 1, desc: props.sort![at].descending };
}
/** Where the press on a header started: a drag isn't a click. */
let pressX = 0;
function onHeadDown(e: MouseEvent) {
  pressX = e.clientX;
  // Shift+click adds a sort key: don't select text.
  if (props.sortable && e.shiftKey) e.preventDefault();
}
function onHeadClick(e: MouseEvent, c: number) {
  if (!props.sortable || resized || e.button !== 0 || Math.abs(e.clientX - pressX) > 4) return;
  emit('sort', nextSort(props.sort ?? [], props.columns[c].name, e.shiftKey));
}
function headTitle(col: ResultColumn) {
  const name = col.type_name ? `${col.name} (${col.type_name})` : col.name;
  return props.sortable ? `${name}\n${t('results:grid.sortHint')}` : name;
}

// -- filters -------------------------------------------------------------------
/** What's typed in each filter box, by column name. */
const ftext = ref<Record<string, string>>({});
watch(
  () => props.filters,
  (f) => { ftext.value = Object.fromEntries(Object.entries(f ?? {}).map(([k, v]) => [k, v.text])); },
  { immediate: true, deep: true },
);
const kindOf = (c: number) => columnKind(props.columns[c]?.type_name ?? '', props.rows[0]?.[c]);
const hasFilter = (name: string) => !!props.filters?.[name];

function applyText(c: number) {
  const name = props.columns[c].name;
  const text = (ftext.value[name] ?? '').trim();
  if (text === (props.filters?.[name]?.text ?? '')) return;
  emit('filter', name, text ? { text, filters: parseFilter(name, kindOf(c), text) } : null);
}
function onFilterKey(e: KeyboardEvent, c: number) {
  e.stopPropagation();
  if (e.key === 'Enter') { e.preventDefault(); applyText(c); }
  else if (e.key === 'Escape') {
    e.preventDefault();
    const name = props.columns[c].name;
    ftext.value[name] = props.filters?.[name]?.text ?? '';
    (e.target as HTMLInputElement).blur();
  }
}

async function ask(entry: FilterMenuEntry, name: string): Promise<ColumnFilter | null> {
  const op = entry.op!;
  const base: ColumnFilter = { column: name, op, values: [] };
  if (!entry.ask) return base;
  const texts: Record<string, [string, string]> = {
    value: [`${entry.label.replace('…', '')}:`, ''],
    values: [t('results:grid.askValues'), ''],
    sql: [t('results:grid.askSql'), `${name} BETWEEN 1 AND 10`],
    sql_right: [t('results:grid.askSqlRight', { name }), 'BETWEEN 1 AND 10'],
  };
  const [msg, placeholder] = texts[entry.ask];
  try {
    const { value } = await ElMessageBox.prompt(msg, t('results:grid.filterTitle', { name }), {
      confirmButtonText: t('common:filter'),
      cancelButtonText: t('common:cancel'),
      inputPlaceholder: placeholder,
      inputType: entry.ask === 'values' ? 'textarea' : 'text',
      inputValidator: (v) => (v ?? '').trim() !== '' || t('results:grid.valueRequired'),
    });
    const v = String(value ?? '').trim();
    if (entry.ask === 'sql' || entry.ask === 'sql_right') return { ...base, sql: v };
    const c = props.columns.findIndex((x) => x.name === name);
    const parts = entry.ask === 'values' ? v.split(/[\n,]/).map((x) => x.trim()).filter(Boolean) : [v];
    // Typed like the column (numbers stay numbers).
    const typed = parts.map((x) => {
      const f = parseFilter(name, kindOf(c), `=${x}`)[0];
      return f?.values[0] ?? x;
    });
    return { ...base, values: typed };
  } catch {
    return null;
  }
}

function openFilterMenu(e: MouseEvent, c: number) {
  const name = props.columns[c].name;
  const current = props.filters?.[name]?.filters;
  const items: MenuItem[] = menuFor(kindOf(c)).map((entry) => ({
    label: entry.label,
    divided: entry.divided,
    disabled: entry.clear && !hasFilter(name),
    checked: !entry.clear && current?.length === 1 && current[0].op === entry.op,
    action: async () => {
      if (entry.clear) { ftext.value[name] = ''; emit('filter', name, null); return; }
      const f = await ask(entry, name);
      if (f) emit('filter', name, { text: describe(f), filters: [f] });
    },
  }));
  const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
  menu.value = { x: r.left, y: r.bottom, items };
}

// -- full value on hover ---------------------------------------------------------
// Resting on a cell whose text doesn't fit (or has line breaks) shows it whole
// in a fixed-width tooltip; the pointer can move into it to scroll or select.
const HOVER_DELAY = 600;
const HOVER_MAX = 20_000;
const peek = ref<{ text: string; cut: boolean; left: number; top: number; maxH: number } | null>(null);
let peekTimer: ReturnType<typeof setTimeout> | null = null;
let peekLeave: ReturnType<typeof setTimeout> | null = null;
function fullText(v: Cell): string | null {
  if (v === null || v === undefined) return null;
  if (typeof v === 'object') return JSON.stringify(v, null, 2);
  return String(v);
}
function hoverStart(e: MouseEvent, r: number, c: number) {
  // Passing over cells on the way into an open tooltip mustn't close it.
  hideSoon(peek.value ? 150 : 0);
  if (dragging || editing.value || isEdited(r, c) || isUnset(r, c)) return;
  const el = e.currentTarget as HTMLElement;
  if (peekTimer) clearTimeout(peekTimer);
  peekTimer = setTimeout(() => {
    const text = fullText(valueAt(r, c));
    if (!text || !(el.scrollWidth > el.clientWidth || text.includes('\n') || text.length > 500)) return;
    const box = el.getBoundingClientRect();
    const W = 520;
    const left = Math.max(8, Math.min(box.left, window.innerWidth - W - 8));
    // Below the cell when there's room, else above it.
    const below = window.innerHeight - box.bottom - 12;
    const above = box.top - 12;
    const maxH = Math.max(120, Math.min(window.innerHeight * 0.6, Math.max(below, above)));
    // Flush with the cell, so the pointer can go straight into it.
    const top = below >= Math.min(maxH, 200) || below >= above ? box.bottom : Math.max(8, box.top - maxH);
    peek.value = { text: text.length > HOVER_MAX ? text.slice(0, HOVER_MAX) : text, cut: text.length > HOVER_MAX, left, top, maxH };
  }, HOVER_DELAY);
}
function hideSoon(ms = 150) {
  if (peekTimer) { clearTimeout(peekTimer); peekTimer = null; }
  if (peekLeave) clearTimeout(peekLeave);
  if (!ms) { peek.value = null; return; }
  peekLeave = setTimeout(() => { peek.value = null; }, ms);
}
function keepPeek() {
  if (peekLeave) { clearTimeout(peekLeave); peekLeave = null; }
}
onBeforeUnmount(() => hideSoon(0));

// -- foreign keys ----------------------------------------------------------------------
const parentsAt = (c: number) => props.links?.parents.filter((l) => l.cols.includes(c)) ?? [];
const childrenAt = (c: number) => props.links?.children.filter((l) => l.cols.includes(c)) ?? [];
/** The row's saved key values for `link`, or null when one is NULL (or the
 *  row is new: it isn't in the table yet). */
function keyOf(link: FkLink, r: number): Cell[] | null {
  if (isAdded(r)) return null;
  const v = link.cols.map((c) => props.rows[r]?.[c] ?? null);
  return v.some((x) => x === null) ? null : v;
}
const isLinkCell = (r: number, c: number) => parentsAt(c).some((l) => keyOf(l, r));
/** ⌘/Ctrl held: key cells look like links. */
const modHeld = ref(false);
function onMod(e: KeyboardEvent) { modHeld.value = isMac ? e.metaKey : e.ctrlKey; }
function offMod() { modHeld.value = false; }
onMounted(() => {
  window.addEventListener('keydown', onMod);
  window.addEventListener('keyup', onMod);
  window.addEventListener('blur', offMod);
});
onBeforeUnmount(() => {
  window.removeEventListener('keydown', onMod);
  window.removeEventListener('keyup', onMod);
  window.removeEventListener('blur', offMod);
});
const tableName = (l: FkLink) => (l.table.schema ? `${l.table.schema}.${l.table.name}` : l.table.name);
const clickKey = () => t('results:grid.modClick', { key: isMac ? '⌘' : 'Ctrl' });
function linkItems(r: number, c: number): MenuItem[] {
  const items: MenuItem[] = [];
  const parents = parentsAt(c);
  parents.forEach((l, k) => items.push({
    label: parents.length > 1 ? t('results:grid.goToReferencedIn', { table: tableName(l) }) : t('results:grid.goToReferenced'),
    shortcut: k === 0 ? clickKey() : undefined,
    divided: k === 0,
    disabled: !keyOf(l, r),
    hint: keyOf(l, r) ? tableName(l) : isAdded(r) ? t('results:grid.linkNewRow') : t('results:grid.linkNull'),
    action: () => emit('follow', l, r),
  }));
  const children = childrenAt(c);
  if (children.length) {
    const none = children.every((l) => !keyOf(l, r));
    items.push({
      label: t('results:grid.referencingRows'),
      divided: !parents.length,
      disabled: none,
      hint: none ? (isAdded(r) ? t('results:grid.linkNewRow') : t('results:grid.linkNull')) : undefined,
      children: none ? undefined : children.map((l) => ({
        label: t('results:grid.referencingIn', { table: tableName(l), columns: l.otherColumns.join(', ') }),
        hint: l.name ?? undefined,
        action: () => emit('follow', l, r),
      })),
    });
  }
  return items;
}

// -- viewer / menu ---------------------------------------------------------------
const viewer = ref<{ title: string; value: Cell } | null>(null);
function openViewer(r: number, c: number) {
  viewer.value = { title: props.columns[c]?.name ?? '', value: props.rows[r]?.[c] ?? null };
}

const menu = ref<{ x: number; y: number; items: MenuItem[] } | null>(null);
function onContext(e: MouseEvent, r: number, c: number | null) {
  e.preventDefault();
  if (c !== null && !rowSelected(r) && !inBlock(r, c)) selectCell(r, c);
  const items: MenuItem[] = [];
  // The rows the delete item takes: the selection when the click is on it.
  const targets = rowSelected(r) ? selectedRows() : [r];
  const restore = targets.every(isDeleted);
  const onlyNew = targets.every(isAdded);
  const delItem: MenuItem = onlyNew ? {
    label: t(targets.length > 1 ? 'results:grid.removeNewRows' : 'results:grid.removeNewRow', { count: targets.length }),
    shortcut: rowSel.value && rowSelected(r) ? '⌦' : undefined,
    action: () => toggleDelete(targets),
  } : {
    label: targets.length > 1
      ? t(restore ? 'results:grid.restoreRows' : 'results:grid.deleteRows', { count: targets.length })
      : t(restore ? 'results:grid.restoreRow' : 'results:grid.deleteRow'),
    shortcut: rowSel.value && rowSelected(r) ? '⌦' : undefined,
    danger: !restore && props.deletable,
    disabled: !props.deletable,
    hint: props.deletable ? undefined : (props.noDeleteReason ?? props.noEditReason ?? undefined),
    action: () => toggleDelete(targets),
  };
  const addItem: MenuItem = {
    label: t('results:grid.addRow'),
    disabled: !props.insertable,
    action: () => emit('add'),
  };
  if (c === null) items.push(delItem, addItem);
  if (c !== null) items.push({ label: t('results:grid.copyCell'), shortcut: copyFormat.value === 'tsv' ? '⌘C' : undefined, action: () => copy(tsv(props.rows[r][c])) });
  if (c !== null) items.push({ label: t('results:grid.viewValue'), action: () => openViewer(r, c) });
  if (c !== null) items.push(...linkItems(r, c));
  if (c !== null) {
    items.push({ label: t('results:grid.editCell'), shortcut: 'F2', divided: true, disabled: !canEditCell(r, c), action: () => startEdit(r, c) });
    items.push({ label: t('results:grid.setNull'), disabled: !canEditCell(r, c) || isDeleted(r), action: () => setNull(r, c) });
    items.push(delItem, addItem);
    if (isEdited(r, c)) items.push({ label: t('results:grid.undoChange'), action: () => emit('edit', r, c, undefined) });
    if (!canEditRow(r) && props.noEditReason) items.push({ label: t('results:grid.notEditable', { reason: props.noEditReason }), disabled: true });
    else if (canEditRow(r) && props.readOnlyColumns?.[c]) items.push({ label: t('results:grid.notEditable', { reason: props.readOnlyColumns[c] }), disabled: true });
  }
  const scope = t(rowSel.value || block.value || cellExtra.value.length ? 'results:grid.scopeSelected' : selection.value ? 'results:grid.scopeRow' : 'results:grid.scopeAll');
  items.push({ label: t('results:grid.copyHeader', { scope }), header: true, divided: true });
  for (const f of COPY_FORMATS) {
    items.push({ label: t(`results:grid.copyFormat.${f.id}.label`, f.label), shortcut: f.id === copyFormat.value ? '⌘C' : undefined, action: () => copyAs(f.id) });
  }
  items.push({ label: t('results:grid.copyAllWithHeaders'), action: () => { rowSel.value = null; selection.value = null; clearExtras(); copy(formatRows('tsv_headers', props.columns, props.rows)); } });
  items.push({ label: t('results:grid.copyFormatHeader'), header: true, divided: true });
  for (const f of COPY_FORMATS) {
    items.push({
      label: t(`results:grid.copyFormat.${f.id}.name`, f.name),
      checked: f.id === copyFormat.value,
      action: () => { copyFormat.value = f.id; setDefaultCopyFormat(f.id); },
    });
  }
  if (c !== null) {
    items.push({ label: t('results:grid.hideColumn', { name: props.columns[c].name }), divided: true, disabled: shownCount() < 2, action: () => hide(c) });
  }
  items.push({ label: t('results:grid.columnsMenu'), divided: c === null, action: () => columnsMenu(e) });
  menu.value = { x: e.clientX, y: e.clientY, items };
}

// -- showing and hiding columns ---------------------------------------------------------
const shownCount = () => props.columns.filter((_, i) => !isHidden(i)).length;
function hide(c: number) {
  hiddenNames.value = new Set([...hiddenNames.value, props.columns[c].name]);
  if (selection.value?.c === c) selection.value = null;
}
function toggle(c: number) {
  const name = props.columns[c].name;
  const next = new Set(hiddenNames.value);
  if (next.has(name)) next.delete(name);
  else next.add(name);
  hiddenNames.value = next;
  if (selection.value?.c === c && next.has(name)) selection.value = null;
}
/** The list of columns with a check on the shown ones (right click on the
 *  headers). A hidden column's filter still applies: it's marked. */
function columnsMenu(e: MouseEvent) {
  e.preventDefault();
  const items: MenuItem[] = [{ label: t('results:grid.columns'), header: true }];
  props.columns.forEach((col, c) => {
    const shown = !isHidden(c);
    items.push({
      label: hasFilter(col.name) ? `${col.name} · ${t('results:grid.filtered')}` : col.name,
      checked: shown,
      // At least one column stays.
      disabled: shown && shownCount() < 2,
      action: () => toggle(c),
    });
  });
  items.push({
    label: t('results:grid.showAll'),
    divided: true,
    disabled: !hiddenNames.value.size,
    action: () => { hiddenNames.value = new Set(); },
  });
  menu.value = { x: e.clientX, y: e.clientY, items };
}
</script>

<template>
  <div
    ref="scroller"
    class="rg"
    tabindex="0"
    :style="{ '--rg-row': rowH + 'px' }"
    @scroll="scrollTop = ($event.target as HTMLDivElement).scrollTop; hideSoon(0)"
    @keydown="hideSoon(0); onKey($event)"
    @mousedown="hideSoon(0)"
  >
    <div class="rg-inner" :style="{ width: totalW + 'px', height: (rows.length + HEAD) * rowH + 'px' }">
      <div class="rg-head" :style="{ width: totalW + 'px' }" @contextmenu="columnsMenu">
        <div class="rg-num rg-hcell" :style="{ width: NUM_W + 'px' }">#</div>
        <div
          v-for="(col, c) in columns"
          :key="c"
          v-show="!isHidden(c)"
          class="rg-hcell"
          :class="{ sortable, sorted: !!sortOf(col.name) }"
          :style="{ width: widths[c] + 'px' }"
          :title="headTitle(col)"
          :aria-sort="sortOf(col.name) ? (sortOf(col.name)!.desc ? 'descending' : 'ascending') : undefined"
          @mousedown="onHeadDown"
          @click="onHeadClick($event, c)"
        >
          <span class="rg-hname">{{ col.name }}</span>
          <span v-if="sortOf(col.name)" class="rg-sort" :data-sort="sortOf(col.name)!.desc ? 'desc' : 'asc'">
            <el-icon><CaretBottom v-if="sortOf(col.name)!.desc" /><CaretTop v-else /></el-icon>
            <sup v-if="(sort?.length ?? 0) > 1">{{ sortOf(col.name)!.n }}</sup>
          </span>
          <span class="rg-resize" @pointerdown.prevent.stop="startResize($event, c)" @click.stop />
        </div>
      </div>
      <div v-if="filterable" class="rg-filters" :style="{ width: totalW + 'px' }">
        <div class="rg-num rg-fnum" :style="{ width: NUM_W + 'px' }">
          <el-icon :title="$t('results:grid.columnFilters')"><Filter /></el-icon>
        </div>
        <div
          v-for="(col, c) in columns"
          :key="c"
          v-show="!isHidden(c)"
          class="rg-fcell"
          :class="{ on: hasFilter(col.name) }"
          :style="{ width: widths[c] + 'px' }"
        >
          <input
            v-model="ftext[col.name]"
            class="rg-finput"
            :placeholder="$t('common:filter')"
            spellcheck="false"
            :title="$t('results:grid.filterHelp')"
            @keydown="onFilterKey($event, c)"
            @blur="applyText(c)"
          />
          <button class="rg-fmenu" :title="$t('results:grid.filterOptions')" @mousedown.prevent @click.stop="openFilterMenu($event, c)">
            <el-icon class="rg-kebab"><MoreFilled /></el-icon>
          </button>
        </div>
      </div>
      <div
        v-for="{ r, i } in visible"
        :key="i"
        class="rg-row"
        :class="{ odd: i % 2 === 1, selected: rowSelected(i), deleted: isDeleted(i), added: isAdded(i) }"
        :title="isDeleted(i) ? $t('results:grid.markedDelete') : isAdded(i) ? $t('results:grid.newRow') : undefined"
        :style="{ top: (i + HEAD) * rowH + 'px', width: totalW + 'px' }"
      >
        <div class="rg-num" :style="{ width: NUM_W + 'px' }" @click="selectRow(i, $event)" @contextmenu="onContext($event, i, null)">
          {{ isAdded(i) ? '+' : i + 1 }}
        </div>
        <div
          v-for="(v, c) in r"
          :key="c"
          v-show="!isHidden(c)"
          class="rg-cell"
          :class="[isUnset(i, c) ? 'rg-default' : cellClass(valueAt(i, c)), { active: selection && selection.r === i && selection.c === c, ranged: inBlock(i, c), edited: isEdited(i, c), 'rg-link': modHeld && isLinkCell(i, c) }]"
          :style="{ width: widths[c] + 'px' }"
          :title="isEdited(i, c) ? $t('results:grid.before', { value: display(v) }) : undefined"
          @mousedown="onCellDown($event, i, c)"
          @mouseenter="onCellEnter(i, c); hoverStart($event, i, c)"
          @mouseleave="hideSoon()"
          @dblclick="canEditCell(i, c) ? startEdit(i, c) : openViewer(i, c)"
          @contextmenu="onContext($event, i, c)"
        >{{ isUnset(i, c) ? $t('results:grid.defaultValue') : display(valueAt(i, c)) }}</div>
      </div>
    </div>
    <input
      v-if="editing"
      ref="editInput"
      v-model="editing.text"
      class="rg-editor"
      :style="{ top: (editing.r + HEAD) * rowH + 'px', left: editLeft + 'px', width: Math.max(widths[editing.c], 120) + 'px' }"
      spellcheck="false"
      @keydown="onEditKey"
      @blur="commitEdit()"
    />
    <ContextMenu v-if="menu" :x="menu.x" :y="menu.y" :items="menu.items" @close="menu = null" />
    <CellViewer v-if="viewer" :title="viewer.title" :value="viewer.value" @close="viewer = null" />
    <Teleport to="body">
      <div
        v-if="peek"
        class="rg-peek nm-selectable"
        :style="{ left: peek.left + 'px', top: peek.top + 'px', maxHeight: peek.maxH + 'px' }"
        @mouseenter="keepPeek"
        @mouseleave="hideSoon()"
      ><div class="rg-peek-text">{{ peek.text }}</div><div v-if="peek.cut" class="rg-peek-cut">{{ $t('results:grid.peekCut') }}</div></div>
    </Teleport>
  </div>
</template>

<style>
/* Teleported to <body>: not scoped. */
.rg-peek {
  position: fixed; z-index: 3000; width: 520px; overflow: auto; padding: 8px 10px;
  background: var(--nm-bg-elev); color: var(--nm-text); border: 1px solid var(--nm-border); border-radius: 6px;
  box-shadow: 0 6px 24px rgba(0, 0, 0, 0.35); font-family: var(--nm-mono); font-size: 12px; line-height: 1.45;
}
.rg-peek-text { white-space: pre-wrap; word-break: break-word; }
.rg-peek-cut { margin-top: 6px; color: var(--nm-text-dim); font-family: var(--nm-font, inherit); font-style: italic; }
</style>

<style scoped>
.rg {
  position: relative;
  flex: 1;
  min-height: 0;
  overflow: auto;
  outline: none;
  font-family: var(--nm-mono);
  font-size: var(--nm-grid-font-size);
  background: var(--ide-editor);
}
.rg-inner { position: relative; }
.rg-head {
  position: sticky;
  top: 0;
  z-index: 2;
  display: flex;
  height: var(--rg-row);
  background: var(--ide-sidebar);
  border-bottom: 1px solid var(--nm-border);
}
.rg-hcell {
  position: relative;
  flex-shrink: 0;
  display: flex;
  align-items: center;
  padding: 0 8px;
  font-family: var(--nm-font);
  font-weight: 600;
  font-size: calc(var(--nm-grid-font-size) - 0.5px);
  color: var(--nm-text-strong);
  border-right: 1px solid var(--nm-border-soft);
  overflow: hidden;
}
.rg-hname { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.rg-hcell.sortable { cursor: pointer; user-select: none; }
.rg-hcell.sortable:hover { background: color-mix(in srgb, var(--nm-text) 6%, transparent); }
.rg-sort {
  flex-shrink: 0;
  display: inline-flex;
  align-items: center;
  margin-left: 3px;
  color: var(--ide-focus);
  font-size: 11px;
}
.rg-sort sup { font-size: 9px; line-height: 1; margin-left: 1px; }
.rg-resize {
  position: absolute;
  right: -3px;
  top: 0;
  width: 6px;
  height: 100%;
  cursor: col-resize;
  z-index: 1;
}
.rg-resize:hover { background: var(--ide-focus); }
.rg-filters {
  position: sticky;
  top: var(--rg-row);
  z-index: 2;
  display: flex;
  height: var(--rg-row);
  background: var(--ide-sidebar);
  border-bottom: 1px solid var(--nm-border);
}
.rg-filters .rg-num { z-index: 3; color: var(--nm-text-muted); cursor: default; }
.rg-fcell {
  flex-shrink: 0;
  display: flex;
  align-items: center;
  border-right: 1px solid var(--nm-border-soft);
  overflow: hidden;
}
.rg-finput {
  flex: 1;
  min-width: 0;
  height: 100%;
  box-sizing: border-box;
  padding: 0 6px;
  border: none;
  outline: none;
  background: transparent;
  color: var(--nm-text-strong);
  font: inherit;
  font-family: var(--nm-font);
  font-size: calc(var(--nm-grid-font-size) - 0.5px);
}
.rg-finput::placeholder { color: var(--nm-text-muted); opacity: 0.6; }
.rg-finput:focus { box-shadow: inset 0 0 0 1px var(--ide-focus); background: var(--ide-input, transparent); }
.rg-fcell.on { background: color-mix(in srgb, var(--ide-focus) 16%, transparent); }
.rg-cell.rg-link { cursor: pointer; text-decoration: underline; color: var(--nm-accent-2); }
.rg-fmenu {
  flex-shrink: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 18px;
  height: 100%;
  padding: 0;
  border: none;
  background: transparent;
  color: var(--nm-text-muted);
  cursor: pointer;
}
.rg-fmenu:hover { color: var(--nm-text-strong); background: var(--ide-hover); }
.rg-kebab { font-size: 12px; transform: rotate(90deg); }
.rg-row {
  position: absolute;
  left: 0;
  display: flex;
  height: var(--rg-row);
}
.rg-row.odd { background: var(--nm-overlay-row); }
.rg-row:hover { background: var(--ide-hover); }
.rg-row.selected { background: var(--ide-selection-focus); }
.rg-num {
  position: sticky;
  left: 0;
  z-index: 1;
  flex-shrink: 0;
  display: flex;
  align-items: center;
  justify-content: flex-end;
  padding-right: 8px;
  color: var(--nm-text-muted);
  background: var(--ide-sidebar);
  border-right: 1px solid var(--nm-border-soft);
  cursor: pointer;
}
.rg-head .rg-num { z-index: 3; justify-content: flex-end; }
.rg-cell {
  flex-shrink: 0;
  padding: 0 8px;
  line-height: var(--rg-row);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  border-right: 1px solid var(--nm-border-soft);
  border-bottom: 1px solid var(--nm-overlay-line);
}
.rg-cell.ranged { background: var(--ide-selection); }
.rg-cell.active { outline: 1px solid var(--ide-focus); outline-offset: -1px; background: var(--ide-selection); }
.rg-row.added { background: color-mix(in srgb, var(--nm-success) 12%, transparent); }
.rg-row.added .rg-num { color: var(--nm-success); box-shadow: inset 2px 0 0 var(--nm-success); }
.rg-cell.rg-default { color: var(--nm-text-dim); font-style: italic; }
.rg-row.deleted { background: color-mix(in srgb, var(--nm-danger) 14%, transparent); }
.rg-row.deleted .rg-cell { text-decoration: line-through; text-decoration-color: color-mix(in srgb, var(--nm-danger) 70%, transparent); opacity: 0.7; }
.rg-row.deleted .rg-num { color: var(--nm-danger); box-shadow: inset 2px 0 0 var(--nm-danger); }
.rg-cell.edited { background: color-mix(in srgb, var(--nm-warning) 22%, transparent); box-shadow: inset 2px 0 0 var(--nm-warning); }
.rg-editor {
  position: absolute; z-index: 5; height: var(--rg-row); box-sizing: border-box; padding: 0 5px; border: 1px solid var(--ide-focus, var(--nm-accent));
  background: var(--ide-input); color: var(--nm-text-strong); font: inherit; font-family: var(--nm-mono); font-size: var(--nm-grid-font-size); outline: none;
}
</style>
