<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { t } from '../i18n';
import type { Cell, ResultColumn } from '../api/types';
import type { Edits } from '../composables/gridEdit';
import { columnKind, parseFilter, type FilterState } from '../composables/gridFilter';
import { isSaveShortcut } from '../composables/shortcuts';
import ContextMenu, { type MenuItem } from './ContextMenu.vue';
import CellViewer from './CellViewer.vue';
import CodeEditor from './CodeEditor.vue';

// The result as a tree: one node per row (a document), its fields under it,
// nested objects and arrays as far down as they go. Virtualized like the
// grid: only the nodes in view are in the DOM, and a node's children are
// built when it opens, so a big result with deep documents stays smooth.
// Arrays with many items open in groups of CHUNK ([0…99], [100…199]…).
// Nested values that arrive as JSON text (document engines send nested
// fields like that; SQL json/jsonb columns too) are parsed for the tree.
// Editing works as in the grid and shares its edits: a value (nested too)
// is edited in place, a container as JSON, a nested field can be removed and
// a document marked for deletion. A nested change becomes a new value of its
// top-level field (the whole field, as JSON), so every engine's update code
// applies it; the pane sends it as an object to document engines.

const props = defineProps<{
  columns: ResultColumn[];
  rows: Cell[][];
  /** Edited values (shown), row → column → value. */
  edits?: Edits;
  /** Rows marked for deletion. */
  deleted?: Set<number>;
  /** The last `added` rows are new (not saved yet). */
  added?: number;
  /** "Filtrar por este valor" on top-level fields. */
  filterable?: boolean;
  /** The result's rows can be edited (as in the grid). */
  editable?: boolean;
  /** Why they can't, shown when trying. */
  noEditReason?: string | null;
  /** New rows can be added (their values edited here too). */
  insertable?: boolean;
  /** Rows can be marked for deletion. */
  deletable?: boolean;
  noDeleteReason?: string | null;
}>();
const emit = defineEmits<{
  filter: [column: string, state: FilterState | null];
  /** A top-level field's new value (`undefined` reverts it), as the grid's. */
  edit: [row: number, col: number, value: Cell | undefined];
  /** Mark (`true`) or unmark rows for deletion (new rows are dropped). */
  delete: [rows: number[], mark: boolean];
}>();

const ROW_H = 22;
const INDENT = 16;
const CHUNK = 100;

type Kind = 'object' | 'array' | 'string' | 'number' | 'bool' | 'null' | 'objectId' | 'date' | 'chunk';
type Seg = string | number;
interface TreeNode {
  /** Row index + path, unique: the key of the expanded set. */
  id: string;
  row: number;
  path: Seg[];
  depth: number;
  /** The field name or array index (null: the row itself). */
  key: Seg | null;
  value: unknown;
  kind: Kind;
  /** Children count (objects, arrays, chunks). */
  size: number;
  /** An array chunk's item range, inclusive. */
  range?: [number, number];
}

const SEP = '\u0001';
const nodeId = (row: number, path: Seg[], range?: [number, number]) =>
  `${row}${SEP}${path.join(SEP)}${range ? `${SEP}#${range[0]}-${range[1]}` : ''}`;

// -- values ---------------------------------------------------------------------------------
/** A cell as the tree shows it: JSON text of an object / array is parsed. */
function parseCell(v: Cell): unknown {
  if (typeof v !== 'string') return v;
  const s = v.trim();
  if ((s.startsWith('{') && s.endsWith('}')) || (s.startsWith('[') && s.endsWith(']'))) {
    try { return JSON.parse(s); } catch { /* plain text */ }
  }
  return v;
}
const base = computed(() => props.rows.length - (props.added ?? 0));
function cellAt(r: number, c: number): Cell {
  const e = props.deleted?.has(r) ? undefined : props.edits?.[r];
  return e && c in e ? e[c] : props.rows[r]?.[c] ?? null;
}
/** Parsed rows, built on first use (a row's object only when it's shown). */
let cache = new Map<number, Record<string, unknown>>();
watch(() => [props.rows, props.columns], () => { cache = new Map(); original = new Map(); version.value++; });
// Edits change in place (a cell set in the grid or here): deep.
watch(() => props.edits, () => { cache = new Map(); version.value++; }, { deep: true });
watch(() => props.deleted, () => { version.value++; }, { deep: true });
/** Bumped when the parsed rows change, so the flat list recomputes. */
const version = ref(0);
function rowObject(r: number): Record<string, unknown> {
  let o = cache.get(r);
  if (!o) {
    o = {};
    const unset = r >= base.value;
    props.columns.forEach((col, c) => {
      // A new row's untouched cell isn't part of the document.
      if (unset && !(props.edits?.[r] && c in props.edits[r])) return;
      o![col.name] = parseCell(cellAt(r, c));
    });
    cache.set(r, o);
  }
  return o;
}
/** A field's value as the server sent it (no edits), parsed, for "edited". */
let original = new Map<string, unknown>();
function originalField(r: number, c: number): unknown {
  const k = `${r}:${c}`;
  if (!original.has(k)) original.set(k, parseCell(props.rows[r]?.[c] ?? null));
  return original.get(k);
}
function valueAtPath(v: unknown, path: Seg[]): unknown {
  let cur = v;
  for (const s of path) {
    if (cur === null || typeof cur !== 'object') return undefined;
    cur = (cur as Record<string, unknown>)[s as string];
  }
  return cur;
}
const same = (a: unknown, b: unknown) => a === b || JSON.stringify(a) === JSON.stringify(b);
const colIndex = (name: Seg | undefined) => props.columns.findIndex((c) => c.name === name);
/** The node's value differs from what the server sent (an edit here or in the grid). */
function isEditedNode(n: TreeNode): boolean {
  if (!n.path.length || n.kind === 'chunk' || n.row >= base.value) return false;
  const c = colIndex(n.path[0]);
  if (c < 0 || !(props.edits?.[n.row] && c in props.edits[n.row])) return false;
  return !same(valueAtPath(originalField(n.row, c), n.path.slice(1)), n.value);
}

