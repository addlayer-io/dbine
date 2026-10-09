<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, reactive, ref, watch } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { invoke } from '@tauri-apps/api/core';
import { errorMessage } from '../api/client';
import type { Field, Language, QueryOutcome } from '../api/types';
import type { DdlParts, DesignerSpec, ForeignKeyDef, IndexDef, TableSchema } from '../api/schema-types';
import type { CodeObject, SyncScript } from '../api/compare';
import CodeEditor from '../components/CodeEditor.vue';
import { useTranslation } from 'i18next-vue';
import { tb } from '../i18n/backend';
import { lineComment } from '../composables/scriptComment';
import { isProdConnection } from '../composables/tags';
import { useConnectionsStore } from '../stores/connections';

// Designer for a new table / collection / index / key… in any engine. What
// it offers comes entirely from the driver's DesignerSpec; it builds a
// TableSchema and the driver turns it into DDL in its own language
// (`table_ddl`), which is previewed live and run with `execute_query`.
//
// With `alter`, it edits the existing table `initial` ("Modificar tabla…"):
// the script is the engine's ALTER from the table as loaded to the one being
// designed (`schema_sync_script`, as "Comparar esquemas"), reviewed before
// it runs. A column renamed in the grid goes through `resolveRename` (the
// "Renombrar" impact and script); those scripts run first, then the ALTER.

export type DesignerSection = 'columns' | 'indexes' | 'foreign_keys' | 'options' | 'script';

const props = withDefaults(defineProps<{
  connectionId: string;
  database: string;
  spec: DesignerSpec;
  schemas?: string[];
  /** Tables of the database, for the foreign key target pickers. */
  existingTables?: { schema: string | null; name: string; columns: string[] }[];
  initial?: TableSchema;
  /** Driver language / dialect, for the script preview. */
  language?: Language;
  dialect?: string;
  /** Sub-tab shown first. */
  initialSection?: DesignerSection;
  /** Edit `initial` in place instead of creating a new table. */
  alter?: boolean;
  /** Edit mode: the rename script for an existing column (the user reviewed
   *  its impact), or null when the user cancelled. Absent: names are fixed. */
  resolveRename?: ((from: string, to: string) => Promise<{ script: SyncScript; rewritten: CodeObject[] } | null>) | null;
  /** Edit mode: why existing columns can't be renamed here, if they can't. */
  renameBlocked?: string | null;
  /** Edit mode: run the script all or nothing (engines with DDL in transactions). */
  atomic?: boolean;
  /** Edit mode: the database's code objects (views over the table are made
   *  again around the ALTER, and the triggers of a rebuilt table). */
  codeObjects?: CodeObject[];
}>(), {
  schemas: () => [], existingTables: () => [], initial: undefined,
  language: 'sql', dialect: '', initialSection: 'columns',
  alter: false, resolveRename: null, renameBlocked: null, atomic: false, codeObjects: () => [],
});

const emit = defineEmits<{
  created: [table: TableSchema];
  altered: [table: TableSchema];
  close: [];
  'open-script': [ddl: string];
}>();

const { t } = useTranslation();

const inTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
const sessionId = `designer:${crypto.randomUUID()}`;

// -- state ---------------------------------------------------------------------------
type OptValues = Record<string, string | boolean>;

interface ColRow {
  key: number;
  name: string;
  data_type: string;
  nullable: boolean;
  default_value: string;
  pk: boolean;
  auto_increment: boolean;
  comment: string;
  options: OptValues;
  /** Edit mode: the column's name in the database (null: a new column). */
  orig: string | null;
  /** Edit mode: the name last confirmed (a rename reviewed, or the original). */
  confirmed: string;
}
interface IdxRow {
  key: number; name: string; columns: string[]; unique: boolean; kind: string; filter: string;
  /** Edit mode: the index's name in the database (null: a new one). */
  orig?: string | null;
}
interface FkRow {
  key: number; name: string; columns: string[]; target: string; ref_columns: string[];
  on_delete: string; on_update: string;
}

let seq = 0;
const nextKey = () => ++seq;

const name = ref('');
const schema = ref<string | null>(null);
const cols = ref<ColRow[]>([]);
const idxs = ref<IdxRow[]>([]);
const fks = ref<FkRow[]>([]);
const tableComment = ref('');
const tableOpts = reactive<OptValues>({});
const pkName = ref<string | null>(null);

function fieldDefault(f: Field): string | boolean {
  return f.kind.type === 'bool' ? f.default === 'true' : f.default;
}
function optsIn(fields: Field[], src: Record<string, string> | undefined): OptValues {
  const out: OptValues = {};
  for (const f of fields) {
    const v = src?.[f.key];
    out[f.key] = v === undefined ? fieldDefault(f) : f.kind.type === 'bool' ? v === 'true' : v;
  }
  return out;
}
function optsOut(fields: Field[], vals: OptValues): Record<string, string> {
  const out: Record<string, string> = {};
  for (const f of fields) {
    const v = vals[f.key];
    if (f.kind.type === 'bool') out[f.key] = v ? 'true' : 'false';
    else if (String(v ?? '').trim() !== '') out[f.key] = String(v).trim();
  }
  return out;
}

const INT_RE = /^(u?(tiny|small|medium|big)?int(eger)?\d*|int\d+|u?int\d+|(big|small)?serial\d*|number|long|counter|identity)\b|^(numeric|decimal|number)\s*\(\s*\d+\s*(,\s*0\s*)?\)/i;
const isIntegerType = (t: string) => INT_RE.test(t.trim());

function defaultIdType(): string {
  const t = props.spec.data_types;
  return t.find((x) => /^(int|integer)$/i.test(x)) ?? t.find(isIntegerType) ?? 'int';
}

function newCol(o: Partial<ColRow> = {}): ColRow {
  return {
    key: nextKey(), name: '', data_type: '', nullable: true, default_value: '', pk: false,
    auto_increment: false, comment: '', options: optsIn(props.spec.column_options, undefined), orig: null, confirmed: '', ...o,
  };
}

function load() {
  const t = props.initial;
  const spec = props.spec;
  name.value = t?.name ?? '';
  schema.value = t?.schema
    ?? (spec.schemas ? (['dbo', 'public'].find((s) => props.schemas.includes(s)) ?? props.schemas[0] ?? null) : null);
  pkName.value = t?.primary_key?.name ?? null;
  const pkCols = new Set(t?.primary_key?.columns ?? []);
  if (t) {
    cols.value = t.columns.map((c) => newCol({
      name: c.name, data_type: c.data_type, nullable: c.nullable, default_value: c.default_value ?? '',
      pk: pkCols.has(c.name), auto_increment: c.auto_increment, comment: c.comment ?? '',
      options: optsIn(spec.column_options, c.options),
      orig: props.alter ? c.name : null, confirmed: c.name,
    }));
  } else if (spec.primary_key) {
    cols.value = [newCol({ name: 'id', data_type: defaultIdType(), nullable: false, pk: true, auto_increment: spec.auto_increment })];
  } else {
    cols.value = [];
  }
  idxs.value = (t?.indexes ?? []).map((i) => ({
    key: nextKey(), name: i.name, columns: [...i.columns], unique: i.unique, kind: i.kind ?? '', filter: i.filter ?? '',
    orig: props.alter ? i.name : null,
  }));
  fks.value = (t?.foreign_keys ?? []).map((f) => ({
    key: nextKey(), name: f.name ?? '', columns: [...f.columns], target: targetKey(f.ref_schema, f.ref_table),
    ref_columns: [...f.ref_columns], on_delete: f.on_delete ?? '', on_update: f.on_update ?? '',
  }));
  tableComment.value = t?.comment ?? '';
  for (const k of Object.keys(tableOpts)) delete tableOpts[k];
  Object.assign(tableOpts, optsIn(spec.table_options, t?.options));
  renames.clear();
}
/** Edit mode: reviewed renames, by the column's name in the database. */
const renames = reactive(new Map<string, { to: string; script: SyncScript; rewritten: CodeObject[] }>());
let baseline: TableSchema | null = null;
load();
watch(() => [props.spec, props.initial], () => { load(); takeBaseline(); });

