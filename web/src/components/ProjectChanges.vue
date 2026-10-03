<script setup lang="ts">
import { computed, ref } from 'vue';
import { useTranslation } from 'i18next-vue';
import type { FileChange } from '../api/types';
import { isScriptFile } from '../composables/tabDocument';
import { useProjectsStore } from '../stores/projects';
import { baseName, useTabsStore } from '../stores/tabs';

// A project's "Cambios": what changed since the last commit, the message
// and Confirmar (stages everything), and Pull / Push / Sincronizar. While a
// merge or rebase is stopped on conflicts, those files come first with
// their own actions, plus Continuar / Abortar.

const props = defineProps<{ projectId: string }>();
const projects = useProjectsStore();
const tabs = useTabsStore();
const { t } = useTranslation();

const st = computed(() => projects.status[props.projectId]?.data ?? null);
const busy = computed(() => projects.busy[props.projectId] ?? null);
const conflicts = computed(() => st.value?.changes.filter((c) => c.mark === 'C') ?? []);
const changes = computed(() => st.value?.changes.filter((c) => c.mark !== 'C') ?? []);
const message = ref('');

const dirOf = (p: string) => (p.includes('/') ? p.slice(0, p.lastIndexOf('/')) : '');

/** Why the remote buttons are off (null: on). */
const remoteBlock = computed(() => {
  const s = st.value;
  if (!s) return t('common:loading');
  if (busy.value) return t('projects:git.busy', { op: t(`projects:op.${busy.value}`) });
  if (s.detached) return t('projects:git.detachedTip');
  if (!s.has_remote) return t('projects:git.noRemoteTip');
  if (s.operation) return t('projects:conflicts.finishFirst');
  return null;
});
const commitBlock = computed(() => {
  const s = st.value;
  if (!s) return t('common:loading');
  if (busy.value) return t('projects:git.busy', { op: t(`projects:op.${busy.value}`) });
  if (s.detached) return t('projects:git.detachedTip');
  if (s.operation === 'rebase') return t('projects:conflicts.rebaseCommit');
  if (conflicts.value.length) return t('projects:conflicts.resolveFirst');
  if (!changes.value.length && !s.operation) return t('projects:changes.nothing');
  if (!message.value.trim()) return t('projects:changes.writeMessage');
  return null;
});

async function commit() {
  if (commitBlock.value) return;
  if (st.value?.identity_missing) {
    projects.dialog = { kind: 'identity', projectId: props.projectId, retry: () => void commit() };
    return;
  }
  if (await projects.commit(props.projectId, message.value.trim())) message.value = '';
}
function onKey(e: KeyboardEvent) {
  if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) { e.preventDefault(); void commit(); }
}

function open(c: FileChange) {
  tabs.openFileDiff(props.projectId, c.path);
}
function openFile(c: FileChange) {
  tabs.openFile(props.projectId, c.path, false, projects.activeTarget(props.projectId).target);
}
</script>

