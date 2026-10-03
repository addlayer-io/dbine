import { acceptHMRUpdate, defineStore } from 'pinia';
import { ElMessage, ElMessageBox } from 'element-plus';
import { errorMessage } from '../api/client';
import { projectsApi } from '../api/projects';
import type {
  ChangeMark, FsEntry, GitProgress, ProjectBinding, ProjectEnvironment, ProjectFilesChanged, ProjectInfo, ProjectPullOut,
  ProjectStatus, ProjectTarget,
} from '../api/types';
import { t } from '../i18n';
import { useConnectionsStore } from './connections';
import { readJson, writeJson } from './storage';
import { baseName, useTabsStore } from './tabs';
import { runTask, type TaskHandle } from './tasks';
import { useUiStore } from './ui';
import { fileDocs } from '../composables/tabDocument';

// Proyectos (docs/proyectos.md): git repos of scripts linked on this
// machine. The list, each one's git status, the folders shown in the
// sidebar's file tree, and the git operations (as background tasks).
// A project never owns connections: its "base activa" points at one of the
// user's connections and databases, directly or through the environments
// its .dbine.json declares.

export interface Loadable<T> {
  status: 'loading' | 'ready' | 'error';
  data: T | null;
  error: string | null;
}

/** What the sidebar shows open, per project (kept per machine). */
export interface ProjectExpanded { open: boolean; files: boolean; changes: boolean; dirs: string[] }

export type TargetProblem = 'unbound' | 'missing-connection' | 'missing-env' | null;

export interface ActiveTarget {
  target: ProjectTarget | null;
  env: ProjectEnvironment | null;
  /** Alias of the active environment (even when it no longer exists). */
  alias: string | null;
  problem: TargetProblem;
}

/** Dialogs of the Proyectos feature (ProjectDialogs.vue shows them). */
export type ProjectDialog =
  | { kind: 'link'; mode: 'link' | 'clone'; binding: ProjectBinding | null; target: ProjectTarget | null }
  /** Pick the connection › database of the direct binding (`alias` null) or of an environment. */
  | { kind: 'target'; projectId: string; alias: string | null }
  /** The Explorer's database isn't any environment's: which one gets it. */
  | { kind: 'assign'; projectId: string; target: ProjectTarget }
  | { kind: 'environments'; projectId: string }
  | { kind: 'identity'; projectId: string; retry: (() => void) | null };

const EXPANDED_KEY = 'dbine.projects.expanded';
const dirKey = (id: string, dir: string) => `${id}\u0000${dir}`;
const sameTarget = (a: ProjectTarget | null | undefined, b: ProjectTarget | null | undefined) =>
  !!a && !!b && a.connection_id === b.connection_id && a.database === b.database;

/** The folder a path is in ('' = the root). */
export const parentDir = (path: string) => (path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : '');

let focusSeq = 0;
const statusTimers = new Map<string, ReturnType<typeof setTimeout>>();

