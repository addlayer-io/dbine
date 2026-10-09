import { acceptHMRUpdate, defineStore } from 'pinia';
import { listen } from '@tauri-apps/api/event';
import { scheduledApi, type TaskItem } from '../api/scheduled';

// The scheduled tasks list (docs/scheduled-tasks.md), shared by the
// sidebar and the tasks' tabs. It reloads when a run or an edit changes it
// (`state-changed`), and every minute while something looks at it: a run
// the OS started writes to the state from another process.

let listening = false;

export const useScheduledStore = defineStore('scheduled', {
  state: () => ({
    items: [] as TaskItem[],
    loaded: false,
    loading: false,
    error: null as string | null,
  }),
  getters: {
    byId: (s) => (id: string | null) => (id ? s.items.find((t) => t.id === id) : undefined),
  },
  actions: {
    async load() {
      this.loading = true;
      try {
        this.items = await scheduledApi.list();
        this.error = null;
      } catch (e) {
        this.error = String((e as any)?.message ?? e);
      } finally {
        this.loading = false;
        this.loaded = true;
      }
      if (!listening) {
        listening = true;
        listen<{ kind: string }>('state-changed', (e) => {
          if (e.payload.kind === 'scheduled_task' || e.payload.kind === 'task_run') this.load();
        }).catch(() => { listening = false; });
      }
    },
    upsert(item: TaskItem) {
      const i = this.items.findIndex((t) => t.id === item.id);
      if (i >= 0) this.items.splice(i, 1, item);
      else this.items.push(item);
      this.items.sort((a, b) => a.name.localeCompare(b.name));
    },
    drop(id: string) {
      this.items = this.items.filter((t) => t.id !== id);
    },
  },
});

if (import.meta.hot) import.meta.hot.accept(acceptHMRUpdate(useScheduledStore, import.meta.hot));
