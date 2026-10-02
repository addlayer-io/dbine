<script lang="ts">
import { ref as moduleRef } from 'vue';

// Kept outside the component: LibrarySidebar mounts the dialog only while it's
// open, and a git operation (a task) outlives it. Reopening shows it running,
// or the conflicts a pull left behind.
const busy = moduleRef<string | null>(null);
const conflicts = moduleRef<string[]>([]);
</script>

<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { tb } from '../i18n/backend';
import { libraryGitApi, type GitApplied, type LibraryGitStatus } from '../api/library';
import { confirmNative } from '../native';
import { useLibraryStore } from '../stores/library';
import { useSettingsStore } from '../stores/settings';
import { runTask, type TaskHandle } from '../stores/tasks';

// The Library in a git repo (docs/biblioteca.md): link a repo, then commit,
// pull, push or sync. Git runs on this machine with the user's credentials.
// Each operation is a task (stores/tasks.ts): it shows in Tareas, goes on if
// the dialog closes ("Seguir en segundo plano") and then notifies when it
// ends. Git has no cancel path here, so the task offers no Cancelar.

const open = defineModel<boolean>({ required: true });
const lib = useLibraryStore();
const { t } = useTranslation();
/** File extensions named in the help (one slot each). */
const EXTS = ['sql', 'js', 'json', 'cql', 'redis', 'flux', 'cypher'];

const status = ref<LibraryGitStatus | null>(null);
const loading = ref(false);
const error = ref<string | null>(null);
let task: TaskHandle | null = null;
let alive = true;
onBeforeUnmount(() => {
  alive = false;
  // Closed mid-run: it goes on and notifies at the end.
  if (busy.value) task?.background();
});
const message = ref('');
const remote = ref('');
const branch = ref('main');

const linked = computed(() => !!status.value?.remote && !!status.value?.dir);

async function refresh(fetch = false) {
  loading.value = true;
  try {
    status.value = await libraryGitApi.status(fetch);
    if (status.value.remote) remote.value = status.value.remote;
    if (status.value.branch) branch.value = status.value.branch;
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    loading.value = false;
  }
}
watch(open, (o) => {
  if (!o) return;
  error.value = null;
  // Reopened while an operation runs in the background: don't fetch alongside it.
  refresh(!busy.value);
}, { immediate: true });

/** The Library and its folders changed on disk: reload them. */
async function reload() {
  await Promise.all([lib.load(true), useSettingsStore().load()]);
}

function summary(a: GitApplied) {
  const parts = [
    a.added && t('library:git.added', { count: a.added }),
    a.updated && t('library:git.updated', { count: a.updated }),
    a.deleted && t('library:git.deleted', { count: a.deleted }),
  ].filter(Boolean);
  return parts.length ? t('library:git.repoScripts', { list: parts.join(', ') }) : t('library:git.upToDate');
}

/** "Seguir en segundo plano": the operation goes on, the dialog closes. */
function toBackground() {
  task?.background();
  open.value = false;
}

async function run(what: string, step: () => Promise<string | void>) {
  if (busy.value) return;
  busy.value = what;
  error.value = null;
  if (what !== 'resolve') conflicts.value = [];
  const { task: current, promise } = runTask<string | void>({
    kind: 'library-git',
    title: t(`tasks:settingsGit.git.${what}`),
    run: step,
    summary: (note) => note || undefined,
    // A pull that stopped on conflicts didn't apply anything: say so.
    outcome: () => (conflicts.value.length ? 'error' : 'done'),
  });
  task = current;
  try {
    const note = await promise;
    if (note && alive && !conflicts.value.length) ElMessage.success({ message: note, duration: 3500 });
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    busy.value = null;
    if (task === current) task = null;
    if (alive) await refresh();
  }
}

const link = () =>
  run('link', async () => {
    const r = await libraryGitApi.link(remote.value.trim(), branch.value.trim() || 'main');
    await reload();
    return r.pushed ? t('library:git.linkedPushed', { summary: summary(r.applied) }) : summary(r.applied);
  });
const commit = () =>
  run('commit', async () => {
    await libraryGitApi.commit(message.value);
    message.value = '';
    return t('library:git.committed');
  });
const pull = () =>
  run('pull', async () => {
    const r = await libraryGitApi.pull();
    conflicts.value = r.conflicts;
    if (r.conflicts.length) return t('tasks:settingsGit.git.conflicts', { count: r.conflicts.length });
    await reload();
    return summary(r.applied);
  });
