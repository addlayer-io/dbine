import { defineStore } from 'pinia';
import { errorMessage } from '../api/client';
import { jobsApi, type JobList, type ServerJob } from '../api/jobs';
import { schedulerApi, type SchedulerInfo, type SchedulerItem, type SchedulerKind, type SchedulerList } from '../api/scheduler';
import type { SavedConnection } from '../api/types';
import { t } from '../i18n';
import { useConnectionsStore } from './connections';
import { readJson, writeJson } from './storage';

// "Programadas › En los servidores" (docs/server-jobs.md): the jobs of each
// connection whose engine has them. A connection's jobs are read only when
// its group is open: opening the view never connects to every server.
// Engines whose scheduler DBine manages whole (`capabilities.scheduler`)
// also read what their scheduler has (its kinds of object, notes) with the
// jobs, each kind's objects when its node is opened, and the job kind's
// objects with the jobs: what each job may do (edit, delete, which of its
// kind's actions) comes with them, not with the jobs list.

export interface JobsEntry {
  loading: boolean;
  error: string | null;
  list: JobList | null;
  /** When the list was read (ms), 0: never. */
  at: number;
}

/** A kind's objects (`schedulerApi.items`), read when its node opens. */
export interface KindItems {
  loading: boolean;
  error: string | null;
  list: SchedulerList | null;
}

/** What a connection's scheduler has, read with its jobs. */
export interface SchedulerEntry {
  info: SchedulerInfo | null;
  error: string | null;
  /** Kind key → its objects, once its node was opened. */
  items: Record<string, KindItems>;
}

const EXPANDED = 'dbine.serverJobs.expanded';
const KINDS_OPEN = 'dbine.serverJobs.kindsOpen';
const kindKey = (connectionId: string, kind: string) => `${connectionId}\u0000${kind}`;
let focusSeq = 0;
let changeSeq = 0;
/** Reads asked while the same read was running (connection ids, and
 *  connection + kind for a kind's objects): they run once more when it
 *  ends, so a refresh after a change never shows what was read before it. */
const readAgain = new Set<string>();

