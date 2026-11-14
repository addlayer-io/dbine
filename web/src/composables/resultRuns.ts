import type { QueryOutcome } from '../api/types';

// A query editor's result tabs: each run of the editor keeps its own results
// (with the script, connection and database they came from) until it's
// closed. A new run closes the oldest unpinned ones beyond the setting's
// count; a pinned or named one only closes when the user closes it.

/** How many unpinned results a query editor keeps (Configuración). */
export const KEPT_RESULTS_SETTING = 'query.keptResults';
export const KEPT_RESULTS_DEFAULT = 1;
export const KEPT_RESULTS_OPTIONS = [1, 2, 3, 5, 10, 20];

export type PlanMode = 'none' | 'estimated' | 'actual';

export interface ResultRun {
  id: number;
  /** Its id in the results store (composables/resultsStore.ts). */
  key: string;
  /** Read back from the store: its outcome loads when it's shown. */
  restored?: boolean;
  outcome: QueryOutcome | null;
  /** The script that ran (parameters filled): exports and "Volver a
   *  ejecutar" run it again. */
  script: string;
  connectionId: string;
  database: string;
  /** The engine's language (`sql`, `json`, ...): how edits become code. */
  language: string;
  plan: PlanMode;
  /** When it last ran (ms). */
  at: number;
  pinned: boolean;
  /** A named result (composables/resultNames.ts): a statement with this
   *  name runs again in this tab. */
  name?: string | null;
  /** A script whose named statements went to their tabs: the names of its
   *  units (a rerun splits it again), and what its tab shows instead of the
   *  script's first line (its first unnamed statement). A result named by
   *  its place (resultNames.ts, `ByPlace`): its unit's name, so a rerun
   *  sends the unit's other results to that tab. */
  parts?: (string | null)[] | null;
  label?: string | null;
  /** Such a script's tab: the statement each of its results came from (by
   *  result index), which its export, editing and chart read, since its
   *  results are only some of the script's. Where one statement's results
   *  start in different places (a DAX `DEFINE` and its `EVALUATE`s), any
   *  tab of them: each one's own text (resultNames.ts, `placeText`). */
  resultSql?: (string | null)[] | null;
  /** A run on several databases: its sub-tab labels and summary. */
  multi: { labels: string[]; summary: string } | null;
  /** The editor's line of the script's first line, minus one (messages'
   *  "Línea N"), and where its text started in the editor. */
  lineOffset: number | null;
  runBase: number;
}

/** The kept limit as a whole number of at least one, whatever was stored. */
export function keptLimit(v: unknown): number {
  return typeof v === 'number' && Number.isFinite(v) && v >= 1 ? Math.floor(v) : KEPT_RESULTS_DEFAULT;
}

/** The runs a new one (`fresh`) closes: the least recently run unpinned
 *  ones beyond `limit`. Never a pinned or named one, `fresh` itself or one
 *  that's running. */
export function overflow(runs: ResultRun[], fresh: ResultRun, limit: number, running: ResultRun | null): ResultRun[] {
  const loose = runs.filter((r) => !r.pinned && !r.name);
  const extra = loose.length - limit;
  if (extra <= 0) return [];
  return loose
    .filter((r) => r !== fresh && r !== running)
    .sort((a, b) => a.at - b.at)
    .slice(0, extra);
}

/** The tab's name: the script's first line that isn't a comment, its
 *  spaces collapsed. */
export function runLabel(script: string): string {
  const lines = script.split('\n').map((l) => l.trim()).filter(Boolean);
  const line = lines.find((l) => !/^(--|\/\/|\/\*|\*|#(?!\w))/.test(l)) ?? lines[0] ?? '';
  return line.replace(/\s+/g, ' ');
}

/** The tab's tooltip text: the script's first lines. */
export function scriptPreview(script: string, maxLines = 12): string {
  const lines = script.split('\n');
  return lines.length > maxLines ? `${lines.slice(0, maxLines).join('\n')}\n…` : script;
}
