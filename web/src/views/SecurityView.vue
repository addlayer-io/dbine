<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import { securityApi, type Grant, type Principal, type SecurityAction } from '../api/security';
import type { ObjectRef } from '../api/types';
import { tb } from '../i18n/backend';
import { dbKey, useConnectionsStore } from '../stores/connections';
import { startTask, useTasksStore, type TaskHandle } from '../stores/tasks';
import { closeGuards, type SecurityTab } from '../stores/tabs';

// Users and permissions (docs/users-and-permissions.md): the server's (or the
// database's) users and roles, what each can do, and changes as scripts in
// the engine's language — shown (password hidden) and run only on the
// user's click, never kept in the history.

const props = defineProps<{ tab: SecurityTab }>();
const conns = useConnectionsStore();
const { t } = useTranslation();
const spec = computed(() => conns.driverOf(props.tab.connectionId)?.security ?? null);
/** "Asignar login…": users for logins the server already has (SQL Server, SAP ASE). */
const canMapLogin = computed(() => !!conns.driverOf(props.tab.connectionId)?.supports_map_login);
const readOnly = computed(() => !!conns.byId(props.tab.connectionId)?.config.read_only);
/** Why the login can't manage users and grants ('' when it can). */
const noPermission = computed(() => {
  const missing = conns.denied(props.tab.connectionId, props.tab.database, 'manage_security');
  return missing ? t('common:noPermission', { missing }) : '';
});
/** Changes are offered: not read-only and the login may make them. */
const canEdit = computed(() => !readOnly.value && !noPermission.value);

const principals = ref<Principal[]>([]);
const loading = ref(false);
const error = ref<string | null>(null);
const search = ref('');
const selected = ref<string | null>(null);
const grants = ref<Grant[]>([]);
const grantsError = ref<string | null>(null);

async function load() {
  loading.value = true;
  error.value = null;
  try {
    if (!(await conns.ensureConnected(props.tab.connectionId))) return;
    conns.loadPermissions(props.tab.connectionId, props.tab.database);
    principals.value = await securityApi.principals(props.tab.connectionId, props.tab.database);
    if (selected.value && !principals.value.some((p) => p.name === selected.value)) selected.value = null;
    if (selected.value) await loadGrants();
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    loading.value = false;
  }
}
async function loadGrants() {
  grants.value = [];
  grantsError.value = null;
  if (!selected.value) return;
  try {
    grants.value = await securityApi.grants(props.tab.connectionId, props.tab.database, selected.value);
  } catch (e) {
    grantsError.value = errorMessage(e);
  }
}
onMounted(load);
watch(selected, loadGrants);

const filtered = computed(() => {
  const q = search.value.trim().toLowerCase();
  return principals.value.filter((p) => !q || p.name.toLowerCase().includes(q));
});
const users = computed(() => filtered.value.filter((p) => p.kind === 'user'));
const roles = computed(() => filtered.value.filter((p) => p.kind === 'role'));
const current = computed(() => principals.value.find((p) => p.name === selected.value) ?? null);
const roleNames = computed(() => principals.value.filter((p) => p.kind === 'role' && p.name !== selected.value).map((p) => p.name));

// -- objects to grant on (the explorer's list of this database) ---------------
const objects = computed(() => {
  const kinds = new Set(spec.value?.object_kinds.filter(Boolean) ?? []);
  return (conns.objects[dbKey(props.tab.connectionId, props.tab.database)]?.items ?? []).filter((o) => kinds.has(o.kind));
});
onMounted(() => conns.loadObjects(props.tab.connectionId, props.tab.database).catch(() => {}));
const schemas = computed(() => [...new Set(objects.value.map((o) => o.schema).filter((s): s is string => !!s))]);
const databases = computed(() => conns.live[props.tab.connectionId]?.databases ?? []);

// -- a change: its script, reviewed, then run --------------------------------
// The run is a task in the tasks store: it keeps going (and can be cancelled
// from the Tareas panel, through cancel_query on its own session) after the
// dialog closes or this tab does. The dialog is only a view of it.
const tasks = useTasksStore();
let alive = true;
let runTask: TaskHandle | null = null;
onBeforeUnmount(() => {
  alive = false;
  // The dialog is gone: "Ver detalle" falls back to the panel's own detail and
  // a run still going notifies when it ends (⌘W closes the tab without a close).
  if (runTask) {
    runTask.setReopen(undefined);
    runTask.background();
  }
});
const review = reactive<{
  open: boolean;
  /** The pending changes this review runs (their ids), in order. */
  ids: number[];
  script: string; shown: string; error: string | null; running: boolean; cancelling: boolean;
}>({
  open: false, ids: [], script: '', shown: '', error: null, running: false, cancelling: false,
});

