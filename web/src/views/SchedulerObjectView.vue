<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { errorMessage } from '../api/client';
import { schedulerApi, type FormField, type FormRow, type FormSection, type ObjectForm, type SchedulerAction } from '../api/scheduler';
import ConnectionState from '../components/ConnectionState.vue';
import EngineIcon from '../components/EngineIcon.vue';
import SchedulerDialogs from '../components/SchedulerDialogs.vue';
import SchedulerFormFields from '../components/SchedulerFormFields.vue';
import SchedulerFormList from '../components/SchedulerFormList.vue';
import { newQuery } from '../composables/actions';
import { useConnectionGate } from '../composables/connectionGate';
import { actionLabel, canChange, deleteSchedulerObject, dialectOf, objectAbilities, prodWarning, runSchedulerAction } from '../composables/schedulerActions';
import { askScript } from '../composables/schedulerDialogs';
import {
  formValues, hasTypedSecret, initialLists, initialValues, listVisible, refOptions, sectionVisible, snapshot,
  topContext, validate, type FormCtx, type Scope,
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
// script and confirmed (one that asks for values first opens its form
// before: composables/schedulerActions.ts); a job also "Iniciar en el
// paso…" and "Borrar historial" where its kind has them. Lists are
// SchedulerFormList's. Driver texts (labels, help, options, notes) come in
// Spanish: `tb`.

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
const jobItem = computed(() => (props.tab.objectId ? store.item(cid.value, 'job', props.tab.objectId) : undefined));
const jobRunFrom = computed(() => props.tab.objectKind === 'job' && !isNew.value && !connReadOnly.value
  && (jobItem.value?.run_from_step ?? !!kind.value?.run_from_step)
  && (store.job(cid.value, props.tab.objectId!)?.can_run ?? true));
const jobClearHistory = computed(() => props.tab.objectKind === 'job' && !isNew.value && !connReadOnly.value
  && (jobItem.value?.clear_history ?? !!kind.value?.clear_history)
  && (store.job(cid.value, props.tab.objectId!)?.has_history ?? true));
/** Up to two actions show as buttons; more go to "Más acciones" with the job's. */
const actionButtons = computed(() => (actions.value.length <= 2 ? actions.value : []));
const moreItems = computed(() => [
  ...(actions.value.length > 2 ? actions.value.map((a) => ({ command: `action:${a.key}`, label: actionLabel(a) })) : []),
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

/** A top-level `ref` field's choices (a source with `{field}`s fills from
 *  the form's values), or null: free text. A list's rows have theirs in
 *  SchedulerFormList. */
function choicesFor(f: FormField, ctx: FormCtx = topCtx.value): [string, string][] | null {
  return form.value && f.kind.type === 'ref' ? refOptions(f.kind.source, form.value, lists.value, ctx) : [];
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

/** A list changed (SchedulerFormList): its rows, and the top-level values
 *  and other lists when a renamed row took its references along. */
function listChanged(next: { values: Scope; lists: Record<string, FormRow[]> }) {
  if (next.values !== values.value) values.value = next.values;
  lists.value = next.lists;
}
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
        <el-button v-for="a in actionButtons" :key="a.key" size="small" :disabled="!!busy" @click="runAction(a)">{{ actionLabel(a) }}</el-button>
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
            <SchedulerFormList
              v-for="l in shownLists(s)"
              :key="l.key"
              :list="l"
              :form="form"
              :values="values"
              :lists="lists"
              :top="topCtx"
              :read-only="readOnly"
              :is-new="isNew"
              :dialect="dialect"
              :problems="shown"
              @change="listChanged"
            />
          </div>
        </el-tab-pane>
      </el-tabs>
    </div>

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
</style>
