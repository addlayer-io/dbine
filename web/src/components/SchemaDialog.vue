<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import { schemasApi, type SchemaGrant } from '../api/schemas';
import { securityApi, type Principal } from '../api/security';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { newQuery } from '../composables/actions';

// "Nuevo esquema…" (name, owner, grants) and "Borrar esquema…" (how many
// objects it holds, with its content where the engine can): the script in
// the engine's language is shown live and runs only once the user confirms.

const props = defineProps<{ connectionId: string; database: string; mode: 'create' | 'drop'; schema?: string }>();
const emit = defineEmits<{ close: []; done: [] }>();

const { t } = useTranslation();
const conns = useConnectionsStore();
const driver = computed(() => conns.driverOf(props.connectionId));
const spec = computed(() => driver.value?.schema_spec ?? null);
const where = computed(() => props.database || conns.byId(props.connectionId)?.name || '');

// -- create ---------------------------------------------------------------------
// Engines whose schema names carry their database (Couchbase: `bucket.scope`)
// start with it written, so a bare scope name doesn't fail in the preview.
const PATH_SCHEMAS = new Set(['couchbase']);
const prefix = PATH_SCHEMAS.has(driver.value?.id ?? '') && props.database ? `${props.database}.` : '';
const name = ref(prefix);
const owner = ref('');
const grants = reactive<SchemaGrant[]>([]);
const principals = ref<Principal[]>([]);
// Only the kinds that can own a schema on this engine (users, roles or both).
const owners = computed(() => {
  const k = spec.value?.owner_kinds ?? 'both';
  return k === 'both' ? principals.value : principals.value.filter((p) => p.kind === (k === 'users' ? 'user' : 'role'));
});

function addGrant() {
  grants.push({ principal: '', privileges: [], grantable: false });
}
const complete = (g: SchemaGrant) => !!g.principal.trim() && g.privileges.length > 0;
// Engines that refuse a grant on a schema with grant option hide the switch.
const grantOption = computed(() => spec.value?.grant_option !== false);
const incomplete = computed(() => grants.some((g) => !complete(g)));

async function loadPrincipals() {
  // Engines whose users live in each database (SQL Server) list that one's.
  const perDb = driver.value?.security?.per_database ?? false;
  try {
    principals.value = await securityApi.principals(props.connectionId, perDb ? props.database : '');
  } catch {
    principals.value = []; // the selects still take a typed name
  }
}

// -- drop -----------------------------------------------------------------------
const cascade = ref(false);
const count = ref<number | null>(null);
const countError = ref<string | null>(null);

async function loadCount() {
  if (!props.schema) return;
  try {
    count.value = await schemasApi.objectCount(props.connectionId, props.database, props.schema);
  } catch (e) {
    countError.value = errorMessage(e);
  }
}

// -- the script, regenerated as the form changes ---------------------------------
const script = ref('');
const scriptError = ref<string | null>(null);
const running = ref(false);
const runError = ref<string | null>(null);
let timer: ReturnType<typeof setTimeout> | undefined;
let generation = 0;

async function build() {
  const n = ++generation;
  scriptError.value = null;
  try {
    let s = '';
    if (props.mode === 'create') {
      if (name.value.trim() && name.value.trim() !== prefix) {
        s = await schemasApi.createScript(props.connectionId, props.database, name.value.trim(), (spec.value?.owner && owner.value.trim()) || null, grants.filter(complete).map((g) => ({ ...g, grantable: grantOption.value && g.grantable })));
      }
    } else if (props.schema) {
      s = await schemasApi.dropScript(props.connectionId, props.database, props.schema, !!spec.value?.cascade && cascade.value);
    }
    if (n === generation) script.value = s;
  } catch (e) {
    if (n === generation) {
      script.value = '';
      scriptError.value = errorMessage(e);
    }
  }
}
function schedule() {
  clearTimeout(timer);
  timer = setTimeout(build, 300);
}
watch([name, owner, cascade, () => JSON.stringify(grants)], schedule);
onBeforeUnmount(() => clearTimeout(timer));

onMounted(() => {
  if (props.mode === 'create') {
    if (spec.value?.owner || spec.value?.privileges.length) loadPrincipals();
  } else {
    loadCount();
  }
  build();
});

const canRun = computed(() => !!script.value && !scriptError.value && !running.value && (props.mode === 'drop' || !incomplete.value));