export const useProjectsStore = defineStore('projects', {
  state: () => ({
    list: [] as ProjectInfo[],
    loaded: false,
    loading: false,
    loadError: null as string | null,
    status: {} as Record<string, Loadable<ProjectStatus>>,
    /** `${id}\0${dir}` → the folder's entries ('' = the root). */
    dirs: {} as Record<string, Loadable<FsEntry[]>>,
    expanded: readJson<Record<string, ProjectExpanded>>(EXPANDED_KEY, {}),
    /** Ask the sidebar to show a project (scroll to it, open). */
    focus: null as { seq: number; id: string } | null,
    /** The git operation running in each project (its name), from this window. */
    busy: {} as Record<string, string>,
    dialog: null as ProjectDialog | null,
  }),

  getters: {
    byId: (s) => (id: string) => s.list.find((p) => p.id === id),

    /** Where the project's file tabs run: the active environment's target, or the direct one. */
    activeTarget(): (id: string) => ActiveTarget {
      return (id: string) => {
        const p = this.byId(id);
        const none: ActiveTarget = { target: null, env: null, alias: null, problem: 'unbound' };
        if (!p) return none;
        const envs = p.manifest?.environments ?? [];
        const alias = envs.length ? p.binding.active_environment : null;
        let target: ProjectTarget | null;
        let env: ProjectEnvironment | null = null;
        if (alias) {
          env = envs.find((e) => e.name.toLowerCase() === alias.toLowerCase()) ?? null;
          if (!env) return { target: null, env: null, alias, problem: 'missing-env' };
          target = p.binding.environments[env.name] ?? p.binding.environments[alias] ?? null;
        } else {
          target = p.binding.direct;
        }
        if (!target) return { target: null, env, alias, problem: 'unbound' };
        if (!useConnectionsStore().byId(target.connection_id)) return { target: null, env, alias, problem: 'missing-connection' };
        return { target, env, alias, problem: null };
      };
    },

    /** Projects whose direct target or any environment is this database. */
    projectsFor: (s) => (connectionId: string, database: string): ProjectInfo[] => {
      const here = { connection_id: connectionId, database };
      return s.list.filter((p) => sameTarget(p.binding.direct, here) || Object.values(p.binding.environments).some((x) => sameTarget(x, here)));
    },

    markOf: (s) => (id: string, path: string): ChangeMark | null =>
      s.status[id]?.data?.changes.find((c) => c.path === path)?.mark ?? null,

    dirHasChanges: (s) => (id: string, dir: string): boolean =>
      !!s.status[id]?.data?.changes.some((c) => dir === '' || c.path.startsWith(`${dir}/`)),

    /** Every project's pending changes (the activity bar's badge). */
    totalChanges: (s) => s.list.reduce((n, p) => n + (s.status[p.id]?.data?.changes.length ?? 0), 0),
  },

  actions: {
    async load(force = false) {
      if ((this.loaded && !force) || this.loading) return;
      this.loading = true;
      try {
        this.list = await projectsApi.list();
        this.loadError = null;
        this.loaded = true;
        const ids = new Set(this.list.map((p) => p.id));
        for (const k of Object.keys(this.status)) if (!ids.has(k)) delete this.status[k];
        void this.refreshAllStatus();
      } catch (e) {
        this.loadError = errorMessage(e);
      } finally {
        this.loading = false;
      }
    },

    replace(info: ProjectInfo) {
      const i = this.list.findIndex((p) => p.id === info.id);
      if (i >= 0) this.list.splice(i, 1, info);
      else this.list.push(info);
    },

    expandedOf(id: string): ProjectExpanded {
      return (this.expanded[id] ??= { open: true, files: true, changes: true, dirs: [] });
    },
    saveExpanded() { writeJson(EXPANDED_KEY, this.expanded); },
    toggleDir(id: string, dir: string, open?: boolean) {
      const e = this.expandedOf(id);
      const has = e.dirs.includes(dir);
      const want = open ?? !has;
      if (want && !has) { e.dirs.push(dir); void this.loadDir(id, dir); }
      if (!want && has) e.dirs = e.dirs.filter((d) => d !== dir);
      this.saveExpanded();
    },

    async refreshStatus(id: string, fetch = false) {
      const cur = this.status[id];
      if (!cur) this.status[id] = { status: 'loading', data: null, error: null };
      try {
        const st = await projectsApi.status(id, fetch);
        this.status[id] = { status: 'ready', data: st, error: null };
      } catch (e) {
        this.status[id] = { status: 'error', data: cur?.data ?? null, error: errorMessage(e) };
      }
    },

    /** A status refresh a little later, once per burst of changes. */
    refreshStatusSoon(id: string, ms = 300) {
      clearTimeout(statusTimers.get(id));
      statusTimers.set(id, setTimeout(() => { statusTimers.delete(id); void this.refreshStatus(id); }, ms));
    },

    async refreshAllStatus(fetch = false) {
      await Promise.all(this.list.filter((p) => p.exists).map((p) => this.refreshStatus(p.id, fetch)));
    },

    async loadDir(id: string, dir: string, force = false) {
      const k = dirKey(id, dir);
      const cur = this.dirs[k];
      if (cur && !force && cur.status !== 'error') return;
      if (!cur) this.dirs[k] = { status: 'loading', data: null, error: null };
      try {
        this.dirs[k] = { status: 'ready', data: await projectsApi.listDir(id, dir), error: null };
      } catch (e) {
        this.dirs[k] = { status: 'error', data: cur?.data ?? null, error: errorMessage(e) };
      }
    },
    dirOf(id: string, dir: string): Loadable<FsEntry[]> | undefined {
      return this.dirs[dirKey(id, dir)];
    },
    /** Reload the folders a change touched (the ones loaded). */
    invalidateDirs(id: string, paths: string[]) {
      const dirs = new Set(paths.map(parentDir));
      // A renamed or deleted folder: its own listing too.
      for (const p of paths) dirs.add(p);
      for (const d of dirs) if (this.dirs[dirKey(id, d)]) void this.loadDir(id, d, true);
    },

    // -- registry ----------------------------------------------------------------------

    async link(a: { path: string; name?: string | null; init?: boolean; binding?: ProjectBinding | null }) {
      const info = await projectsApi.link(a);
      this.replace(info);
      void this.refreshStatus(info.id);
      this.expandedOf(info.id).open = true;
      this.saveExpanded();
      void this.loadDir(info.id, '');
      this.focusProject(info.id);
      return info;
    },

    /** "Clonar repositorio…": a background task (cancellable while it downloads). */
    clone(a: { url: string; parentDir: string; name?: string | null; branch?: string | null; binding?: ProjectBinding | null }) {
      const opId = crypto.randomUUID();
      const label = a.name || a.url.replace(/\.git$/, '').split(/[/:]/).filter(Boolean).pop() || a.url;
      return runTask<ProjectInfo>({
        kind: 'project-git',
        title: t('projects:task.clone', { name: label }),
        cancel: () => projectsApi.cancel(opId),
        run: async (task) => {
          await followProgress(task, opId);
          const info = await projectsApi.clone({ ...a, opId });
          this.replace(info);
          void this.refreshStatus(info.id);
          void this.loadDir(info.id, '');
          this.focusProject(info.id);
          return info;
        },
      }).promise;
    },

    async rename(id: string, name: string) {
      this.replace(await projectsApi.rename(id, name));
    },

    async relocate(id: string, path: string) {
      this.replace(await projectsApi.relocate(id, path));
      for (const k of Object.keys(this.dirs)) if (k.startsWith(`${id}\u0000`)) delete this.dirs[k];
      void this.refreshStatus(id);
      void this.loadDir(id, '');
    },

    /** "Desvincular": only DBine forgets it; the folder stays as it is. */
    async unlink(id: string) {
      const p = this.byId(id);
      if (!p) return;
      try {
        await ElMessageBox.confirm(t('projects:confirm.unlink', { name: p.name, path: p.path }), t('projects:confirm.unlinkTitle'), {
          type: 'warning', confirmButtonText: t('projects:menu.unlink'), cancelButtonText: t('common:cancel'),
        });
      } catch { return; }
      try {
        await projectsApi.unlink(id);
      } catch (e) {
        ElMessage.error(errorMessage(e));
        return;
      }
      useTabsStore().closeProject(id);
      this.list = this.list.filter((x) => x.id !== id);
      delete this.status[id];
      delete this.expanded[id];
      this.saveExpanded();
    },

    async setBinding(id: string, binding: ProjectBinding) {
      try {
        this.replace(await projectsApi.setBinding(id, binding));
      } catch (e) {
        ElMessage.error(errorMessage(e));
        return;
      }
      useTabsStore().retargetProject(id, this.activeTarget(id).target);
    },

    /** Switch the active environment (null = the direct base). */
    async setEnvironment(id: string, alias: string | null) {
      const p = this.byId(id);
      if (!p || p.binding.active_environment === alias) return;
      await this.setBinding(id, { ...p.binding, active_environment: alias });
    },

    /** Map an environment (or the direct base, `alias` null) to a connection › database. */
    async setTarget(id: string, alias: string | null, target: ProjectTarget | null) {
      const p = this.byId(id);
      if (!p) return;
      const b: ProjectBinding = { ...p.binding, environments: { ...p.binding.environments } };
      if (alias === null) b.direct = target;
      else if (target) b.environments[alias] = target;
      else delete b.environments[alias];
      await this.setBinding(id, b);
    },

    /** The Explorer's "Proyectos" node was clicked under a database: that
     *  database becomes the project's active base. */
    async useDatabase(id: string, connectionId: string, database: string) {
      const p = this.byId(id);
      if (!p) return;
      const target = { connection_id: connectionId, database };
      const envs = p.manifest?.environments ?? [];
      const alias = Object.entries(p.binding.environments).find(([a, x]) => sameTarget(x, target) && envs.some((e) => e.name === a))?.[0];
      if (alias) return this.setEnvironment(id, alias);
      if (!envs.length) return this.setBinding(id, { ...p.binding, direct: target, active_environment: null });
      this.dialog = { kind: 'assign', projectId: id, target };
    },

    focusProject(id: string) {
      this.expandedOf(id).open = true;
      this.saveExpanded();
      this.focus = { seq: ++focusSeq, id };
      useUiStore().sidebarView = 'projects';
    },

    // -- git -----------------------------------------------------------------------------

    async commit(id: string, message: string): Promise<boolean> {
      const p = this.byId(id);
      if (!p || this.busy[id]) return false;
      this.busy[id] = 'commit';
      try {
        const oid = await runTask<string>({
          kind: 'project-git', title: t('projects:task.commit', { name: p.name }),
          reopen: () => this.focusProject(id),
          run: () => projectsApi.commit(id, message),
          summary: (oid) => oid,
        }).promise;
        ElMessage.success({ message: t('projects:git.committed', { oid }), duration: 2500 });
        return true;
      } catch (e) {
        const msg = errorMessage(e);
        if (/nombre y email|name and email/i.test(String((e as { message?: string })?.message ?? msg))) {
          this.dialog = { kind: 'identity', projectId: id, retry: () => void this.commit(id, message) };
        } else {
          ElMessage.error({ message: msg, duration: 6000 });
        }
        return false;
      } finally {
        delete this.busy[id];
        void this.refreshStatus(id);
      }
    },

    /** Pull, Push, Sincronizar and Fetch: background tasks, cancellable
     *  while they talk to the remote. */
    async remoteOp(id: string, op: 'pull' | 'push' | 'sync' | 'fetch'): Promise<boolean> {
      const p = this.byId(id);
      if (!p || this.busy[id]) return false;
      if ((op === 'pull' || op === 'sync') && !(await this.saveBeforePull(id))) return false;
      const opId = crypto.randomUUID();
      this.busy[id] = op;
      type Out = ProjectPullOut | boolean | ProjectStatus;
      try {
        const out = await runTask<Out>({
          kind: 'project-git',
          title: t(`projects:task.${op}`, { name: p.name }),
          cancel: () => projectsApi.cancel(opId),
          reopen: () => this.focusProject(id),
          run: async (task) => {
            await followProgress(task, opId);
            if (op === 'pull') return projectsApi.pull(id, opId);
            if (op === 'sync') return projectsApi.sync(id, opId);
            if (op === 'push') return projectsApi.push(id, opId);
            return projectsApi.status(id, true, opId);
          },
          summary: (r) => summaryOf(op, r),
          outcome: (r) => (typeof r === 'object' && r && 'conflicts' in r && r.conflicts.length ? 'error' : 'done'),
        }).promise;
        const text = summaryOf(op, out);
        if (typeof out === 'object' && out && 'conflicts' in out && out.conflicts.length) {
          ElMessage.warning({ message: t('projects:git.conflicts', { count: out.conflicts.length }), duration: 6000 });
          this.expandedOf(id).changes = true;
        } else if (op === 'fetch' && typeof out === 'object' && out && 'fetch_error' in out && out.fetch_error) {
          ElMessage.error({ message: out.fetch_error, duration: 6000 });
        } else if (text) {
          ElMessage.success({ message: text, duration: 2500 });
        }
        if (op === 'fetch' && typeof out === 'object' && out && 'branch' in out) this.status[id] = { status: 'ready', data: out, error: null };
        return true;
      } catch (e) {
        ElMessage.error({ message: errorMessage(e), duration: 7000 });
        return false;
      } finally {
        delete this.busy[id];
        if (op !== 'fetch') void this.refreshStatus(id);
      }
    },

    /** Before a pull: the project's unsaved file tabs here are saved first (or the pull waits). */
    async saveBeforePull(id: string): Promise<boolean> {
      const dirty = [...fileDocs.values()].filter((d) => d.projectId() === id && d.dirty());
      if (!dirty.length) return true;
      try {
        await ElMessageBox.confirm(
          t('projects:confirm.saveBeforePull', { count: dirty.length, list: dirty.map((d) => baseName(d.path())).join(', ') }),
          t('projects:confirm.saveBeforePullTitle'),
          { type: 'warning', confirmButtonText: t('projects:confirm.saveAllContinue'), cancelButtonText: t('common:cancel') },
        );
      } catch { return false; }
      const saved = await Promise.all(dirty.map((d) => d.save()));
      return saved.every(Boolean);
    },

    async resolveConflict(id: string, path: string, action: 'ours' | 'theirs' | 'resolved') {
      try {
        await projectsApi.conflict(id, path, action);
      } catch (e) {
        ElMessage.error(errorMessage(e));
      }
      void this.refreshStatus(id);
    },

    async finishOperation(id: string, action: 'continue' | 'abort') {
      if (action === 'abort') {
        try {
          await ElMessageBox.confirm(t('projects:confirm.abort'), t('projects:confirm.abortTitle'), {
            type: 'warning', confirmButtonText: t('projects:conflicts.abort'), cancelButtonText: t('common:cancel'),
          });
        } catch { return; }
      }
      try {
        const out = await projectsApi.operation(id, action);
        if (out.conflicts.length) ElMessage.warning({ message: t('projects:git.conflicts', { count: out.conflicts.length }), duration: 6000 });
      } catch (e) {
        ElMessage.error({ message: errorMessage(e), duration: 6000 });
      }
      void this.refreshStatus(id);
    },

    async discard(id: string, paths: string[]) {
      if (!paths.length) return;
      try {
        await ElMessageBox.confirm(
          paths.length === 1 ? t('projects:confirm.discardOne', { name: paths[0] }) : t('projects:confirm.discardMany', { count: paths.length }),
          t('projects:confirm.discardTitle'),
          { type: 'warning', confirmButtonText: t('projects:changes.discard'), cancelButtonText: t('common:cancel'), confirmButtonClass: 'el-button--danger' },
        );
      } catch { return; }
      try {
        await projectsApi.discard(id, paths);
      } catch (e) {
        ElMessage.error(errorMessage(e));
      }
      void this.refreshStatus(id);
    },

    /** `project-files-changed`: the folders it touched reload, and the status. */
    applyFilesChanged(e: ProjectFilesChanged) {
      if (!this.byId(e.project_id)) return;
      this.invalidateDirs(e.project_id, e.paths);
      // The manifest (environments) is read with the list.
      if (e.paths.includes('.dbine.json')) void this.load(true);
      this.refreshStatusSoon(e.project_id);
    },
  },
});