const push = () => run('push', async () => { await libraryGitApi.push(); return t('library:git.pushed'); });
const sync = () =>
  run('sync', async () => {
    const r = await libraryGitApi.sync(message.value);
    conflicts.value = r.conflicts;
    if (r.conflicts.length) return t('tasks:settingsGit.git.conflicts', { count: r.conflicts.length });
    message.value = '';
    await reload();
    return t('library:git.synced', { summary: summary(r.applied) });
  });
async function resolve(keep: 'remote' | 'local') {
  const ok = await confirmNative(
    keep === 'remote'
      ? t('library:git.useRemoteConfirm')
      : t('library:git.useLocalConfirm'),
    { title: keep === 'remote' ? t('library:git.useRemote') : t('library:git.useLocal'), okLabel: t('common:continue') },
  );
  if (!ok) return;
  await run('resolve', async () => {
    const a = await libraryGitApi.resolve(keep);
    conflicts.value = [];
    await reload();
    return keep === 'remote' ? summary(a) : t('library:git.localKept');
  });
}
async function unlink() {
  const ok = await confirmNative(t('library:git.unlinkConfirm'), {
    title: t('library:git.unlinkTitle'),
    okLabel: t('library:git.unlink'),
  });
  if (ok) await run('unlink', async () => { await libraryGitApi.unlink(); return t('library:git.unlinked'); });
}

const STATE: Record<string, { label: string; cls: string }> = {
  added: { label: 'A', cls: 'add' },
  modified: { label: 'M', cls: 'mod' },
  deleted: { label: 'D', cls: 'del' },
  renamed: { label: 'R', cls: 'mod' },
  conflict: { label: '!', cls: 'del' },
};
</script>

