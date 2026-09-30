<script setup lang="ts">
import { computed, onMounted, reactive, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import { securityApi, type Grant, type Principal, type SecurityAction } from '../api/security';
import type { ObjectRef } from '../api/types';
import { tb } from '../i18n/backend';
import { dbKey, useConnectionsStore } from '../stores/connections';
import type { SecurityTab } from '../stores/tabs';

// Users and permissions (docs/usuarios-y-permisos.md): the server's (or the
// database's) users and roles, what each can do, and changes as scripts in
// the engine's language — shown (password hidden) and run only on the
// user's click, never kept in the history.

const props = defineProps<{ tab: SecurityTab }>();
const conns = useConnectionsStore();
const { t } = useTranslation();
const spec = computed(() => conns.driverOf(props.tab.connectionId)?.security ?? null);
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
const review = reactive<{ open: boolean; action: SecurityAction | null; script: string; shown: string; error: string | null; running: boolean }>({
  open: false, action: null, script: '', shown: '', error: null, running: false,
});
async function propose(action: SecurityAction) {
  try {
    const s = await securityApi.script(props.tab.connectionId, action);
    Object.assign(review, { open: true, action, script: s.script, shown: s.shown, error: null, running: false });
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}
async function copyScript() {
  // The copy never carries the password.
  await navigator.clipboard.writeText(review.shown);
  ElMessage.success(t('security:copied'));
}
async function run() {
  review.running = true;
  review.error = null;
  const sessionId = `security-run:${Date.now()}`;
  try {
    const o = await api.executeQuery({ sessionId, connectionId: props.tab.connectionId, database: props.tab.database, sql: review.script, maxRows: 10, record: false });
    if (o.error) { review.error = tb(o.error); return; }
    review.open = false;
    ElMessage.success(t('security:done'));
    const a = review.action;
    if (a && (a.action === 'create_user' || a.action === 'create_role')) selected.value = a.name;
    if (a && a.action === 'drop') selected.value = null;
    await load();
  } catch (e) {
    review.error = errorMessage(e);
  } finally {
    review.running = false;
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
function submitGrant() {
  if (!current.value || !grant.privileges.length || (grant.scope && !grantObject.value)) return;
  propose({ action: 'grant', privileges: grant.privileges, object: grantObject.value, to: current.value.name, grantable: grant.grantable });
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
          <span v-for="r in current.member_of" :key="r" class="sv-chip">
            {{ r }}
            <button v-if="canEdit" :title="$t('security:removeFromRole')" @click="propose({ action: 'remove_member', role: r, member: current.name })">×</button>
          </span>
          <span v-if="!current.member_of.length" class="sv-dim">{{ $t('security:noRoles') }}</span>
          <template v-if="canEdit">
            <el-select v-model="addRole" size="small" filterable :placeholder="$t('security:addToRole')" style="width: 200px">
              <el-option v-for="r in roleNames.filter((x) => !current!.member_of.includes(x))" :key="r" :label="r" :value="r" />
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
              <td class="sv-act"><el-button v-if="!g.via && canEdit" size="small" text type="danger" @click="revoke(g)">{{ $t('security:revoke') }}</el-button></td>
            </tr>
            <tr v-if="!grants.length"><td colspan="4" class="sv-dim">{{ $t('security:noPermissions') }}</td></tr>
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
        <el-button type="primary" :disabled="!create.name.trim()" @click="submitCreate">{{ $t('security:seeScript') }}</el-button>
      </template>
    </el-dialog>
    <el-dialog v-model="password.open" :title="$t('security:setPassword')" width="420px" append-to-body>
      <el-input v-model="password.value" type="password" show-password autofocus @keyup.enter="submitPassword" />
      <template #footer>
        <el-button @click="password.open = false">{{ $t('common:cancel') }}</el-button>
        <el-button type="primary" :disabled="!password.value" @click="submitPassword">{{ $t('security:seeScript') }}</el-button>
      </template>
    </el-dialog>
    <el-dialog v-model="review.open" :title="$t('security:reviewTitle')" width="680px" append-to-body>
      <p class="sv-dim">{{ $t('security:reviewHint') }}</p>
      <pre class="sv-script nm-selectable">{{ review.shown }}</pre>
      <div v-if="review.error" class="sv-error">{{ review.error }}</div>
      <template #footer>
        <div class="sv-foot">
          <el-button @click="copyScript">{{ $t('common:copy') }}</el-button>
          <span style="flex: 1" />
          <el-button :disabled="review.running" @click="review.open = false">{{ $t('common:cancel') }}</el-button>
          <el-button type="primary" :loading="review.running" @click="run">{{ $t('security:run') }}</el-button>
        </div>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.sv { display: flex; height: 100%; min-height: 0; background: var(--ide-editor); }
.sv-list { width: 280px; flex: none; display: flex; flex-direction: column; border-right: 1px solid var(--nm-border); }
.sv-list-head { display: flex; gap: 4px; padding: 10px 10px 6px; }
.sv-list-actions { display: flex; gap: 6px; padding: 0 10px 8px; }
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