export const useServerJobsStore = defineStore('serverJobs', {
  state: () => ({
    /** Connections whose group is open (kept between sessions). */
    expanded: readJson<string[]>(EXPANDED, []),
    entries: {} as Record<string, JobsEntry>,
    /** The scheduler of connections whose engine has `capabilities.scheduler`. */
    scheduler: {} as Record<string, SchedulerEntry>,
    /** Kind nodes opened or closed by the user (connection + kind); the rest
     *  keep their default: "job" open, others closed. */
    kindsOpen: readJson<Record<string, boolean>>(KINDS_OPEN, {}),
    /** "Jobs del servidor" from the explorer: App opens the sidebar on it. */
    focus: null as { seq: number; connectionId: string } | null,
    /** The last object changed from outside its designer (one of its kind's
     *  actions, a job turned on or off): an open designer of it reads it
     *  again, asking first if it has changes. */
    changed: null as { seq: number; connectionId: string; kind: string; id: string } | null,
  }),
  getters: {
    /** Saved connections whose driver lists jobs (or manages its
     *  scheduler), by name. */
    connections(): SavedConnection[] {
      const conns = useConnectionsStore();
      return conns.list
        .filter((c) => !!conns.driverOf(c.id)?.capabilities.jobs || !!conns.driverOf(c.id)?.capabilities.scheduler)
        .sort((a, b) => a.name.localeCompare(b.name));
    },
  },
  actions: {
    /** The connection's entry, created empty: always the reactive one (an
     *  `??=` would hand back the plain object, and changes to it wouldn't
     *  show). */
    entry(connectionId: string): JobsEntry {
      if (!this.entries[connectionId]) this.entries[connectionId] = { loading: false, error: null, list: null, at: 0 };
      return this.entries[connectionId];
    },
    /** The connection's scheduler entry, created empty (reactive, as `entry`). */
    sched(connectionId: string): SchedulerEntry {
      if (!this.scheduler[connectionId]) this.scheduler[connectionId] = { info: null, error: null, items: {} };
      return this.scheduler[connectionId];
    },
    /** The engine's scheduler is managed whole from DBine (the designer). */
    hasScheduler(connectionId: string): boolean {
      return !!useConnectionsStore().driverOf(connectionId)?.capabilities.scheduler;
    },
    /** One of the scheduler's kinds, as its last read has it. */
    kind(connectionId: string, key: string): SchedulerKind | undefined {
      return this.scheduler[connectionId]?.info?.kinds.find((k) => k.key === key);
    },
    /** One object of a kind as the last read of its objects has it (a job,
     *  with what it may do: kind `job`). */
    item(connectionId: string, kind: string, id: string): SchedulerItem | undefined {
      return this.scheduler[connectionId]?.items[kind]?.list?.items.find((i) => i.id === id);
    },
    /** Tells an open designer of the object that the server's copy changed. */
    objectChanged(connectionId: string, kind: string, id: string) {
      this.changed = { seq: ++changeSeq, connectionId, kind, id };
    },
    isKindOpen(connectionId: string, kind: string): boolean {
      return this.kindsOpen[kindKey(connectionId, kind)] ?? kind === 'job';
    },
    toggleKind(connectionId: string, kind: string) {
      const open = !this.isKindOpen(connectionId, kind);
      this.kindsOpen = { ...this.kindsOpen, [kindKey(connectionId, kind)]: open };
      writeJson(KINDS_OPEN, this.kindsOpen);
      if (open && kind !== 'job' && !this.sched(connectionId).items[kind]?.list) void this.loadItems(connectionId, kind);
    },
    /** Reads a kind's objects ("job": what each job may do; the tree shows
     *  the jobs list's rows). */
    async loadItems(connectionId: string, kind: string) {
      const s = this.sched(connectionId);
      if (!s.items[kind]) s.items[kind] = { loading: false, error: null, list: null };
      const k = s.items[kind];
      const again = kindKey(connectionId, kind);
      if (k.loading) { readAgain.add(again); return; }
      k.loading = true;
      try {
        k.list = await schedulerApi.items(connectionId, kind);
        k.error = null;
      } catch (err) {
        k.error = errorMessage(err);
      } finally {
        k.loading = false;
        if (readAgain.delete(again)) void this.loadItems(connectionId, kind);
      }
    },
    /** What the scheduler has, then the objects of its open kinds and the
     *  job kind's (read with the jobs: a job's menu and tab need them). */
    async loadScheduler(connectionId: string) {
      const s = this.sched(connectionId);
      try {
        s.info = await schedulerApi.info(connectionId);
        s.error = null;
      } catch (err) {
        s.error = errorMessage(err);
        return;
      }
      await Promise.all(s.info.kinds
        .filter((k) => k.key === 'job' || this.isKindOpen(connectionId, k.key) || s.items[k.key]?.list)
        .map((k) => this.loadItems(connectionId, k.key)));
    },
    /** What the job kind's objects say each job may do, read once for a
     *  view that needs it (a job's tab or designer restored with the app,
     *  its connection closed in the sidebar). */
    async ensureJobItems(connectionId: string) {
      await this.ensureInfo(connectionId);
      const k = this.scheduler[connectionId]?.items.job;
      if (this.kind(connectionId, 'job') && !k?.list && !k?.loading) await this.loadItems(connectionId, 'job');
    },
    /** The scheduler's kinds for a view that needs them (a designer tab
     *  restored with the app): read once, connecting if needed. */
    async ensureInfo(connectionId: string) {
      const s = this.sched(connectionId);
      if (s.info || !this.hasScheduler(connectionId)) return;
      if (!(await useConnectionsStore().ensureConnected(connectionId, { auto: true }))) return;
      try {
        s.info = await schedulerApi.info(connectionId);
        s.error = null;
      } catch (err) {
        s.error = errorMessage(err);
      }
    },
    /** After a change made from DBine: the jobs again, and with them every
     *  kind that's open or was read (`loadScheduler`), the changed one's
     *  objects included. */
    refreshAfterChange(connectionId: string) {
      void this.load(connectionId);
    },
    isExpanded(connectionId: string): boolean {
      return this.expanded.includes(connectionId);
    },
    toggle(connectionId: string) {
      if (this.isExpanded(connectionId)) {
        this.expanded = this.expanded.filter((id) => id !== connectionId);
      } else {
        this.expanded = [...this.expanded, connectionId];
        if (!this.entry(connectionId).at) void this.load(connectionId);
      }
      writeJson(EXPANDED, this.expanded);
    },
    /** Reads the connection's jobs. `auto`: an unattended refresh, which
     *  doesn't connect again after a failed connection (see connectionGate). */
    async load(connectionId: string, opts: { auto?: boolean } = {}) {
      const conns = useConnectionsStore();
      const e = this.entry(connectionId);
      if (e.loading) {
        // An unattended read (the timer) can go: one is running. Any other
        // (a refresh, the read after a change) runs again when it ends: the
        // running one may have read before the change.
        if (!opts.auto) readAgain.add(connectionId);
        return;
      }
      e.loading = true;
      try {
        if (!(await conns.ensureConnected(connectionId, opts))) {
          e.error = conns.live[connectionId]?.error ?? t('common:error');
          return;
        }
        const scheduler = this.hasScheduler(connectionId) ? this.loadScheduler(connectionId) : null;
        e.list = conns.driverOf(connectionId)?.capabilities.jobs ? await jobsApi.list(connectionId) : { jobs: [] };
        e.error = null;
        await scheduler;
      } catch (err) {
        e.error = errorMessage(err);
      } finally {
        e.loading = false;
        e.at = Date.now();
        if (readAgain.delete(connectionId)) void this.load(connectionId);
      }
    },
    /** The open groups' lists again (the sidebar's timer and refresh). */
    refreshOpen(opts: { auto?: boolean } = {}) {
      const live = new Set(this.connections.map((c) => c.id));
      for (const id of this.expanded) if (live.has(id)) void this.load(id, opts);
    },
    /** "Jobs del servidor" on a connection: show its group, open and read. */
    reveal(connectionId: string) {
      if (!this.isExpanded(connectionId)) this.toggle(connectionId);
      else void this.load(connectionId);
      this.focus = { seq: ++focusSeq, connectionId };
    },
    /** A job as the last read of its connection has it. */
    job(connectionId: string, id: string): ServerJob | undefined {
      return this.entries[connectionId]?.list?.jobs.find((j) => j.id === id);
    },
    /** Replaces one job after reading it again (the job's tab). */
    put(connectionId: string, job: ServerJob) {
      const list = this.entries[connectionId]?.list;
      if (!list) return;
      const i = list.jobs.findIndex((j) => j.id === job.id);
      if (i >= 0) list.jobs[i] = job;
    },
  },
});