const columnType = computed(() => new Map(props.columns.map((c) => [c.name, (c.type_name ?? '').toLowerCase()])));
const ISO = /^\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}(:\d{2}(\.\d+)?)?(Z|[+-]\d{2}:?\d{2})?$/;
function kindOf(v: unknown, key: Seg | null, depth: number): Kind {
  if (v === null || v === undefined) return 'null';
  if (Array.isArray(v)) return 'array';
  if (typeof v === 'object') return 'object';
  if (typeof v === 'number') return 'number';
  if (typeof v === 'boolean') return 'bool';
  const s = String(v);
  // Top-level fields: the column's type says it; nested ones: by shape.
  const ty = depth === 1 && typeof key === 'string' ? columnType.value.get(key) ?? '' : '';
  if (/objectid/.test(ty) || (key === '_id' && /^[0-9a-f]{24}$/i.test(s))) return 'objectId';
  if (/(^|\|)(date|datetime|timestamp)/.test(ty) || ISO.test(s)) return 'date';
  return 'string';
}
const sizeOf = (v: unknown) => (Array.isArray(v) ? v.length : v && typeof v === 'object' ? Object.keys(v).length : 0);
const isContainer = (n: TreeNode) => n.kind === 'object' || n.kind === 'array' || n.kind === 'chunk';

function rootNode(r: number): TreeNode {
  const v = rowObject(r);
  return { id: nodeId(r, []), row: r, path: [], depth: 0, key: null, value: v, kind: 'object', size: sizeOf(v) };
}
function childNode(parent: TreeNode, key: Seg, value: unknown): TreeNode {
  const path = [...parent.path, key];
  const depth = parent.depth + 1;
  const kind = kindOf(value, key, path.length);
  return { id: nodeId(parent.row, path), row: parent.row, path, depth, key, value, kind, size: sizeOf(value) };
}
/** A node's children: fields, items, or (big arrays) the chunks of items. */
function children(n: TreeNode): TreeNode[] {
  if (n.kind === 'chunk') {
    const arr = n.value as unknown[];
    const [a, b] = n.range!;
    const out: TreeNode[] = [];
    for (let i = a; i <= b; i++) out.push({ ...childNode({ ...n, depth: n.depth - 1 }, i, arr[i]), depth: n.depth + 1 });
    return out;
  }
  if (n.kind === 'array') {
    const arr = n.value as unknown[];
    if (arr.length > CHUNK) {
      const out: TreeNode[] = [];
      for (let a = 0; a < arr.length; a += CHUNK) {
        const range: [number, number] = [a, Math.min(arr.length - 1, a + CHUNK - 1)];
        out.push({ id: nodeId(n.row, n.path, range), row: n.row, path: n.path, depth: n.depth + 1, key: null, value: arr, kind: 'chunk', size: range[1] - range[0] + 1, range });
      }
      return out;
    }
    return arr.map((v, i) => childNode(n, i, v));
  }
  if (n.kind === 'object') return Object.entries(n.value as Record<string, unknown>).map(([k, v]) => childNode(n, k, v));
  return [];
}

// -- expansion ------------------------------------------------------------------------------
const expanded = ref<Set<string>>(new Set());
watch(() => props.columns, () => { expanded.value = new Set(); selected.value = null; });
function toggle(n: TreeNode, open = !expanded.value.has(n.id)) {
  if (!isContainer(n) || !n.size) return;
  const next = new Set(expanded.value);
  if (open) next.add(n.id); else next.delete(n.id);
  expanded.value = next;
}
/** Opens nodes down to `levels` below the given ones (all rows if none),
 *  up to a cap of visible nodes so "Expandir todo" never freezes the UI. */
const EXPAND_CAP = 20000;
function expandLevels(levels: number, from?: TreeNode[]) {
  const next = new Set(from ? expanded.value : []);
  let count = 0;
  let capped = false;
  const visit = (n: TreeNode, left: number) => {
    if (!isContainer(n) || !n.size || left <= 0) return;
    if (count + n.size > EXPAND_CAP) { capped = true; return; }
    next.add(n.id);
    count += n.size;
    for (const c of children(n)) visit(c, left - 1);
  };
  const roots = from ?? Array.from({ length: props.rows.length }, (_, r) => rootNode(r));
  for (const n of roots) visit(n, levels);
  expanded.value = next;
  if (capped) ElMessage.info({ message: t('results:tree.capped', { count: EXPAND_CAP.toLocaleString() }), duration: 3500 });
}
function collapseAll() {
  expanded.value = new Set();
}