// -- pending changes -----------------------------------------------------------
// Each change is queued with its script (written right away, so a mistake
// shows at once) and applied with the others in one reviewed run.
interface PendingChange {
  id: number;
  /** What it does, in words ("Otorgar SELECT sobre dbo.t a ana"). */
  label: string;
  script: string;
  /** The script with the password hidden. */
  shown: string;
  action: SecurityAction | null;
  /** The user or role it creates, selected once applied. */
  creates: string | null;
}
const pending = ref<PendingChange[]>([]);
const pendingOpen = ref(true);
let pendingSeq = 0;
/** Write a change's script and queue it; an error shows and queues nothing. */
/** Whether it was queued (its script came back). */
async function queue(label: string, load: () => Promise<{ script: string; shown: string }>, action: SecurityAction | null, creates: string | null): Promise<boolean> {
  try {
    const s = await load();
    pending.value.push({ id: ++pendingSeq, label, script: s.script, shown: s.shown, action, creates });
    return true;
  } catch (e) {
    ElMessage.error(errorMessage(e));
    return false;
  }
}
function objectLabel(o: ObjectRef | null): string {
  if (!o) return t('security:everything');
  return o.schema ? `${o.schema}.${o.name}` : o.name;
}
function labelOf(a: SecurityAction): string {
  switch (a.action) {
    case 'create_user': return t('security:change.createUser', { name: a.name });
    case 'create_role': return t('security:change.createRole', { name: a.name });
    case 'drop': return t('security:change.drop', { name: a.name });
    case 'set_password': return t('security:change.setPassword', { name: a.name });
    case 'set_login': return t(a.enabled ? 'security:change.enable' : 'security:change.disable', { name: a.name });
    case 'grant': return t('security:change.grant', { privileges: a.privileges.join(', '), object: objectLabel(a.object), name: a.to });
    case 'revoke': return t('security:change.revoke', { privileges: a.privileges.join(', '), object: objectLabel(a.object), name: a.from });
    case 'add_member': return t('security:change.addMember', { member: a.member, role: a.role });
    case 'remove_member': return t('security:change.removeMember', { member: a.member, role: a.role });
  }
}
function propose(action: SecurityAction) {
  const creates = action.action === 'create_user' || action.action === 'create_role' ? action.name : null;
  return queue(labelOf(action), () => securityApi.script(props.tab.connectionId, action), action, creates);
}
function unqueue(id: number) {
  pending.value = pending.value.filter((p) => p.id !== id);
}
async function discardPending() {
  if (pending.value.length > 1) {
    try {
      await ElMessageBox.confirm(t('security:pending.discardConfirm', { count: pending.value.length }), t('security:pending.discard'), {
        type: 'warning', confirmButtonText: t('security:pending.discard'), cancelButtonText: t('common:cancel'),
      });
    } catch { return; }
  }
  pending.value = [];
}
/** "Aplicar…": every pending change in one script, reviewed before it runs. */
function applyPending() {
  // A run still going owns the dialog: show it instead of replacing it.
  if (review.running) { review.open = true; return; }
  if (!pending.value.length) return;
  Object.assign(review, {
    open: true,
    ids: pending.value.map((p) => p.id),
    script: pending.value.map((p) => p.script).join('\n\n'),
    shown: pending.value.map((p) => p.shown).join('\n\n'),
    error: null, running: false, cancelling: false,
  });
}
// Closing the tab with changes not applied asks first.
async function guardClose(): Promise<boolean> {
  try {
    await ElMessageBox.confirm(t('security:pending.closeConfirm', { count: pending.value.length }), t('security:pending.title', { count: pending.value.length }), {
      type: 'warning', confirmButtonText: t('security:pending.closeAnyway'), cancelButtonText: t('common:cancel'),
    });
    return true;
  } catch {
    return false;
  }
}
watch(() => pending.value.length > 0, (on) => {
  if (on) closeGuards.set(props.tab.id, guardClose);
  else closeGuards.delete(props.tab.id);
}, { immediate: true });
onBeforeUnmount(() => closeGuards.delete(props.tab.id));

/** Same privilege on the same object (a pending revoke of a listed grant). */
function sameObject(a: ObjectRef | null, b: ObjectRef | null): boolean {
  if (!a || !b) return !a && !b;
  return a.name === b.name && (a.schema ?? null) === (b.schema ?? null);
}
/** Pending grants to the selected principal (shown muted in its table). */
const pendingGrants = computed(() => pending.value.flatMap((p) =>
  p.action?.action === 'grant' && p.action.to === current.value?.name ? [{ id: p.id, privileges: p.action.privileges.join(', '), object: objectLabel(p.action.object) }] : []));
