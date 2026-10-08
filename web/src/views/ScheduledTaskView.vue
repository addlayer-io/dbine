<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { newStep, newTask, scheduledApi, type ScheduledTask, type StepKind, type TaskRun, type WriteScope } from '../api/scheduled';
import { tb } from '../i18n/backend';
import { confirmNative } from '../native';
import { useScheduledStore } from '../stores/scheduled';
import { useTabsStore, type ScheduledTaskTab } from '../stores/tabs';
import ScheduledStepEditor from '../components/ScheduledStepEditor.vue';

// A scheduled task (docs/tareas-programadas.md): when it runs, its steps,
// when to notify, and its runs. The OS scheduler runs it with DBine closed.

const props = defineProps<{ tab: ScheduledTaskTab }>();
const { t } = useTranslation();
const store = useScheduledStore();
const tabs = useTabsStore();

const KINDS: StepKind[] = ['run_script', 'export', 'compare_schemas', 'backup'];
const DAYS = [1, 2, 3, 4, 5, 6, 7];

const task = ref<ScheduledTask>(newTask());
const saved = ref('');
const saving = ref(false);
const runs = ref<TaskRun[]>([]);
const openRun = ref<string | null>(null);
const approval = ref<{ scope: WriteScope[]; resolve: (ok: boolean) => void } | null>(null);
const item = computed(() => store.byId(props.tab.taskId));
const dirty = computed(() => JSON.stringify(task.value) !== saved.value);

function load() {
  const it = props.tab.taskId ? store.byId(props.tab.taskId) : undefined;
  if (props.tab.taskId && !it) return;
  const base = it
    ? { id: it.id, name: it.name, enabled: it.enabled, schedule: it.schedule, steps: it.steps, notify: it.notify, approved_writes: it.approved_writes, created_at: it.created_at, updated_at: it.updated_at }
    : newTask();
  task.value = JSON.parse(JSON.stringify(base));
  saved.value = JSON.stringify(task.value);
}

async function loadRuns() {
  if (!props.tab.taskId) return;
  try { runs.value = await scheduledApi.runs(props.tab.taskId, 50); } catch { /* the list shows what it has */ }
}

let unlisten: UnlistenFn | undefined;
onMounted(async () => {
  if (!store.loaded) await store.load();
  load();
  loadRuns();
  unlisten = await listen<{ kind: string; id: string | null }>('state-changed', (e) => {
    if (e.payload.kind === 'task_run' && e.payload.id === props.tab.taskId) loadRuns();
  });
});
onUnmounted(() => unlisten?.());
// Loaded after the tab opened (a restored tab): fill it once.
watch(() => store.loaded && item.value, (v, before) => { if (v && !before && !dirty.value) load(); });

const scheduleType = computed({
  get: () => task.value.schedule.type,
  set: (type) => {
    const time = 'time' in task.value.schedule ? task.value.schedule.time : '08:00';
    task.value.schedule = type === 'daily' ? { type, time }
      : type === 'weekly' ? { type, days: [1, 2, 3, 4, 5], time }
        : type === 'monthly' ? { type, day: 1, time }
          : { type: 'interval', minutes: 60 };
  },
});

function addStep(kind: StepKind) {
  task.value.steps.push(newStep(kind));
}
function moveStep(i: number, delta: number) {
  const s = task.value.steps;
  const [x] = s.splice(i, 1);
  s.splice(i + delta, 0, x);
}

/** The steps that write, shown for an explicit approval. */
function askApproval(scope: WriteScope[]): Promise<boolean> {
  return new Promise((resolve) => { approval.value = { scope, resolve }; });
}
function answer(ok: boolean) {
  approval.value?.resolve(ok);
  approval.value = null;
}

async function save() {
  if (saving.value) return;
  saving.value = true;
  try {
    // Steps that write and aren't what was approved: ask.
    const check = await scheduledApi.check(task.value);
    let approve = false;
    if (check.needs_approval) {
      approve = await askApproval(check.scope);
      if (!approve) return;
    }
    const r = await scheduledApi.save(task.value, approve);
    store.upsert(r.item);
    if (props.tab.taskId !== r.item.id) props.tab.taskId = r.item.id;
    load();
    loadRuns();
    if (r.schedule_error) ElMessage.warning({ message: t('scheduled:notScheduled', { error: tb(r.schedule_error) }), duration: 8000 });
    else ElMessage.success(t('scheduled:saved'));
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    saving.value = false;
  }
}