// -- search -----------------------------------------------------------------------------------
const query = ref('');
const needle = ref('');
let searchTimer: ReturnType<typeof setTimeout> | undefined;
watch(query, (q) => { clearTimeout(searchTimer); searchTimer = setTimeout(() => { needle.value = q.trim().toLowerCase(); }, 200); });
/** Matches: node ids whose key or value contains the text, the ids of their
 *  ancestors (opened to show them), and the rows that have any. */
const search = computed(() => {
  void version.value;
  const q = needle.value;
  if (!q) return null;
  const hits = new Set<string>();
  const open = new Set<string>();
  const rows = new Set<number>();
  const order: string[] = [];
  const visit = (n: TreeNode, chain: string[]) => {
    const keyHit = n.key !== null && String(n.key).toLowerCase().includes(q);
    const valHit = !isContainer(n) && String(n.value ?? 'null').toLowerCase().includes(q);
    if (keyHit || valHit) {
      hits.add(n.id);
      order.push(n.id);
      rows.add(n.row);
      for (const a of chain) open.add(a);
    }
    if (isContainer(n)) for (const c of children(n)) visit(c, [...chain, n.id]);
  };
  for (let r = 0; r < props.rows.length; r++) visit(rootNode(r), []);
  return { hits, open, rows, order };
});
const matchIndex = ref(0);
watch(search, () => { matchIndex.value = 0; });

// -- the flat list of visible nodes ------------------------------------------------------------
const flat = computed<TreeNode[]>(() => {
  void version.value;
  const s = search.value;
  const isOpen = (id: string) => expanded.value.has(id) || !!s?.open.has(id);
  const out: TreeNode[] = [];
  const push = (n: TreeNode) => {
    out.push(n);
    if (isContainer(n) && isOpen(n.id)) for (const c of children(n)) push(c);
  };
  for (let r = 0; r < props.rows.length; r++) {
    if (s && !s.rows.has(r)) continue;
    push(rootNode(r));
  }
  return out;
});
const isOpenNode = (n: TreeNode) => expanded.value.has(n.id) || !!search.value?.open.has(n.id);

// -- virtual scroll -------------------------------------------------------------------------
const scroller = ref<HTMLDivElement | null>(null);
const scrollTop = ref(0);
const viewH = ref(400);
const first = computed(() => Math.max(0, Math.floor(scrollTop.value / ROW_H) - 10));
const last = computed(() => Math.min(flat.value.length, Math.ceil((scrollTop.value + viewH.value) / ROW_H) + 10));
const visible = computed(() => flat.value.slice(first.value, last.value).map((n, k) => ({ n, i: first.value + k })));
let ro: ResizeObserver | null = null;
onMounted(() => {
  ro = new ResizeObserver(() => { viewH.value = scroller.value?.clientHeight ?? 400; });
  if (scroller.value) ro.observe(scroller.value);
});
onBeforeUnmount(() => { ro?.disconnect(); clearTimeout(searchTimer); });
function scrollIntoView(i: number) {
  const el = scroller.value;
  if (!el) return;
  const top = i * ROW_H;
  if (top < el.scrollTop) el.scrollTop = top;
  else if (top + ROW_H > el.scrollTop + el.clientHeight) el.scrollTop = top + ROW_H - el.clientHeight;
}

// -- selection and keyboard ------------------------------------------------------------------
const selected = ref<string | null>(null);
const selIndex = computed(() => (selected.value ? flat.value.findIndex((n) => n.id === selected.value) : -1));
function select(i: number) {
  const n = flat.value[i];
  if (!n) return;
  selected.value = n.id;
  scrollIntoView(i);
}
function parentIndex(i: number) {
  const d = flat.value[i]?.depth ?? 0;
  for (let k = i - 1; k >= 0; k--) if (flat.value[k].depth < d) return k;
  return -1;
}
function onKey(e: KeyboardEvent) {
  const i = selIndex.value;
  const n = flat.value[i];
  const mod = e.metaKey || e.ctrlKey;
  if (mod && e.key.toLowerCase() === 'c' && n) { e.preventDefault(); copyValue(n); return; }
  if (mod && e.key.toLowerCase() === 'f') { e.preventDefault(); searchInput.value?.focus(); return; }
  if (n && !mod && !e.altKey) {
    if (e.key === 'F2') { e.preventDefault(); startEdit(n); return; }
    if ((e.key === 'Delete' || e.key === 'Backspace') && n.depth === 0) { e.preventDefault(); toggleDelete(n); return; }
    if (!isContainer(n) && canEditNode(n)) {
      if (e.key === 'Delete' || e.key === 'Backspace') { e.preventDefault(); startEdit(n, ''); return; }
      if (e.key.length === 1 && e.key !== ' ') { e.preventDefault(); startEdit(n, e.key); return; }
    }
  }
  const moves: Record<string, () => void> = {
    ArrowDown: () => select(Math.min(flat.value.length - 1, i + 1)),
    ArrowUp: () => select(Math.max(0, i - 1)),
    Home: () => select(0),
    End: () => select(flat.value.length - 1),
    PageDown: () => select(Math.min(flat.value.length - 1, i + Math.floor(viewH.value / ROW_H))),
    PageUp: () => select(Math.max(0, i - Math.floor(viewH.value / ROW_H))),
    ArrowRight: () => {
      if (!n) return;
      if (isContainer(n) && n.size && !isOpenNode(n)) toggle(n, true);
      else if (isContainer(n) && isOpenNode(n)) select(i + 1);
    },
    ArrowLeft: () => {
      if (!n) return;
      if (isContainer(n) && isOpenNode(n) && expanded.value.has(n.id)) toggle(n, false);
      else { const p = parentIndex(i); if (p >= 0) select(p); }
    },
    Enter: () => { if (n) { if (isContainer(n)) toggle(n); else if (canEditNode(n)) startEdit(n); else openViewer(n); } },
    ' ': () => { if (n && isContainer(n)) toggle(n); },
  };
  const f = moves[e.key];
  if (!f) return;
  e.preventDefault();
  if (i < 0 && e.key !== 'Home' && e.key !== 'End') { select(0); return; }
  f();
}

