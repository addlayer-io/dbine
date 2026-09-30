import { acceptHMRUpdate, defineStore } from 'pinia';
import { syncApi } from '../api/sync';
import { readJson } from './storage';

// User preferences: kept in the local state (not localStorage) so the cloud
// backup carries them to other machines. Layout (panel sizes, open tabs)
// stays per machine in localStorage.

/** Preferences that used to live in localStorage: moved over once. */
const LEGACY: Record<string, string> = {
  'grid.copyFormat': 'dbine.copyFormat',
  'query.maxRows': 'dbine.maxRows',
};

export const useSettingsStore = defineStore('settings', {
  state: () => ({
    values: {} as Record<string, unknown>,
    loaded: false,
  }),
  actions: {
    async load() {
      try {
        this.values = await syncApi.listSettings();
      } catch {
        this.values = {};
      }
      for (const [key, old] of Object.entries(LEGACY)) {
        const v = readJson<unknown>(old, undefined);
        if (!(key in this.values) && v !== undefined) this.set(key, v);
      }
      this.loaded = true;
    },
    get<T>(key: string, def: T): T {
      return key in this.values ? (this.values[key] as T) : def;
    },
    async set(key: string, value: unknown) {
      this.values[key] = value;
      try { await syncApi.setSetting(key, value); } catch { /* kept in memory for this run */ }
    },
  },
});

// Development: swap the store's code in place when it changes (no reload).
if (import.meta.hot) import.meta.hot.accept(acceptHMRUpdate(useSettingsStore, import.meta.hot));
