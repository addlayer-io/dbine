import { invoke } from '@tauri-apps/api/core';
import type { FeedbackSystemInfo } from './types';

// "Enviar comentarios" (docs/feedback.md): the dialog builds a prefilled
// GitHub issue-form URL (composables/feedbackUrl.ts) and the backend opens
// it. Mirrors src-tauri/src/commands/feedback.rs.
export const feedbackApi = {
  /** App version, OS and architecture, for the optional diagnostics. */
  systemInfo: () => invoke<FeedbackSystemInfo>('feedback_system_info'),
  /** Open one of the repo's issue forms in the browser (other URLs are refused). */
  openIssueForm: (url: string) => invoke<void>('feedback_open_issue_form', { args: { url } }),
};
