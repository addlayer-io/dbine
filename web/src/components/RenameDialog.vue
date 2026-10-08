<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import type { SyncScript } from '../api/compare';
import type { RenameImpact, RenameImpactItem } from '../api/types';
import { tb } from '../i18n/backend';
import { newQuery } from '../composables/actions';
import { oldName, renameImpact, renameScript, renameSpec, runRename, targetLabel, type RenameDialogTarget } from '../composables/rename';
import { isProdConnection } from '../composables/tags';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore } from '../stores/tabs';
import { startTask, useTasksStore, type TaskHandle } from '../stores/tasks';

// "Renombrar…": the new name, what depends on the target (rewritten by DBine,
// updated by the engine, or left to the user), and the script that does it
// all (read-only, copyable, or opened as a query). It runs only on
// "Renombrar", as a task that can go on in the background and be cancelled
// (`sync:<runId>`), like "Eliminar índice…". On a production connection the
// new name has to be typed again first.

const props = defineProps<{ target: RenameDialogTarget }>();
const emit = defineEmits<{ close: [] }>();
const { t } = useTranslation();
const conns = useConnectionsStore();
const tabs = useTabsStore();

const old = computed(() => oldName(props.target.target));
const label = computed(() => targetLabel(props.target.target));
const spec = computed(() => renameSpec(props.target.connectionId));
const conn = computed(() => conns.byId(props.target.connectionId));
const where = computed(() => `${conn.value?.name ?? ''}${props.target.database ? ` › ${props.target.database}` : ''}`);
const what = computed(() => {
  const tg = props.target.target;
  if (tg.what === 'object') return t('rename:what.object', { kind: t(`dependencies:kind.${tg.object.kind}`, { defaultValue: tg.object.kind }), name: label.value });
  return t(`rename:what.${tg.what}`, { name: label.value });
});
/** Only a column rename touches the views' output columns. */
const isColumn = computed(() => props.target.target.what === 'column');

const name = ref(old.value);
const trimmed = computed(() => name.value.trim());
const nameProblem = computed(() => (!trimmed.value ? t('rename:empty') : trimmed.value === old.value ? t('rename:same') : null));
const keepViewColumns = ref(true);

// -- impact (debounced on the name) ---------------------------------------
const impact = ref<RenameImpact | null>(null);
const impactFor = ref('');
const loading = ref(false);
const impactError = ref<string | null>(null);
const selected = ref<Set<number>>(new Set());
let seq = 0;
let timer: ReturnType<typeof setTimeout> | null = null;
watch([trimmed, keepViewColumns], () => {
  if (timer) clearTimeout(timer);
  if (nameProblem.value) { impact.value = null; script.value = null; return; }
  timer = setTimeout(loadImpact, 400);
}, { immediate: false });

async function loadImpact() {
  const n = trimmed.value;
  const mine = ++seq;
  loading.value = true;
  impactError.value = null;
  try {
    const r = await renameImpact(props.target, n, keepViewColumns.value);
    if (mine !== seq) return;
    impact.value = r;
    impactFor.value = n;
    selected.value = new Set(r.items.flatMap((it, i) => (it.action.kind === 'rewrite' && it.action.default_selected ? [i] : [])));
  } catch (e) {
    if (mine === seq) { impact.value = null; impactError.value = errorMessage(e); }
  } finally {
    if (mine === seq) loading.value = false;
  }
}

const indexed = computed(() => (impact.value?.items ?? []).map((item, i) => ({ item, i })));
const rewrites = computed(() => indexed.value.filter(({ item }) => item.action.kind === 'rewrite'));
const engine = computed(() => indexed.value.filter(({ item }) => item.action.kind === 'engine' || item.action.kind === 'tracked'));
const manual = computed(() => indexed.value.filter(({ item }) => item.action.kind === 'manual'));

