<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { t } from '../i18n';
import { Filter, MoreFilled } from '@element-plus/icons-vue';
import { coerce, sameValue, type Edits } from '../composables/gridEdit';
import { isSaveShortcut } from '../composables/shortcuts';
import type { Cell, ResultColumn } from '../api/types';
import ContextMenu, { type MenuItem } from './ContextMenu.vue';
import CellViewer from './CellViewer.vue';
import { columnKind, describe, menuFor, parseFilter, type ColumnFilter, type FilterMenuEntry, type FilterState } from '../composables/gridFilter';
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
}>();
const emit = defineEmits<{
  /** A cell's new value; `undefined` reverts it. */
  edit: [row: number, col: number, value: Cell | undefined];
  /** Mark (`true`) or unmark rows for deletion. */
  delete: [rows: number[], mark: boolean];
  /** A column's filter changed; `null` clears it. */
  filter: [column: string, state: FilterState | null];
}>();

const ROW_H = 22;
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
/** Columns the user hid, by name. Hidden columns keep their index (edits,
 *  selection and filters use it) and are only left out of the layout. */
const hiddenNames = ref<Set<string>>(new Set());

function measure(text: string) {
  return Math.min(360, Math.max(64, text.length * 7.2 + 20));
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
    scroller.value?.scrollTo({ top: 0, left: 0 });
  },
  { immediate: true },
);

const isHidden = (c: number) => hiddenNames.value.has(props.columns[c]?.name);
/** Width of the columns before `c` (or all of them), hidden ones left out. */
const widthUpTo = (c: number) => widths.value.slice(0, c).reduce((a, w, i) => (isHidden(i) ? a : a + w), 0);
const totalW = computed(() => NUM_W + widthUpTo(widths.value.length));
/** The next shown column from `c` in direction `d` (itself if none). */
function step(c: number, d: number) {
  for (let x = c + d; x >= 0 && x < props.columns.length; x += d) if (!isHidden(x)) return x;
  return c;
}
const first = computed(() => Math.max(0, Math.floor(scrollTop.value / ROW_H) - 10));
const last = computed(() => Math.min(props.rows.length, Math.ceil((scrollTop.value + viewH.value) / ROW_H) + 10));
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
/** The value shown: the edit, if any (a row marked for deletion shows its
 *  original values: its edits are ignored). */
function valueAt(r: number, c: number): Cell {
  const e = isDeleted(r) ? undefined : props.edits?.[r];
  return e && c in e ? e[c] : props.rows[r]?.[c] ?? null;
}
const isEdited = (r: number, c: number) => !isDeleted(r) && !!props.edits?.[r] && c in props.edits[r];

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
  scroller.value?.focus();
}
/** A click on a cell: shift extends the block from the active cell, and
 *  dragging with the button down extends it to the cell under the pointer. */
