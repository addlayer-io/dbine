<script setup lang="ts">
import { computed } from 'vue';
import CodeDiff from './CodeDiff.vue';
import { lineDiff } from '../composables/lineDiff';

// An older text of the tab (a version, a commit, a run) against the text
// in its editor now, side by side (left: then, right: now), with
// "Restaurar esta versión" (docs/history.md).

const props = defineProps<{
  title: string;
  /** The left side's heading (what the older text is). */
  beforeLabel: string;
  /** The older text; `null` with `note` when it can't be shown. */
  before: string | null;
  after: string;
  note?: string | null;
  canRestore: boolean;
  restoring?: boolean;
}>();
const emit = defineEmits<{ close: []; restore: [] }>();

const strip = (s: string) => s.replace(/\r\n/g, '\n').replace(/\n$/, '');
const lines = computed(() => (props.before === null ? [] : lineDiff(strip(props.before), strip(props.after), { exact: true })));
const same = computed(() => props.before !== null && strip(props.before) === strip(props.after));
const stats = computed(() => ({
  added: lines.value.filter((l) => l.kind === 'right' || l.kind === 'changed').length,
  removed: lines.value.filter((l) => l.kind === 'left' || l.kind === 'changed').length,
}));
</script>

<template>
  <el-dialog :model-value="true" :title="title" width="min(1100px, 92vw)" top="6vh" append-to-body class="tld" @close="emit('close')">
    <div class="tld-body">
      <div v-if="before !== null && !same" class="tld-heads">
        <div>{{ beforeLabel }} <span class="tld-stats"><span class="add">+{{ stats.added }}</span> <span class="del">−{{ stats.removed }}</span></span></div>
        <div>{{ $t('history:tl.current') }}</div>
      </div>
      <div v-if="note" class="tld-empty">{{ note }}</div>
      <div v-else-if="same" class="tld-empty">{{ $t('history:tl.same') }}</div>
      <CodeDiff v-else :lines="lines" git />
    </div>
    <template #footer>
      <el-button @click="emit('close')">{{ $t('common:close') }}</el-button>
      <el-button v-if="canRestore" type="primary" :disabled="before === null || same" :loading="restoring" @click="emit('restore')">
        <el-icon><ei-refresh-left /></el-icon>&nbsp;{{ $t('history:tl.restore') }}
      </el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.tld-body { display: flex; flex-direction: column; height: 68vh; min-height: 200px; border: 1px solid var(--nm-border-soft); border-radius: 4px; overflow: hidden; }
.tld-heads {
  display: grid; grid-template-columns: 1fr 1fr; flex-shrink: 0; font-size: 11px; text-transform: uppercase; letter-spacing: 0.04em;
  color: var(--nm-text-dim); border-bottom: 1px solid var(--nm-border-soft);
}
.tld-heads > div { padding: 4px 12px 4px 52px; }
.tld-stats { font-family: var(--nm-mono); text-transform: none; margin-left: 6px; }
.tld-stats .add { color: #73c991; }
.tld-stats .del { color: var(--nm-danger); }
.tld-empty { padding: 24px; color: var(--nm-text-dim); }
</style>