function revoking(g: Grant): boolean {
  const name = current.value?.name;
  return pending.value.some((p) => p.action?.action === 'revoke' && p.action.from === name
    && p.action.privileges.includes(g.privilege) && sameObject(p.action.object, objectOf(g)));
}
/** Roles the selected principal is pending to join / leave. */
const joining = computed(() => pending.value.flatMap((p) =>
  p.action?.action === 'add_member' && p.action.member === current.value?.name ? [p.action.role] : []));
function leaving(role: string): boolean {
  return pending.value.some((p) => p.action?.action === 'remove_member' && p.action.member === current.value?.name && p.action.role === role);
}
async function copyScript() {
  // The copy never carries the password.
  await navigator.clipboard.writeText(review.shown);
  ElMessage.success(t('security:copied'));
}
/** "Seguir en segundo plano", or the dialog closed (Esc) while the run goes on. */
function sendToBackground(close?: () => void) {
  if (runTask && tasks.byId(runTask.id)?.state === 'running') runTask.background();
  close?.();
}
function cancelRun() {
  if (runTask) tasks.cancel(runTask.id);
}
async function run() {
  if (review.running) return;
  review.running = true;
  review.cancelling = false;
  review.error = null;
  // Its own session, so Cancelar stops exactly this run.
  const sessionId = `security-run:${Date.now()}`;
  const connectionId = props.tab.connectionId;
  const database = props.tab.database;
  const name = conns.byId(connectionId)?.name ?? '';
  const task = startTask({
    kind: 'security', title: t('tasks:backupsSecurity.run', { where: database ? `${name} / ${database}` : name }), connectionId, database,
    cancel: async () => {
      review.cancelling = true;
      // A cancel that didn't land re-enables the dialog's button (the store logs why).
      try { await api.cancelQuery(sessionId); } catch (e) { review.cancelling = false; throw e; }
    },
    reopen: () => { review.open = true; },
  });
  runTask = task;
  // One change at a time, in order, on the same session: each leaves the
  // list as soon as it ran, so a re-run after an error starts at the one
  // that failed instead of repeating what already went through.
  const entries = pending.value.filter((p) => review.ids.includes(p.id));
  const total = entries.length;
  const unit = t('security:pending.unit');
  const done: PendingChange[] = [];
  /** The error to show, or null when every change ran. */
  let failure: string | null = null;
  let cancelled = false;
  task.progress({ done: 0, total, unit });
  try {
    for (const entry of entries) {
      // Cancelling between changes: stop before the next one.
      if (task.isCancelling) { cancelled = true; break; }
      task.progress({ phase: entry.label });
      let err: string | null = null;
      try {
        const o = await api.executeQuery({ sessionId, connectionId, database, sql: entry.script, maxRows: 10, record: false });
        if (o.error) err = tb(o.error);
      } catch (e) {
        err = errorMessage(e);
      }
      if (err !== null) {
        if (task.isCancelling) { cancelled = true; break; }
        failure = `${t('security:pending.failed', { label: entry.label, applied: done.length, total })}\n\n${err}`;
        break;
      }
      done.push(entry);
      pending.value = pending.value.filter((p) => p.id !== entry.id);
      task.progress({ done: done.length, total, unit });
    }
    if (cancelled) {
      failure = t('security:pending.cancelled', { applied: done.length, total });
      task.cancelled(failure);
    } else if (failure !== null) {
      task.fail(failure);
    } else {
      task.finish(undefined, t('security:pending.summary', { applied: done.length }));
    }
    if (failure === null) {
      review.open = false;
      if (!tasks.byId(task.id)?.background) ElMessage.success(t('security:done'));
    } else {
      // The dialog now offers what's left: the failed change and the ones after it.
      const left = entries.filter((e) => !done.includes(e) && pending.value.some((p) => p.id === e.id));
      Object.assign(review, {
        error: failure,
        ids: left.map((e) => e.id),
        script: left.map((e) => e.script).join('\n\n'),
        shown: left.map((e) => e.shown).join('\n\n'),
      });
    }
    if (!alive) return;
    // Select the last user or role that was created; a dropped selection clears.
    let next: string | null | undefined;
    for (const p of done) {
      if (p.creates) next = p.creates;
      else if (p.action?.action === 'drop' && p.action.name === (next ?? selected.value)) next = null;
    }
    if (next !== undefined) selected.value = next;
    await load();
  } finally {
    review.running = false;
    review.cancelling = false;
    // Finished: "Ver detalle" shows the panel's detail, not the script ready to run again.
    task.setReopen(undefined);
    if (runTask === task) runTask = null;
    api.closeSession(sessionId).catch(() => {});
  }
}