// -- sections ------------------------------------------------------------------------
/** SQL tables have columns; documents and the like, fields. */
const isColumns = computed(() => props.language === 'sql' || props.spec.primary_key);
const noun = computed(() => t(isColumns.value ? 'designer:sections.columns' : 'designer:sections.fields'));
const sections = computed(() => {
  const s: { id: DesignerSection; label: string; count?: number }[] = [
    { id: 'columns', label: noun.value, count: cols.value.length },
  ];
  if (props.spec.indexes) s.push({ id: 'indexes', label: t('designer:sections.indexes'), count: idxs.value.length });
  if (props.spec.foreign_keys) s.push({ id: 'foreign_keys', label: t('designer:sections.foreignKeys'), count: fks.value.length });
  if (props.spec.table_options.length || props.spec.comments) s.push({ id: 'options', label: t('designer:sections.options') });
  s.push({ id: 'script', label: props.language === 'sql' ? 'SQL' : 'Script' });
  return s;
});
const section = ref<DesignerSection>(props.initialSection);
watch(sections, (s) => { if (!s.some((x) => x.id === section.value)) section.value = 'columns'; }, { immediate: true });

// -- the TableSchema being designed ---------------------------------------------------------
const table = computed<TableSchema>(() => {
  const spec = props.spec;
  const targets = targetMap.value;
  // Edit mode: what the designer doesn't show is kept as the table has it
  // (CHECKs, index INCLUDE and settings, options it doesn't offer, the key's
  // column order), so a rebuilt table or a recreated index doesn't lose it.
  const src = props.alter ? props.initial : undefined;
  const now = new Map(cols.value.filter((c) => c.orig).map((c) => [c.orig!, c.name.trim()]));
  const cur = (n: string) => (now.has(n) ? now.get(n)! : n);
  const alive = (n: string) => !!n && colNames.value.includes(n);
  let pk = cols.value.filter((c) => c.pk && c.name.trim()).map((c) => c.name.trim());
  const pkOrder = (src?.primary_key?.columns ?? []).map(cur);
  if (pkOrder.length) pk = [...pkOrder.filter((n) => pk.includes(n)), ...pk.filter((n) => !pkOrder.includes(n))];
  const srcCol = (c: ColRow) => (c.orig ? src?.columns.find((x) => x.name === c.orig) : undefined);
  const srcIdx = (i: IdxRow) => (i.orig ? src?.indexes.find((x) => x.name === i.orig) : undefined);
  return {
    kind: spec.kind,
    schema: spec.schemas ? schema.value || null : null,
    name: name.value.trim(),
    columns: cols.value.filter((c) => c.name.trim() || c.data_type.trim()).map((c) => ({
      name: c.name.trim(),
      data_type: c.data_type.trim(),
      nullable: spec.nullability ? c.nullable && !c.pk : !c.pk,
      default_value: spec.defaults && c.default_value.trim() ? c.default_value.trim() : null,
      auto_increment: spec.auto_increment && c.auto_increment,
      comment: spec.comments && c.comment.trim() ? c.comment.trim() : null,
      options: { ...srcCol(c)?.options, ...optsOut(spec.column_options, c.options) },
    })),
    primary_key: spec.primary_key && pk.length ? { name: pkName.value, columns: pk } : null,
    foreign_keys: spec.foreign_keys
      ? fks.value.map((f): ForeignKeyDef => {
        const t = targets.get(f.target);
        return {
          name: f.name.trim() || null, columns: [...f.columns],
          ref_schema: t?.schema ?? null, ref_table: t?.name ?? '', ref_columns: [...f.ref_columns],
          on_delete: f.on_delete || null, on_update: f.on_update || null,
        };
      })
      : [],
    indexes: spec.indexes
      ? idxs.value.map((i): IndexDef => {
        const o = srcIdx(i);
        return {
          name: i.name.trim(), columns: [...i.columns], unique: i.unique,
          kind: i.kind.trim() || null, filter: i.filter.trim() || null,
          ...(o?.include?.length ? { include: o.include.map(cur).filter(alive) } : {}),
          ...(o?.options && Object.keys(o.options).length ? { options: { ...o.options } } : {}),
        };
      })
      : [],
    ...(src?.checks?.length ? { checks: JSON.parse(JSON.stringify(src.checks)) } : {}),
    comment: spec.comments && tableComment.value.trim() ? tableComment.value.trim() : null,
    options: { ...src?.options, ...optsOut(spec.table_options, tableOpts) },
  };
});

const colNames = computed(() => cols.value.map((c) => c.name.trim()).filter(Boolean));

// -- validation ------------------------------------------------------------------------------
interface Issue { level: 'error' | 'warning'; section: DesignerSection; text: string }

const issues = computed(() => {
  const out: Issue[] = [];
  const spec = props.spec;
  const err = (section: DesignerSection, text: string) => out.push({ level: 'error', section, text });
  const warn = (section: DesignerSection, text: string) => out.push({ level: 'warning', section, text });

  if (!name.value.trim()) err('columns', t('designer:issues.nameMissing'));
  const named = cols.value.filter((c) => c.name.trim() || c.data_type.trim());
  if (spec.columns_required && !named.length) err('columns', t(isColumns.value ? 'designer:issues.addColumn' : 'designer:issues.addField'));
  for (const [i, c] of cols.value.entries()) {
    const label = c.name.trim() || t('designer:issues.row', { n: i + 1 });
    if (!c.name.trim() && c.data_type.trim()) err('columns', t('designer:issues.rowNameMissing', { n: i + 1 }));
    if (c.name.trim() && !c.data_type.trim()) err('columns', t('designer:issues.typeMissing', { label }));
    if (spec.auto_increment && c.auto_increment && c.data_type.trim() && !isIntegerType(c.data_type)) {
      warn('columns', t('designer:issues.autoIncNotInteger', { label, type: c.data_type.trim() }));
    }
    if (spec.auto_increment && c.auto_increment && spec.primary_key && !c.pk) {
      warn('columns', t('designer:issues.autoIncOutsidePk', { label }));
    }
  }
  for (const d of dupNames.value) err('columns', t('designer:issues.duplicateName', { name: d }));
  if (props.alter) {
    for (const c of cols.value) {
      if (c.orig && c.name.trim() && c.name.trim() !== c.confirmed) err('columns', t('designer:alter.confirmName', { name: c.confirmed }));
    }
  }
  if (spec.auto_increment && cols.value.filter((c) => c.auto_increment).length > 1) {
    warn('columns', t('designer:issues.multipleAutoInc'));
  }
  if (spec.primary_key && spec.kind === 'table' && named.length && !named.some((c) => c.pk)) {
    warn('columns', t('designer:issues.noPrimaryKey'));
  }

  if (spec.indexes) {
    const seen = new Set<string>();
    for (const [i, x] of idxs.value.entries()) {
      const label = x.name.trim() || t('designer:issues.index', { n: i + 1 });
      if (!x.name.trim()) err('indexes', t('designer:issues.indexNameMissing', { n: i + 1 }));
      else if (seen.has(x.name.trim().toLowerCase())) err('indexes', t('designer:issues.duplicateIndex', { name: x.name.trim() }));
      seen.add(x.name.trim().toLowerCase());
      if (!x.columns.length) err('indexes', t('designer:issues.pickColumn', { label }));
      for (const c of x.columns) if (!colNames.value.includes(c)) warn('indexes', t('designer:issues.notAColumn', { label, column: c }));
    }
  }
  if (spec.foreign_keys) {
    for (const [i, f] of fks.value.entries()) {
      const label = f.name.trim() || t('designer:issues.foreignKey', { n: i + 1 });
      if (!f.columns.length) err('foreign_keys', t('designer:issues.pickColumns', { label }));
      if (!f.target) err('foreign_keys', t('designer:issues.pickTargetTable', { label }));
      else if (!f.ref_columns.length) err('foreign_keys', t('designer:issues.pickTargetColumns', { label }));
      if (f.columns.length && f.ref_columns.length && f.columns.length !== f.ref_columns.length) {
        err('foreign_keys', t('designer:issues.columnCountMismatch', { label, columns: f.columns.length, refColumns: f.ref_columns.length }));
      }
      if (f.on_delete === 'SET NULL' || f.on_update === 'SET NULL') {
        const notNull = f.columns.filter((n) => cols.value.some((c) => c.name.trim() === n && (c.pk || (spec.nullability && !c.nullable))));
        if (notNull.length) warn('foreign_keys', t('designer:issues.setNullOnNotNull', { label, columns: notNull.join(', ') }));
      }
    }
  }
  for (const f of spec.table_options) {
    if (f.required && String(tableOpts[f.key] ?? '').trim() === '') err('options', t('designer:issues.optionRequired', { option: tb(f.label) }));
  }
  for (const f of spec.column_options) {
    if (!f.required || f.kind.type === 'bool') continue;
    for (const c of named) if (String(c.options[f.key] ?? '').trim() === '') err('columns', t('designer:issues.columnOptionMissing', { column: c.name.trim() || t('designer:issues.unnamed'), option: tb(f.label) }));
  }
  return out;
});
const errors = computed(() => issues.value.filter((i) => i.level === 'error'));
const warnings = computed(() => issues.value.filter((i) => i.level === 'warning'));
const sectionHasError = (s: DesignerSection) => errors.value.some((e) => e.section === s);

