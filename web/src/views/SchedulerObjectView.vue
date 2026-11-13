<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { errorMessage } from '../api/client';
import { schedulerApi, type FormField, type FormList, type FormRow, type FormSection, type ObjectForm, type SchedulerAction } from '../api/scheduler';
import ConnectionState from '../components/ConnectionState.vue';
import EngineIcon from '../components/EngineIcon.vue';
import SchedulerDialogs from '../components/SchedulerDialogs.vue';
import SchedulerFormFields from '../components/SchedulerFormFields.vue';
import { newQuery } from '../composables/actions';
import { useConnectionGate } from '../composables/connectionGate';
import { canChange, deleteSchedulerObject, dialectOf, objectAbilities, prodWarning, runSchedulerAction } from '../composables/schedulerActions';
import { askScript } from '../composables/schedulerDialogs';
import {
  canRemoveRow, formValues, hasTypedSecret, humanDuration, initialLists, initialValues, isVisible,
  listVisible, moveRow, newRow, optionLabel, problemText, refOptions, refsToCheck, renameRefs, rowProblems,
  sectionVisible, snapshot, summaryFields, topContext, validate, whenText, type FormCtx, type Problem, type RowRef,
  type Scope,
} from '../composables/schedulerForm';
import { clearServerJobHistory, runServerJobFrom } from '../composables/serverJobActions';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { useServerJobsStore } from '../stores/serverJobs';
import { closeGuards, useTabsStore, type SchedulerObjectTab } from '../stores/tabs';
import { useUiStore } from '../stores/ui';

// The scheduler designer (docs/server-jobs.md): one object of the server's
// scheduler — a job, a schedule, an operator, an alert… — drawn from the
// form its driver describes. Sections are tabs; lists (a job's steps and
// schedules) are tables edited row by row. Nothing reaches the server
// without its script shown first: "Ver script", "Abrir en una consulta", and
// "Guardar", which asks with the script before it runs. Tabs, lists and
// fields show by their conditions (`when`, `when_all`): a tab that hides
// takes its fields and lists with it, and what doesn't show isn't checked
// nor sent. An existing object offers what it says it can (its own edit /
// delete flags, else its kind's) and its kind's actions, each shown as a
// script and confirmed; a job also "Iniciar en el paso…" and "Borrar
// historial" where its kind has them. Driver texts (labels, help, options,
// notes) come in Spanish: `tb`.

const props = defineProps<{ tab: SchedulerObjectTab }>();
const { t } = useTranslation();
const conns = useConnectionsStore();
const store = useServerJobsStore();
const tabs = useTabsStore();
const ui = useUiStore();
const { blocked: connBlocked, ensure: ensureConnection } = useConnectionGate(() => props.tab.connectionId);

const form = ref<ObjectForm | null>(null);
const values = ref<Scope>({});
const lists = ref<Record<string, FormRow[]>>({});
/** The values as they were read: what "modified" compares with. */
const loadedSnap = ref('');
const loading = ref(false);
/** Each read's number: only the latest one's answer is kept. */
let loadSeq = 0;
const loadError = ref<string | null>(null);
const actionError = ref<string | null>(null);
const busy = ref<'' | 'script' | 'query' | 'save' | 'delete' | 'action'>('');
const section = ref('');
/** The checks ran (script or save): problems show from then on. */
const showErrors = ref(false);

const cid = computed(() => props.tab.connectionId);
const isNew = computed(() => props.tab.objectId === null);
const conn = computed(() => conns.byId(cid.value));
const driver = computed(() => conns.driverOf(cid.value));
const kind = computed(() => store.kind(cid.value, props.tab.objectKind));
const singular = computed(() => (kind.value ? tb(kind.value.singular) : props.tab.objectKind));
const kindLabel = computed(() => (kind.value ? tb(kind.value.label) : props.tab.objectKind));
const dialect = computed(() => dialectOf(cid.value));
const connReadOnly = computed(() => !!conn.value?.config.read_only);
/** What the object offers, as the last read of its kind's objects has it
 *  (a job's: the job kind's objects); null until that list is read. */
const abilities = computed(() => (props.tab.objectId === null ? null : objectAbilities(cid.value, kind.value, props.tab.objectId)));
/** Nothing is offered on a guess: until the kind's abilities are known (the
 *  scheduler's info) and, for an existing object, its own (its kind's
 *  list), it's read-only, with no Borrar and no actions. */
const readOnly = computed(() => !!form.value?.read_only || connReadOnly.value || !kind.value
  || (isNew.value ? !kind.value.can_create : !abilities.value?.edit));