function toggle(i: number, on: boolean) {
  const s = new Set(selected.value);
  if (on) s.add(i); else s.delete(i);
  selected.value = s;
}

// -- script (refreshed on each checkbox) ---------------------------------
const script = ref<SyncScript | null>(null);
const scriptError = ref<string | null>(null);
const text = computed(() => script.value?.statements.join('\n') ?? '');
watch([impact, selected], async () => {
  const im = impact.value;
  if (!im) { script.value = null; return; }
  const mine = seq;
  const chosen = [...selected.value].sort((a, b) => a - b).map((i) => im.items[i].action).flatMap((a) => (a.kind === 'rewrite' ? [{ object: a.object, schemabound: a.schemabound }] : []));
  try {
    const s = await renameScript(props.target, impactFor.value, im, chosen);
    if (mine === seq) { script.value = s; scriptError.value = null; }
  } catch (e) {
    if (mine === seq) { script.value = null; scriptError.value = errorMessage(e); }
  }
});

// -- production ------------------------------------------------------------
const prod = computed(() => isProdConnection(conn.value, props.target.database));
const confirmText = ref('');
const confirmed = computed(() => !prod.value || confirmText.value.trim() === impactFor.value);

function dependentName(item: RenameImpactItem) {
  const d = item.dependent;
  return d.schema ? `${d.schema}.${d.name}` : d.name;
}
function kindLabel(item: RenameImpactItem) {
  return t(`dependencies:kind.${item.dependent.kind}`, { defaultValue: item.dependent.kind });
}
function openDefinition(item: RenameImpactItem) {
  const d = item.dependent;
  tabs.openObject(props.target.connectionId, props.target.database, { kind: d.kind, schema: d.schema, name: d.name }, 'definition', false);
}

async function copyScript() {
  try {
    await navigator.clipboard.writeText(text.value);
    ElMessage.success({ message: t('common:copied'), duration: 1200 });
  } catch { /* no clipboard */ }
}
function openInQuery() {
  newQuery(props.target.connectionId, props.target.database, text.value, t('rename:scriptName', { name: old.value }));
  emit('close');
}

// -- run ---------------------------------------------------------------------
const running = ref(false);
const runError = ref<string | null>(null);
let task: TaskHandle | null = null;
const taskId = ref<string | null>(null);
const tasks = useTasksStore();
const cancelling = computed(() => !!(taskId.value && tasks.byId(taskId.value)?.cancelling));
function cancel() {
  if (running.value && taskId.value) tasks.cancel(taskId.value);
  else emit('close');
}
let alive = true;
onBeforeUnmount(() => {
  alive = false;
  if (timer) clearTimeout(timer);
  if (running.value) task?.background();
});

const canRun = computed(() => !!text.value && !scriptError.value && !loading.value && !nameProblem.value && impactFor.value === trimmed.value && confirmed.value);

async function run() {
  if (!canRun.value || !script.value || !impact.value) return;
  running.value = true;
  runError.value = null;
  const target = props.target;
  const to = impactFor.value;
  const statements = script.value.statements;
  const atomic = impact.value.atomic;
  const runId = crypto.randomUUID();
  let ready = false;
  let settled = false;
  const current = task = startTask({
    kind: 'rename',
    title: t('rename:task', { name: label.value, to }),
    connectionId: target.connectionId, database: target.database,
    cancel: () => {
      const send = () => api.cancelQuery(`sync:${runId}`).catch(() => {});
      if (!ready) {
        const tm = setInterval(() => {
          if (settled) clearInterval(tm);
          else { if (ready) clearInterval(tm); send(); }
        }, 500);
      }
      return send();
    },
  });
  taskId.value = current.id;
  current.log(statements.join('\n'));
  current.progress({ done: 0, total: statements.length, unit: 'statements' });
  try {
    await current.listen<{ run_id: string; done: number }>('schema-sync-progress', ({ payload }) => {
      if (payload.run_id !== runId || settled) return;
      ready = true;
      current.progress({ done: payload.done });
    });
  } catch { /* no live progress: the run still goes */ }
  const r = await runRename(target, to, statements, atomic, runId);
  settled = true;
  running.value = false;
  if (r.error) {
    runError.value = r.rolledBack ? `${r.error}\n\n${t('rename:rolledBack')}` : r.error;
    if (current.isCancelling) current.cancelled(); else current.fail(r.error);
    return;
  }
  const done = t('rename:done', { name: label.value, to });
  current.finish(undefined, done);
  if (!alive) return;
  ElMessage.success(done);
  emit('close');
}

