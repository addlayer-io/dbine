import { acceptHMRUpdate, defineStore } from 'pinia';
import { listen } from '@tauri-apps/api/event';
import { syncApi, type RunStatus, type SyncStatus } from '../api/sync';
import { dbKey, useConnectionsStore } from './connections';
import { useSettingsStore } from './settings';
import { useLibraryStore } from './library';
import { useTabsStore } from './tabs';
import { useUiStore } from './ui';

// Cloud backup state for the status bar and Configuración › Sincronización.
// The backend runs the automatic sync and reports through events.

export const useSyncStore = defineStore('sync', {
  state: () => ({
    info: null as SyncStatus | null,
    listening: false,
  }),
  getters: {
    enabled: (s) => !!s.info?.config.enabled,
    run: (s): RunStatus | null => s.info?.status ?? null,
  },
  actions: {
    async refresh() {
      try { this.info = await syncApi.status(); } catch { /* outside Tauri */ }
    },
    async init() {
      await this.refresh();
      if (this.listening) return;
      this.listening = true;
      try {
        await listen<RunStatus>('sync-status', (e) => {
          if (this.info) this.info.status = e.payload;
          if (!e.payload.running) this.refresh();
        });
        await listen('sync-applied', () => this.afterRestore());
      } catch { /* outside Tauri */ }
    },
    /** A restore replaced the local state: reload what the UI shows. */
    async afterRestore() {
      const conns = useConnectionsStore();
      const tabs = useTabsStore();
      const before = new Map(conns.list.map((c) => [c.id, c.updated_at]));
      await Promise.all([conns.load(), useSettingsStore().load(), useLibraryStore().load(true)]);
      // Connections gone or changed: the backend closed their sessions.
      for (const [id, at] of before) {
        if (conns.byId(id)?.updated_at !== at) conns.forget(id);
      }
      const ids = new Set(conns.list.map((c) => c.id));
      tabs.closeWhere((t) => !ids.has(t.connectionId));
      // Query lists the explorer shows, and the ones of open query tabs.
      const lists = new Set(Object.keys(conns.queries));
      for (const t of tabs.tabs) if (t.kind === 'query') lists.add(dbKey(t.connectionId, t.database));
      await Promise.all([...lists].map((k) => {
        const [cid, db] = k.split('\u0000');
        return ids.has(cid) ? conns.loadQueries(cid, db, true) : Promise.resolve();
      }));
      tabs.closeWhere((t) => t.kind === 'query' && !conns.queries[dbKey(t.connectionId, t.database)]?.items.some((q) => q.id === t.queryId));
      useUiStore().syncSeq++;
    },
  },
});

// Development: swap the store's code in place when it changes (no reload).
if (import.meta.hot) import.meta.hot.accept(acceptHMRUpdate(useSyncStore, import.meta.hot));