const canDelete = computed(() => !isNew.value && !!abilities.value?.delete && !connReadOnly.value && !form.value?.read_only);
/** The kind's actions this object offers now (none on a read-only connection). */
const actions = computed<SchedulerAction[]>(() => (isNew.value || !canChange(cid.value) ? [] : abilities.value?.actions ?? []));
/** Why what the object may do is unknown: reading the scheduler's info, or
 *  its kind's objects, failed. */
const infoError = computed(() => {
  if (!kind.value) return store.scheduler[cid.value]?.error ?? null;
  return isNew.value || abilities.value ? null : store.scheduler[cid.value]?.items[props.tab.objectKind]?.error ?? null;
});
/** A job's "Iniciar en el paso…" and "Borrar historial", where its kind has
 *  them (and its row, once read, agrees: it can run, it keeps a history). */
const jobRunFrom = computed(() => props.tab.objectKind === 'job' && !isNew.value && !connReadOnly.value
  && !!kind.value?.run_from_step && (store.job(cid.value, props.tab.objectId!)?.can_run ?? true));
const jobClearHistory = computed(() => props.tab.objectKind === 'job' && !isNew.value && !connReadOnly.value
  && !!kind.value?.clear_history && (store.job(cid.value, props.tab.objectId!)?.has_history ?? true));
/** Up to two actions show as buttons; more go to "Más acciones" with the job's. */
const actionButtons = computed(() => (actions.value.length <= 2 ? actions.value : []));
const moreItems = computed(() => [
  ...(actions.value.length > 2 ? actions.value.map((a) => ({ command: `action:${a.key}`, label: tb(a.label) })) : []),
  ...(jobRunFrom.value ? [{ command: 'runFrom', label: t('scheduled:server.runFrom') }] : []),
  ...(jobClearHistory.value ? [{ command: 'clearHistory', label: t('scheduled:server.clearHistory') }] : []),
]);

/** The object's name as typed now (its `name` field), else the tab's. */
const name = computed(() => {
  const v = values.value.name;
  return typeof v === 'string' && v.trim() ? v.trim() : props.tab.title || t('scheduled:designer.untitled');
});
const title = computed(() => {
  if (!isNew.value) return props.tab.title || name.value;
  return props.tab.objectKind === 'job' ? t('scheduled:server.newJob') : t('scheduled:designer.newTitle', { kind: singular.value });
});

const topCtx = computed<FormCtx>(() => (form.value ? topContext(form.value, values.value) : { fields: [], values: values.value }));
/** The tabs that show now (a job's type picks some of them). */
const sections = computed<FormSection[]>(() => (form.value ? form.value.sections.filter((s) => sectionVisible(s, topCtx.value)) : []));
/** A tab's lists that show now. */
const shownLists = (s: FormSection) => (s.lists ?? []).filter((l) => listVisible(l, topCtx.value));
// The tab shown hides (its field changed): the first one that shows instead.
watch(sections, (now) => {
  if (now.length && !now.some((s) => s.key === section.value)) section.value = now[0].key;
});
/** Changed since it was read. A read-only object never is: nothing of it
 *  can be saved, so closing it never asks. */
const dirty = computed(() => !!form.value && !readOnly.value && snapshot(form.value, values.value, lists.value) !== loadedSnap.value);
const problems = computed(() => (form.value ? validate(form.value, values.value, lists.value, isNew.value) : null));
const shown = computed(() => (showErrors.value ? problems.value : null));

/** A `ref` field's choices where it is (`ctx`: its row, or the top level;
 *  a source with `{field}`s fills from it), or null: free text. `skip`: the
 *  row being edited (it isn't offered to its own `list:` fields — a step
 *  doesn't go to itself). */
function choicesFor(f: FormField, ctx: FormCtx = topCtx.value, skip?: RowRef): [string, string][] | null {
  return form.value && f.kind.type === 'ref' ? refOptions(f.kind.source, form.value, lists.value, ctx, skip) : [];
}

// -- reading -------------------------------------------------------------------------------

/** Reads the object's kind's list if the tree hasn't: it says what the
 *  object may do. A job's comes with the jobs (their row: can it run, does
 *  it keep a history). Never fails: a failed read leaves the object
 *  read-only and says why. */
async function readOwnAbilities() {
  const k = props.tab.objectKind;
  if (isNew.value || !store.hasScheduler(cid.value)) return;
  const items = store.scheduler[cid.value]?.items[k];
  if (k === 'job' && !store.entries[cid.value]?.at) await store.load(cid.value, { auto: true });
  else if (!items?.list && !items?.loading) await store.loadItems(cid.value, k);
}