defineExpose({ name, loadImpact });
</script>

<template>
  <el-dialog
    :model-value="true" width="760px" append-to-body :title="$t('rename:title', { name: old })"
    :close-on-click-modal="false" :close-on-press-escape="!running" :show-close="!running" @close="emit('close')"
  >
    <p class="rn-p"><b>{{ what }}</b> <span class="rn-dim">{{ $t('rename:where', { where }) }}</span></p>
    <el-alert v-if="spec?.note" type="info" :title="tb(spec.note)" :closable="false" show-icon class="rn-warn" />

    <div class="rn-name">
      <label class="rn-label" for="rn-new">{{ $t('rename:newName') }}</label>
      <el-input id="rn-new" v-model="name" :disabled="running" autofocus class="nm-selectable" />
    </div>
    <div v-if="nameProblem && name !== old" class="rn-hint">{{ nameProblem }}</div>
    <div v-else-if="impact && impact.quoted_name !== impactFor" class="rn-hint">{{ $t('rename:quotedHint', { quoted: impact.quoted_name }) }}</div>
    <el-checkbox v-if="isColumn" v-model="keepViewColumns" :disabled="running" class="rn-keep">{{ $t('rename:keepViewColumns') }}</el-checkbox>

    <div v-if="loading" class="rn-loading"><el-icon class="is-loading"><EiLoading /></el-icon> {{ $t('rename:reading', { name: label }) }}</div>
    <div v-if="impactError" class="rn-error">{{ impactError }}</div>

    <template v-if="impact && !loading">
      <el-alert v-if="impact.collides" type="error" :title="$t('rename:collides', { name: impactFor })" :closable="false" show-icon class="rn-warn" />
      <p class="rn-dim rn-small">
        {{ impact.items.length
          ? $t('rename:summary', { rewrite: rewrites.length, engine: engine.length, manual: manual.length + impact.unreadable.length, scanned: impact.scanned })
          : $t('rename:nothing') }}
      </p>
      <el-alert v-if="impact.note" type="warning" :title="tb(impact.note)" :closable="false" show-icon class="rn-warn" />
      <el-alert v-if="impact.unreadable.length" type="warning" :title="$t('rename:unreadable', { names: impact.unreadable.join(', ') })" :closable="false" show-icon class="rn-warn" />

      <div class="rn-groups">
        <section v-if="rewrites.length" class="rn-group">
          <h4>{{ $t('rename:group.rewrite') }} <span class="rn-count">{{ rewrites.length }}</span></h4>
          <p class="rn-dim rn-small">{{ $t('rename:groupHint.rewrite') }}</p>
          <div v-for="{ item, i } in rewrites" :key="i" class="rn-item">
            <template v-if="item.action.kind === 'rewrite'">
              <div class="rn-head">
                <el-checkbox :model-value="selected.has(i)" :disabled="running" @update:model-value="(v: unknown) => toggle(i, !!v)" />
                <span class="rn-kind">{{ kindLabel(item) }}</span>
                <span class="rn-obj">{{ dependentName(item) }}</span>
                <el-tag v-if="item.action.unresolved.length" type="warning" size="small" :title="$t('rename:reviewHint')">{{ $t('rename:review') }}</el-tag>
                <el-tag v-if="item.action.schemabound" type="info" size="small">{{ $t('rename:schemabound') }}</el-tag>
              </div>
              <div v-for="e in item.action.edits" :key="`e${e.line}`" class="rn-edit">
                <span class="rn-line">{{ $t('rename:line', { line: e.line }) }}</span>
                <code class="rn-before">{{ e.before }}</code>
                <code class="rn-after">{{ e.after }}</code>
              </div>
              <div v-for="u in item.action.unresolved" :key="`u${u.line}${u.reason}`" class="rn-unres">
                <span class="rn-line">{{ $t('rename:line', { line: u.line }) }}</span>
                <code>{{ u.text }}</code>
                <span class="rn-why">{{ $t(`rename:reason.${u.reason}`) }}</span>
              </div>
            </template>
          </div>
        </section>

        <section v-if="engine.length" class="rn-group">
          <h4>{{ $t('rename:group.engine') }} <span class="rn-count">{{ engine.length }}</span></h4>
          <p class="rn-dim rn-small">{{ $t('rename:groupHint.engine') }}</p>
          <div v-for="{ item, i } in engine" :key="i" class="rn-row">
            <span class="rn-kind">{{ kindLabel(item) }}</span>
            <span class="rn-obj">{{ dependentName(item) }}</span>
            <span class="rn-dim">{{ item.dependent.detail ?? $t(item.action.kind === 'tracked' ? 'rename:tracked' : 'rename:engine') }}</span>
          </div>
        </section>

        <section v-if="manual.length" class="rn-group">
          <h4>{{ $t('rename:group.manual') }} <span class="rn-count">{{ manual.length }}</span></h4>
          <p class="rn-dim rn-small">{{ $t('rename:groupHint.manual') }}</p>
          <div v-for="{ item, i } in manual" :key="i" class="rn-item">
            <template v-if="item.action.kind === 'manual'">
              <div class="rn-head">
                <span class="rn-kind">{{ kindLabel(item) }}</span>
                <span class="rn-obj">{{ dependentName(item) }}</span>
                <span class="rn-why">{{ $t(`rename:manualReason.${item.action.reason}`) }}</span>
                <span style="flex: 1" />
                <el-button link size="small" @click="openDefinition(item)">{{ $t('rename:openDefinition') }}</el-button>
              </div>
              <div v-for="m in item.dependent.mentions.slice(0, 3)" :key="`m${m.line}`" class="rn-unres">
                <span class="rn-line">{{ $t('rename:line', { line: m.line }) }}</span>
                <code>{{ m.text }}</code>
              </div>
            </template>
          </div>
        </section>
      </div>

      <p class="rn-dim rn-small">{{ $t('rename:outside') }}</p>
      <p class="rn-dim rn-small">{{ $t(impact.atomic ? 'rename:atomic' : 'rename:notAtomic') }}</p>

      <el-alert v-for="w in script?.warnings ?? []" :key="w" type="warning" :title="tb(w)" :closable="false" show-icon class="rn-warn" />
      <div class="rn-script-head">{{ $t('rename:script') }}</div>
      <div v-if="scriptError" class="rn-error">{{ scriptError }}</div>
      <pre v-else v-loading="!script" class="rn-script nm-selectable">{{ text }}</pre>

      <div v-if="prod" class="rn-prod">
        <label for="rn-confirm">{{ $t('rename:prodConfirm', { name: impactFor }) }}</label>
        <el-input id="rn-confirm" v-model="confirmText" :disabled="running" :placeholder="impactFor" />
      </div>
    </template>
    <div v-if="runError" class="rn-error">{{ runError }}</div>

    <template #footer>
      <div class="rn-foot">
        <el-button :disabled="!text" @click="copyScript">{{ $t('common:copy') }}</el-button>
        <el-button :disabled="!text || running" @click="openInQuery">{{ $t('rename:openInQuery') }}</el-button>
        <span style="flex: 1" />
        <el-button v-if="running" @click="task?.background(); emit('close')">{{ $t('tasks:panel.background') }}</el-button>
        <el-button :disabled="cancelling" @click="cancel">{{ $t('common:cancel') }}</el-button>
        <el-button type="danger" :loading="running" :disabled="!canRun" @click="run">{{ $t('rename:run') }}</el-button>
      </div>
    </template>
  </el-dialog>
