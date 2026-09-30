import { acceptHMRUpdate, defineStore } from 'pinia';
import { ANY_SQL, libraryApi, type LibraryScript } from '../api/library';
import type { DriverInfo } from '../api/types';
import { useConnectionsStore } from './connections';
import { useSettingsStore } from './settings';

// The script Library: reusable scripts per engine (not per database). Which
// ones fit a connection: its driver, one with the same dialect (a
// PostgreSQL script fits CockroachDB), or "any SQL engine".

/** `{{name}}` placeholders, asked for when the script is opened. */
export function placeholders(text: string): string[] {
  const out: string[] = [];
  for (const m of text.matchAll(/\{\{\s*([\p{L}\p{N}_ ]+?)\s*\}\}/gu)) if (!out.includes(m[1])) out.push(m[1]);
  return out;
}
export function fillPlaceholders(text: string, values: Record<string, string>) {
  return text.replace(/\{\{\s*([\p{L}\p{N}_ ]+?)\s*\}\}/gu, (all, k: string) => (k in values ? values[k] : all));
}

export const useLibraryStore = defineStore('library', {
  state: () => ({
    items: [] as LibraryScript[],
    loaded: false,
    /** Script dialog: `null` closed; a script (id '' = new) being edited. */
    editing: null as LibraryScript | null,
    /** "Mover a carpeta…" dialog: the script being moved. */
    moving: null as LibraryScript | null,
    /** Open-with-parameters dialog. */
    opening: null as { script: LibraryScript; mode: 'new' | 'append' } | null,
  }),
  getters: {
    /** Folders created on purpose (kept even when empty), with parents. */
    createdFolders: (): string[] => {
      const all = new Set<string>(useSettingsStore().get<string[]>('library.folders', []));
      for (const f of [...all]) {
        const parts = f.split('/');
        for (let k = 1; k < parts.length; k++) all.add(parts.slice(0, k).join('/'));
      }
      return [...all].sort((a, b) => a.localeCompare(b));
    },
    /** Every folder: the ones created (kept even when empty, synced with the
     *  preferences) and the ones scripts are in, with their parents. */
    folders: (s): string[] => {
      const all = new Set<string>(useSettingsStore().get<string[]>('library.folders', []));
      for (const i of s.items) if (i.folder) all.add(i.folder);
      for (const f of [...all]) {
        const parts = f.split('/');
        for (let k = 1; k < parts.length; k++) all.add(parts.slice(0, k).join('/'));
      }
      return [...all].sort((a, b) => a.localeCompare(b));
    },
  },
  actions: {
    async load(force = false) {
      if (this.loaded && !force) return;
      try { this.items = await libraryApi.list(); } catch { this.items = []; }
      this.loaded = true;
    },
    async save(s: LibraryScript) {
      const saved = await libraryApi.save(s);
      const i = this.items.findIndex((x) => x.id === saved.id);
      if (i >= 0) this.items[i] = saved;
      else this.items.push(saved);
      return saved;
    },
    async remove(id: string) {
      await libraryApi.remove(id);
      this.items = this.items.filter((x) => x.id !== id);
    },
    /** Whether `s` is written for `driver`. */
    fits(s: LibraryScript, driver: DriverInfo | null | undefined) {
      if (!driver) return false;
      if (s.engines.includes(driver.id)) return true;
      if (s.engines.includes(ANY_SQL) && driver.language === 'sql') return true;
      if (driver.dialect && driver.dialect !== 'standard') {
        const drivers = useConnectionsStore().drivers;
        return s.engines.some((e) => drivers.find((d) => d.id === e)?.dialect === driver.dialect);
      }
      return false;
    },
    async setFolders(list: string[]) {
      await useSettingsStore().set('library.folders', [...new Set(list.filter(Boolean))].sort());
    },
    async createFolder(path: string) {
      const clean = path.split('/').map((p) => p.trim()).filter(Boolean).join('/');
      if (!clean) return;
      await this.setFolders([...useSettingsStore().get<string[]>('library.folders', []), clean]);
      return clean;
    },
    /** Move / rename a folder (and everything under it). */
    async moveFolder(from: string, to: string) {
      if (!from || from === to || to.startsWith(from + '/')) return;
      const re = (f: string) => (f === from ? to : f.startsWith(from + '/') ? to + f.slice(from.length) : f);
      for (const s of this.items.filter((i) => i.folder === from || i.folder.startsWith(from + '/'))) {
        await this.save({ ...s, folder: re(s.folder) });
      }
      await this.setFolders(useSettingsStore().get<string[]>('library.folders', []).map(re).concat(to ? [to] : []));
    },
    /** Delete a folder: its scripts and subfolders move up to its parent. */
    async deleteFolder(path: string) {
      const parent = path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : '';
      const re = (f: string) => (f === path ? parent : f.startsWith(path + '/') ? (parent ? parent + '/' : '') + f.slice(path.length + 1) : f);
      for (const s of this.items.filter((i) => i.folder === path || i.folder.startsWith(path + '/'))) {
        await this.save({ ...s, folder: re(s.folder) });
      }
      await this.setFolders(useSettingsStore().get<string[]>('library.folders', []).filter((f) => f !== path).map(re));
    },
    async moveScript(id: string, folder: string) {
      const s = this.items.find((i) => i.id === id);
      if (s && s.folder !== folder) await this.save({ ...s, folder });
    },
    newScript(text = '', engines: string[] = [], name = '') {
      this.editing = { id: '', name, folder: '', description: '', engines, text, updated_at: '' };
    },
  },
});

// Development: swap the store's code in place when it changes (no reload).
if (import.meta.hot) import.meta.hot.accept(acceptHMRUpdate(useLibraryStore, import.meta.hot));