async function load() {
  // The latest read wins: one started while another runs (Recargar, the
  // read after a save) replaces what the earlier one would show.
  const seq = ++loadSeq;
  loading.value = true;
  loadError.value = null;
  actionError.value = null;
  try {
    if (!(await ensureConnection())) return;
    // The kind's abilities (can it be created, edited?) before the form:
    // until they're known the object is read-only.
    await store.ensureInfo(cid.value);
    // The object's own abilities come with the tree's list of its kind:
    // read with the form if the tree hasn't (a tab restored with the app),
    // so the form shows with them.
    const [f] = await Promise.all([schedulerApi.form(cid.value, props.tab.objectKind, props.tab.objectId), readOwnAbilities()]);
    if (seq !== loadSeq) return;
    form.value = f;
    values.value = initialValues(f, isNew.value);
    lists.value = initialLists(f);
    loadedSnap.value = snapshot(f, values.value, lists.value);
    showErrors.value = false;
    const visible = f.sections.filter((s) => sectionVisible(s, topContext(f, values.value)));
    if (!visible.some((s) => s.key === section.value)) section.value = visible[0]?.key ?? '';
    // The tab follows the name the server has.
    const n = values.value.name;
    if (!isNew.value && typeof n === 'string' && n && n !== props.tab.title) tabs.schedulerObjectSaved(props.tab.id, props.tab.objectId!, n);
  } catch (e) {
    if (seq === loadSeq) loadError.value = errorMessage(e);
  } finally {
    if (seq === loadSeq) loading.value = false;
  }
}
watch(() => [props.tab.connectionId, props.tab.objectKind, props.tab.objectId], () => void load(), { immediate: true });

async function reload() {
  if (dirty.value) {
    const ok = await ElMessageBox.confirm(t('scheduled:designer.dirtyReload', { name: name.value }), t('scheduled:designer.unsavedTitle'), {
      type: 'warning', confirmButtonText: t('scheduled:designer.discard'), cancelButtonText: t('common:cancel'),
    }).then(() => true, () => false);
    if (!ok) return;
  }
  void load();
}

// The server's copy changed from elsewhere (one of its actions, from here or
// the tree; a job turned on or off): read it again, asking first if there
// are changes here.
watch(() => store.changed?.seq, () => {
  const c = store.changed;
  if (c && c.connectionId === cid.value && c.kind === props.tab.objectKind && c.id === props.tab.objectId && form.value) void reload();
});

// -- modified mark and closing -------------------------------------------------------------

async function guardClose(): Promise<boolean> {
  return ElMessageBox.confirm(t('scheduled:designer.dirtyClose', { name: name.value }), t('scheduled:designer.unsavedTitle'), {
    type: 'warning', confirmButtonText: t('scheduled:designer.discard'), cancelButtonText: t('common:cancel'),
  }).then(() => true, () => false);
}
watch(dirty, (on) => {
  if (on) {
    ui.unsaved[props.tab.id] = 'dirty';
    closeGuards.set(props.tab.id, guardClose);
  } else {
    delete ui.unsaved[props.tab.id];
    closeGuards.delete(props.tab.id);
  }
}, { immediate: true });
onBeforeUnmount(() => {
  delete ui.unsaved[props.tab.id];
  closeGuards.delete(props.tab.id);
});

// -- script, save, delete ------------------------------------------------------------------

/** The checks before anything is sent: false (and the problems shown, on
 *  their section) when one fails. */
function checked(): boolean {
  const p = problems.value;
  if (!p?.count) return true;
  showErrors.value = true;
  if (!p.sections.includes(section.value)) section.value = p.sections[0] ?? section.value;
  ElMessage.warning(t('scheduled:designer.invalid'));
  return false;
}

const current = () => formValues(form.value!, values.value, lists.value);

/** The script for the values as they are now, or null (problems, an error). */
async function script(): Promise<string | null> {
  if (!form.value || !checked()) return null;
  actionError.value = null;
  try {
    return await schedulerApi.script(cid.value, props.tab.objectKind, props.tab.objectId, current());
  } catch (e) {
    actionError.value = errorMessage(e);
    return null;
  }
}

/** A secret was typed: the script shows it as the driver's mask, and only
 *  "Guardar" writes the real value. */
const secretTyped = computed(() => !!form.value && hasTypedSecret(form.value, values.value, lists.value));

/** The script in a new query. With a masked secret, a first line says the
 *  mask has to be replaced before running it. */