// -- search navigation ----------------------------------------------------------------------
const searchInput = ref<HTMLInputElement | null>(null);
/** Moves `step` matches (0: shows the current one). */
function goMatch(step: number) {
  const s = search.value;
  if (!s || !s.order.length) return;
  matchIndex.value = (matchIndex.value + step + s.order.length) % s.order.length;
  const id = s.order[matchIndex.value];
  const i = flat.value.findIndex((n) => n.id === id);
  if (i >= 0) { selected.value = id; nextTick(() => scrollIntoView(i)); }
}
function onSearchKey(e: KeyboardEvent) {
  e.stopPropagation();
  if (e.key === 'Enter') {
    e.preventDefault();
    // A new search starts on its first match; Enter again goes on.
    const q = query.value.trim().toLowerCase();
    const fresh = q !== needle.value;
    clearTimeout(searchTimer);
    needle.value = q;
    nextTick(() => goMatch(fresh ? 0 : e.shiftKey ? -1 : 1));
  }
  else if (e.key === 'Escape') { e.preventDefault(); query.value = ''; needle.value = ''; scroller.value?.focus(); }
}
/** The text split around the search matches, for highlighting. */
function parts(text: string): { s: string; hit: boolean }[] {
  const q = needle.value;
  if (!q) return [{ s: text, hit: false }];
  const out: { s: string; hit: boolean }[] = [];
  const low = text.toLowerCase();
  let at = 0;
  for (let k = low.indexOf(q); k >= 0; k = low.indexOf(q, k + q.length)) {
    if (k > at) out.push({ s: text.slice(at, k), hit: false });
    out.push({ s: text.slice(k, k + q.length), hit: true });
    at = k + q.length;
  }
  if (at < text.length) out.push({ s: text.slice(at), hit: false });
  return out;
}

// -- display --------------------------------------------------------------------------------
const MAX_TEXT = 300;
function scalarText(n: TreeNode): string {
  const v = n.value;
  if (v === null || v === undefined) return 'null';
  if (n.kind === 'string') {
    const s = String(v);
    return JSON.stringify(s.length > MAX_TEXT ? s.slice(0, MAX_TEXT) + '…' : s);
  }
  if (n.kind === 'objectId') return `ObjectId("${v}")`;
  return String(v);
}
/** `{ nombre: "Ana", edad: 31, … }` / `[1, 2, 3, …]`: a closed container's preview. */
function preview(v: unknown, budget = 90): string {
  const one = (x: unknown): string => {
    if (x === null || x === undefined) return 'null';
    if (Array.isArray(x)) return `[${x.length}]`;
    if (typeof x === 'object') return '{…}';
    if (typeof x === 'string') return JSON.stringify(x.length > 24 ? x.slice(0, 24) + '…' : x);
    return String(x);
  };
  const items = Array.isArray(v) ? v.map(one) : Object.entries(v as Record<string, unknown>).map(([k, x]) => `${k}: ${one(x)}`);
  let out = '';
  for (const it of items) {
    const next = out ? `${out}, ${it}` : it;
    // A cut array says how many items it has, like a debugger's console.
    if (next.length > budget) return Array.isArray(v) ? `(${v.length}) [${out}, …]` : `{ ${out}, … }`;
    out = next;
  }
  return Array.isArray(v) ? `[${out}]` : `{ ${out} }`;
}
function keyText(n: TreeNode): string {
  if (n.kind === 'chunk') return `[${n.range![0]} … ${n.range![1]}]`;
  if (n.key === null) {
    const id = (n.value as Record<string, unknown>)._id;
    return id !== undefined && id !== null && typeof id !== 'object' ? `${n.row + 1} · _id: ${id}` : String(n.row + 1);
  }
  return typeof n.key === 'number' ? `[${n.key}]` : n.key;
}
function typeLabel(n: TreeNode): string {
  switch (n.kind) {
    case 'object': return n.depth === 0 ? t('results:tree.document') : 'Object';
    case 'array': return 'Array';
    case 'chunk': return '';
    case 'string': return 'String';
    case 'number': return Number.isInteger(n.value) ? 'Int' : 'Double';
    case 'bool': return 'Boolean';
    case 'null': return 'Null';
    case 'objectId': return 'ObjectId';
    case 'date': return 'Date';
  }
}
const rowState = (n: TreeNode) => (props.deleted?.has(n.row) ? 'deleted' : n.row >= base.value ? 'added' : '');