let dragging = false;
function onCellDown(e: MouseEvent, r: number, c: number) {
  if (e.button !== 0) return;
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
const inBlock = (r: number, c: number) => {
  const b = block.value;
  return !!b && r >= b.r0 && r <= b.r1 && c >= b.c0 && c <= b.c1;
};
function selectRow(r: number, e: MouseEvent) {
  if (e.shiftKey && rowSel.value) rowSel.value = { from: rowSel.value.from, to: r };
  else rowSel.value = { from: r, to: r };
  selection.value = null;
  anchor.value = null;
  scroller.value?.focus();
}
const rowSelected = (i: number) => {
  const s = rowSel.value;
  return !!s && i >= Math.min(s.from, s.to) && i <= Math.max(s.from, s.to);
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
  const s = rowSel.value;
  if (s) return props.rows.slice(Math.min(s.from, s.to), Math.max(s.from, s.to) + 1);
  if (selection.value) return [props.rows[selection.value.r]];
  return props.rows;
}

function copyAs(format: CopyFormat) {
  // A block of cells: those columns of those rows, shown columns only, with
  // the values as shown (edits included).
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
    if (props.editable && (e.key === 'Backspace' || e.key === 'Delete')) { e.preventDefault(); startEdit(sel.r, sel.c, ''); return; }
    if (props.editable && e.key.length === 1) { e.preventDefault(); startEdit(sel.r, sel.c, e.key); return; }
  }
  if (rowSel.value && !mod && !e.altKey && (e.key === 'Backspace' || e.key === 'Delete')) { e.preventDefault(); toggleDelete(selectedRows()); return; }
  if (mod && e.key.toLowerCase() === 'c') { e.preventDefault(); copyAs(copyFormat.value); return; }
  if (mod && e.key.toLowerCase() === 'a') { e.preventDefault(); rowSel.value = { from: 0, to: props.rows.length - 1 }; selection.value = null; anchor.value = null; return; }
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
  else anchor.value = null;
  selection.value = { r, c };
  const el = scroller.value;
  if (el) {
    const top = r * ROW_H;
    const below = ROW_H * (HEAD.value + 1);
    if (top < el.scrollTop) el.scrollTop = top;
    else if (top + below > el.scrollTop + el.clientHeight) el.scrollTop = top + below - el.clientHeight;
  }
}

// -- editing -----------------------------------------------------------------------
const editing = ref<{ r: number; c: number; text: string } | null>(null);
const editInput = ref<HTMLInputElement | null>(null);
const editLeft = computed(() => (editing.value ? NUM_W + widthUpTo(editing.value.c) : 0));

function startEdit(r: number, c: number, text?: string) {
  if (!props.editable) {
    if (props.noEditReason) ElMessage.info({ message: t('results:grid.cantEdit', { reason: props.noEditReason }), duration: 3500 });
    return;
  }
  if (isDeleted(r)) { ElMessage.info({ message: t('results:grid.rowDeleted'), duration: 3500 }); return; }
  const v = valueAt(r, c);
  selection.value = { r, c };
  anchor.value = null;
  editing.value = { r, c, text: text ?? (v === null ? '' : String(v)) };
  nextTick(() => {
    editInput.value?.focus();
    if (text === undefined) editInput.value?.select();
  });
}
function commitEdit(move: [number, number] | null = null) {
  const e = editing.value;
  if (!e) return;
  editing.value = null;
  const original = props.rows[e.r]?.[e.c] ?? null;
  const prev = valueAt(e.r, e.c);
  // Leaving a NULL untouched keeps it NULL (the input shows it empty).
  if (!(prev === null && e.text === '')) {
    const v = coerce(original, e.text);
    emit('edit', e.r, e.c, sameValue(v, original) ? undefined : v);
  }
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
  if (!props.editable || isDeleted(r)) return startEdit(r, c);
  emit('edit', r, c, props.rows[r]?.[c] === null ? undefined : null);
}
watch(() => props.rows, () => { editing.value = null; });

// -- deleting rows (marked here; the DELETE code comes with the UPDATEs) -----------------
function selectedRows(): number[] {
  const s = rowSel.value;
  if (!s) return [];
  const out: number[] = [];
  for (let i = Math.min(s.from, s.to); i <= Math.max(s.from, s.to); i++) out.push(i);
  return out;
}
/** Marks the rows, or unmarks them when they're all marked already. */
function toggleDelete(rows: number[]) {
  if (!rows.length) return;
  if (!props.deletable) {
    const reason = props.noDeleteReason ?? props.noEditReason;
    if (reason) ElMessage.info({ message: t('results:grid.cantDelete', { reason }), duration: 3500 });
    return;
  }
  if (editing.value && rows.includes(editing.value.r)) editing.value = null;
  emit('delete', rows, !rows.every(isDeleted));
}