async function openQuery(text: string, masked: boolean) {
  const body = masked ? `-- ${t('scheduled:designer.secretComment')}\n${text}` : text;
  await newQuery(cid.value, '', body, t('scheduled:server.scriptName', { job: name.value }));
}

async function viewScript() {
  busy.value = 'script';
  const s = await script();
  busy.value = '';
  if (s === null) return;
  const masked = secretTyped.value;
  void askScript({
    mode: 'view', title: t('scheduled:designer.scriptTitle', { name: name.value }), script: s, dialect: dialect.value,
    notice: masked ? t('scheduled:designer.secretMasked') : null,
    openInQuery: () => void openQuery(s, masked),
  });
}

async function scriptInQuery() {
  busy.value = 'query';
  const s = await script();
  busy.value = '';
  if (s === null) return;
  const masked = secretTyped.value;
  if (masked) {
    const ok = await ElMessageBox.confirm(t('scheduled:designer.secretMasked'), t('scheduled:designer.openInQuery'), {
      type: 'warning', confirmButtonText: t('scheduled:designer.openInQuery'), cancelButtonText: t('common:cancel'),
    }).then(() => true, () => false);
    if (!ok) return;
  }
  await openQuery(s, masked);
}

async function save() {
  if (readOnly.value || !form.value) return;
  busy.value = 'save';
  try {
    const s = await script();
    if (s === null) return;
    if (!s.trim()) { ElMessage.info(t('scheduled:designer.noScript')); return; }
    const vars = { name: name.value, kind: singular.value, connection: conn.value?.name ?? '' };
    const ok = await askScript({
      mode: 'confirm',
      title: t('scheduled:designer.saveTitle'),
      message: isNew.value ? t('scheduled:designer.confirmCreate', vars) : t('scheduled:designer.confirmSave', vars),
      warning: prodWarning(cid.value),
      notice: secretTyped.value ? t('scheduled:designer.secretMaskedSave') : null,
      script: s,
      dialect: dialect.value,
      okLabel: t('common:save'),
    });
    if (!ok) return;
    const was = props.tab.objectId;
    const id = (await schedulerApi.save(cid.value, props.tab.objectKind, was, current())) ?? was;
    ElMessage.success(t('scheduled:designer.saved', { name: name.value }));
    store.refreshAfterChange(cid.value);
    // What's saved is what's shown: no "modified" mark while it reads again.
    loadedSnap.value = snapshot(form.value, values.value, lists.value);
    if (id === null) { tabs.close(props.tab.id, true); return; }
    tabs.schedulerObjectSaved(props.tab.id, id, name.value);
    // A new object (or one whose id changed) reads again through the watcher.
    if (id === was) await load();
  } catch (e) {
    actionError.value = errorMessage(e);
  } finally {
    busy.value = '';
  }
}

async function remove() {
  if (!canDelete.value || !props.tab.objectId) return;
  busy.value = 'delete';
  try {
    await deleteSchedulerObject(cid.value, props.tab.objectKind, props.tab.objectId, props.tab.title || name.value);
  } finally {
    busy.value = '';
  }
}

/** One of the kind's actions on this object: script, confirmation, action. */
async function runAction(a: SchedulerAction) {
  if (!kind.value || !props.tab.objectId) return;
  busy.value = 'action';
  try {
    await runSchedulerAction(cid.value, kind.value, props.tab.objectId, props.tab.title || name.value, a);
  } finally {
    busy.value = '';
  }
}

/** "Más acciones": the kind's actions beyond two, then a job's own. */
async function moreAction(cmd: string) {
  const id = props.tab.objectId;
  if (!id) return;
  const job = { id, name: props.tab.title || name.value };
  if (cmd === 'runFrom') { await runServerJobFrom(cid.value, job); return; }
  if (cmd === 'clearHistory') { await clearServerJobHistory(cid.value, job); return; }
  const a = actions.value.find((x) => `action:${x.key}` === cmd);
  if (a) await runAction(a);
}

// -- lists ---------------------------------------------------------------------------------

/** Rows can be added, removed and reordered. */
const editable = (l: FormList) => !readOnly.value && !l.read_only;
const titleField = (l: FormList) => l.fields.find((f) => f.key === l.title_field);
const rowsOf = (l: FormList) => lists.value[l.key] ?? [];
const rowCtx = (l: FormList, row: FormRow): FormCtx => ({ fields: l.fields, values: row, parent: topCtx.value });
/** A summary column's cell: the field of that key that shows in this row
 *  (several may share a key under exclusive `when`s), or nothing. */