const dupNames = computed(() => {
  const seen = new Map<string, number>();
  for (const n of colNames.value) seen.set(n.toLowerCase(), (seen.get(n.toLowerCase()) ?? 0) + 1);
  return [...seen.entries()].filter(([, n]) => n > 1).map(([k]) => k);
});
const nameInvalid = (c: ColRow) =>
  (!c.name.trim() && !!c.data_type.trim()) || (!!c.name.trim() && dupNames.value.includes(c.name.trim().toLowerCase()));
const typeInvalid = (c: ColRow) => !!c.name.trim() && !c.data_type.trim();
const aiWarn = (c: ColRow) => c.auto_increment && !!c.data_type.trim() && !isIntegerType(c.data_type);

// -- columns grid --------------------------------------------------------------------------
const grid = ref<HTMLElement | null>(null);

function focusRow(key: number, cls = 'dz-name') {
  nextTick(() => {
    grid.value?.querySelector<HTMLInputElement>(`tr[data-row="${key}"] .${cls} input`)?.focus();
  });
}

function addCol(after?: ColRow) {
  const c = newCol();
  const i = after ? cols.value.indexOf(after) + 1 : cols.value.length;
  cols.value.splice(i, 0, c);
  focusRow(c.key);
}

function removeCol(c: ColRow) {
  const i = cols.value.indexOf(c);
  if (i < 0) return;
  cols.value.splice(i, 1);
  if (c.orig) renames.delete(c.orig);
  const n = c.name.trim();
  if (n) {
    for (const x of idxs.value) x.columns = x.columns.filter((y) => y !== n);
    for (const f of fks.value) {
      const k = f.columns.indexOf(n);
      if (k >= 0) { f.columns.splice(k, 1); f.ref_columns.splice(k, 1); }
    }
  }
  const next = cols.value[Math.min(i, cols.value.length - 1)];
  if (next) focusRow(next.key);
}

function moveCol(c: ColRow, delta: number) {
  const i = cols.value.indexOf(c);
  const j = i + delta;
  if (i < 0 || j < 0 || j >= cols.value.length) return;
  cols.value.splice(i, 1);
  cols.value.splice(j, 0, c);
}

/** Renaming a column keeps the indexes and foreign keys that use it. */
function renameCol(c: ColRow, v: string) {
  const old = c.name.trim();
  c.name = v;
  const now = v.trim();
  if (!old || old === now) return;
  const swap = (a: string[]) => a.map((x) => (x === old ? now : x)).filter(Boolean);
  for (const x of idxs.value) x.columns = swap(x.columns);
  for (const f of fks.value) {
    f.columns = swap(f.columns);
    if (isSelf(f.target)) f.ref_columns = swap(f.ref_columns);
  }
}

/** Edit mode: an existing column's new name, once typed (Enter or leaving
 *  the field), goes through the rename's impact review; cancelled, the name
 *  goes back. */
const renaming = ref(false);
async function confirmName(c: ColRow) {
  if (!props.alter || !c.orig || renaming.value) return;
  const now = c.name.trim();
  if (now === c.confirmed || !now || dupNames.value.includes(now.toLowerCase())) return;
  if (now === c.orig) {
    renames.delete(c.orig);
    c.confirmed = now;
    return;
  }
  if (!props.resolveRename) { renameCol(c, c.confirmed); return; }
  renaming.value = true;
  try {
    const got = await props.resolveRename(c.orig, now);
    if (got) {
      renames.set(c.orig, { to: now, script: got.script, rewritten: got.rewritten });
      c.confirmed = now;
    } else {
      renameCol(c, c.confirmed);
    }
  } catch (e) {
    ElMessage.error(errorMessage(e));
    renameCol(c, c.confirmed);
  } finally {
    renaming.value = false;
  }
}

function setPk(c: ColRow, v: boolean) {
  c.pk = v;
  if (v) c.nullable = false;
}

function setAi(c: ColRow, v: boolean) {
  c.auto_increment = v;
  if (v && props.spec.primary_key && !cols.value.some((x) => x.pk)) setPk(c, true);
}

function onRowKey(e: KeyboardEvent, c: ColRow) {
  const mod = e.metaKey || e.ctrlKey;
  if (e.altKey && (e.key === 'ArrowUp' || e.key === 'ArrowDown')) {
    e.preventDefault();
    const cls = (e.target as HTMLElement).closest('td')?.classList[0];
    moveCol(c, e.key === 'ArrowUp' ? -1 : 1);
    if (cls) focusRow(c.key, cls);
  } else if (mod && e.key === 'Enter') {
    e.preventDefault();
    addCol(c);
  } else if (mod && e.key === 'Backspace') {
    e.preventDefault();
    removeCol(c);
  } else if (!mod && !e.altKey && e.key === 'Enter' && (e.target as HTMLElement).tagName === 'INPUT'
    && !(e.target as HTMLElement).closest('.el-autocomplete') && c === cols.value[cols.value.length - 1] && c.name.trim()) {
    e.preventDefault();
    addCol(c);
  }
}

// drag & drop reorder (from the handle)
const dragKey = ref<number | null>(null);
const dropKey = ref<number | null>(null);
function onDragStart(e: DragEvent, c: ColRow) {
  dragKey.value = c.key;
  e.dataTransfer?.setData('text/plain', String(c.key));
  if (e.dataTransfer) e.dataTransfer.effectAllowed = 'move';
}
function onDragOver(e: DragEvent, c: ColRow) {
  if (dragKey.value === null) return;
  e.preventDefault();
  dropKey.value = c.key;
}
function onDrop(c: ColRow) {
  const from = cols.value.findIndex((x) => x.key === dragKey.value);
  const to = cols.value.indexOf(c);
  if (from >= 0 && to >= 0 && from !== to) {
    const [m] = cols.value.splice(from, 1);
    cols.value.splice(to, 0, m);
  }
  dragKey.value = dropKey.value = null;
}
function onDragEnd() { dragKey.value = dropKey.value = null; }

function typeSuggestions(q: string, cb: (items: { value: string }[]) => void) {
  const s = q.trim().toLowerCase();
  const all = props.spec.data_types;
  const hits = s ? all.filter((t) => t.toLowerCase().includes(s)) : all;
  // Prefix matches first.
  hits.sort((a, b) => Number(!a.toLowerCase().startsWith(s)) - Number(!b.toLowerCase().startsWith(s)));
  cb(hits.map((value) => ({ value })));
}

function selectOptions(f: Field): [string, string][] {
  return f.kind.type === 'select' ? f.kind.options.map(([v, l]): [string, string] => [v, tb(l)]) : [];
}