// -- editing ----------------------------------------------------------------------------------
const isDeletedRow = (r: number) => !!props.deleted?.has(r);
/** A row's values can be edited: the result's when editable, new rows when
 *  rows can be added. `_id` (the key) isn't edited here. */
const canEditRow = (r: number) => (r >= base.value ? !!props.insertable : !!props.editable);
function canEditNode(n: TreeNode) {
  return n.path.length > 0 && n.kind !== 'chunk' && !(n.path.length === 1 && n.key === '_id') && canEditRow(n.row) && !isDeletedRow(n.row);
}
function explainNoEdit(n: TreeNode) {
  if (isDeletedRow(n.row)) { ElMessage.info({ message: t('results:grid.rowDeleted'), duration: 3500 }); return; }
  if (n.path.length === 1 && n.key === '_id') { ElMessage.info({ message: t('results:tree.idNotEditable'), duration: 3500 }); return; }
  if (!canEditRow(n.row) && props.noEditReason) ElMessage.info({ message: t('results:grid.cantEdit', { reason: props.noEditReason }), duration: 3500 });
}
/** Sets (or removes) the value at the node's path: a top-level scalar goes as
 *  is; anything else rewrites its top-level field as JSON text, the way the
 *  driver sends nested values. Back to the server's value: the edit goes. */
function change(n: TreeNode, op: { set: unknown } | { remove: true }) {
  const [field, ...rest] = n.path;
  const c = colIndex(field);
  if (c < 0) return;
  const r = n.row;
  let next: unknown;
  if (!rest.length) {
    if ('remove' in op) return;
    next = op.set;
  } else {
    const root = structuredClone(rowObject(r)[field as string]);
    let parent = root as Record<string, unknown> | unknown[];
    for (const s of rest.slice(0, -1)) parent = (parent as Record<string, unknown>)[s as string] as Record<string, unknown>;
    const last = rest[rest.length - 1];
    if ('remove' in op) {
      if (Array.isArray(parent)) parent.splice(last as number, 1);
      else delete (parent as Record<string, unknown>)[last as string];
    } else (parent as Record<string, unknown>)[last as string] = op.set;
    next = root;
  }
  const cell: Cell = next !== null && typeof next === 'object' ? JSON.stringify(next) : (next as Cell);
  if (r < base.value && same(parseCell(cell), originalField(r, c))) { emit('edit', r, c, undefined); return; }
  emit('edit', r, c, cell);
}

const editing = ref<{ id: string; text: string } | null>(null);
const editInput = ref<HTMLInputElement[] | HTMLInputElement | null>(null);
function startEdit(n: TreeNode, text?: string) {
  if (!canEditNode(n)) { explainNoEdit(n); return; }
  if (isContainer(n)) { openJson(n); return; }
  selected.value = n.id;
  editing.value = { id: n.id, text: text ?? (n.value === null || n.value === undefined ? '' : String(n.value)) };
  nextTick(() => {
    const el = Array.isArray(editInput.value) ? editInput.value[0] : editInput.value;
    el?.focus();
    if (text === undefined) el?.select();
  });
}
/** The typed text as a value like the one it replaces: numbers stay numbers,
 *  booleans booleans; a null field takes JSON (`12`, `true`, `{…}`) or text. */
function typed(n: TreeNode, text: string): unknown {
  const v = text.trim();
  switch (n.kind) {
    case 'number': return v === '' ? null : Number.isNaN(Number(v)) ? text : Number(v);
    case 'bool':
      if (/^(true|verdadero|1|sí|si)$/i.test(v)) return true;
      if (/^(false|falso|0|no)$/i.test(v)) return false;
      return v === '' ? null : text;
    case 'null':
      if (v === '') return null;
      try { return JSON.parse(v); } catch { return text; }
    default: return text;
  }
}
function commitEdit(move = 0) {
  const e = editing.value;
  if (!e) return;
  editing.value = null;
  const i = flat.value.findIndex((x) => x.id === e.id);
  const n = flat.value[i];
  if (n) {
    const v = typed(n, e.text);
    if (!same(v, n.value)) change(n, { set: v });
    if (move) nextTick(() => select(Math.max(0, Math.min(flat.value.length - 1, i + move))));
  }
  scroller.value?.focus();
}
function cancelEdit() {
  editing.value = null;
  scroller.value?.focus();
}
function onEditKey(ev: KeyboardEvent) {
  if (ev.key === 'Enter') { ev.preventDefault(); commitEdit(1); }
  else if (ev.key === 'Tab') { ev.preventDefault(); commitEdit(ev.shiftKey ? -1 : 1); }
  else if (ev.key === 'Escape') { ev.preventDefault(); cancelEdit(); }
  // ⌘S keeps the value and goes on to the pane, which saves.
  else if (isSaveShortcut(ev)) { commitEdit(); return; }
  ev.stopPropagation();
}
watch(() => props.rows, () => { editing.value = null; });