function summaryCell(l: FormList, c: FormField, row: FormRow): string {
  const ctx = rowCtx(l, row);
  const f = l.fields.find((x) => x.key === c.key && isVisible(x, ctx));
  return f ? cellText(f, row[c.key], ctx) : '';
}
const rowHasProblem = (l: FormList, i: number) => !!shown.value?.rows[l.key]?.some((p) => p.row === i);

/** A cell of a list's table, as the user reads it. A select's or a ref's
 *  value is looked up first: "" may be an option ("Cuenta de servicio del
 *  Agente"). */
function cellText(f: FormField | undefined, v: unknown, ctx: FormCtx): string {
  const k = f?.kind;
  if (f && (k?.type === 'select' || k?.type === 'multi_select' || k?.type === 'ref') && v !== null && v !== undefined) {
    const choices = choicesFor(f, ctx) ?? [];
    return (Array.isArray(v) ? v : [v]).map((x) => {
      const label = optionLabel(f, x, choices);
      return label === null ? whenText(x) : tb(label);
    }).join(', ');
  }
  if (v === null || v === undefined || v === '') return '';
  if (!f || !k) return typeof v === 'object' ? JSON.stringify(v) : String(v);
  if (k.type === 'bool') return v === true || v === 'true' ? t('common:yes') : t('common:no');
  if (k.type === 'duration' && typeof v === 'number') return humanDuration(v);
  if (k.type === 'secret') return '••••••';
  return typeof v === 'object' ? JSON.stringify(v) : String(v);
}

function setRows(l: FormList, rows: FormRow[]) {
  lists.value = { ...lists.value, [l.key]: rows };
}

function move(l: FormList, i: number, dir: -1 | 1) {
  setRows(l, moveRow(rowsOf(l), i, dir));
}

function removeRow(l: FormList, i: number) {
  if (!canRemoveRow(l, rowsOf(l).length, readOnly.value)) return;
  setRows(l, rowsOf(l).filter((_, j) => j !== i));
}

function rowProblemText(l: FormList, p: { row: number; field: string; problem: Problem }): string {
  const f = l.fields.find((x) => x.key === p.field);
  return t('scheduled:designer.errors.row', { n: p.row + 1, field: f ? tb(f.label) : p.field, error: problemText(p.problem, t) });
}

// The row editor: a copy of the row, written back on "Aceptar".
const rowEdit = ref<{ list: FormList; index: number | null; row: FormRow; showErrors: boolean } | null>(null);
const rowReadOnly = computed(() => !!rowEdit.value && (readOnly.value || !!rowEdit.value.list.read_only));
/** The edited row, left out of its own list's choices. */
const rowSkip = computed<RowRef | undefined>(() => (rowEdit.value ? { list: rowEdit.value.list.key, index: rowEdit.value.index } : undefined));
const rowChoicesFor = (f: FormField, ctx: FormCtx) => choicesFor(f, ctx, rowSkip.value);
const rowEditProblems = computed(() => {
  const r = rowEdit.value;
  if (!r || !form.value) return null;
  const refs = refsToCheck(form.value, lists.value, rowSkip.value);
  return Object.fromEntries(rowProblems(r.list, r.row, topCtx.value, isNew.value, refs).map((p) => [p.field, p.problem]));
});

function editRow(l: FormList, i: number) {
  rowEdit.value = { list: l, index: i, row: JSON.parse(JSON.stringify(rowsOf(l)[i])), showErrors: showErrors.value };
}

function addRow(l: FormList) {
  rowEdit.value = { list: l, index: null, row: newRow(l), showErrors: false };
}

function acceptRow() {
  const r = rowEdit.value;
  if (!r) return;
  if (rowReadOnly.value) { rowEdit.value = null; return; }
  if (Object.keys(rowEditProblems.value ?? {}).length) { r.showErrors = true; return; }
  const rows = rowsOf(r.list).slice();
  const was = r.index === null ? '' : whenText(rows[r.index]?.[r.list.title_field]);
  if (r.index === null) rows.push(r.row);
  else rows[r.index] = r.row;
  setRows(r.list, rows);
  // A row renamed: what referred to it by its title (a job's start step, the
  // steps' "go to step") follows the new one.
  const now = whenText(r.row[r.list.title_field]);
  if (form.value && was && now && was !== now) {
    const out = renameRefs(form.value, values.value, lists.value, r.list.key, was, now);
    if (out.count) {
      values.value = out.values;
      lists.value = out.lists;
    }
  }
  rowEdit.value = null;
}