</template>

<style scoped lang="scss">
.rn-p { margin: 0 0 8px; }
.rn-dim { color: var(--nm-text-dim); }
.rn-small { font-size: 12px; margin: 6px 0; }
.rn-warn { margin: 6px 0; }
.rn-name { display: flex; align-items: center; gap: 10px; margin-top: 10px; }
.rn-label { flex: none; font-weight: 600; font-size: 13px; color: var(--nm-text-strong); }
.rn-hint { margin: 4px 0 0; font-size: 12px; color: var(--nm-text-dim); }
.rn-keep { margin-top: 6px; }
.rn-loading { margin: 12px 0; display: flex; align-items: center; gap: 6px; color: var(--nm-text-dim); font-size: 12.5px; }
.rn-groups { max-height: 34vh; overflow: auto; margin: 4px 0 8px; border: 1px solid var(--nm-border); border-radius: 4px; padding: 4px 10px; }
.rn-group h4 { margin: 8px 0 0; font-size: 13px; color: var(--nm-text-strong); }
.rn-count { font-weight: 400; color: var(--nm-text-dim); margin-left: 4px; }
.rn-item { padding: 4px 0; border-bottom: 1px solid color-mix(in srgb, var(--nm-border) 60%, transparent); }
.rn-item:last-child { border-bottom: none; }
.rn-head, .rn-row { display: flex; align-items: center; gap: 8px; font-size: 12.5px; padding: 2px 0; }
.rn-kind { color: var(--nm-text-dim); min-width: 92px; }
.rn-obj { font-family: var(--nm-mono); color: var(--nm-text-strong); }
.rn-edit, .rn-unres { display: grid; grid-template-columns: 64px 1fr; gap: 2px 8px; font-size: 12px; margin: 2px 0 2px 26px; }
.rn-edit code, .rn-unres code { font-family: var(--nm-mono); white-space: pre-wrap; word-break: break-word; }
.rn-before { grid-column: 2; color: var(--nm-danger); text-decoration: line-through; opacity: .85; }
.rn-after { grid-column: 2; color: var(--nm-success, #4ec9b0); }
.rn-line { color: var(--nm-text-dim); grid-row: span 2; }
.rn-unres code { color: var(--nm-warning, #d7ba7d); }
.rn-why { color: var(--nm-warning, #d7ba7d); font-size: 12px; grid-column: 2; }
.rn-head .rn-why { grid-column: auto; }
.rn-script-head { margin-top: 10px; font-weight: 600; font-size: 13px; color: var(--nm-text-strong); }
.rn-script {
  margin: 8px 0 0; padding: 10px 12px; min-height: 48px; max-height: 24vh; overflow: auto; border: 1px solid var(--nm-border); border-radius: 4px;
  background: var(--ide-editor, var(--nm-bg-elev)); font-family: var(--nm-mono); font-size: 12.5px; color: var(--nm-text-strong); white-space: pre-wrap;
}
.rn-prod { margin-top: 10px; display: flex; flex-direction: column; gap: 6px; font-size: 12.5px; color: var(--nm-danger); }
.rn-error {
  margin-top: 8px; padding: 8px 10px; border-radius: 3px; border: 1px solid color-mix(in srgb, var(--nm-danger) 50%, transparent);
  background: color-mix(in srgb, var(--nm-danger) 10%, transparent); color: var(--nm-text); white-space: pre-wrap; font-size: 12px;
}
.rn-foot { display: flex; align-items: center; gap: 8px; }
.rn-foot .el-button { margin: 0; }
</style>