/** "Editar como JSON…": an object or array (or any value) typed as JSON. */
const jsonEdit = ref<{ node: TreeNode; text: string; error: string | null } | null>(null);
function openJson(n: TreeNode) {
  if (!canEditNode(n)) { explainNoEdit(n); return; }
  jsonEdit.value = { node: n, text: JSON.stringify(n.value ?? null, null, 2), error: null };
}
function saveJson() {
  const j = jsonEdit.value;
  if (!j) return;
  let v: unknown;
  try { v = JSON.parse(j.text); } catch (e) {
    j.error = t('results:addDocument.invalidJson', { error: e instanceof Error ? e.message : String(e) });
    return;
  }
  jsonEdit.value = null;
  if (!same(v, j.node.value)) change(j.node, { set: v });
}
function toggleDelete(n: TreeNode) {
  if (n.row >= base.value) { emit('delete', [n.row], true); return; }
  if (!props.deletable) {
    const reason = props.noDeleteReason ?? props.noEditReason;
    if (reason) ElMessage.info({ message: t('results:grid.cantDelete', { reason }), duration: 3500 });
    return;
  }
  emit('delete', [n.row], !isDeletedRow(n.row));
}

// -- copy / viewer / menu --------------------------------------------------------------------
async function copy(text: string) {
  await navigator.clipboard.writeText(text);
  ElMessage.success({ message: t('common:copied'), duration: 1200 });
}
const asJson = (n: TreeNode) => {
  const v = n.kind === 'chunk' ? (n.value as unknown[]).slice(n.range![0], n.range![1] + 1) : n.value;
  return typeof v === 'string' ? v : JSON.stringify(v ?? null, null, 2);
};
function copyValue(n: TreeNode) { copy(asJson(n)); }
/** `cliente.items[0].nombre`: the path as MongoDB / JSONPath-ish dotted text. */
function pathText(n: TreeNode): string {
  return n.path.reduce<string>((acc, s) => (typeof s === 'number' ? `${acc}[${s}]` : acc ? `${acc}.${s}` : s), '');
}
const viewer = ref<{ title: string; value: Cell } | null>(null);
function openViewer(n: TreeNode) {
  viewer.value = { title: pathText(n) || keyText(n), value: asJson(n) };
}
const menu = ref<{ x: number; y: number; items: MenuItem[] } | null>(null);
function onContext(e: MouseEvent, i: number) {
  e.preventDefault();
  const n = flat.value[i];
  selected.value = n.id;
  const editable = canEditNode(n);
  const items: MenuItem[] = [];
  if (n.depth === 0) {
    const restore = isDeletedRow(n.row);
    items.push({
      label: n.row >= base.value ? t('results:grid.removeNewRow') : t(restore ? 'results:tree.restoreDocument' : 'results:tree.deleteDocument'),
      danger: !restore && n.row < base.value && props.deletable, shortcut: '⌦',
      disabled: n.row < base.value && !props.deletable,
      hint: n.row < base.value && !props.deletable ? (props.noDeleteReason ?? props.noEditReason ?? undefined) : undefined,
      action: () => toggleDelete(n),
    });
  } else if (n.kind !== 'chunk') {
    if (!isContainer(n)) items.push({ label: t('results:tree.editValue'), shortcut: 'F2', disabled: !editable, action: () => startEdit(n) });
    items.push({ label: t('results:tree.editJson'), disabled: !editable, action: () => openJson(n) });
    items.push({ label: t('results:grid.setNull'), disabled: !editable || n.value === null, action: () => change(n, { set: null }) });
    if (n.path.length > 1) items.push({ label: t('results:tree.removeField'), danger: editable, disabled: !editable, action: () => change(n, { remove: true }) });
    if (isEditedNode(n)) {
      items.push({ label: t('results:grid.undoChange'), action: () => change(n, { set: valueAtPath(originalField(n.row, colIndex(n.path[0])), n.path.slice(1)) ?? null }) });
    }
    if (!editable && props.noEditReason && !canEditRow(n.row)) items.push({ label: t('results:grid.notEditable', { reason: props.noEditReason }), disabled: true });
  }
  items.push(
    { label: t('results:tree.copyValue'), shortcut: '⌘C', divided: items.length > 0, action: () => copyValue(n) },
    { label: t('results:tree.copyPath'), disabled: !n.path.length, action: () => copy(pathText(n)) },
    { label: t('results:tree.copyDocument'), action: () => copy(JSON.stringify(rowObject(n.row), null, 2)) },
    { label: t('results:tree.viewValue'), action: () => openViewer(n) },
  );
  if (isContainer(n) && n.size) {
    items.push({ label: t('results:tree.expandBelow'), divided: true, action: () => expandLevels(64, [n]) });
    items.push({ label: t('results:tree.collapse'), disabled: !expanded.value.has(n.id), action: () => toggle(n, false) });
  }
  // A top-level scalar field: filter the data by it (server side, like the grid's filters).
  const col = n.path.length === 1 && typeof n.key === 'string' ? props.columns.find((c) => c.name === n.key) : undefined;
  if (props.filterable && col && !isContainer(n)) {
    const raw = n.value === null ? 'NULL' : `=${n.value}`;
    items.push({
      label: t('results:tree.filterBy', { name: col.name }), divided: true,
      action: () => emit('filter', col.name, { text: raw, filters: parseFilter(col.name, columnKind(col.type_name ?? '', n.value as Cell), raw) }),
    });
  }
  menu.value = { x: e.clientX, y: e.clientY, items };
}
</script>