/** The git operation's `--progress` lines, as the task's progress. */
async function followProgress(task: TaskHandle<unknown>, opId: string) {
  try {
    await task.listen<GitProgress>('project-git-progress', (e) => {
      if (e.payload.op_id !== opId) return;
      task.progress({ phase: e.payload.phase, ...(e.payload.percent != null ? { done: e.payload.percent, total: 100, unit: '%' } : {}) });
    });
  } catch { /* outside Tauri */ }
}

function summaryOf(op: 'pull' | 'push' | 'sync' | 'fetch', r: unknown): string | undefined {
  if (op === 'push') return r ? t('projects:git.pushedUpstream') : t('projects:git.pushed');
  if (op === 'fetch') return t('projects:git.fetched');
  const o = r as ProjectPullOut & { pushed?: boolean };
  if (o.conflicts?.length) return t('projects:git.conflicts', { count: o.conflicts.length });
  if (o.note) return o.note;
  const pulled = o.up_to_date ? t('projects:git.upToDate') : t('projects:git.updated', { count: o.updated.length });
  return op === 'sync' && o.pushed ? `${pulled} · ${t('projects:git.pushed')}` : pulled;
}

// Development: swap the store's code in place when it changes (no reload).
if (import.meta.hot) import.meta.hot.accept(acceptHMRUpdate(useProjectsStore, import.meta.hot));
