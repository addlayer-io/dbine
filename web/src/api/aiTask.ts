import { invoke } from '@tauri-apps/api/core';
import type { AiProviderKind } from './ai';

// One-shot AI tasks (src-tauri/src/commands/ai_task.rs): DBine asks for an
// answer it reads (renames, filters…) and shows it as a suggestion.

/** Who answers, in which language, and the id that stops it. */
export interface Asker {
  provider: AiProviderKind;
  model: string | null;
  ui_language: string;
  run_id: string;
}

export const aiTaskApi = {
  cancel: (runId: string) => invoke<void>('ai_task_cancel', { args: { run_id: runId } }),
};