// -- indexes -------------------------------------------------------------------------------
function addIndex() {
  const base = `${props.spec.kind === 'collection' ? 'idx' : 'ix'}_${name.value.trim() || t('designer:defaultTableName')}`;
  let n = idxs.value.length + 1;
  while (idxs.value.some((i) => i.name === `${base}_${n}`)) n++;
  idxs.value.push({ key: nextKey(), name: `${base}_${n}`, columns: [], unique: false, kind: '', filter: '' });
}

// -- foreign keys --------------------------------------------------------------------------
const SELF = '\u0000self';
/** A foreign key to this same table (new: SELF; edit mode: the table's own key). */
const isSelf = (target: string) => target === SELF || (props.alter && target === targetKey(props.spec.schemas ? schema.value || null : null, name.value.trim()));
function targetKey(s: string | null, n: string): string {
  return `${s ?? ''}\u0001${n}`;
}
const targetMap = computed(() => {
  const m = new Map<string, { schema: string | null; name: string; columns: string[]; label: string }>();
  for (const t of props.existingTables) {
    m.set(targetKey(t.schema, t.name), { ...t, label: t.schema ? `${t.schema}.${t.name}` : t.name });
  }
  m.set(SELF, {
    schema: props.spec.schemas ? schema.value || null : null, name: name.value.trim(), columns: colNames.value,
    label: t('designer:foreignKeys.selfTarget', { name: name.value.trim() || t('designer:foreignKeys.selfUnnamed') }),
  });
  return m;
});
const targetOptions = computed(() =>
  [...targetMap.value.entries()].map(([value, t]) => ({ value, label: t.label })).sort((a, b) =>
    (a.value === SELF ? -1 : b.value === SELF ? 1 : a.label.localeCompare(b.label))));

const FK_ACTIONS = ['CASCADE', 'SET NULL', 'SET DEFAULT', 'RESTRICT', 'NO ACTION'];

function addFk() {
  const base = `fk_${name.value.trim() || t('designer:defaultTableName')}`;
  let n = fks.value.length + 1;
  while (fks.value.some((f) => f.name === `${base}_${n}`)) n++;
  fks.value.push({ key: nextKey(), name: `${base}_${n}`, columns: [], target: '', ref_columns: [], on_delete: '', on_update: '' });
}

function setTarget(f: FkRow, v: string) {
  f.target = v;
  const t = targetMap.value.get(v);
  if (!t) { f.ref_columns = []; return; }
  // Guess: the target's primary key is usually its `id`.
  const guess = t.columns.find((c) => /^id$/i.test(c)) ?? t.columns[0];
  f.ref_columns = guess && f.columns.length <= 1 ? [guess] : [];
  if (!f.columns.length) {
    const fkCol = colNames.value.find((c) => c.toLowerCase() === `${t.name.toLowerCase()}_id`);
    if (fkCol) f.columns = [fkCol];
  }
}

// -- script preview -------------------------------------------------------------------------
const PARTS: DdlParts = { drop: false, if_exists: false, create: true, indexes: true, foreign_keys: true };
const ddl = ref('');
const ddlError = ref<string | null>(null);
const ddlLoading = ref(false);
let ddlSeq = 0;
let ddlTimer: ReturnType<typeof setTimeout> | null = null;

function fetchDdl(t: TableSchema): Promise<string> {
  if (props.alter) return alterScript().then(scriptText);
  return invoke<string>('table_ddl', { args: { connection_id: props.connectionId, database: props.database, table: t, parts: PARTS } });
}

/** Edit mode: the table as loaded, with the reviewed renames applied (they
 *  run first, so the ALTER starts from there). */
function renamedBaseline(): TableSchema {
  const b = JSON.parse(JSON.stringify(baseline)) as TableSchema;
  const to = new Map([...renames].map(([from, r]) => [from, r.to]));
  const f = (n: string) => to.get(n) ?? n;
  for (const c of b.columns) c.name = f(c.name);
  if (b.primary_key) b.primary_key.columns = b.primary_key.columns.map(f);
  for (const i of b.indexes) {
    i.columns = i.columns.map(f);
    if (i.include) i.include = i.include.map(f);
  }
  for (const k of b.foreign_keys) {
    k.columns = k.columns.map(f);
    if (k.ref_table === b.name && (k.ref_schema ?? null) === (b.schema ?? null)) k.ref_columns = k.ref_columns.map(f);
  }
  return b;
}

/** Edit mode: the rename scripts, then the engine's ALTER for the rest. */
const review = ref<SyncScript | null>(null);
async function alterScript(): Promise<SyncScript> {
  // The code objects as they'll be once the renames ran (their rewritten views).
  const same = (a: CodeObject, b: CodeObject) => a.kind === b.kind && a.name === b.name && (a.schema ?? null) === (b.schema ?? null);
  const rewritten = [...renames.values()].flatMap((r) => r.rewritten);
  const views = props.codeObjects.map((o) => rewritten.find((r) => same(r, o)) ?? o);
  const rest = await invoke<SyncScript>('schema_sync_script', {
    args: { connection_id: props.connectionId, tables: [{ op: 'alter', old: renamedBaseline(), new: table.value }], objects: [], views },
  });
  const pre = [...renames.values()].map((r) => r.script);
  const out = {
    statements: [...pre.flatMap((x) => x.statements), ...rest.statements],
    warnings: [...pre.flatMap((x) => x.warnings), ...rest.warnings],
  };
  review.value = out;
  return out;
}

function scriptText(sc: SyncScript): string {
  const c = props.language === 'sql' ? '--' : '//';
  // Warnings carry server text (names, CHECK expressions): one line each.
  const warn = sc.warnings.map((w) => lineComment(c, `⚠ ${tb(w)}`)).join('\n');
  if (!sc.statements.length) return `${warn ? `${warn}\n\n` : ''}${c} ${t(sc.warnings.length ? 'designer:alter.nothingRuns' : 'designer:alter.noChanges')}`;
  return `${warn ? `${warn}\n\n` : ''}${sc.statements.join('\n\n')}`;
}

async function refreshDdl() {
  if (!inTauri) return;
  if (!table.value.name) { ddl.value = ''; ddlError.value = null; return; }
  const my = ++ddlSeq;
  ddlLoading.value = true;
  try {
    const text = await fetchDdl(table.value);
    if (my === ddlSeq) { ddl.value = text; ddlError.value = null; }
  } catch (e) {
    if (my === ddlSeq) ddlError.value = errorMessage(e);
  } finally {
    if (my === ddlSeq) ddlLoading.value = false;
  }
}

watch([table, section], () => {
  if (section.value !== 'script') return;
  if (ddlTimer) clearTimeout(ddlTimer);
  ddlTimer = setTimeout(refreshDdl, 300);
}, { deep: true, immediate: true });

const previewText = computed(() => {
  if (!inTauri) {
    const c = props.language === 'sql' ? '--' : '//';
    return `${c} ${t('designer:script.previewUnavailable')}\n${c} ${t('designer:script.object')}: ${JSON.stringify(table.value, null, 2).split('\n').join(`\n${c} `)}`;
  }
  if (!table.value.name) return '';
  return ddl.value;
});

/** "Nueva tabla" → "tabla" (the spec label, translated, without its "new"). */
const objectNoun = (label: string) => label.replace(/^(nuev[oa]|new|nov[oa]|nouvel(le)?|nouveau|nuov[oa])\s+/i, '');

// -- actions ------------------------------------------------------------------------------
const creating = ref(false);
const runError = ref<string | null>(null);

async function freshDdl(): Promise<string | null> {
  if (!inTauri) { ElMessage.info(t('designer:script.noBackend')); return null; }
  try {
    const text = await fetchDdl(table.value);
    ddl.value = text;
    ddlError.value = null;
    return text;
  } catch (e) {
    runError.value = errorMessage(e);
    return null;
  }
}