<template>
  <div class="jt">
    <div class="jt-bar">
      <input
        ref="searchInput"
        v-model="query"
        class="jt-search"
        :placeholder="$t('results:tree.search')"
        spellcheck="false"
        @keydown="onSearchKey"
      />
      <span v-if="search" class="jt-count">
        {{ search.order.length ? $t('results:tree.matches', { at: matchIndex + 1, count: search.order.length }) : $t('results:tree.noMatches') }}
      </span>
      <button v-if="search?.order.length" class="jt-btn" :title="$t('results:tree.prevMatch')" @click="goMatch(-1)"><el-icon><ei-arrow-up /></el-icon></button>
      <button v-if="search?.order.length" class="jt-btn" :title="$t('results:tree.nextMatch')" @click="goMatch(1)"><el-icon><ei-arrow-down /></el-icon></button>
      <div class="nm-spacer" />
      <span class="jt-label">{{ $t('results:tree.expand') }}</span>
      <button v-for="l in [1, 2, 3]" :key="l" class="jt-btn" :title="$t('results:tree.levels', { count: l })" @click="expandLevels(l)">{{ l }}</button>
      <button class="jt-btn" :title="$t('results:tree.expandAll')" @click="expandLevels(64)"><el-icon><ei-plus /></el-icon></button>
      <button class="jt-btn" :title="$t('results:tree.collapseAll')" @click="collapseAll"><el-icon><ei-minus /></el-icon></button>
    </div>
    <div ref="scroller" class="jt-body" tabindex="0" role="tree" @keydown="onKey" @scroll="scrollTop = ($event.target as HTMLElement).scrollTop">
      <div class="jt-inner" :style="{ height: flat.length * ROW_H + 'px' }">
        <div
          v-for="{ n, i } in visible"
          :key="n.id"
          class="jt-row"
          :class="[rowState(n), { sel: selected === n.id, root: n.depth === 0, hit: search?.hits.has(n.id) }]"
          :style="{ top: i * ROW_H + 'px', paddingLeft: 6 + n.depth * INDENT + 'px' }"
          role="treeitem"
          :aria-level="n.depth + 1"
          :aria-expanded="isContainer(n) ? isOpenNode(n) : undefined"
          @mousedown="selected = n.id"
          @click="isContainer(n) && ($event.target as HTMLElement).closest('.jt-twisty') ? toggle(n) : null"
          @dblclick="isContainer(n) ? toggle(n) : canEditNode(n) ? startEdit(n) : openViewer(n)"
          @contextmenu="onContext($event, i)"
        >
          <span class="jt-twisty" :class="{ open: isOpenNode(n), leaf: !isContainer(n) || !n.size }">
            <el-icon v-if="isContainer(n) && n.size"><ei-arrow-right /></el-icon>
          </span>
          <span class="jt-key" :class="{ idx: typeof n.key === 'number' || n.kind === 'chunk' || n.key === null }">
            <template v-for="(p, k) in parts(keyText(n))" :key="k"><mark v-if="p.hit">{{ p.s }}</mark><template v-else>{{ p.s }}</template></template>
          </span>
          <template v-if="n.kind !== 'chunk'">
            <span class="jt-colon">:</span>
            <span v-if="isContainer(n)" class="jt-preview" :class="{ edited: isEditedNode(n) }">
              <template v-if="isOpenNode(n)">{{ n.kind === 'array' ? `[ ${n.size} ]` : `{ ${n.size} }` }}</template>
              <template v-else>{{ preview(n.value) }}</template>
            </span>
            <input
              v-else-if="editing?.id === n.id"
              ref="editInput"
              v-model="editing.text"
              class="jt-editor"
              spellcheck="false"
              @keydown="onEditKey"
              @blur="commitEdit()"
              @mousedown.stop
              @dblclick.stop
            />
            <span v-else class="jt-val" :class="['k-' + n.kind, { edited: isEditedNode(n) }]" :title="isEditedNode(n) ? $t('results:tree.edited') : undefined">
              <template v-for="(p, k) in parts(scalarText(n))" :key="k"><mark v-if="p.hit">{{ p.s }}</mark><template v-else>{{ p.s }}</template></template>
            </span>
          </template>
          <span class="jt-type">{{ typeLabel(n) }}</span>
        </div>
      </div>
      <div v-if="!flat.length" class="jt-empty nm-muted">{{ search ? $t('results:tree.noMatches') : $t('results:tree.empty') }}</div>
    </div>
    <ContextMenu v-if="menu" :x="menu.x" :y="menu.y" :items="menu.items" @close="menu = null" />
    <CellViewer v-if="viewer" :title="viewer.title" :value="viewer.value" @close="viewer = null" />
    <el-dialog
      :model-value="!!jsonEdit"
      :title="jsonEdit ? $t('results:tree.editJsonTitle', { path: pathText(jsonEdit.node) }) : ''"
      width="620px"
      append-to-body
      @close="jsonEdit = null"
    >
      <div v-if="jsonEdit" class="jt-json">
        <CodeEditor v-model="jsonEdit.text" language="json" @save="saveJson" />
      </div>
      <div v-if="jsonEdit?.error" class="jt-json-error" role="alert">{{ jsonEdit.error }}</div>
      <template #footer>
        <el-button @click="jsonEdit = null">{{ $t('common:cancel') }}</el-button>
        <el-button type="primary" @click="saveJson">{{ $t('results:tree.apply') }}</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.jt { display: flex; flex-direction: column; flex: 1; min-height: 0; background: var(--ide-editor); }
