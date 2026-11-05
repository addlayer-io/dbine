import { invoke } from '@tauri-apps/api/core';

// The AI assistant's contexts: the "Contextos" node (src-tauri/src/commands/contexts.rs,
// crates/dbine-core/src/state.rs `SavedContext`).

export interface SavedContext {
  id: string;
  connection_id: string;
  /** '': the whole connection. */
  database: string;
  name: string;
  body: string;
  active: boolean;
  updated_at: string;
}

export const contextsApi = {
  list: (connectionId: string) => invoke<SavedContext[]>('list_contexts', { args: { connection_id: connectionId } }),
  save: (context: SavedContext) => invoke<SavedContext>('save_context', { args: { context } }),
  delete: (id: string) => invoke<void>('delete_context', { args: { id } }),
};