async function create() {
  if (errors.value.length) {
    ElMessage.warning(errors.value[0].text);
    section.value = errors.value[0].section;
    return;
  }
  creating.value = true;
  runError.value = null;
  try {
    const text = await freshDdl();
    if (text === null) return;
    const outcome = await invoke<QueryOutcome>('execute_query', {
      args: {
        session_id: sessionId, connection_id: props.connectionId, database: props.database,
        sql: text, max_rows: 1, query_id: null, plan: 'none',
      },
    });
    if (outcome.error) { runError.value = outcome.error; return; }
    ElMessage.success(t('designer:createdMessage', { object: objectNoun(tb(props.spec.label)).replace(/^./, (x) => x.toUpperCase()), name: table.value.name }));
    emit('created', table.value);
  } catch (e) {
    runError.value = errorMessage(e);
  } finally {
    creating.value = false;
  }
}

/** Edit mode, step 1: the script, in the SQL tab, for review. */
const reviewing = ref(false);
async function startReview() {
  if (errors.value.length) {
    ElMessage.warning(errors.value[0].text);
    section.value = errors.value[0].section;
    return;
  }
  runError.value = null;
  const text = await freshDdl();
  if (text === null) return;
  if (!review.value?.statements.length) {
    // Nothing runs: say why when the engine left changes out.
    if (review.value?.warnings.length) {
      section.value = 'script';
      ElMessage.warning(t('designer:alter.nothingRuns'));
    } else {
      ElMessage.info(t('designer:alter.noChanges'));
    }
    return;
  }
  section.value = 'script';
  reviewing.value = true;
}
watch(table, () => { reviewing.value = false; }, { deep: true });

/** Edit mode, step 2: run what was reviewed. */
async function applyReviewed() {
  const sc = review.value;
  if (!sc?.statements.length) return;
  if (sc.warnings.length) {
    const ok = await ElMessageBox.confirm(sc.warnings.map((w) => `• ${tb(w)}`).join('\n'), t('designer:alter.warningsTitle'), {
      type: 'warning', confirmButtonText: t('designer:alter.runAnyway'), cancelButtonText: t('common:cancel'),
    }).then(() => true, () => false);
    if (!ok) return;
  }
  // On a production connection the table's name is typed again first.
  if (isProdConnection(useConnectionsStore().byId(props.connectionId), props.database)) {
    const want = table.value.name;
    const ok = await ElMessageBox.prompt(t('designer:alter.prodConfirm', { name: want }), t('designer:alter.prodTitle'), {
      type: 'warning', inputPlaceholder: want, confirmButtonText: t('designer:alter.runAnyway'), cancelButtonText: t('common:cancel'),
      inputValidator: (v: string) => v.trim() === want || t('designer:alter.prodMismatch'),
    }).then(() => true, () => false);
    if (!ok) return;
  }
  creating.value = true;
  runError.value = null;
  try {
    const r = await invoke<{ done: number; failed: [number, string] | null; rolled_back?: boolean }>('schema_sync_run', {
      args: { connection_id: props.connectionId, database: props.database, statements: sc.statements, run_id: sessionId, atomic: props.atomic },
    });
    if (r.failed) {
      runError.value = `${tb(r.failed[1])} ${r.rolled_back ? t('designer:alter.rolledBack') : t('designer:alter.partial', { done: r.done })}`;
      return;
    }
    reviewing.value = false;
    ElMessage.success(t('designer:alter.done', { name: table.value.name }));
    emit('altered', table.value);
  } catch (e) {
    runError.value = errorMessage(e);
  } finally {
    creating.value = false;
  }
}

async function openScript() {
  if (!table.value.name) { ElMessage.warning(t('designer:issues.nameMissing')); return; }
  const text = await freshDdl();
  if (text !== null) emit('open-script', text);
}

onBeforeUnmount(() => {
  if (ddlTimer) clearTimeout(ddlTimer);
  if (inTauri) invoke('close_session', { args: { session_id: sessionId } }).catch(() => {});
});

const nameInput = ref<{ focus: () => void } | null>(null);
nextTick(() => nameInput.value?.focus());

/** The table as loaded, through the same normalization as the edited one:
 *  what the designer doesn't show never turns into a change. */
function takeBaseline() {
  baseline = props.alter ? JSON.parse(JSON.stringify(table.value)) as TableSchema : null;
}
takeBaseline();

defineExpose({ table });
</script>