.jt-bar { display: flex; align-items: center; gap: 4px; padding: 3px 8px; border-bottom: 1px solid var(--nm-border-soft); font-size: 11.5px; }
.jt-search {
  width: 220px; padding: 2px 6px; font: inherit; color: var(--nm-text); background: var(--ide-input);
  border: 1px solid var(--nm-border); border-radius: 3px; outline: none;
}
.jt-search:focus { border-color: var(--ide-focus); }
.jt-count { color: var(--nm-text-dim); padding: 0 4px; }
.jt-label { color: var(--nm-text-dim); }
.jt-btn {
  display: inline-flex; align-items: center; justify-content: center; min-width: 22px; height: 20px; padding: 0 5px;
  font: inherit; color: var(--nm-text); background: transparent; border: 1px solid var(--nm-border); border-radius: 3px; cursor: pointer;
}
.jt-btn:hover { color: var(--nm-text-strong); border-color: #5a5a5a; }
.jt-body { position: relative; flex: 1; min-height: 0; overflow: auto; outline: none; font-family: var(--nm-mono); font-size: 12px; }
.jt-inner { position: relative; min-width: 100%; }
.jt-row {
  position: absolute; left: 0; right: 0; height: 22px; display: flex; align-items: center; gap: 4px;
  white-space: nowrap; cursor: default; padding-right: 8px;
}
.jt-row:hover { background: var(--ide-hover); }
.jt-row.sel { background: var(--ide-selection); }
.jt-body:focus .jt-row.sel { box-shadow: inset 0 0 0 1px var(--ide-focus); }
.jt-row.root { border-top: 1px solid var(--nm-border-soft); }
.jt-row.hit { background: color-mix(in srgb, var(--nm-warning) 12%, transparent); }
.jt-row.deleted { text-decoration: line-through; opacity: 0.6; }
.jt-row.added { background: color-mix(in srgb, var(--nm-success) 12%, transparent); }
.jt-twisty { display: inline-flex; align-items: center; justify-content: center; width: 14px; flex-shrink: 0; color: var(--nm-text-dim); cursor: pointer; }
.jt-twisty .el-icon { transition: transform 0.1s; }
.jt-twisty.open .el-icon { transform: rotate(90deg); }
.jt-twisty.leaf { cursor: default; }
.jt-key { color: #9cdcfe; }
.jt-key.idx { color: var(--nm-text-dim); }
.jt-colon { color: var(--nm-text-dim); }
.jt-preview { color: var(--nm-text-dim); overflow: hidden; text-overflow: ellipsis; min-width: 0; }
.jt-val { overflow: hidden; text-overflow: ellipsis; min-width: 0; }
.k-string { color: #ce9178; }
.k-number { color: #b5cea8; }
.k-bool { color: #569cd6; }
.k-null { color: #569cd6; font-style: italic; }
.k-objectId { color: #4ec9b0; }
.k-date { color: #d7ba7d; }
.jt-type { margin-left: auto; padding-left: 12px; color: var(--nm-text-dim); font-family: var(--nm-font); font-size: 10.5px; flex-shrink: 0; }
mark { background: color-mix(in srgb, var(--nm-warning) 45%, transparent); color: inherit; border-radius: 2px; }
.jt-editor {
  flex: 1; min-width: 120px; height: 18px; padding: 0 4px; font: inherit; color: var(--nm-text-strong);
  background: var(--ide-input); border: 1px solid var(--ide-focus); border-radius: 2px; outline: none;
}
.edited { background: color-mix(in srgb, var(--nm-warning) 22%, transparent); border-radius: 2px; }
.jt-json { height: 320px; border: 1px solid var(--nm-border); border-radius: 3px; overflow: hidden; display: flex; }
.jt-json > * { flex: 1; min-width: 0; }
.jt-json-error { margin-top: 8px; color: var(--nm-danger); font-size: 12px; white-space: pre-wrap; }
.jt-empty { padding: 16px; font-family: var(--nm-font); }
</style>