<template>
  <el-dialog v-model="open" :title="$t('library:git.title')" width="620px" append-to-body>
    <div v-if="!status && loading" class="lg-empty"><el-icon class="is-loading"><ei-loading /></el-icon></div>

    <template v-else-if="status">
      <el-alert
        v-if="!status.git"
        type="warning"
        :closable="false"
        show-icon
        :title="$t('library:git.noGit')"
        :description="$t('library:git.noGitHelp')"
      />

      <!-- Not linked yet -->
      <template v-else-if="!linked">
        <p class="lg-help">
          {{ $t('library:git.help') }}
          <i18next :translation="$t('library:git.helpFiles')">
            <template v-for="x in EXTS" #[x]><code>.{{ x }}</code></template>
          </i18next>
        </p>
        <el-form label-position="top" @submit.prevent="link">
          <el-form-item :label="$t('library:git.repository')">
            <el-input v-model="remote" :placeholder="$t('library:git.repositoryPlaceholder')" />
          </el-form-item>
          <el-form-item :label="$t('library:git.branch')">
            <el-input v-model="branch" placeholder="main" style="width: 200px" />
          </el-form-item>
        </el-form>
        <p class="lg-help dim">
          {{ $t('library:git.credentialsHelp') }}
        </p>
      </template>

      <!-- Linked -->
      <template v-else>
        <div class="lg-repo">
          <el-icon><ei-link /></el-icon>
          <span class="lg-remote" :title="status.remote ?? ''">{{ status.remote }}</span>
          <span class="lg-branch">{{ status.branch }}</span>
          <span v-if="status.ahead" class="lg-count" :title="$t('library:git.ahead')">↑ {{ status.ahead }}</span>
          <span v-if="status.behind" class="lg-count" :title="$t('library:git.behind')">↓ {{ status.behind }}</span>
          <div style="flex: 1" />
          <button class="lg-link" :disabled="loading" @click="refresh(true)">{{ $t('library:git.refresh') }}</button>
        </div>
        <div v-if="status.last_commit" class="lg-last">{{ $t('library:git.lastCommit', { commit: status.last_commit }) }}</div>
        <el-alert v-if="status.fetch_error" type="warning" :closable="false" show-icon class="lg-alert"
          :title="$t('library:git.fetchError', { error: tb(status.fetch_error) })" />

        <div class="lg-section">{{ $t('library:git.uncommitted') }}</div>
        <div class="lg-changes">
          <div v-for="c in status.changes" :key="c.path" class="lg-change">
            <span class="lg-state" :class="STATE[c.state]?.cls">{{ STATE[c.state]?.label ?? '?' }}</span>
            <span class="lg-path">{{ c.path }}</span>
          </div>
          <div v-if="!status.changes.length" class="lg-none">{{ $t('library:git.noChanges') }}</div>
        </div>

        <div v-if="conflicts.length" class="lg-conflict">
          <div class="lg-conflict-title">
            <el-icon><ei-warning-filled /></el-icon>
            {{ $t('library:git.conflicts') }}
          </div>
          <div v-for="f in conflicts" :key="f" class="lg-path">{{ f }}</div>
          <div class="lg-conflict-actions">
            <el-button size="small" :loading="busy === 'resolve'" @click="resolve('remote')">{{ $t('library:git.useRemote') }}</el-button>
            <el-button size="small" :loading="busy === 'resolve'" @click="resolve('local')">{{ $t('library:git.useLocal') }}</el-button>
          </div>
        </div>

        <el-input v-model="message" class="lg-message" :placeholder="$t('library:git.messagePlaceholder')" @keydown.enter="sync" />
      </template>

      <el-alert v-if="error" type="error" :closable="false" show-icon class="lg-alert" :title="error" />
    </template>

    <template #footer>
      <template v-if="status?.git && !linked">
        <el-button v-if="busy" @click="toBackground">{{ $t('tasks:panel.background') }}</el-button>
        <el-button @click="open = false">{{ $t('common:cancel') }}</el-button>
        <el-button type="primary" :loading="busy === 'link'" :disabled="!remote.trim()" @click="link">{{ $t('library:git.link') }}</el-button>
      </template>
      <div v-else-if="linked" class="lg-footer">
        <el-button text type="danger" :disabled="!!busy" @click="unlink">{{ $t('library:git.unlink') }}</el-button>
        <div style="flex: 1" />
        <el-button v-if="busy" @click="toBackground">{{ $t('tasks:panel.background') }}</el-button>
        <el-button :loading="busy === 'commit'" :disabled="!!busy || !status?.changes.length" @click="commit">Commit</el-button>
        <el-button :loading="busy === 'pull'" :disabled="!!busy" @click="pull">Pull</el-button>
        <el-button :loading="busy === 'push'" :disabled="!!busy" @click="push">Push</el-button>
        <el-button type="primary" :loading="busy === 'sync'" :disabled="!!busy" @click="sync">{{ $t('library:git.sync') }}</el-button>
      </div>
      <el-button v-else @click="open = false">{{ $t('common:close') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.lg-empty { display: flex; justify-content: center; padding: 30px; color: var(--nm-text-dim); }
.lg-help { margin: 0 0 12px; font-size: 12.5px; line-height: 1.5; color: var(--nm-text); }
.lg-help.dim { color: var(--nm-text-dim); font-size: 12px; margin-top: 4px; }
.lg-repo { display: flex; align-items: center; gap: 8px; min-width: 0; font-size: 12.5px; }
.lg-remote { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--nm-text-strong); }
.lg-branch {
  padding: 0 6px; border-radius: 8px; font-size: 11px; line-height: 17px; white-space: nowrap;
  color: var(--nm-text); border: 1px solid var(--nm-border-soft);
}
.lg-count { font-size: 11.5px; color: var(--nm-warning); white-space: nowrap; }
.lg-link { border: 0; background: none; padding: 0; font: inherit; font-size: 12px; cursor: pointer; color: var(--nm-link, #3794ff); }
.lg-link:disabled { opacity: 0.5; cursor: default; }
.lg-last { margin-top: 4px; font-size: 11.5px; color: var(--nm-text-dim); }
.lg-alert { margin-top: 10px; }
.lg-section { margin: 14px 0 6px; font-size: 11px; text-transform: uppercase; letter-spacing: 0.04em; color: var(--nm-text-dim); }
.lg-changes {
  max-height: 200px; overflow: auto; border: 1px solid var(--nm-border-soft); border-radius: 4px; padding: 4px 0;
  background: var(--nm-bg);
}
.lg-change { display: flex; gap: 8px; padding: 2px 10px; font-size: 12px; }
.lg-state { width: 12px; font-family: var(--nm-mono); font-weight: 600; text-align: center; }
.lg-state.add { color: #89d185; }
.lg-state.mod { color: #d7ba7d; }
.lg-state.del { color: var(--nm-danger); }
.lg-path { font-family: var(--nm-mono); font-size: 12px; color: var(--nm-text); overflow-wrap: anywhere; }
.lg-none { padding: 6px 10px; font-size: 12px; color: var(--nm-text-dim); }
.lg-conflict {
  margin-top: 10px; padding: 8px 10px; border-radius: 4px; font-size: 12.5px;
  border: 1px solid color-mix(in srgb, var(--nm-warning) 55%, transparent);
  background: color-mix(in srgb, var(--nm-warning) 8%, transparent);
}
.lg-conflict-title { display: flex; align-items: center; gap: 6px; margin-bottom: 6px; color: var(--nm-warning); }
.lg-conflict-actions { display: flex; gap: 8px; margin-top: 8px; }
.lg-message { margin-top: 12px; }
.lg-footer { display: flex; align-items: center; gap: 4px; }
</style>
