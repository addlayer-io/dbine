import { invoke } from '@tauri-apps/api/core';

// DBine's encrypted local store (src-tauri/src/sealed_store.rs). It holds what
// the UI keeps between sessions that can quote database content: the AI
// assistant's conversations and untitled notebooks' drafts (chart points,
// cell outputs). Such data never goes to localStorage.

export type SealedSlot = 'ai_conversation' | 'ai_history' | 'notebook_draft';

export interface SealedRead {
  /** `false`: this system has no vault, so nothing can be kept. */
  available: boolean;
  /** `null`: nothing kept. */
  value: string | null;
}

/** `id`: the tab, for a per-tab slot (`notebook_draft`). */
export const sealedApi = {
  get: (slot: SealedSlot, id?: string) => invoke<SealedRead>('sealed_get', { args: { slot, id: id ?? null } }),
  /** `false`: this system has no vault, so nothing was written. */
  set: (slot: SealedSlot, value: string, id?: string) => invoke<boolean>('sealed_set', { args: { slot, id: id ?? null, value } }),
  delete: (slot: SealedSlot, id?: string) => invoke<void>('sealed_delete', { args: { slot, id: id ?? null } }),
  /** Delete a per-tab slot's files of every tab not in `keep`. */
  prune: (slot: SealedSlot, keep: string[]) => invoke<void>('sealed_prune', { args: { slot, keep } }),
};
