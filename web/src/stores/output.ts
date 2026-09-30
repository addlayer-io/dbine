import { acceptHMRUpdate, defineStore } from 'pinia';

// The bottom panel's log: what ran where, how long it took, what failed.

export type Level = 'info' | 'error' | 'warn';

export interface OutputEntry {
  id: number;
  at: Date;
  level: Level;
  text: string;
  /** "conexión · base", when the entry is about one. */
  where?: string;
  elapsedMs?: number;
}

const MAX = 500;
let seq = 0;

export const useOutputStore = defineStore('output', {
  state: () => ({ entries: [] as OutputEntry[] }),
  getters: {
    errors: (s) => s.entries.filter((e) => e.level === 'error').length,
    warnings: (s) => s.entries.filter((e) => e.level === 'warn').length,
  },
  actions: {
    add(level: Level, text: string, extra: { where?: string; elapsedMs?: number } = {}) {
      this.entries.push({ id: ++seq, at: new Date(), level, text, ...extra });
      if (this.entries.length > MAX) this.entries.splice(0, this.entries.length - MAX);
    },
    clear() { this.entries = []; },
  },
});

// Development: swap the store's code in place when it changes (no reload).
if (import.meta.hot) import.meta.hot.accept(acceptHMRUpdate(useOutputStore, import.meta.hot));