const rowTitle = computed(() => {
  const r = rowEdit.value;
  if (!r) return '';
  const v = r.row[r.list.title_field];
  const what = r.index === null ? t('scheduled:designer.newRow') : v ? String(v) : t('scheduled:designer.untitled');
  return `${tb(r.list.label)} · ${what}`;
});
</script>

<template>
  <div class="sod">
    <header class="sod-head">
      <EngineIcon v-if="driver" :id="driver.id" :name="driver.name" :size="20" />
      <div class="sod-title">
        <strong>{{ title }}</strong>
        <span class="nm-muted">{{ conn?.name }}</span>
      </div>
      <span class="sod-kind">{{ kindLabel }}</span>
      <span v-if="form && readOnly" class="sod-chip" :title="$t('scheduled:designer.readOnlyHint')">{{ $t('scheduled:designer.readOnly') }}</span>
      <div class="sod-sp" />
      <template v-if="form">
        <el-button size="small" :loading="busy === 'script'" :disabled="!!busy && busy !== 'script'" @click="viewScript">{{ $t('scheduled:designer.viewScript') }}</el-button>
        <el-button size="small" :loading="busy === 'query'" :disabled="!!busy && busy !== 'query'" @click="scriptInQuery">{{ $t('scheduled:designer.openInQuery') }}</el-button>
        <el-button v-for="a in actionButtons" :key="a.key" size="small" :disabled="!!busy" @click="runAction(a)">{{ tb(a.label) }}</el-button>
        <el-dropdown v-if="moreItems.length" trigger="click" :disabled="!!busy" @command="moreAction">
          <el-button size="small" :disabled="!!busy" :aria-label="$t('scheduled:server.more')" :title="$t('scheduled:server.more')">
            <el-icon><ei-more-filled /></el-icon>
          </el-button>
          <template #dropdown>
            <el-dropdown-menu>
              <el-dropdown-item v-for="m in moreItems" :key="m.command" :command="m.command">{{ m.label }}</el-dropdown-item>
            </el-dropdown-menu>
          </template>
        </el-dropdown>
        <el-button v-if="canDelete" size="small" type="danger" plain :loading="busy === 'delete'" :disabled="!!busy && busy !== 'delete'" @click="remove">{{ $t('scheduled:server.delete') }}</el-button>
        <el-button
          v-if="!readOnly"
          size="small"
          type="primary"
          :loading="busy === 'save'"
          :disabled="(!!busy && busy !== 'save') || (!isNew && !dirty)"
          @click="save"
        >{{ $t('common:save') }}</el-button>
      </template>
      <el-button size="small" :loading="loading" :disabled="!!busy" :aria-label="$t('scheduled:designer.reload')" :title="$t('scheduled:designer.reload')" @click="reload">
        <el-icon><ei-refresh /></el-icon>
      </el-button>
    </header>
    <ConnectionState v-if="connBlocked" :connection-id="tab.connectionId" @retry="load" />
    <el-alert v-if="loadError" type="error" :title="loadError" :closable="false" show-icon class="sod-alert" />
    <el-alert v-if="actionError" type="error" :title="actionError" show-icon class="sod-alert" @close="actionError = null" />
    <el-alert v-if="form && infoError" type="warning" :title="infoError" :closable="false" show-icon class="sod-alert" />
    <div v-if="loading && !form && !loadError" class="sod-empty"><el-icon class="is-loading" :size="22"><ei-loading /></el-icon></div>

    <div v-if="form" class="sod-body">
      <div v-if="form.notes?.length || (readOnly && connReadOnly)" class="sod-notes">
        <div v-for="(n, i) in form.notes ?? []" :key="i" class="sod-notice">
          <el-icon><ei-info-filled /></el-icon><span>{{ tb(n) }}</span>
        </div>
        <div v-if="readOnly && connReadOnly" class="sod-notice">
          <el-icon><ei-lock /></el-icon><span>{{ $t('scheduled:designer.readOnlyHint') }}</span>
        </div>
      </div>

      <el-tabs v-model="section" class="sod-tabs">
        <el-tab-pane v-for="s in sections" :key="s.key" :name="s.key" lazy>
          <template #label>
            <span class="sod-tab-label" :class="{ bad: shown?.sections.includes(s.key) }">
              {{ tb(s.label) }}
              <i v-if="shown?.sections.includes(s.key)" class="sod-tab-dot" />
            </span>
          </template>
          <div class="sod-section">
            <SchedulerFormFields
              v-if="s.fields?.length"
              :fields="s.fields"
              :values="values"
              :ctx="topCtx"
              :problems="shown?.fields"
              :read-only="readOnly"
              :is-new="isNew"
              :dialect="dialect"
              :choices-for="choicesFor"
              @update="(k, v) => (values[k] = v)"
            />
            <section v-for="l in shownLists(s)" :key="l.key" class="sod-list">
              <div class="sod-list-head">
                <h3 class="nm-section-title">{{ tb(l.label) }}</h3>
                <span class="sod-count">{{ rowsOf(l).length }}</span>
                <div class="sod-sp" />
                <el-button v-if="editable(l)" size="small" @click="addRow(l)">
                  <el-icon><ei-plus /></el-icon><span>{{ $t('scheduled:designer.add') }}</span>
                </el-button>
              </div>
              <table class="sod-table">
                <thead>
                  <tr>
                    <th v-if="l.orderable" class="num">#</th>
                    <th>{{ titleField(l) ? tb(titleField(l)!.label) : l.title_field }}</th>
                    <th v-for="c in summaryFields(l)" :key="c.key">{{ tb(c.label) }}</th>
                    <th class="acts" />
                  </tr>
                </thead>
                <tbody>
                  <tr v-for="(r, i) in rowsOf(l)" :key="i" :class="{ invalid: rowHasProblem(l, i) }" @dblclick="editRow(l, i)">
                    <td v-if="l.orderable" class="num">{{ i + 1 }}</td>
                    <td class="title">{{ cellText(titleField(l), r[l.title_field], rowCtx(l, r)) || $t('scheduled:designer.untitled') }}</td>
                    <td v-for="c in summaryFields(l)" :key="c.key">{{ summaryCell(l, c, r) }}</td>
                    <td class="acts">
                      <button
                        class="sod-icon"
                        :title="editable(l) ? $t('common:edit') : $t('scheduled:designer.view')"
                        :aria-label="editable(l) ? $t('common:edit') : $t('scheduled:designer.view')"
                        @click="editRow(l, i)"
                      ><el-icon><ei-edit v-if="editable(l)" /><ei-view v-else /></el-icon></button>
                      <template v-if="l.orderable && editable(l)">
                        <button class="sod-icon" :disabled="i === 0" :title="$t('scheduled:designer.moveUp')" :aria-label="$t('scheduled:designer.moveUp')" @click="move(l, i, -1)"><el-icon><ei-top /></el-icon></button>
                        <button class="sod-icon" :disabled="i === rowsOf(l).length - 1" :title="$t('scheduled:designer.moveDown')" :aria-label="$t('scheduled:designer.moveDown')" @click="move(l, i, 1)"><el-icon><ei-bottom /></el-icon></button>
                      </template>
                      <button
                        v-if="editable(l)"
                        class="sod-icon danger"
                        :disabled="!canRemoveRow(l, rowsOf(l).length, readOnly)"
                        :title="canRemoveRow(l, rowsOf(l).length, readOnly) ? $t('common:remove') : $t('scheduled:designer.minRowsHint', { count: l.min_rows ?? 0 })"
                        :aria-label="$t('common:remove')"
                        @click="removeRow(l, i)"
                      ><el-icon><ei-delete /></el-icon></button>
                    </td>
                  </tr>
                  <tr v-if="!rowsOf(l).length">
                    <td :colspan="summaryFields(l).length + (l.orderable ? 3 : 2)" class="nm-muted">{{ $t('scheduled:designer.noRows') }}</td>
                  </tr>
                </tbody>
              </table>
              <div v-if="shown?.lists[l.key]" class="sod-err">{{ problemText(shown.lists[l.key], t) }}</div>
              <div v-for="(p, j) in shown?.rows[l.key] ?? []" :key="j" class="sod-err">{{ rowProblemText(l, p) }}</div>
            </section>
          </div>
        </el-tab-pane>
      </el-tabs>
    </div>

    <el-dialog
      :model-value="!!rowEdit"
      :title="rowTitle"
      width="min(860px, 92vw)"
      append-to-body
      :close-on-click-modal="false"
      @close="rowEdit = null"
    >
      <div v-if="rowEdit" class="sod-row-editor">
        <SchedulerFormFields
          :fields="rowEdit.list.fields"
          :values="rowEdit.row"
          :ctx="rowCtx(rowEdit.list, rowEdit.row)"
          :problems="rowEdit.showErrors ? rowEditProblems : null"
          :read-only="rowReadOnly"
          :is-new="isNew"
          :dialect="dialect"
          :choices-for="rowChoicesFor"
          @update="(k, v) => { if (rowEdit) rowEdit.row[k] = v; }"
        />
      </div>
      <template #footer>
        <template v-if="rowReadOnly">
          <el-button type="primary" @click="rowEdit = null">{{ $t('common:close') }}</el-button>
        </template>
        <template v-else>
          <el-button @click="rowEdit = null">{{ $t('common:cancel') }}</el-button>
          <el-button type="primary" @click="acceptRow">{{ $t('common:accept') }}</el-button>
        </template>
      </template>
    </el-dialog>
    <SchedulerDialogs />
  </div>