<template>
  <div class="dz">
    <div class="nm-toolbar">
      <span class="dz-title"><el-icon><ei-grid /></el-icon>{{ alter ? $t('designer:alter.title', { object: objectNoun(tb(spec.label)) }) : tb(spec.label) }}</span>
      <el-select
        v-if="spec.schemas"
        v-model="schema"
        :disabled="alter"
        class="dz-schema"
        filterable
        allow-create
        default-first-option
        :placeholder="$t('designer:toolbar.schemaPlaceholder')"
        :title="$t('designer:toolbar.schema')"
      >
        <el-option v-for="s in schemas" :key="s" :label="s" :value="s" />
      </el-select>
      <span v-if="spec.schemas" class="dz-dot">.</span>
      <el-input
        ref="nameInput"
        v-model="name"
        :disabled="alter"
        :title="alter ? $t('designer:alter.tableNameFixed') : undefined"
        class="dz-name-input"
        :class="{ invalid: !name.trim() }"
        :placeholder="$t('designer:namePlaceholder')"
        spellcheck="false"
      />
      <span class="nm-muted dz-db" :title="$t('designer:toolbar.database', { name: database })"><el-icon><ei-coin /></el-icon>{{ database }}</span>
      <div class="nm-spacer" />
      <el-popover v-if="issues.length" placement="bottom-end" :width="420" trigger="click">
        <template #reference>
          <button class="dz-issues" :class="errors.length ? 'err' : 'warn'">
            <el-icon><ei-warning-filled v-if="errors.length" /><ei-info-filled v-else /></el-icon>
            <template v-if="errors.length">{{ $t('designer:toolbar.errors', { count: errors.length }) }}</template>
            <template v-if="errors.length && warnings.length"> · </template>
            <template v-if="warnings.length">{{ $t('designer:toolbar.warnings', { count: warnings.length }) }}</template>
          </button>
        </template>
        <ul class="dz-issue-list">
          <li v-for="(i, n) in issues" :key="n" :class="i.level" @click="section = i.section">
            <el-icon><ei-circle-close-filled v-if="i.level === 'error'" /><ei-warning v-else /></el-icon>
            <span>{{ i.text }}</span>
          </li>
        </ul>
      </el-popover>
      <el-button :title="$t('designer:toolbar.openAsQueryTitle')" @click="openScript">
        <el-icon><ei-document-add /></el-icon>&nbsp;{{ $t('designer:toolbar.openAsQuery') }}
      </el-button>
      <el-button @click="emit('close')">{{ $t('common:cancel') }}</el-button>
      <el-button v-if="alter" type="primary" :disabled="!!errors.length || renaming" @click="startReview">
        <el-icon><ei-view /></el-icon>&nbsp;{{ $t('designer:alter.review') }}
      </el-button>
      <el-button v-else type="primary" :loading="creating" :disabled="!!errors.length" @click="create">
        <el-icon v-if="!creating"><ei-check /></el-icon>&nbsp;{{ $t('designer:toolbar.create') }}
      </el-button>
    </div>

    <div class="nm-subtabs">
      <button
        v-for="s in sections"
        :key="s.id"
        class="nm-subtab"
        :class="{ active: section === s.id }"
        @click="section = s.id"
      >
        {{ s.label }}
        <span v-if="s.count" class="dz-count">{{ s.count }}</span>
        <span v-if="sectionHasError(s.id)" class="dz-err-dot" />
      </button>
    </div>

    <el-alert
      v-if="runError"
      type="error"
      :title="runError"
      class="dz-run-error nm-selectable"
      show-icon
      @close="runError = null"
    />

    <!-- columns -->
    <div v-show="section === 'columns'" class="dz-body">
      <div ref="grid" class="dz-scroll">
        <table class="dz-grid">
          <thead>
            <tr>
              <th class="dz-h-handle" />
              <th class="dz-h-name">{{ $t('designer:columns.name') }}</th>
              <th class="dz-h-type">{{ $t('designer:columns.type') }}</th>
              <th v-if="spec.nullability" class="dz-h-flag" :title="$t('designer:columns.nullableTitle')">{{ $t('designer:columns.nullable') }}</th>
              <th v-if="spec.primary_key" class="dz-h-flag" :title="$t('designer:columns.primaryKey')">PK</th>
              <th v-if="spec.auto_increment" class="dz-h-flag" :title="$t('designer:columns.autoIncrementTitle')">{{ $t('designer:columns.autoIncrement') }}</th>
              <th v-if="spec.defaults" class="dz-h-default">{{ $t('designer:columns.default') }}</th>
              <th v-for="f in spec.column_options" :key="f.key" :class="f.kind.type === 'bool' ? 'dz-h-flag' : 'dz-h-opt'" :title="tb(f.help || f.label)">{{ tb(f.label) }}</th>
              <th v-if="spec.comments" class="dz-h-comment">{{ $t('designer:columns.comment') }}</th>
              <th class="dz-h-actions" />
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="(c, i) in cols"
              :key="c.key"
              :data-row="c.key"
              :class="{ dragging: dragKey === c.key, 'drop-target': dropKey === c.key && dragKey !== c.key }"
              @keydown="onRowKey($event, c)"
              @dragover="onDragOver($event, c)"
              @drop.prevent="onDrop(c)"
            >
              <td class="dz-handle">
                <span
                  class="dz-grip"
                  :draggable="!alter"
                  :title="$t('designer:columns.dragTitle')"
                  @dragstart="onDragStart($event, c)"
                  @dragend="onDragEnd"
                >
                  <el-icon v-if="c.pk" class="dz-key" :title="$t('designer:columns.primaryKey')"><ei-key /></el-icon>
                  <template v-else>{{ i + 1 }}</template>
                </span>
              </td>
              <td class="dz-name">
                <el-input
                  :model-value="c.name"
                  :class="{ invalid: nameInvalid(c), renamed: !!c.orig && c.confirmed !== c.orig }"
                  :placeholder="$t('designer:namePlaceholder')"
                  :disabled="alter && !!c.orig && (!resolveRename || renaming)"
                  :title="alter && c.orig ? (!resolveRename ? renameBlocked ?? '' : c.confirmed !== c.orig ? $t('designer:alter.renamedFrom', { name: c.orig }) : $t('designer:alter.renameHint')) : undefined"
                  spellcheck="false"
                  @update:model-value="renameCol(c, $event)"
                  @change="confirmName(c)"
                />
              </td>
              <td class="dz-type">
                <el-autocomplete
                  v-model="c.data_type"
                  :class="{ invalid: typeInvalid(c) }"
                  :fetch-suggestions="typeSuggestions"
                  :trigger-on-focus="true"
                  :placeholder="$t('designer:columns.typePlaceholder')"
                  spellcheck="false"
                  popper-class="dz-type-popper"
                  fit-input-width
                />
              </td>
              <td v-if="spec.nullability" class="dz-flag">
                <el-checkbox :model-value="c.nullable && !c.pk" :disabled="c.pk" :title="$t('designer:columns.nullableTitle')" @update:model-value="c.nullable = !!$event" />
              </td>
              <td v-if="spec.primary_key" class="dz-flag">
                <el-checkbox :model-value="c.pk" :title="$t('designer:columns.primaryKey')" @update:model-value="setPk(c, !!$event)" />
              </td>
              <td v-if="spec.auto_increment" class="dz-flag" :class="{ warn: aiWarn(c) }">
                <el-checkbox
                  :model-value="c.auto_increment"
                  :title="aiWarn(c) ? $t('designer:columns.notIntegerTitle') : $t('designer:columns.autoIncrementTitle')"
                  @update:model-value="setAi(c, !!$event)"
                />
              </td>
              <td v-if="spec.defaults" class="dz-default">
                <el-input v-model="c.default_value" placeholder="—" spellcheck="false" />
              </td>
              <td v-for="f in spec.column_options" :key="f.key" :class="f.kind.type === 'bool' ? 'dz-flag' : 'dz-opt'">
                <el-checkbox v-if="f.kind.type === 'bool'" v-model="c.options[f.key] as boolean" :title="tb(f.label)" />
                <el-select v-else-if="f.kind.type === 'select'" v-model="c.options[f.key] as string" :placeholder="tb(f.placeholder) || '—'" clearable>
                  <el-option v-for="[v, l] in selectOptions(f)" :key="v" :label="l" :value="v" />
                </el-select>
                <el-input
                  v-else
                  v-model="c.options[f.key] as string"
                  :type="f.kind.type === 'number' ? 'number' : 'text'"
                  :placeholder="tb(f.placeholder) || '—'"
                  spellcheck="false"
                />
              </td>
              <td v-if="spec.comments" class="dz-comment">
                <el-input v-model="c.comment" placeholder="—" />
              </td>
              <td class="dz-actions">
                <template v-if="!alter">
                  <button class="ide-icon-btn" :title="$t('designer:columns.moveUp')" :disabled="i === 0" @click="moveCol(c, -1)"><el-icon><ei-arrow-up /></el-icon></button>
                  <button class="ide-icon-btn" :title="$t('designer:columns.moveDown')" :disabled="i === cols.length - 1" @click="moveCol(c, 1)"><el-icon><ei-arrow-down /></el-icon></button>
                </template>
                <button class="ide-icon-btn dz-del" :title="$t('designer:columns.removeTitle')" @click="removeCol(c)"><el-icon><ei-close /></el-icon></button>
              </td>
            </tr>
          </tbody>
        </table>
        <div v-if="!cols.length" class="dz-empty">
          <template v-if="spec.columns_required">{{ $t(isColumns ? 'designer:columns.emptyColumns' : 'designer:columns.emptyFields') }}</template>
          <template v-else>{{ $t(isColumns ? 'designer:columns.noColumns' : 'designer:columns.noFields') }}</template>
        </div>
        <button class="dz-add" @click="addCol()">
          <el-icon><ei-plus /></el-icon>{{ $t(isColumns ? 'designer:columns.addColumn' : 'designer:columns.addField') }}
        </button>
      </div>
      <div class="dz-foot">
        <span><kbd>⏎</kbd> {{ $t('designer:columns.hintEnter') }}</span>
        <span><kbd>⌘⏎</kbd> {{ $t('designer:columns.hintInsert') }}</span>
        <span><kbd>Alt</kbd>+<kbd>↑↓</kbd> {{ $t('designer:columns.hintMove') }}</span>
        <span><kbd>⌘⌫</kbd> {{ $t('designer:columns.hintRemove') }}</span>
      </div>
    </div>

    <!-- indexes -->
    <div v-if="spec.indexes" v-show="section === 'indexes'" class="dz-body">
      <div class="dz-scroll">
        <table class="dz-grid">
          <thead>
            <tr>
              <th class="dz-h-name">{{ $t('designer:columns.name') }}</th>
              <th class="dz-h-cols">{{ $t('designer:sections.columns') }}</th>
              <th class="dz-h-flag">{{ $t('designer:indexes.unique') }}</th>
              <th class="dz-h-opt">{{ $t('designer:columns.type') }}</th>
              <th class="dz-h-filter">{{ $t('designer:indexes.filter') }}</th>
              <th class="dz-h-actions" />
            </tr>
          </thead>
          <tbody>
            <tr v-for="x in idxs" :key="x.key">
              <td class="dz-name"><el-input v-model="x.name" :class="{ invalid: !x.name.trim() }" :placeholder="$t('designer:namePlaceholder')" spellcheck="false" /></td>
              <td class="dz-cols">
                <el-select v-model="x.columns" :class="{ invalid: !x.columns.length }" multiple filterable allow-create default-first-option collapse-tags collapse-tags-tooltip :max-collapse-tags="3" :placeholder="$t('designer:columnsPlaceholder')">
                  <el-option v-for="n in colNames" :key="n" :label="n" :value="n" />
                </el-select>
              </td>
              <td class="dz-flag"><el-checkbox v-model="x.unique" /></td>
              <td class="dz-opt"><el-input v-model="x.kind" :placeholder="$t('designer:defaultPlaceholder')" spellcheck="false" /></td>
              <td class="dz-filter"><el-input v-model="x.filter" :placeholder="$t('designer:indexes.filterPlaceholder')" spellcheck="false" /></td>
              <td class="dz-actions">
                <button class="ide-icon-btn dz-del" :title="$t('common:remove')" @click="idxs.splice(idxs.indexOf(x), 1)"><el-icon><ei-close /></el-icon></button>
              </td>
            </tr>
          </tbody>
        </table>
        <div v-if="!idxs.length" class="dz-empty">{{ $t('designer:indexes.empty') }}</div>
        <button class="dz-add" @click="addIndex"><el-icon><ei-plus /></el-icon>{{ $t('designer:indexes.add') }}</button>
      </div>
    </div>

    <!-- foreign keys -->
    <div v-if="spec.foreign_keys" v-show="section === 'foreign_keys'" class="dz-body">
      <div class="dz-scroll">
        <table class="dz-grid">
          <thead>
            <tr>
              <th class="dz-h-name">{{ $t('designer:columns.name') }}</th>
              <th class="dz-h-cols">{{ $t('designer:sections.columns') }}</th>
              <th class="dz-h-target">{{ $t('designer:foreignKeys.targetTable') }}</th>
              <th class="dz-h-cols">{{ $t('designer:foreignKeys.targetColumns') }}</th>
              <th class="dz-h-action">{{ $t('designer:foreignKeys.onDelete') }}</th>
              <th class="dz-h-action">{{ $t('designer:foreignKeys.onUpdate') }}</th>
              <th class="dz-h-actions" />
            </tr>
          </thead>
          <tbody>
            <tr v-for="f in fks" :key="f.key">
              <td class="dz-name"><el-input v-model="f.name" :placeholder="$t('designer:foreignKeys.namePlaceholder')" spellcheck="false" /></td>
              <td class="dz-cols">
                <el-select v-model="f.columns" :class="{ invalid: !f.columns.length }" multiple filterable collapse-tags :max-collapse-tags="2" :placeholder="$t('designer:columnsPlaceholder')">
                  <el-option v-for="n in colNames" :key="n" :label="n" :value="n" />
                </el-select>
              </td>
              <td class="dz-target">
                <el-select :model-value="f.target" :class="{ invalid: !f.target }" filterable :placeholder="$t('designer:foreignKeys.tablePlaceholder')" @update:model-value="setTarget(f, $event)">
                  <el-option v-for="t in targetOptions" :key="t.value" :label="t.label" :value="t.value" />
                </el-select>
              </td>
              <td class="dz-cols">
                <el-select
                  v-model="f.ref_columns"
                  :class="{ invalid: !!f.target && (!f.ref_columns.length || f.ref_columns.length !== f.columns.length) }"
                  multiple
                  filterable
                  allow-create
                  default-first-option
                  collapse-tags
                  :max-collapse-tags="2"
                  :disabled="!f.target"
                  :placeholder="$t('designer:columnsPlaceholder')"
                >
                  <el-option v-for="n in targetMap.get(f.target)?.columns ?? []" :key="n" :label="n" :value="n" />
                </el-select>
              </td>
              <td class="dz-action">
                <el-select v-model="f.on_delete" :placeholder="$t('designer:defaultPlaceholder')" clearable>
                  <el-option v-for="a in FK_ACTIONS" :key="a" :label="a" :value="a" />
                </el-select>
              </td>
              <td class="dz-action">
                <el-select v-model="f.on_update" :placeholder="$t('designer:defaultPlaceholder')" clearable>
                  <el-option v-for="a in FK_ACTIONS" :key="a" :label="a" :value="a" />
                </el-select>
              </td>
              <td class="dz-actions">
                <button class="ide-icon-btn dz-del" :title="$t('common:remove')" @click="fks.splice(fks.indexOf(f), 1)"><el-icon><ei-close /></el-icon></button>
              </td>
            </tr>
          </tbody>
        </table>
        <div v-if="!fks.length" class="dz-empty">{{ $t('designer:foreignKeys.empty') }}</div>
        <button class="dz-add" @click="addFk"><el-icon><ei-plus /></el-icon>{{ $t('designer:foreignKeys.add') }}</button>
      </div>
    </div>

    <!-- options -->
    <div v-show="section === 'options'" class="dz-body">
      <div class="nm-content">
        <el-form label-position="top" class="dz-form" @submit.prevent>
          <div class="dz-fields">
            <el-form-item
              v-for="f in spec.table_options"
              :key="f.key"
              :label="f.kind.type === 'bool' ? '' : tb(f.label)"
              :required="f.required"
              :class="{ 'dz-wide': f.kind.type === 'textarea', 'dz-bool': f.kind.type === 'bool' }"
            >
              <el-checkbox v-if="f.kind.type === 'bool'" v-model="tableOpts[f.key] as boolean">{{ tb(f.label) }}</el-checkbox>
              <el-select v-else-if="f.kind.type === 'select'" v-model="tableOpts[f.key] as string" :placeholder="tb(f.placeholder) || $t('designer:defaultPlaceholder')" clearable style="width: 100%">
                <el-option v-for="[v, l] in selectOptions(f)" :key="v" :label="l" :value="v" />
              </el-select>
              <el-input
                v-else-if="f.kind.type === 'textarea'"
                v-model="tableOpts[f.key] as string"
                type="textarea"
                :rows="5"
                class="dz-mono"
                :placeholder="tb(f.placeholder)"
                spellcheck="false"
              />
              <el-input
                v-else
                v-model="tableOpts[f.key] as string"
                :type="f.kind.type === 'number' ? 'number' : 'text'"
                :placeholder="tb(f.placeholder)"
                spellcheck="false"
              />
              <div v-if="f.help" class="dz-help">{{ tb(f.help) }}</div>
            </el-form-item>
            <el-form-item v-if="spec.comments" :label="$t('designer:columns.comment')" class="dz-wide">
              <el-input v-model="tableComment" type="textarea" :rows="3" :placeholder="$t('designer:options.commentPlaceholder')" />
            </el-form-item>
          </div>
        </el-form>
      </div>
    </div>

    <!-- script -->
    <div v-if="section === 'script'" class="dz-body">
      <el-alert v-if="ddlError" type="error" :title="ddlError" :closable="false" class="dz-run-error nm-selectable" show-icon />
      <div v-else-if="alter && reviewing && review?.statements.length" class="dz-review">
        <span>{{ $t('designer:alter.reviewText') }}</span>
        <div class="nm-spacer" />
        <el-button size="small" @click="reviewing = false">{{ $t('common:cancel') }}</el-button>
        <el-button size="small" type="primary" :loading="creating" @click="applyReviewed">
          {{ $t('designer:alter.run', { count: review.statements.length }) }}
        </el-button>
      </div>
      <div v-else-if="inTauri && !table.name" class="nm-content nm-muted">{{ $t('designer:script.typeName') }}</div>
      <div class="dz-script" :class="{ stale: ddlLoading }">
        <CodeEditor :model-value="previewText" :language="inTauri ? language : 'sql'" :dialect="dialect" read-only />
      </div>
    </div>
  </div>