// -- forms ------------------------------------------------------------------
const create = reactive({ open: false, kind: 'user' as 'user' | 'role', name: '', password: '' });
function openCreate(kind: 'user' | 'role') {
  Object.assign(create, { open: true, kind, name: '', password: '' });
}
function submitCreate() {
  if (!create.name.trim()) return;
  create.open = false;
  propose(create.kind === 'user'
    ? { action: 'create_user', name: create.name.trim(), password: create.password || null }
    : { action: 'create_role', name: create.name.trim() });
}
// "Asignar login…": a user of this database for a login the server already has.
const mapLogin = reactive({
  open: false, login: '', user: '', schema: '',
  logins: [] as string[], loading: false,
  /** Why the logins couldn't be listed ('' when they were): the login is typed. */
  unlisted: '',
});
/** The engine has schemas to default to (SQL Server's family; not SAP ASE). */
const mapSchemas = computed(() => !!spec.value?.object_kinds.includes('schema'));
async function openMapLogin() {
  const driver = conns.driverOf(props.tab.connectionId)?.id ?? '';
  const schema = mapSchemas.value && ['sqlserver', 'azuresql', 'babelfish'].includes(driver) ? 'dbo' : '';
  Object.assign(mapLogin, { open: true, login: '', user: '', schema, logins: [], loading: true, unlisted: '' });
  try {
    mapLogin.logins = await securityApi.unmappedLogins(props.tab.connectionId, props.tab.database);
  } catch (e) {
    mapLogin.unlisted = errorMessage(e);
  } finally {
    mapLogin.loading = false;
  }
}
/** The user's name follows the login until it's edited. */
watch(() => mapLogin.login, (login, before) => {
  if (!mapLogin.user || mapLogin.user === before) mapLogin.user = login;
});
function submitMapLogin() {
  const login = mapLogin.login.trim();
  const user = mapLogin.user.trim();
  if (!login || !user) return;
  mapLogin.open = false;
  const schema = (mapSchemas.value && mapLogin.schema.trim()) || null;
  queue(t('security:change.mapLogin', { login, user }), () => securityApi.mapLoginScript(props.tab.connectionId, login, user, schema), null, user);
}
const password = reactive({ open: false, value: '' });
function submitPassword() {
  if (!current.value || !password.value) return;
  password.open = false;
  propose({ action: 'set_password', name: current.value.name, password: password.value });
}
const grant = reactive({ privileges: [] as string[], scope: '' as string, object: '' as string, grantable: false });
const grantObject = computed<ObjectRef | null>(() => {
  if (!grant.scope) return null;
  if (grant.scope === 'schema' || grant.scope === 'database') return grant.object ? { kind: grant.scope, schema: null, name: grant.object } : null;
  const o = objects.value.find((x) => `${x.schema ?? ''}\u0001${x.name}` === grant.object);
  return o ? { kind: o.kind, schema: o.schema, name: o.name } : null;
});
async function submitGrant() {
  if (!current.value || !grant.privileges.length || (grant.scope && !grantObject.value)) return;
  const queued = await propose({ action: 'grant', privileges: [...grant.privileges], object: grantObject.value, to: current.value.name, grantable: grant.grantable });
  // Ready for the next one; the scope and object stay (several grants on one object).
  if (queued) {
    grant.privileges = [];
    grant.grantable = false;
  }
}
/** A direct grant's object as an ObjectRef, to revoke it. */
function objectOf(g: Grant): ObjectRef | null {
  if (!g.object) return null;
  // A named database (InfluxDB, CouchDB, MongoDB…); SQL Server's whole-database
  // grants come without an object.
  if (g.object_kind === 'database') return { kind: 'database', schema: null, name: g.object };
  if (g.object_kind === 'schema') return { kind: 'schema', schema: null, name: g.object };
  const dot = g.object.indexOf('.');
  return dot > 0 ? { kind: g.object_kind ?? 'table', schema: g.object.slice(0, dot), name: g.object.slice(dot + 1) } : { kind: g.object_kind ?? 'table', schema: null, name: g.object };
}
function revoke(g: Grant) {
  if (!current.value) return;
  propose({ action: 'revoke', privileges: [g.privilege], object: objectOf(g), from: current.value.name });
}
const addRole = ref('');
function submitAddRole() {
  if (!current.value || !addRole.value) return;
  propose({ action: 'add_member', role: addRole.value, member: current.value.name });
  addRole.value = '';
}
</script>

