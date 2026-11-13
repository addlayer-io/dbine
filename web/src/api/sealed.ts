import { invoke } from '@tauri-apps/api/core';

// DBine's encrypted local store (src-tauri/src/sealed_store.rs). It holds what
// the UI keeps between sessions that can quote database content, like the AI
// assistant's conversations. Such data never goes to localStorage.

export type SealedSlot = 'ai_conversation' | 'ai_history';

export interface SealedRead {
  /** `false`: this system has no vault, so nothing can be kept. */
  available: boolean;
  /** `null`: nothing kept. */
  value: string | null;
}

export const sealedApi = {
  get: (slot: SealedSlot) => invoke<SealedRead>('sealed_get', { args: { slot } }),
  /** `false`: this system has no vault, so nothing was written. */
  set: (slot: SealedSlot, value: string) => invoke<boolean>('sealed_set', { args: { slot, value } }),
  delete: (slot: SealedSlot) => invoke<void>('sealed_delete', { args: { slot } }),
};