// -- column resize -------------------------------------------------------------
function startResize(e: PointerEvent, c: number) {
  const start = e.clientX;
  const from = widths.value[c];
  const move = (ev: PointerEvent) => { widths.value[c] = Math.max(40, from + ev.clientX - start); };
  const up = () => { window.removeEventListener('pointermove', move); window.removeEventListener('pointerup', up); };
  window.addEventListener('pointermove', move);
  window.addEventListener('pointerup', up);
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
  const delItem: MenuItem = {
    label: targets.length > 1
      ? t(restore ? 'results:grid.restoreRows' : 'results:grid.deleteRows', { count: targets.length })
      : t(restore ? 'results:grid.restoreRow' : 'results:grid.deleteRow'),
    shortcut: rowSel.value && rowSelected(r) ? '⌦' : undefined,
    danger: !restore && props.deletable,
    disabled: !props.deletable,
    hint: props.deletable ? undefined : (props.noDeleteReason ?? props.noEditReason ?? undefined),
    action: () => toggleDelete(targets),
  };
  if (c === null) items.push(delItem);
  if (c !== null) items.push({ label: t('results:grid.copyCell'), shortcut: copyFormat.value === 'tsv' ? '⌘C' : undefined, action: () => copy(tsv(props.rows[r][c])) });
  if (c !== null) items.push({ label: t('results:grid.viewValue'), action: () => openViewer(r, c) });
  if (c !== null) {
    items.push({ label: t('results:grid.editCell'), shortcut: 'F2', divided: true, disabled: !props.editable, action: () => startEdit(r, c) });
    items.push({ label: t('results:grid.setNull'), disabled: !props.editable || isDeleted(r), action: () => setNull(r, c) });
    items.push(delItem);
    if (isEdited(r, c)) items.push({ label: t('results:grid.undoChange'), action: () => emit('edit', r, c, undefined) });
    if (!props.editable && props.noEditReason) items.push({ label: t('results:grid.notEditable', { reason: props.noEditReason }), disabled: true });
  }
  const scope = t(rowSel.value || block.value ? 'results:grid.scopeSelected' : selection.value ? 'results:grid.scopeRow' : 'results:grid.scopeAll');
  items.push({ label: t('results:grid.copyHeader', { scope }), header: true, divided: true });
  for (const f of COPY_FORMATS) {
    items.push({ label: t(`results:grid.copyFormat.${f.id}.label`, f.label), shortcut: f.id === copyFormat.value ? '⌘C' : undefined, action: () => copyAs(f.id) });
  }
  items.push({ label: t('results:grid.copyAllWithHeaders'), action: () => { rowSel.value = null; selection.value = null; copy(formatRows('tsv_headers', props.columns, props.rows)); } });
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
    @scroll="scrollTop = ($event.target as HTMLDivElement).scrollTop"
    @keydown="onKey"
  >
    <div class="rg-inner" :style="{ width: totalW + 'px', height: (rows.length + HEAD) * ROW_H + 'px' }">
      <div class="rg-head" :style="{ width: totalW + 'px' }" @contextmenu="columnsMenu">
        <div class="rg-num rg-hcell" :style="{ width: NUM_W + 'px' }">#</div>
        <div
          v-for="(col, c) in columns"
          :key="c"
          v-show="!isHidden(c)"
          class="rg-hcell"
          :style="{ width: widths[c] + 'px' }"
          :title="col.type_name ? `${col.name} (${col.type_name})` : col.name"
        >
          <span class="rg-hname">{{ col.name }}</span>
          <span class="rg-resize" @pointerdown.prevent.stop="startResize($event, c)" />
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
        :class="{ odd: i % 2 === 1, selected: rowSelected(i), deleted: isDeleted(i) }"
        :title="isDeleted(i) ? $t('results:grid.markedDelete') : undefined"
        :style="{ top: (i + HEAD) * ROW_H + 'px', width: totalW + 'px' }"
      >
        <div class="rg-num" :style="{ width: NUM_W + 'px' }" @click="selectRow(i, $event)" @contextmenu="onContext($event, i, null)">
          {{ i + 1 }}
        </div>
        <div
          v-for="(v, c) in r"
          :key="c"
          v-show="!isHidden(c)"
          class="rg-cell"
          :class="[cellClass(valueAt(i, c)), { active: selection && selection.r === i && selection.c === c, ranged: inBlock(i, c), edited: isEdited(i, c) }]"
          :style="{ width: widths[c] + 'px' }"
          :title="isEdited(i, c) ? $t('results:grid.before', { value: display(v) }) : undefined"
          @mousedown="onCellDown($event, i, c)"
          @mouseenter="onCellEnter(i, c)"
          @dblclick="editable ? startEdit(i, c) : openViewer(i, c)"
          @contextmenu="onContext($event, i, c)"
        >{{ display(valueAt(i, c)) }}</div>
      </div>
    </div>
    <input
      v-if="editing"
      ref="editInput"
      v-model="editing.text"
      class="rg-editor"
      :style="{ top: (editing.r + HEAD) * ROW_H + 'px', left: editLeft + 'px', width: Math.max(widths[editing.c], 120) + 'px' }"
      spellcheck="false"
      @keydown="onEditKey"
      @blur="commitEdit()"
    />
    <ContextMenu v-if="menu" :x="menu.x" :y="menu.y" :items="menu.items" @close="menu = null" />
    <CellViewer v-if="viewer" :title="viewer.title" :value="viewer.value" @close="viewer = null" />
  </div>
</template>

<style scoped>
.rg {
  position: relative;
  flex: 1;
  min-height: 0;
  overflow: auto;
  outline: none;
  font-family: var(--nm-mono);
  font-size: 12px;
  background: var(--ide-editor);
}
.rg-inner { position: relative; }
.rg-head {
  position: sticky;
  top: 0;
  z-index: 2;
  display: flex;
  height: 22px;
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
  font-size: 11.5px;
  color: var(--nm-text-strong);
  border-right: 1px solid var(--nm-border-soft);
  overflow: hidden;
}
.rg-hname { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
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
  top: 22px;
  z-index: 2;
  display: flex;
  height: 22px;
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
  font-size: 11.5px;
}
.rg-finput::placeholder { color: var(--nm-text-muted); opacity: 0.6; }
.rg-finput:focus { box-shadow: inset 0 0 0 1px var(--ide-focus); background: var(--ide-input, transparent); }
.rg-fcell.on { background: color-mix(in srgb, var(--ide-focus) 16%, transparent); }
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
  height: 22px;
}
.rg-row.odd { background: rgba(255, 255, 255, 0.018); }
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
  line-height: 22px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  border-right: 1px solid var(--nm-border-soft);
  border-bottom: 1px solid rgba(255, 255, 255, 0.03);
}
.rg-cell.ranged { background: var(--ide-selection); }
.rg-cell.active { outline: 1px solid var(--ide-focus); outline-offset: -1px; background: var(--ide-selection); }
.rg-row.deleted { background: color-mix(in srgb, var(--nm-danger) 14%, transparent); }
.rg-row.deleted .rg-cell { text-decoration: line-through; text-decoration-color: color-mix(in srgb, var(--nm-danger) 70%, transparent); opacity: 0.7; }
.rg-row.deleted .rg-num { color: var(--nm-danger); box-shadow: inset 2px 0 0 var(--nm-danger); }
.rg-cell.edited { background: color-mix(in srgb, var(--nm-warning) 22%, transparent); box-shadow: inset 2px 0 0 var(--nm-warning); }
.rg-editor {
  position: absolute; z-index: 5; height: 22px; box-sizing: border-box; padding: 0 5px; border: 1px solid var(--ide-focus, var(--nm-accent));
  background: var(--ide-input, #1e1e1e); color: var(--nm-text-strong); font: inherit; font-family: var(--nm-mono); font-size: 12px; outline: none;
}
</style>