<template>
  <div class="sv-root">
  <div v-if="pending.length" class="sv-pending">
    <div class="sv-pending-head">
      <button class="sv-pending-toggle" @click="pendingOpen = !pendingOpen">
        <el-icon class="sv-caret" :class="{ open: pendingOpen }"><ei-arrow-right /></el-icon>
        {{ $t('security:pending.title', { count: pending.length }) }}
      </button>
      <span style="flex: 1" />
      <el-button size="small" :disabled="review.running" @click="discardPending">{{ $t('security:pending.discard') }}</el-button>
      <el-button size="small" type="primary" :disabled="!canEdit" @click="applyPending">{{ $t('security:pending.apply') }}</el-button>
    </div>
    <ol v-if="pendingOpen" class="sv-pending-list">
      <li v-for="p in pending" :key="p.id">
        <span>{{ p.label }}</span>
        <button :title="$t('security:pending.remove')" :disabled="review.running && review.ids.includes(p.id)" @click="unqueue(p.id)">×</button>
      </li>
    </ol>
  </div>
  <div class="sv">
    <aside class="sv-list">
      <div class="sv-list-head">
        <el-input v-model="search" size="small" clearable :placeholder="$t('security:search')">
          <template #prefix><el-icon><ei-search /></el-icon></template>
        </el-input>
        <el-button size="small" text :title="$t('common:refresh')" @click="load"><el-icon><ei-refresh /></el-icon></el-button>
      </div>
      <div class="sv-list-actions" v-if="spec && !readOnly" :title="noPermission">
        <el-button v-if="spec.create_user" size="small" :disabled="!canEdit" @click="openCreate('user')">{{ $t('security:newUser') }}</el-button>
        <el-button v-if="canMapLogin" size="small" :disabled="!canEdit" @click="openMapLogin">{{ $t('security:mapLogin') }}</el-button>
        <el-button v-if="spec.create_role" size="small" :disabled="!canEdit" @click="openCreate('role')">{{ $t('security:newRole') }}</el-button>
      </div>
      <div v-if="noPermission && !readOnly" class="sv-error">{{ noPermission }}</div>
      <div v-if="error" class="sv-error">{{ error }}</div>
      <div class="sv-items" v-loading="loading">
        <template v-for="[title, list] in [[$t('security:users'), users], [$t('security:roles'), roles]] as const" :key="title">
          <div v-if="list.length" class="sv-group">{{ title }} <span>{{ list.length }}</span></div>
          <button v-for="p in list" :key="p.name" class="sv-item" :class="{ on: selected === p.name, muted: p.system }" @click="selected = p.name">
            <el-icon><ei-user v-if="p.kind === 'user'" /><ei-avatar v-else /></el-icon>
            <span class="sv-name">{{ p.name }}</span>
            <span v-if="p.superuser" class="sv-badge gold">{{ $t('security:superuser') }}</span>
            <span v-if="p.disabled" class="sv-badge">{{ $t('security:disabled') }}</span>
          </button>
        </template>
      </div>
    </aside>

    <section class="sv-detail">
      <div v-if="!current" class="sv-empty">{{ $t('security:pick') }}</div>
      <template v-else>
        <header class="sv-head">
          <h2>{{ current.name }}</h2>
          <span class="sv-kind">{{ current.kind === 'user' ? $t('security:user') : $t('security:role') }}</span>
          <span v-if="current.system" class="sv-badge">{{ $t('security:system') }}</span>
          <span style="flex: 1" />
          <template v-if="spec && canEdit">
            <el-button v-if="current.kind === 'user' && spec.passwords" size="small" @click="password.open = true; password.value = ''">{{ $t('security:setPassword') }}</el-button>
            <el-button v-if="current.kind === 'user' && current.disabled !== null" size="small" @click="propose({ action: 'set_login', name: current.name, enabled: !!current.disabled })">
              {{ current.disabled ? $t('security:enable') : $t('security:disable') }}
            </el-button>
            <el-button v-if="!current.system" size="small" type="danger" plain @click="propose({ action: 'drop', name: current.name, kind: current.kind })">{{ $t('common:delete') }}</el-button>
          </template>
        </header>
        <div v-if="current.details.length" class="sv-details">
          <div v-for="[k, v] in current.details" :key="k"><span>{{ tb(k) }}</span><b>{{ tb(v) }}</b></div>
        </div>

        <h3 v-if="spec?.membership">{{ $t('security:memberOf') }}</h3>
        <div v-if="spec?.membership" class="sv-roles">
          <span v-for="r in current.member_of" :key="r" class="sv-chip" :class="{ queued: leaving(r) }">
            {{ r }}
            <span v-if="leaving(r)" class="sv-dim">· {{ $t('security:pending.leaving') }}</span>
            <button v-else-if="canEdit" :title="$t('security:removeFromRole')" @click="propose({ action: 'remove_member', role: r, member: current.name })">×</button>
          </span>
          <span v-for="r in joining" :key="`+${r}`" class="sv-chip queued">{{ r }} <span class="sv-dim">· {{ $t('security:pending.mark') }}</span></span>
          <span v-if="!current.member_of.length" class="sv-dim">{{ $t('security:noRoles') }}</span>
          <template v-if="canEdit">
            <el-select v-model="addRole" size="small" filterable :placeholder="$t('security:addToRole')" style="width: 200px">
              <el-option v-for="r in roleNames.filter((x) => !current!.member_of.includes(x) && !joining.includes(x))" :key="r" :label="r" :value="r" />
            </el-select>
            <el-button size="small" :disabled="!addRole" @click="submitAddRole">{{ $t('security:add') }}</el-button>
          </template>
        </div>

        <h3>{{ $t('security:permissions') }}</h3>
        <div v-if="grantsError" class="sv-error">{{ grantsError }}</div>
        <table v-else class="sv-table">
          <thead><tr><th>{{ $t('security:privilege') }}</th><th>{{ $t('security:object') }}</th><th>{{ $t('security:via') }}</th><th /></tr></thead>
          <tbody>
            <tr v-for="(g, i) in grants" :key="i" :class="{ denied: g.denied, inherited: g.via }">
              <td>{{ g.privilege }}<span v-if="g.denied" class="sv-badge">DENY</span><span v-if="g.grantable" class="sv-badge">{{ $t('security:grantable') }}</span></td>
              <td>{{ g.object ?? $t('security:everything') }}<span v-if="g.object_kind" class="sv-dim"> · {{ g.object_kind }}</span></td>
              <td>{{ g.via ?? $t('security:direct') }}</td>
              <td class="sv-act">
                <span v-if="revoking(g)" class="sv-dim">{{ $t('security:pending.revoking') }}</span>
                <el-button v-else-if="!g.via && canEdit" size="small" text type="danger" @click="revoke(g)">{{ $t('security:revoke') }}</el-button>
              </td>
            </tr>
            <tr v-for="p in pendingGrants" :key="`p${p.id}`" class="queued">
              <td>{{ p.privileges }}</td>
              <td>{{ p.object }}</td>
              <td>{{ $t('security:direct') }}</td>
              <td class="sv-act sv-dim">{{ $t('security:pending.mark') }}</td>
            </tr>
            <tr v-if="!grants.length && !pendingGrants.length"><td colspan="4" class="sv-dim">{{ $t('security:noPermissions') }}</td></tr>
          </tbody>
        </table>

        <div v-if="spec && canEdit" class="sv-grant">
          <el-select v-model="grant.privileges" multiple filterable allow-create size="small" :placeholder="$t('security:privileges')" style="width: 280px">
            <el-option v-for="p in spec.privileges" :key="p" :label="p" :value="p" />
          </el-select>
          <el-select v-model="grant.scope" size="small" style="width: 150px" @change="grant.object = ''">
            <el-option v-for="k in spec.object_kinds" :key="k" :label="k ? $t(`security:kind.${k}`, { defaultValue: k }) : $t('security:everything')" :value="k" />
          </el-select>
          <el-select v-if="grant.scope === 'schema'" v-model="grant.object" size="small" filterable allow-create style="width: 200px">
            <el-option v-for="s in schemas" :key="s" :label="s" :value="s" />
          </el-select>
          <el-select v-else-if="grant.scope === 'database'" v-model="grant.object" size="small" filterable allow-create style="width: 200px">
            <el-option v-for="d in databases" :key="d" :label="d" :value="d" />
          </el-select>
          <el-select v-else-if="grant.scope" v-model="grant.object" size="small" filterable style="width: 240px">
            <el-option v-for="o in objects.filter((x) => x.kind === grant.scope)" :key="`${o.schema}.${o.name}`" :label="o.schema ? `${o.schema}.${o.name}` : o.name" :value="`${o.schema ?? ''}\u0001${o.name}`" />
          </el-select>
          <el-checkbox v-model="grant.grantable" size="small">{{ $t('security:withGrant') }}</el-checkbox>
          <el-button size="small" type="primary" :disabled="!grant.privileges.length || (!!grant.scope && !grantObject)" @click="submitGrant">{{ $t('security:grant') }}</el-button>
        </div>
      </template>
    </section>

    <el-dialog v-model="create.open" :title="create.kind === 'user' ? $t('security:newUser') : $t('security:newRole')" width="420px" append-to-body>
      <el-form label-position="top" @submit.prevent="submitCreate">
        <el-form-item :label="$t('security:name')"><el-input v-model="create.name" autofocus /></el-form-item>
        <el-form-item v-if="create.kind === 'user' && spec?.passwords" :label="$t('security:password')"><el-input v-model="create.password" type="password" show-password /></el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="create.open = false">{{ $t('common:cancel') }}</el-button>
        <el-button type="primary" :disabled="!create.name.trim()" @click="submitCreate">{{ $t('security:pending.add') }}</el-button>
      </template>
    </el-dialog>
    <el-dialog v-model="mapLogin.open" :title="$t('security:mapLoginTitle')" width="440px" append-to-body>
      <p class="sv-dim sv-hint">{{ $t('security:mapLoginHint') }}</p>
      <el-form label-position="top" @submit.prevent="submitMapLogin">
        <el-form-item :label="$t('security:login')">
          <template v-if="mapLogin.unlisted">
            <el-input v-model="mapLogin.login" :placeholder="$t('security:loginType')" autofocus />
            <!-- Why they couldn't be listed (Azure SQL Database: the logins are in master). -->
            <div class="sv-dim sv-hint">{{ mapLogin.unlisted }}</div>
          </template>
          <template v-else>
            <el-select v-model="mapLogin.login" filterable allow-create default-first-option :loading="mapLogin.loading" :placeholder="$t('security:loginPick')" style="width: 100%">
              <el-option v-for="l in mapLogin.logins" :key="l" :label="l" :value="l" />
            </el-select>
            <div v-if="!mapLogin.loading && !mapLogin.logins.length" class="sv-dim sv-hint">{{ $t('security:noUnmapped') }}</div>
          </template>
        </el-form-item>
        <el-form-item :label="$t('security:userName')"><el-input v-model="mapLogin.user" /></el-form-item>
        <el-form-item v-if="mapSchemas" :label="$t('security:defaultSchema')">
          <el-select v-model="mapLogin.schema" filterable allow-create clearable default-first-option :placeholder="$t('security:optional')" style="width: 100%">
            <el-option v-for="s in schemas" :key="s" :label="s" :value="s" />
          </el-select>
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="mapLogin.open = false">{{ $t('common:cancel') }}</el-button>
        <el-button type="primary" :disabled="!mapLogin.login.trim() || !mapLogin.user.trim()" @click="submitMapLogin">{{ $t('security:pending.add') }}</el-button>
      </template>
    </el-dialog>
    <el-dialog v-model="password.open" :title="$t('security:setPassword')" width="420px" append-to-body>
      <el-input v-model="password.value" type="password" show-password autofocus @keyup.enter="submitPassword" />
      <template #footer>
        <el-button @click="password.open = false">{{ $t('common:cancel') }}</el-button>
        <el-button type="primary" :disabled="!password.value" @click="submitPassword">{{ $t('security:pending.add') }}</el-button>
      </template>
    </el-dialog>
    <el-dialog v-model="review.open" :title="$t('security:reviewTitle')" width="680px" append-to-body :close-on-click-modal="!review.running" :show-close="!review.running" @close="review.running && sendToBackground()">
      <p class="sv-dim">{{ $t('security:reviewHint') }}</p>
      <pre class="sv-script nm-selectable">{{ review.shown }}</pre>
      <div v-if="review.error" class="sv-error">{{ review.error }}</div>
      <template #footer>
        <div class="sv-foot">
          <el-button @click="copyScript">{{ $t('common:copy') }}</el-button>
          <span style="flex: 1" />
          <template v-if="review.running">
            <el-button :disabled="review.cancelling" @click="cancelRun">{{ $t('common:cancel') }}</el-button>
            <el-button @click="sendToBackground(() => (review.open = false))">{{ $t('tasks:panel.background') }}</el-button>
          </template>
          <el-button v-else @click="review.open = false">{{ $t('common:cancel') }}</el-button>
          <el-button type="primary" :loading="review.running" :disabled="!review.ids.length" @click="run">{{ $t('security:run') }}</el-button>
        </div>
      </template>
    </el-dialog>
  </div>
  </div>