<template>
  <div class="pc">
    <div v-if="st?.operation" class="pc-op" role="alert">
      <div class="pc-op-title">
        <el-icon><ei-warning-filled /></el-icon>
        {{ $t('projects:conflicts.title', { op: $t(`projects:operation.${st.operation}`), count: conflicts.length }) }}
      </div>
      <div v-for="c in conflicts" :key="c.path" class="pc-conf">
        <div class="pc-row" :title="c.path" @click="open(c)">
          <span class="pc-name">{{ baseName(c.path) }}</span>
          <span class="pc-dir">{{ dirOf(c.path) }}</span>
          <span class="pc-mark m-C">!</span>
        </div>
        <div class="pc-conf-acts">
          <button v-if="isScriptFile(c.path)" @click="openFile(c)">{{ $t('projects:conflicts.open') }}</button>
          <button @click="projects.resolveConflict(projectId, c.path, 'ours')">{{ $t('projects:conflicts.ours') }}</button>
          <button @click="projects.resolveConflict(projectId, c.path, 'theirs')">{{ $t('projects:conflicts.theirs') }}</button>
          <button @click="projects.resolveConflict(projectId, c.path, 'resolved')">{{ $t('projects:conflicts.resolved') }}</button>
        </div>
      </div>
      <p v-if="!conflicts.length" class="pc-hint">{{ $t('projects:conflicts.allResolved') }}</p>
      <div class="pc-op-acts">
        <el-button size="small" type="primary" :disabled="!!conflicts.length || !!busy" @click="projects.finishOperation(projectId, 'continue')">{{ $t('projects:conflicts.continue') }}</el-button>
        <el-button size="small" :disabled="!!busy" @click="projects.finishOperation(projectId, 'abort')">{{ $t('projects:conflicts.abort') }}</el-button>
      </div>
    </div>

    <div class="pc-commit">
      <el-input
        v-model="message"
        type="textarea"
        :autosize="{ minRows: 1, maxRows: 6 }"
        :placeholder="$t('projects:changes.messagePlaceholder')"
        :disabled="!!busy"
        @keydown="onKey"
      />
      <el-tooltip :disabled="!commitBlock" :content="commitBlock ?? ''" placement="bottom" :show-after="300">
        <span class="pc-full">
          <el-button :type="commitBlock ? undefined : 'primary'" size="small" class="pc-full" :loading="busy === 'commit'" :disabled="!!commitBlock" @click="commit">
            <el-icon v-if="busy !== 'commit'"><ei-check /></el-icon>&nbsp;{{ $t('projects:changes.commit') }}
          </el-button>
        </span>
      </el-tooltip>
      <div class="pc-remote">
        <el-tooltip v-for="op in (['pull', 'push', 'sync'] as const)" :key="op" :disabled="!remoteBlock" :content="remoteBlock ?? ''" placement="bottom" :show-after="300">
          <span class="pc-third">
            <el-button size="small" class="pc-full" :loading="busy === op" :disabled="!!remoteBlock" @click="projects.remoteOp(projectId, op)">
              {{ $t(`projects:op.${op}`) }}<template v-if="op === 'pull' && st?.behind"> ↓{{ st.behind }}</template><template v-if="op === 'push' && st?.ahead"> ↑{{ st.ahead }}</template>
            </el-button>
          </span>
        </el-tooltip>
      </div>
    </div>

    <div v-if="!changes.length && !st?.operation" class="pc-hint">{{ st ? $t('projects:changes.none') : '' }}</div>
    <div
      v-for="c in changes"
      :key="c.path"
      class="pc-row"
      :class="{ active: tabs.active?.kind === 'fileDiff' && tabs.active.projectId === projectId && tabs.active.path === c.path }"
      :title="c.orig_path ? `${c.orig_path} → ${c.path}` : c.path"
      @click="open(c)"
    >
      <span class="pc-name" :class="`m-${c.mark}`">{{ baseName(c.path) }}</span>
      <span class="pc-dir">{{ dirOf(c.path) }}</span>
      <span class="pc-acts">
        <button v-if="c.mark !== 'D' && isScriptFile(c.path)" :title="$t('projects:changes.openFile')" @click.stop="openFile(c)"><el-icon><ei-document /></el-icon></button>
        <button :title="$t('projects:changes.discard')" @click.stop="projects.discard(projectId, [c.path])"><el-icon><ei-refresh-left /></el-icon></button>
      </span>
      <span class="pc-mark" :class="`m-${c.mark}`" :title="$t(`projects:mark.${c.mark}`)">{{ c.mark }}</span>
    </div>
    <p v-if="st?.truncated" class="pc-hint">{{ $t('projects:changes.truncated') }}</p>
  </div>
</template>

<style scoped>
.pc { padding-bottom: 6px; }
.pc-commit { display: flex; flex-direction: column; gap: 6px; padding: 4px 10px 8px 22px; }
.pc-full { width: 100%; display: inline-flex; }
.pc-remote { display: flex; gap: 4px; }
.pc-third { flex: 1; display: inline-flex; min-width: 0; }
.pc-remote :deep(.el-button) { margin: 0; }
.pc-hint { margin: 0; padding: 2px 22px 6px; font-size: 12px; color: var(--nm-text-dim); }
.pc-row { display: flex; align-items: center; gap: 6px; height: 22px; padding: 0 10px 0 22px; cursor: pointer; user-select: none; }
.pc-row:hover { background: var(--ide-hover); }
.pc-row.active { background: var(--ide-selection); }
.pc-name { flex: none; max-width: 60%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--nm-text); }
.pc-dir { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 11.5px; color: var(--nm-text-dim); }
.pc-acts { display: none; gap: 2px; }
.pc-row:hover .pc-acts { display: inline-flex; }
.pc-acts button, .pc-conf-acts button {
  border: 0; background: none; color: var(--nm-text-dim); cursor: pointer; padding: 1px 3px; border-radius: 3px; font: inherit; font-size: 11.5px;
}
.pc-acts button:hover, .pc-conf-acts button:hover { color: var(--nm-text-strong); background: var(--ide-hover); }
.pc-mark { flex: none; width: 12px; text-align: center; font-family: var(--nm-mono); font-size: 11px; font-weight: 700; }
.m-M { color: #e2c08d; }
.m-A, .m-U { color: #73c991; }
.m-R { color: #75beff; }
.m-D { color: var(--nm-danger); text-decoration: line-through; }
.pc-mark.m-D { text-decoration: none; }
.m-C { color: var(--nm-danger); }
.pc-op {
  margin: 4px 10px 8px 22px; padding: 6px 0; border-radius: 4px;
  border: 1px solid color-mix(in srgb, var(--nm-danger) 45%, transparent); background: color-mix(in srgb, var(--nm-danger) 8%, transparent);
}
.pc-op-title { display: flex; align-items: center; gap: 6px; padding: 0 8px 4px; font-size: 12px; font-weight: 600; color: var(--nm-danger); }
.pc-op .pc-row { padding-left: 8px; }
.pc-conf-acts { display: flex; flex-wrap: wrap; gap: 2px; padding: 0 8px 4px; }
.pc-op-acts { display: flex; gap: 6px; padding: 4px 8px 0; }
</style>
