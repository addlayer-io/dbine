import { api } from '../api/client';
import type { QueryOutcome, StoreArgs, StoredRunMeta } from '../api/types';
import type { PlanMode, ResultRun } from './resultRuns';

// The results store (docs/results-store.md): each editor run's results also
// go to an encrypted file on this computer, so closing DBine (or a crash)
// doesn't lose them. The backend keeps them; this file says what to keep
// and turns a stored run back into a result tab.

/** Keep results when DBine closes (Configuración › General). */
export const KEEP_RESULTS_SETTING = 'query.keepResultsOnClose';
/** The space stored results may take, in bytes. */
export const RESULTS_SPACE_SETTING = 'query.resultsSpace';
const GiB = 1024 ** 3;
export const RESULTS_SPACE_DEFAULT = 2 * GiB;
export const RESULTS_SPACE_OPTIONS = [GiB / 2, GiB, 2 * GiB, 5 * GiB, 10 * GiB];

/** On unless turned off. */
export function keepResults(v: unknown): boolean {
  return v !== false;
}

export function resultsSpace(v: unknown): number {
  return typeof v === 'number' && Number.isFinite(v) && v > 0 ? Math.floor(v) : RESULTS_SPACE_DEFAULT;
}

/** What the store keeps of a run besides its outcome. */
export function storeArgs(r: ResultRun, tabId: string, maxBytes: number): StoreArgs {
  return {
    run: {
      id: r.key, tab_id: tabId, at: r.at, pinned: r.pinned,
      script: r.script, connection_id: r.connectionId, database: r.database, language: r.language,
      plan: r.plan, multi: r.multi, line_offset: r.lineOffset, run_base: r.runBase,
    },
    max_bytes: maxBytes,
  };
}

/** A stored run as a result tab; its outcome loads when it's shown. */
export function restoredRun(m: StoredRunMeta, id: number): ResultRun {
  const plan: PlanMode = m.plan === 'estimated' || m.plan === 'actual' ? m.plan : 'none';
  return {
    id, key: m.id, outcome: null, restored: true,
    script: m.script ?? '', connectionId: m.connection_id ?? '', database: m.database ?? '',
    language: m.language ?? 'sql', plan, at: m.at, pinned: m.pinned,
    multi: m.multi ?? null, lineOffset: m.line_offset ?? null, runBase: m.run_base ?? 0,
  };
}

/** A restored run's outcome; one that's gone from the store comes back empty. */
export async function loadRestored(r: ResultRun): Promise<void> {
  if (!r.restored || r.outcome) return;
  let o: QueryOutcome | null = null;
  try {
    o = await api.resultsOutcome(r.key);
  } catch { /* shown empty */ }
  if (!r.outcome) r.outcome = o ?? { results: [], messages: [], error: null, elapsed_ms: 0, plans: [], log: [], errors: [] };
  r.restored = false;
}

/** Quiet best-effort calls: the store failing never gets in the user's way. */
export function forget(keys: string[]) {
  if (keys.length) void api.resultsDelete(keys).catch(() => {});
}