async function copyScript() {
  await navigator.clipboard.writeText(script.value);
  ElMessage.success({ message: t('schemas:copied'), duration: 1200 });
}
function openInQuery() {
  const label = props.mode === 'create' ? name.value.trim() : props.schema ?? '';
  newQuery(props.connectionId, props.database, script.value, t('schemas:scriptName', { name: label }));
  emit('close');
}

async function run() {
  const target = props.mode === 'create' ? name.value.trim() : props.schema ?? '';
  const withContent = props.mode === 'drop' && cascade.value;
  try {
    await ElMessageBox.confirm(
      props.mode === 'create'
        ? t('schemas:confirmCreate', { name: target, db: where.value })
        : t(withContent ? 'schemas:confirmDropCascade' : 'schemas:confirmDrop', { name: target, db: where.value }),
      props.mode === 'create' ? t('schemas:confirmCreateTitle') : t('schemas:dropTitle', { name: target }),
      props.mode === 'create'
        ? { confirmButtonText: t('schemas:run'), cancelButtonText: t('common:cancel'), type: 'info' }
        : { confirmButtonText: t('schemas:drop'), cancelButtonText: t('common:cancel'), type: 'error', confirmButtonClass: 'el-button--danger' },
    );
  } catch { return; }
  running.value = true;
  runError.value = null;
  const sessionId = `schema-run:${Date.now()}`;
  try {
    const o = await api.executeQuery({ sessionId, connectionId: props.connectionId, database: props.database, sql: script.value, maxRows: 10, record: false });
    if (o.error) { runError.value = tb(o.error); return; }
    ElMessage.success(props.mode === 'create' ? t('schemas:created', { name: target }) : t('schemas:dropped', { name: target }));
    emit('done');
    emit('close');
  } catch (e) {
    runError.value = errorMessage(e);
  } finally {
    running.value = false;
    api.closeSession(sessionId).catch(() => {});
  }
}
</script>