</template>

<style scoped>
.dz { display: flex; flex-direction: column; height: 100%; min-height: 0; font-size: 13px; }
.dz-title { display: inline-flex; align-items: center; gap: 6px; font-weight: 600; color: var(--nm-text-strong); margin-right: 6px; white-space: nowrap; }
.dz-title .el-icon { color: var(--nm-accent); }
.nm-toolbar .dz-schema { width: 120px; }
.dz-dot { color: var(--nm-text-dim); margin: 0 -2px; }
.dz-name-input { width: 220px; }
.dz-review { display: flex; align-items: center; gap: 8px; padding: 8px 12px; border-bottom: 1px solid var(--nm-border-soft); background: var(--nm-bg-elev); }
.renamed :deep(.el-input__wrapper) { box-shadow: 0 0 0 1px var(--nm-accent) inset; }
.dz-name-input :deep(input) { font-family: var(--nm-mono); font-size: 12.5px; }
.dz-db { display: inline-flex; align-items: center; gap: 4px; margin-left: 6px; white-space: nowrap; }
.invalid :deep(.el-input__wrapper), .invalid :deep(.el-select__wrapper),
.invalid:deep(.el-input__wrapper) { box-shadow: 0 0 0 1px var(--nm-danger) inset !important; }

.dz-issues {
  display: inline-flex; align-items: center; gap: 4px; height: 22px; padding: 0 8px;
  border: 1px solid transparent; border-radius: 2px; background: transparent; cursor: pointer;
  font: inherit; font-size: 12px;
}
.dz-issues.err { color: var(--nm-danger); border-color: rgba(241, 76, 76, 0.35); }
.dz-issues.warn { color: var(--nm-warning); border-color: rgba(204, 167, 0, 0.35); }
.dz-issues:hover { background: var(--ide-hover); }
.dz-issue-list { list-style: none; margin: 0; padding: 0; max-height: 50vh; overflow: auto; }
.dz-issue-list li { display: flex; gap: 6px; align-items: flex-start; padding: 4px 2px; font-size: 12.5px; cursor: pointer; line-height: 1.4; }
.dz-issue-list li:hover { background: var(--ide-hover); }
.dz-issue-list li .el-icon { margin-top: 2px; flex-shrink: 0; }
.dz-issue-list li.error .el-icon { color: var(--nm-danger); }
.dz-issue-list li.warning .el-icon { color: var(--nm-warning); }

