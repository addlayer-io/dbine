<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import type { EnterpriseDuration, RunningStatement } from '../api/enterprise';
import { useEnterpriseStore } from '../stores/enterprise';

// Asking DBine Enterprise for access to a profile, or for more time on
// access that is about to end (with a justification and what is running).

const ent = useEnterpriseStore();
const { t } = useTranslation();
const target = computed(() => ent.access);
const kind = ref<'hours' | 'until' | 'forever'>('hours');
const hours = ref(8);
const until = ref<Date>(new Date(Date.now() + 7 * 86400_000));
const databases = ref('');
const reason = ref('');
const sending = ref(false);
const running = ref<RunningStatement[]>([]);
const outsideHours = ref(false);
const limited = computed(() => !!target.value?.connection.schedule);
const needsDuration = computed(() => outsideHours.value && kind.value === 'forever');

watch(target, (x) => {
  if (!x) return;
  kind.value = 'hours';
  hours.value = x.mode === 'extend' ? 2 : 8;
  databases.value = '';
  reason.value = '';
  running.value = x.mode === 'extend' ? ent.running(x.profile.id) : [];
  outsideHours.value = !!x.outsideHours;
});

const until_ = computed(() => target.value?.profile.access?.until ?? null);
const past = (d: Date) => d.getTime() < Date.now() - 86400_000;
const minutes = (s: number) => Math.max(1, Math.round(s / 60));

function duration(): EnterpriseDuration {
  if (kind.value === 'hours') return { hours: hours.value, until: null };
  if (kind.value === 'until') return { hours: null, until: until.value.toISOString() };
  return { hours: null, until: null };
}

async function send() {
  const x = target.value;
  if (!x || !reason.value.trim()) return;
  sending.value = true;
  try {
    if (x.mode === 'extend') {
      await ent.extend(x.profile.id, duration(), reason.value.trim());
      ElMessage.success(t('enterprise:access.extensionSent'));
    } else {
      const dbs = databases.value.split(/[,\n]/).map((s) => s.trim()).filter(Boolean);
      await ent.request(x.profile.id, dbs.length ? dbs : null, duration(), reason.value.trim(), limited.value && outsideHours.value);
      ElMessage.success(t('enterprise:access.requestSent'));
    }
    ent.access = null;
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    sending.value = false;
  }
}
</script>

<template>
  <el-dialog
    :model-value="!!target"
    :title="target?.mode === 'extend' ? $t('enterprise:access.extendTitle') : $t('enterprise:access.requestTitle')"
    width="520px"
    append-to-body
    @close="ent.access = null"
  >
    <el-form v-if="target" label-position="top" @submit.prevent="send">
      <p class="ea-what">
        <strong>{{ target.connection.name }}</strong> · {{ target.profile.name }}
        <span v-if="target.profile.description" class="nm-muted"><br>{{ target.profile.description }}</span>
        <span v-if="target.mode === 'extend' && until_" class="nm-muted"><br>{{ $t('enterprise:access.endsAt', { when: new Date(until_).toLocaleString() }) }}</span>
      </p>
      <el-form-item v-if="target.mode === 'request'" :label="$t('enterprise:access.databases')">
        <el-input v-model="databases" :placeholder="$t('enterprise:access.databasesPlaceholder')" />
      </el-form-item>
      <el-form-item :label="target.mode === 'extend' ? $t('enterprise:access.howMuchMore') : $t('enterprise:access.howLong')">
        <div class="ea-duration">
          <el-radio-group v-model="kind">
            <el-radio value="hours">{{ $t('enterprise:access.forHours') }}</el-radio>
            <el-radio value="until">{{ $t('enterprise:access.untilDate') }}</el-radio>
            <el-radio value="forever">{{ $t('enterprise:noExpiry') }}</el-radio>
          </el-radio-group>
          <div v-if="kind === 'hours'" class="ea-line">
            <el-input-number v-model="hours" :min="1" :max="720" size="small" /> <span class="nm-muted">{{ $t('enterprise:access.hoursUnit') }}</span>
          </div>
          <el-date-picker v-else-if="kind === 'until'" v-model="until" type="datetime" size="small" :disabled-date="past" :clearable="false" />
        </div>
      </el-form-item>
      <el-form-item v-if="target.mode === 'request' && limited">
        <el-checkbox v-model="outsideHours">{{ $t('enterprise:access.outsideHours') }}</el-checkbox>
        <p class="ea-hint" :class="{ 'ea-bad': needsDuration }">{{ needsDuration ? $t('enterprise:access.outsideHoursNeedsDuration') : $t('enterprise:access.outsideHoursHint') }}</p>
      </el-form-item>
      <el-form-item :label="target.mode === 'extend' ? $t('enterprise:access.justification') : $t('enterprise:access.reason')" required>
        <el-input
          v-model="reason"
          type="textarea"
          :rows="3"
          :placeholder="target.mode === 'extend' ? $t('enterprise:access.justificationPlaceholder') : $t('enterprise:access.reasonPlaceholder')"
        />
      </el-form-item>
      <div v-if="running.length" class="ea-running">
        <div class="nm-muted">{{ $t('enterprise:access.runningSent') }}</div>
        <div v-for="(r, i) in running" :key="i" class="ea-stmt">
          <span class="nm-muted">{{ $t('enterprise:access.runningFor', { count: minutes(r.seconds) }) }}</span>
          <code>{{ r.statement.length > 300 ? `${r.statement.slice(0, 300)}…` : r.statement }}</code>
        </div>
      </div>
    </el-form>
    <template #footer>
      <el-button @click="ent.access = null">{{ $t('common:cancel') }}</el-button>
      <el-button type="primary" :loading="sending" :disabled="!reason.trim() || needsDuration" @click="send">
        {{ target?.mode === 'extend' ? $t('enterprise:access.sendExtension') : $t('enterprise:access.sendRequest') }}
      </el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.ea-what {
  margin: 0 0 14px;
  line-height: 1.5;
}

.ea-hint {
  width: 100%;
  margin: 2px 0 0;
  font-size: 12px;
  line-height: 1.4;
  color: var(--nm-text-dim);
}

.ea-bad {
  color: var(--nm-danger);
}

.ea-duration {
  display: flex;
  flex-direction: column;
  gap: 8px;
  width: 100%;
}

.ea-line {
  display: flex;
  align-items: center;
  gap: 8px;
}

.ea-running {
  font-size: 12px;
}

.ea-stmt code {
  display: block;
  white-space: pre-wrap;
  word-break: break-word;
  margin: 2px 0 6px;
}
</style>