<template>
  <el-dialog
    :model-value="true" :width="mode === 'create' ? '720px' : '560px'" append-to-body
    :title="mode === 'create' ? $t('schemas:newTitle', { db: where }) : $t('schemas:dropTitle', { name: schema })"
    :close-on-click-modal="false" :close-on-press-escape="!running" :show-close="!running" @close="emit('close')"
  >
    <el-form v-if="mode === 'create'" label-position="top" @submit.prevent>
      <el-form-item :label="$t('schemas:name')">
        <el-input v-model="name" autofocus :disabled="running" />
      </el-form-item>
      <el-form-item v-if="spec?.owner" :label="$t('schemas:owner')">
        <el-select v-model="owner" filterable allow-create clearable :disabled="running" :placeholder="$t('schemas:ownerPlaceholder')" style="width: 100%">
          <el-option v-for="p in owners" :key="p.name" :label="p.name" :value="p.name" />
        </el-select>
        <div class="sd-help">{{ $t('schemas:ownerHelp') }}</div>
      </el-form-item>
      <el-form-item v-if="spec?.privileges.length" :label="$t('schemas:grants')">
        <div class="sd-grants">
          <div class="sd-help">{{ $t('schemas:grantsHelp') }}</div>
          <table class="sd-table">
            <thead>
              <tr><th>{{ $t('schemas:principal') }}</th><th>{{ $t('schemas:privileges') }}</th><th v-if="grantOption">{{ $t('schemas:grantable') }}</th><th /></tr>
            </thead>
            <tbody>
              <tr v-for="(g, i) in grants" :key="i">
                <td>
                  <el-select v-model="g.principal" size="small" filterable allow-create :disabled="running" style="width: 180px">
                    <el-option v-for="p in principals" :key="p.name" :label="p.name" :value="p.name" />
                  </el-select>
                </td>
                <td>
                  <el-select v-model="g.privileges" size="small" multiple :disabled="running" style="width: 100%">
                    <el-option v-for="p in spec.privileges" :key="p" :label="p" :value="p" />
                  </el-select>
                </td>
                <td v-if="grantOption" class="sd-center"><el-switch v-model="g.grantable" size="small" :disabled="running" /></td>
                <td class="sd-center">
                  <el-button size="small" text type="danger" :disabled="running" :title="$t('schemas:removeGrant')" @click="grants.splice(i, 1)">
                    <el-icon><ei-delete /></el-icon>
                  </el-button>
                </td>
              </tr>
              <tr v-if="!grants.length"><td :colspan="grantOption ? 4 : 3" class="sd-dim">{{ $t('schemas:noGrants') }}</td></tr>
            </tbody>
          </table>
          <el-button size="small" :disabled="running" @click="addGrant"><el-icon><ei-plus /></el-icon>{{ $t('schemas:addGrant') }}</el-button>
          <div v-if="incomplete" class="sd-warn">{{ $t('schemas:incomplete') }}</div>
        </div>
      </el-form-item>
    </el-form>

    <template v-else>
      <p class="sd-p">{{ $t('schemas:dropIntro', { name: schema, db: where }) }}</p>
      <p class="sd-p">
        <template v-if="countError">{{ $t('schemas:countFailed', { error: countError }) }}</template>
        <template v-else-if="count === null">{{ $t('schemas:counting') }}</template>
        <template v-else-if="count === 0">{{ $t('schemas:empty') }}</template>
        <template v-else>{{ $t('schemas:objects', { count }) }}</template>
      </p>
      <template v-if="spec?.cascade">
        <el-checkbox v-model="cascade" :disabled="running">{{ $t('schemas:cascade') }}</el-checkbox>
        <div class="sd-help">{{ $t('schemas:cascadeHelp') }}</div>
      </template>
      <div v-if="count && !cascade" class="sd-warn">{{ $t('schemas:notEmpty') }}</div>
    </template>

    <div class="sd-script-head">{{ $t('schemas:script') }}</div>
    <div class="sd-help">{{ $t('schemas:scriptHint') }}</div>
    <div v-if="scriptError" class="sd-error">{{ scriptError }}</div>
    <pre v-else class="sd-script nm-selectable">{{ script || $t('schemas:scriptEmpty') }}</pre>
    <div v-if="runError" class="sd-error">{{ runError }}</div>

    <template #footer>
      <div class="sd-foot">
        <el-button :disabled="!script" @click="copyScript">{{ $t('common:copy') }}</el-button>
        <el-button :disabled="!script || running" @click="openInQuery">{{ $t('schemas:openInQuery') }}</el-button>
        <span style="flex: 1" />
        <el-button :disabled="running" @click="emit('close')">{{ $t('common:cancel') }}</el-button>
        <el-button :type="mode === 'drop' ? 'danger' : 'primary'" :loading="running" :disabled="!canRun" @click="run">
          {{ mode === 'drop' ? $t('schemas:drop') : $t('schemas:run') }}
        </el-button>
      </div>
    </template>
  </el-dialog>
</template>

<style scoped lang="scss">
.sd-help { font-size: 12px; color: var(--nm-text-dim); margin-top: 4px; line-height: 1.4; }
.sd-dim { color: var(--nm-text-dim); font-size: 12px; }
.sd-p { margin: 0 0 8px; }
.sd-grants { width: 100%; display: flex; flex-direction: column; align-items: flex-start; gap: 8px; }
.sd-table { width: 100%; border-collapse: collapse; font-size: 12.5px; }
.sd-table th { text-align: left; font-weight: 500; color: var(--nm-text-dim); padding: 4px 6px; border-bottom: 1px solid var(--nm-border); }
.sd-table td { padding: 4px 6px; border-bottom: 1px solid var(--nm-border-soft); vertical-align: middle; }
.sd-center { text-align: center; }
.sd-warn { font-size: 12px; color: var(--nm-warning); margin-top: 6px; }
.sd-script-head { margin-top: 14px; font-weight: 600; font-size: 13px; color: var(--nm-text-strong); }
.sd-script {
  margin: 8px 0 0; padding: 10px 12px; min-height: 48px; max-height: 32vh; overflow: auto; border: 1px solid var(--nm-border); border-radius: 4px;
  background: var(--ide-editor, var(--nm-bg-elev)); font-family: var(--nm-mono); font-size: 12.5px; color: var(--nm-text-strong); white-space: pre-wrap;
}
.sd-error {
  margin-top: 8px; padding: 8px 10px; border-radius: 3px; border: 1px solid color-mix(in srgb, var(--nm-danger) 50%, transparent);
  background: color-mix(in srgb, var(--nm-danger) 10%, transparent); color: var(--nm-text); white-space: pre-wrap; font-size: 12px;
}
.sd-foot { display: flex; align-items: center; gap: 8px; }
.sd-foot .el-button { margin: 0; }
</style>