</template>

<style scoped>
.sod { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--nm-bg); }
.sod-head { display: flex; align-items: center; gap: 10px; padding: 10px 16px; border-bottom: 1px solid var(--nm-border-soft); flex-wrap: wrap; }
.sod-head .el-button { margin: 0; }
.sod-title { display: flex; flex-direction: column; min-width: 0; }
.sod-title strong { color: var(--nm-text-strong); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sod-title span { font-size: 11.5px; }
.sod-kind { font-size: 10.5px; padding: 1px 6px; border-radius: 3px; color: var(--nm-text-dim); background: var(--ide-hover); }
.sod-chip { font-size: 10.5px; padding: 1px 6px; border-radius: 8px; color: var(--nm-warning); border: 1px solid var(--nm-warning); }
.sod-sp { flex: 1; }
.sod-alert { margin: 10px 16px 0; width: auto; }
.sod-empty { display: flex; justify-content: center; padding: 40px; color: var(--nm-text-dim); }
.sod-body { flex: 1; min-height: 0; display: flex; flex-direction: column; padding: 4px 16px 0; }
.sod-notes { display: flex; flex-direction: column; gap: 4px; padding: 6px 0 2px; }
.sod-notice { display: flex; align-items: baseline; gap: 6px; font-size: 12px; color: var(--nm-text-dim); }
.sod-notice .el-icon { flex: none; position: relative; top: 1px; color: var(--nm-info); }
.sod-tabs { flex: 1; min-height: 0; display: flex; flex-direction: column; }
.sod-tabs :deep(.el-tabs__header) { margin: 0; }
.sod-tabs :deep(.el-tabs__content) { flex: 1; min-height: 0; overflow: auto; }
.sod-tab-label { display: inline-flex; align-items: center; gap: 5px; }
.sod-tab-label.bad { color: var(--nm-danger); }
.sod-tab-dot { width: 6px; height: 6px; border-radius: 50%; background: var(--nm-danger); }
.sod-section { max-width: 1040px; padding: 12px 0 32px; display: flex; flex-direction: column; gap: 16px; }
.sod-list-head { display: flex; align-items: center; gap: 8px; margin-bottom: 6px; }
.sod-list-head .nm-section-title { margin: 0; }
.sod-list-head .el-button .el-icon + span { margin-left: 4px; }
.sod-count { font-size: 10.5px; min-width: 18px; padding: 0 5px; border-radius: 8px; text-align: center; color: var(--nm-text-dim); background: var(--ide-hover); }
.sod-table { width: 100%; border-collapse: collapse; font-size: 12.5px; }
.sod-table th { text-align: left; font-weight: 600; padding: 5px 10px; color: var(--nm-text-dim); border-bottom: 1px solid var(--nm-border); white-space: nowrap; }
.sod-table td { padding: 4px 10px; border-bottom: 1px solid var(--nm-border-soft); color: var(--nm-text); max-width: 320px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sod-table tbody tr:hover td { background: var(--ide-hover); }
.sod-table tr.invalid td { background: color-mix(in srgb, var(--nm-danger) 10%, transparent); }
.sod-table tr.invalid td.title { box-shadow: inset 2px 0 0 var(--nm-danger); }
.sod-table .num { width: 28px; text-align: right; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; }
.sod-table td.title { color: var(--nm-text-strong); }
.sod-table .acts { width: 1%; white-space: nowrap; text-align: right; padding: 2px 6px; }
.sod-icon { display: inline-flex; align-items: center; justify-content: center; width: 22px; height: 22px; border: 0; border-radius: 4px; background: none; color: var(--nm-text-dim); cursor: pointer; }
.sod-icon:hover:not(:disabled) { background: var(--ide-hover); color: var(--nm-text-strong); }
.sod-icon.danger:hover:not(:disabled) { color: var(--nm-danger); }
.sod-icon:disabled { opacity: .35; cursor: default; }
.sod-err { margin-top: 4px; font-size: 11.5px; color: var(--nm-danger); }
.sod-row-editor { max-height: 66vh; overflow: auto; padding-right: 6px; }
</style>
