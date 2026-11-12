import { defineStore } from 'pinia';
import { errorMessage } from '../api/client';
import { jobsApi, type JobList, type ServerJob } from '../api/jobs';
import type { SavedConnection } from '../api/types';
import { t } from '../i18n';
import { useConnectionsStore } from './connections';
import { readJson, writeJson } from './storage';

// "Programadas › En los servidores" (docs/server-jobs.md): the jobs of each
// connection whose engine has them. A connection's jobs are read only when
// its group is open: opening the view never connects to every server.

export interface JobsEntry {
  loading: boolean;
  error: string | null;
  list: JobList | null;
  /** When the list was read (ms), 0: never. */
  at: number;
}

const EXPANDED = 'dbine.serverJobs.expanded';
let focusSeq = 0;

export const useServerJobsStore = defineStore('serverJobs', {
  state: () => ({
    /** Connections whose group is open (kept between sessions). */
    expanded: readJson<string[]>(EXPANDED, []),
    entries: {} as Record<string, JobsEntry>,
    /** "Jobs del servidor" from the explorer: App opens the sidebar on it. */
    focus: null as { seq: number; connectionId: string } | null,
  }),
  getters: {
    /** Saved connections whose driver lists jobs, by name. */
    connections(): SavedConnection[] {
      const conns = useConnectionsStore();
      return conns.list
        .filter((c) => !!conns.driverOf(c.id)?.capabilities.jobs)
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
      if (e.loading) return;
      e.loading = true;
      try {
        if (!(await conns.ensureConnected(connectionId, opts))) {
          e.error = conns.live[connectionId]?.error ?? t('common:error');
          return;
        }
        e.list = await jobsApi.list(connectionId);
        e.error = null;
      } catch (err) {
        e.error = errorMessage(err);
      } finally {
        e.loading = false;
        e.at = Date.now();
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
