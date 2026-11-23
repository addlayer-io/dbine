import { reactive } from 'vue';

/** Where each failed run's proposal stands, by `tabId:runId`: the editor
 *  reports it (QueryView), the card under the error shows it. */
export type FixStatus = 'pending' | 'done' | 'stale' | 'cancelled';
export const fixProposals = reactive<Record<string, FixStatus>>({});
