// A query or file editor not shown for a while releases its result rows (and
// the panes that show them); showing it again reads them back from the results
// store, without connecting (#216, docs/results-store.md). Nothing is released
// unless the store has just written what's in memory.

import type { QueryOutcome } from '../api/types';
import type { PaneState } from './paneState';
import type { ResultRun } from './resultRuns';

export const PAUSE_AFTER_MS = 30 * 60_000;

export interface PauseDeps {
  live(): ResultRun | null;
  /** The editor is still not shown. */
  hidden(): boolean;
  /** Its pane has something a pause would lose (pending edits, an open dialog). */
  busy(r: ResultRun): boolean;
  /** What its pane shows, and its outcome with the rows as they arrived. */
  capture(r: ResultRun): { pane: PaneState | null; outcome: QueryOutcome };
  /** Writes the outcome to the results store and holds it there. */
  hold(r: ResultRun, outcome: QueryOutcome): Promise<boolean>;
  release(keys: string[]): void;
}

/** Rows worth releasing, and an outcome that reads back the same. */
function pausable(r: ResultRun): boolean {
  const o = r.outcome;
  return !!o && !r.restored && !o.error && !o.errors?.length && o.results.some((x) => x.rows.length > 0);
}

/** Releases what it can of `runs` (the editor's own list). True: something
 *  stayed that could be released later. */
export async function pauseRuns(runs: ResultRun[], deps: PauseDeps): Promise<boolean> {
  let later = false;
  for (const r of [...runs]) {
    if (r === deps.live()) { later = true; continue; }
    if (!pausable(r)) continue;
    if (!deps.hidden() || deps.busy(r)) { later = true; continue; }
    const kept = r.outcome;
    const { pane, outcome } = deps.capture(r);
    let held = false;
    try { held = await deps.hold(r, outcome); } catch { /* kept in memory */ }
    if (!held) { later = true; continue; }
    // It may have been shown, run again or closed while the store wrote.
    if (!deps.hidden() || r.outcome !== kept || r === deps.live() || !runs.includes(r) || deps.busy(r)) {
      deps.release([r.key]);
      later = true;
      continue;
    }
    r.paused = true;
    r.pane = pane;
    r.restored = true;
    r.outcome = null;
  }
  return later;
}

export interface WakeDeps {
  load(r: ResultRun): Promise<boolean>;
  release(keys: string[]): void;
  /** What the pane says when the stored rows can't be read. */
  lost: string;
}

/** Reads a paused run's rows back. */
export async function wakeRun(r: ResultRun, deps: WakeDeps): Promise<void> {
  if (!r.paused) return;
  if (await deps.load(r)) {
    if (!r.paused) return;
    r.paused = false;
    deps.release([r.key]);
    return;
  }
  if (!r.paused) return;
  // Still marked restored, so nothing saves this over the stored copy, which
  // stays held.
  r.paused = false;
  r.pane = null;
  if (r.outcome) r.outcome.log = [{ level: 'error', text: deps.lost, statement: null, code: null, line: null }];
}

const sleepers = new Set<() => Promise<void>>();

/** An editor's way to read back everything it paused. */
export function onWakeAll(wake: () => Promise<void>): () => void {
  sleepers.add(wake);
  return () => { sleepers.delete(wake); };
}

/** Before the saved results are cleared: nothing may live only there. */
export async function wakeAll(): Promise<void> {
  await Promise.allSettled([...sleepers].map((wake) => wake()));
}