</template>

<style scoped>
.sv-root { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--ide-editor); }
.sv { display: flex; flex: 1; min-height: 0; background: var(--ide-editor); }
.sv-pending { flex: none; padding: 6px 12px; border-bottom: 1px solid var(--nm-border); background: color-mix(in srgb, var(--nm-warning) 8%, var(--ide-editor)); font-size: 12.5px; }
.sv-pending-head { display: flex; align-items: center; gap: 8px; }
.sv-pending-head .el-button { margin: 0; }
.sv-pending-toggle { display: inline-flex; align-items: center; gap: 4px; border: 0; background: none; padding: 0; font: inherit; font-weight: 600; color: var(--nm-text-strong); cursor: pointer; }
.sv-caret { transition: transform .15s; }
.sv-caret.open { transform: rotate(90deg); }
.sv-pending-list { margin: 6px 0 2px; padding-left: 22px; max-height: 160px; overflow: auto; }
.sv-pending-list li { padding: 1px 0; color: var(--nm-text); }
.sv-pending-list li > * { vertical-align: middle; }
.sv-pending-list button { margin-left: 8px; border: 0; background: none; color: var(--nm-text-dim); cursor: pointer; padding: 0 2px; }
.sv-pending-list button:disabled { cursor: default; opacity: .4; }
.sv-chip.queued { background: none; border: 1px dashed var(--nm-border); color: var(--nm-text-dim); }
.sv-table tr.queued td { color: var(--nm-text-dim); font-style: italic; }
.sv-list { width: 280px; flex: none; display: flex; flex-direction: column; border-right: 1px solid var(--nm-border); }
.sv-list-head { display: flex; gap: 4px; padding: 10px 10px 6px; }
.sv-list-actions { display: flex; flex-wrap: wrap; gap: 6px; padding: 0 10px 8px; }
.sv-list-actions .el-button { margin: 0; }
.sv-items { flex: 1; overflow: auto; padding-bottom: 12px; }
.sv-group { padding: 8px 12px 4px; font-size: 11px; font-weight: 600; letter-spacing: .05em; text-transform: uppercase; color: var(--nm-text-dim); }
.sv-group span { font-weight: 400; }
.sv-item { display: flex; align-items: center; gap: 6px; width: 100%; padding: 4px 12px; border: 0; background: none; color: var(--nm-text); font: inherit; font-size: 12.5px; cursor: pointer; text-align: left; }
.sv-item:hover { background: var(--ide-hover); }
.sv-item.on { background: var(--ide-selection); color: var(--nm-text-strong); }
.sv-item.muted { color: var(--nm-text-dim); }
.sv-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sv-badge { margin-left: 6px; padding: 0 6px; border-radius: 8px; font-size: 10.5px; background: var(--ide-hover); color: var(--nm-text-dim); white-space: nowrap; }
.sv-badge.gold { background: color-mix(in srgb, var(--nm-warning) 25%, transparent); color: var(--nm-text-strong); }
.sv-detail { flex: 1; min-width: 0; overflow: auto; padding: 16px 22px 24px; }
.sv-empty { color: var(--nm-text-dim); padding: 30px 0; }
.sv-head { display: flex; align-items: center; gap: 8px; }
.sv-head h2 { margin: 0; font-size: 17px; color: var(--nm-text-strong); }
.sv-head .el-button { margin: 0; }
.sv-kind { color: var(--nm-text-dim); font-size: 12px; }
.sv-details { display: flex; flex-wrap: wrap; gap: 6px 24px; margin-top: 10px; font-size: 12px; }
.sv-details span { color: var(--nm-text-dim); margin-right: 6px; }
.sv-details b { font-weight: 500; color: var(--nm-text); }
.sv-detail h3 { margin: 20px 0 8px; font-size: 13px; color: var(--nm-text-strong); }
.sv-roles { display: flex; flex-wrap: wrap; align-items: center; gap: 6px; }
.sv-roles .el-button { margin: 0; }
.sv-chip { display: inline-flex; align-items: center; gap: 4px; padding: 2px 8px; border-radius: 10px; background: var(--ide-selection); font-size: 12px; }
.sv-chip button { border: 0; background: none; color: var(--nm-text-dim); cursor: pointer; padding: 0 2px; }
.sv-dim { color: var(--nm-text-dim); font-size: 12px; }
.sv-hint { margin: 4px 0 0; line-height: 1.4; }
.sv-table { width: 100%; border-collapse: collapse; font-size: 12.5px; }
.sv-table th { text-align: left; font-weight: 500; color: var(--nm-text-dim); padding: 5px 8px; border-bottom: 1px solid var(--nm-border); }
.sv-table td { padding: 4px 8px; border-bottom: 1px solid var(--nm-border-soft); color: var(--nm-text); }
.sv-table tr.inherited td { color: var(--nm-text-dim); }
.sv-table tr.denied td:first-child { color: var(--nm-danger); }
.sv-act { text-align: right; }
.sv-grant { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; margin-top: 12px; }
.sv-grant .el-button { margin: 0; }
.sv-error { margin: 8px 10px; padding: 8px 10px; border-radius: 3px; border: 1px solid color-mix(in srgb, var(--nm-danger) 50%, transparent); background: color-mix(in srgb, var(--nm-danger) 10%, transparent); color: var(--nm-text); white-space: pre-wrap; font-size: 12px; }
.sv-script { margin: 0; padding: 10px 12px; max-height: 45vh; overflow: auto; border: 1px solid var(--nm-border); border-radius: 4px; background: var(--ide-editor, var(--nm-bg-elev)); font-family: var(--nm-mono); font-size: 12.5px; color: var(--nm-text-strong); white-space: pre-wrap; }
.sv-foot { display: flex; align-items: center; gap: 8px; }
.sv-foot .el-button { margin: 0; }
</style>
