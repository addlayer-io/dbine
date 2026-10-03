<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { errorMessage } from '../api/client';
import { projectsApi } from '../api/projects';
import type { FileDiff } from '../api/types';
import CodeDiff from '../components/CodeDiff.vue';
import { lineDiff, type DiffLine } from '../composables/lineDiff';
import { isScriptFile } from '../composables/tabDocument';
import { useProjectsStore } from '../stores/projects';
import { useTabsStore, type FileDiffTab } from '../stores/tabs';

// A project file's changes against the last commit, side by side (left:
// the commit, right: the file now). A file in conflict shows the two sides
// being merged (left: this branch's, right: the incoming one).

const props = defineProps<{ tab: FileDiffTab }>();
const projects = useProjectsStore();
const tabs = useTabsStore();

const diff = ref<FileDiff | null>(null);
const error = ref<string | null>(null);
const loading = ref(false);

async function load() {
  loading.value = true;
  try {
    diff.value = await projectsApi.diff(props.tab.projectId, props.tab.path);
    error.value = null;
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    loading.value = false;
  }
}
watch(() => props.tab.path, load, { immediate: true });
// The status refreshes after every change on disk (saves, pulls, other editors).
watch(() => projects.status[props.tab.projectId]?.data, () => { void load(); });

/** A file's lines, without the empty one after its final newline. */
const split = (s: string) => {
  const t = s.replace(/\r\n/g, '\n');
  return (t.endsWith('\n') ? t.slice(0, -1) : t).split('\n');
};
const lines = computed<DiffLine[]>(() => {
  const d = diff.value;
  if (!d || d.binary || d.too_large) return [];
  // A side that doesn't exist (a new or deleted file) has no lines at all.
  if (d.before === null) return d.after === null ? [] : split(d.after).map((l) => ({ left: null, right: l, kind: 'right' }));
  if (d.after === null) return split(d.before).map((l) => ({ left: l, right: null, kind: 'left' }));
  const strip = (s: string) => (s.endsWith('\n') ? s.slice(0, -1) : s);
  return lineDiff(strip(d.before), strip(d.after), { exact: true });
});
const stats = computed(() => ({
  added: lines.value.filter((l) => l.kind === 'right' || l.kind === 'changed').length,
  removed: lines.value.filter((l) => l.kind === 'left' || l.kind === 'changed').length,
}));
const conflict = computed(() => diff.value?.mark === 'C');
/** Committed or discarded since the tab opened: nothing to show any more. */
const noChanges = computed(() => {
  const st = projects.status[props.tab.projectId]?.data;
  return !!st && !st.truncated && !st.changes.some((c) => c.path === props.tab.path);
});
const project = computed(() => projects.byId(props.tab.projectId));
</script>

<template>
  <div class="fd">
    <div class="nm-toolbar fd-bar">
      <span class="fd-mark" :class="diff?.mark">{{ noChanges ? '' : diff?.mark ?? '' }}</span>
      <span class="fd-path nm-selectable" :title="tab.path">
        <span class="nm-muted">{{ project?.name }} › </span>
        <template v-if="diff?.orig_path">{{ diff.orig_path }} → </template>{{ tab.path }}
      </span>
      <span v-if="lines.length && !noChanges" class="fd-stats"><span class="add">+{{ stats.added }}</span> <span class="del">−{{ stats.removed }}</span></span>
      <div class="nm-spacer" />
      <el-button v-if="!noChanges && diff?.mark !== 'D' && isScriptFile(tab.path)" size="small" @click="tabs.openFile(tab.projectId, tab.path, false, projects.activeTarget(tab.projectId).target)">
        <el-icon><ei-document /></el-icon>&nbsp;{{ $t('projects:changes.openFile') }}
      </el-button>
      <el-button v-if="!conflict && !noChanges" size="small" @click="projects.discard(tab.projectId, [tab.path])">
        <el-icon><ei-refresh-left /></el-icon>&nbsp;{{ $t('projects:changes.discard') }}
      </el-button>
      <el-button size="small" :loading="loading" :title="$t('common:refresh')" @click="load"><el-icon v-if="!loading"><ei-refresh /></el-icon></el-button>
    </div>
    <div v-if="!noChanges" class="fd-heads">
      <div>{{ conflict ? $t('projects:diff.ours') : diff?.before === null ? $t('projects:diff.noBefore') : $t('projects:diff.before') }}</div>
      <div>{{ conflict ? $t('projects:diff.theirs') : diff?.after === null ? $t('projects:diff.noAfter') : $t('projects:diff.after') }}</div>
    </div>
    <div v-if="noChanges" class="fd-empty">{{ $t('projects:diff.noChanges') }}</div>
    <div v-else-if="error" class="nm-content"><el-alert type="error" :title="error" :closable="false" /></div>
    <div v-else-if="diff?.binary" class="fd-empty">{{ $t('projects:diff.binary') }}</div>
    <div v-else-if="diff?.too_large" class="fd-empty">{{ $t('projects:diff.tooLarge') }}</div>
    <CodeDiff v-else-if="diff" :lines="lines" git />
  </div>
</template>

<style scoped>
.fd { display: flex; flex-direction: column; height: 100%; min-height: 0; }
.fd-bar { gap: 8px; }
.fd-path { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 12.5px; }
.fd-mark { font-family: var(--nm-mono); font-weight: 700; font-size: 12px; width: 14px; text-align: center; }
.fd-mark.M { color: #e2c08d; }
.fd-mark.A, .fd-mark.U { color: #73c991; }
.fd-mark.D, .fd-mark.C { color: var(--nm-danger); }
.fd-mark.R { color: #75beff; }
.fd-stats { font-family: var(--nm-mono); font-size: 11.5px; }
.fd-stats .add { color: #73c991; }
.fd-stats .del { color: var(--nm-danger); }
.fd-heads {
  display: grid; grid-template-columns: 1fr 1fr; flex-shrink: 0; font-size: 11px; text-transform: uppercase; letter-spacing: 0.04em;
  color: var(--nm-text-dim); border-bottom: 1px solid var(--nm-border-soft);
}
.fd-heads > div { padding: 4px 12px 4px 52px; }
.fd-empty { padding: 24px; color: var(--nm-text-dim); }
</style>