.dz-count {
  display: inline-block; min-width: 16px; padding: 0 4px; border-radius: 8px; font-size: 10px; line-height: 15px;
  text-align: center; background: var(--ide-button-2); color: var(--nm-text); letter-spacing: 0;
}
.dz-err-dot { width: 6px; height: 6px; border-radius: 50%; background: var(--nm-danger); }
.dz-run-error { margin: 8px 10px 0; width: auto; flex-shrink: 0; }

.dz-body { flex: 1; min-height: 0; display: flex; flex-direction: column; }
.dz-scroll { flex: 1; min-height: 0; overflow: auto; padding-bottom: 12px; }

/* grid */
.dz-grid { border-collapse: separate; border-spacing: 0; width: 100%; min-width: 760px; table-layout: fixed; }
.dz-grid th {
  position: sticky; top: 0; z-index: 2; background: var(--ide-sidebar); color: var(--nm-text-dim);
  font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.04em; text-align: left;
  padding: 5px 6px; border-bottom: 1px solid var(--nm-border-soft); white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
}
.dz-grid td { padding: 2px 3px; border-bottom: 1px solid var(--nm-border-soft); vertical-align: middle; }
.dz-grid tbody tr:hover td { background: var(--ide-hover); }
.dz-grid tbody tr:focus-within td { background: rgba(4, 57, 94, 0.35); }
.dz-grid tr.dragging td { opacity: 0.4; }
.dz-grid tr.drop-target td { box-shadow: inset 0 2px 0 var(--ide-focus); }

.dz-h-handle { width: 36px; }
.dz-h-flag { width: 70px; text-align: center !important; }
.dz-h-actions { width: 78px; }
.dz-h-name { width: 20%; }
.dz-h-comment { width: 20%; }
.dz-h-cols { width: 22%; }
.dz-h-action { width: 140px; }

.dz-handle { text-align: center; }
.dz-grip {
  display: inline-flex; align-items: center; justify-content: center; width: 24px; height: 22px;
  color: var(--nm-text-muted); font-size: 11px; font-variant-numeric: tabular-nums; cursor: grab; border-radius: 2px;
}
.dz-grip:hover { background: var(--ide-button-2); color: var(--nm-text); }
.dz-key { color: #d7ba7d; }
.dz-flag { text-align: center; }
.dz-flag.warn :deep(.el-checkbox__inner) { border-color: var(--nm-warning); }
.dz-actions { white-space: nowrap; text-align: right; padding-right: 6px !important; }
.dz-actions .ide-icon-btn { color: var(--nm-text-dim); opacity: 0; }
.dz-grid tr:hover .dz-actions .ide-icon-btn, .dz-grid tr:focus-within .dz-actions .ide-icon-btn { opacity: 1; }
.dz-grid tr:hover .dz-actions .ide-icon-btn:disabled, .dz-grid tr:focus-within .dz-actions .ide-icon-btn:disabled { opacity: 0.25; }
.dz-del:hover { color: var(--nm-danger) !important; }

/* cell editors look flat until hovered / focused */
.dz-grid :deep(.el-input__wrapper), .dz-grid :deep(.el-select__wrapper) {
  background: transparent !important; box-shadow: none !important; padding: 0 6px; min-height: 24px;
}
.dz-grid tr:hover :deep(.el-input__wrapper), .dz-grid tr:hover :deep(.el-select__wrapper) {
  background: var(--ide-input) !important;
}
.dz-grid :deep(.el-input__wrapper.is-focus), .dz-grid :deep(.el-select__wrapper.is-focused) {
  background: var(--ide-input) !important; box-shadow: 0 0 0 1px var(--ide-focus) inset !important;
}
.dz-grid .invalid :deep(.el-input__wrapper), .dz-grid .invalid :deep(.el-select__wrapper),
.dz-grid .invalid:deep(.el-input__wrapper) { box-shadow: 0 0 0 1px var(--nm-danger) inset !important; }
.dz-grid :deep(.el-autocomplete), .dz-grid :deep(.el-select) { width: 100%; }
.dz-grid :deep(.el-select__selection .el-tag) {
  background: var(--ide-button-2); border-color: transparent; color: var(--nm-text-strong); font-family: var(--nm-mono);
}
.dz-name :deep(input), .dz-type :deep(input), .dz-default :deep(input), .dz-filter :deep(input) {
  font-family: var(--nm-mono); font-size: 12.5px;
}
.dz-name :deep(input) { color: var(--nm-text-strong); }
.dz-type :deep(input) { color: #4ec9b0; }

.dz-empty { padding: 14px 12px 4px; color: var(--nm-text-dim); font-size: 12.5px; }
.dz-add {
  display: inline-flex; align-items: center; gap: 5px; margin: 8px 8px 0; padding: 4px 8px;
  border: 1px dashed var(--nm-border); border-radius: 2px; background: transparent; color: var(--nm-text-dim);
  font: inherit; font-size: 12.5px; cursor: pointer;
}
.dz-add:hover { color: var(--nm-text-strong); border-color: var(--ide-focus); background: var(--ide-hover); }

.dz-foot {
  display: flex; gap: 16px; flex-shrink: 0; padding: 4px 10px; border-top: 1px solid var(--nm-border-soft);
  font-size: 11px; color: var(--nm-text-muted);
}
kbd {
  font-family: var(--nm-mono); font-size: 10.5px; padding: 0 4px; border-radius: 2px;
  border: 1px solid var(--nm-border); background: var(--ide-sidebar); color: var(--nm-text-dim);
}

/* options */
.dz-form { max-width: 820px; }
.dz-fields { display: grid; grid-template-columns: 1fr 1fr; column-gap: 16px; }
.dz-wide { grid-column: 1 / -1; }
.dz-bool { padding-top: 30px; }
.dz-help { font-size: 11.5px; color: var(--nm-text-dim); line-height: 1.4; margin-top: 2px; }
.dz-mono :deep(textarea) { font-family: var(--nm-mono); font-size: 12px; }

/* script */
.dz-script { flex: 1; min-height: 0; display: flex; flex-direction: column; transition: opacity 0.12s; }
.dz-script > :deep(.ce) { flex: 1; }
.dz-script.stale { opacity: 0.6; }
</style>

<style>
/* teleported autocomplete list: monospace types */
.dz-type-popper .el-autocomplete-suggestion li { font-family: var(--nm-mono); font-size: 12px; line-height: 26px; }
</style>