async function runNow() {
  if (!props.tab.taskId) return;
  try {
    await scheduledApi.runNow(props.tab.taskId);
    ElMessage.success(t('scheduled:started', { name: task.value.name }));
    setTimeout(loadRuns, 500);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

async function remove() {
  if (!props.tab.taskId) { tabs.close(props.tab.id); return; }
  if (!(await confirmNative(t('scheduled:deleteConfirm', { name: task.value.name }), { title: t('scheduled:delete'), okLabel: t('scheduled:delete'), kind: 'warning' }))) return;
  try {
    await scheduledApi.remove(props.tab.taskId);
    store.drop(props.tab.taskId);
    tabs.close(props.tab.id);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

const VARS = ['{task}', '{date}', '{time}', '{datetime}', '{year}', '{month}', '{day}', '{steps.1.file}'];
const stepLabel = (id: string) => {
  const i = task.value.steps.findIndex((s) => s.id === id);
  const s = task.value.steps[i];
  return s ? `${i + 1}. ${s.name || t(`scheduled:kinds.${s.kind}`)}` : id;
};
</script>

<template>
  <div class="st">
    <header class="st-head">
      <el-input v-model="task.name" class="st-name" :placeholder="$t('scheduled:namePlaceholder')" />
      <el-switch v-model="task.enabled" :active-text="$t('scheduled:enabled')" />
      <span class="st-sp" />
      <el-tag v-if="item?.needs_approval" type="warning" size="small">{{ $t('scheduled:needsApproval') }}</el-tag>
      <el-tag v-else-if="item && item.enabled && !item.registered" type="warning" size="small">{{ $t('scheduled:notRegistered') }}</el-tag>
      <el-button size="small" :disabled="!tab.taskId || dirty" :title="dirty ? $t('scheduled:saveFirst') : ''" @click="runNow">{{ $t('scheduled:runNow') }}</el-button>
      <el-button size="small" @click="remove">{{ $t('scheduled:delete') }}</el-button>
      <el-button size="small" type="primary" :loading="saving" :disabled="!dirty && !!tab.taskId" @click="save">{{ $t('scheduled:save') }}</el-button>
    </header>

    <div class="st-body">
      <section class="st-sec">
        <h3 class="nm-section-title">{{ $t('scheduled:when.title') }}</h3>
        <div class="st-when">
          <el-select v-model="scheduleType" size="small" class="st-type">
            <el-option v-for="k in ['daily', 'weekly', 'monthly', 'interval']" :key="k" :value="k" :label="$t(`scheduled:when.types.${k}`)" />
          </el-select>
          <template v-if="task.schedule.type === 'weekly'">
            <el-checkbox-group v-model="task.schedule.days" size="small">
              <el-checkbox-button v-for="d in DAYS" :key="d" :value="d">{{ $t(`scheduled:days.${d}`) }}</el-checkbox-button>
            </el-checkbox-group>
          </template>
          <template v-if="task.schedule.type === 'monthly'">
            <span>{{ $t('scheduled:when.onDay') }}</span>
            <el-input-number v-model="task.schedule.day" :min="1" :max="28" size="small" controls-position="right" class="st-num" />
          </template>
          <template v-if="task.schedule.type !== 'interval'">
            <span>{{ $t('scheduled:when.at') }}</span>
            <el-time-select v-model="task.schedule.time" start="00:00" end="23:45" step="00:15" size="small" class="st-time" :clearable="false" />
          </template>
          <template v-else>
            <span>{{ $t('scheduled:when.every') }}</span>
            <el-input-number v-model="task.schedule.minutes" :min="5" :step="5" size="small" controls-position="right" class="st-num" />
            <span>{{ $t('scheduled:when.minutes') }}</span>
          </template>
        </div>
        <p class="st-hint">{{ $t('scheduled:when.hint') }}<template v-if="item?.next_run"> {{ $t('scheduled:nextRunAt', { at: item.next_run }) }}</template></p>
      </section>

      <section class="st-sec">
        <h3 class="nm-section-title">{{ $t('scheduled:notify.title') }}</h3>
        <el-radio-group v-model="task.notify" size="small">
          <el-radio value="failure">{{ $t('scheduled:notify.failure') }}</el-radio>
          <el-radio value="always">{{ $t('scheduled:notify.always') }}</el-radio>
          <el-radio value="never">{{ $t('scheduled:notify.never') }}</el-radio>
        </el-radio-group>
      </section>

      <section class="st-sec">
        <h3 class="nm-section-title">{{ $t('scheduled:steps') }}</h3>
        <ScheduledStepEditor
          v-for="(s, i) in task.steps"
          :key="s.id || i"
          :step="s"
          :index="i"
          :count="task.steps.length"
          @remove="task.steps.splice(i, 1)"
          @move="(d) => moveStep(i, d)"
        />
        <el-dropdown trigger="click" @command="addStep">
          <el-button size="small"><el-icon><ei-plus /></el-icon>&nbsp;{{ $t('scheduled:addStep') }}</el-button>
          <template #dropdown>
            <el-dropdown-menu>
              <el-dropdown-item v-for="k in KINDS" :key="k" :command="k">{{ $t(`scheduled:kinds.${k}`) }}</el-dropdown-item>
            </el-dropdown-menu>
          </template>
        </el-dropdown>
        <p class="st-hint">{{ $t('scheduled:varsHint') }} <code v-for="v in VARS" :key="v" class="st-var">{{ v }}</code></p>
      </section>

      <section v-if="tab.taskId" class="st-sec">
        <h3 class="nm-section-title">{{ $t('scheduled:history') }}</h3>
        <p v-if="!runs.length" class="st-hint">{{ $t('scheduled:noRuns') }}</p>
        <div v-for="r in runs" :key="r.id" class="st-run" :class="r.status">
          <button class="st-run-row" @click="openRun = openRun === r.id ? null : r.id">
            <span class="st-dot" />
            <span class="st-run-when">{{ r.started_at }}</span>
            <span class="st-run-status">{{ $t(`scheduled:status.${r.status}`) }}</span>
            <span class="st-run-trigger">{{ $t(`scheduled:trigger.${r.trigger}`, { defaultValue: r.trigger }) }}</span>
            <span v-if="r.steps.some((s) => s.alert)" class="st-alert"><el-icon><ei-bell-filled /></el-icon></span>
            <span class="st-sp" />
            <el-icon class="st-chev" :class="{ open: openRun === r.id }"><ei-arrow-right /></el-icon>
          </button>
          <div v-if="openRun === r.id" class="st-run-detail">
            <p v-if="r.error" class="st-err">{{ tb(r.error) }}</p>
            <div v-for="s in r.steps" :key="s.step_id" class="st-step" :class="s.status">
              <div><strong>{{ stepLabel(s.step_id) }}</strong> · {{ tb(s.summary) }}</div>
              <ul v-if="s.messages.length" class="st-msgs"><li v-for="(m, i) in s.messages" :key="i">{{ m }}</li></ul>
            </div>
          </div>
        </div>
      </section>
    </div>

    <el-dialog :model-value="!!approval" :title="$t('scheduled:approve.title')" width="560px" @close="answer(false)">
      <p>{{ $t('scheduled:approve.text') }}</p>
      <ul class="st-scope">
        <li v-for="w in approval?.scope ?? []" :key="w.step_id">
          <span v-if="w.production" class="st-prod">PROD</span>
          <strong>{{ stepLabel(w.step_id) }}</strong>: {{ w.what }} · {{ w.connection }}<template v-if="w.database"> › {{ w.database }}</template>
        </li>
      </ul>
      <el-alert v-if="approval?.scope.some((w) => w.production)" type="error" :closable="false" show-icon :title="$t('scheduled:approve.production')" />
      <template #footer>
        <el-button @click="answer(false)">{{ $t('common:cancel') }}</el-button>
        <el-button type="danger" @click="answer(true)">{{ $t('scheduled:approve.ok') }}</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.st { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--nm-bg); }
.st-head { display: flex; align-items: center; gap: 10px; padding: 10px 16px; border-bottom: 1px solid var(--nm-border-soft); }
.st-head .el-button { margin: 0; }
.st-name { max-width: 360px; }
.st-sp { flex: 1; }
.st-body { flex: 1; overflow: auto; padding: 8px 16px 32px; }
.st-sec { margin: 10px 0 18px; max-width: 980px; }
.st-sec h3 { margin: 6px 0 10px; }
.st-when { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; font-size: 13px; }
.st-type { width: 170px; }
.st-time { width: 110px; }
.st-num { width: 110px; }
.st-hint { margin: 8px 0 0; font-size: 11.5px; color: var(--nm-text-dim); line-height: 1.5; }
.st-var { margin: 0 3px; padding: 0 4px; border-radius: 3px; background: var(--ide-hover); font-family: var(--nm-mono); font-size: 11px; }
.st-run { border: 1px solid var(--nm-border-soft); border-left-width: 3px; border-radius: 4px; margin-bottom: 6px; }
.st-run.ok { border-left-color: var(--nm-success); }
.st-run.partial { border-left-color: var(--nm-warning); }
.st-run.failed { border-left-color: var(--nm-danger); }
.st-run.running { border-left-color: var(--nm-info); }
.st-run-row { display: flex; align-items: center; gap: 10px; width: 100%; padding: 6px 10px; border: 0; background: none; cursor: pointer; font: inherit; font-size: 12.5px; color: var(--nm-text); text-align: left; }
.st-run-row:hover { background: var(--ide-hover); }
.st-run-when { font-variant-numeric: tabular-nums; }
.st-run-trigger { color: var(--nm-text-dim); font-size: 11.5px; }
.st-alert { color: var(--nm-warning); display: inline-flex; }
.st-chev { transition: transform .15s; color: var(--nm-text-dim); }
.st-chev.open { transform: rotate(90deg); }
.st-run-detail { padding: 0 12px 10px; font-size: 12.5px; }
.st-err { color: var(--nm-danger); margin: 0 0 6px; }
.st-step { padding: 4px 0; }
.st-step.failed strong { color: var(--nm-danger); }
.st-msgs { margin: 4px 0 0; padding-left: 18px; max-height: 160px; overflow: auto; font-family: var(--nm-mono); font-size: 11.5px; color: var(--nm-text-dim); }
.st-scope { padding-left: 18px; line-height: 1.7; }
.st-prod { font-size: 10px; font-weight: 700; padding: 0 4px; margin-right: 6px; border-radius: 3px; color: #fff; background: var(--nm-danger); }
</style>
