import type { SavedConnection } from '../api/types';
import { useProjectsStore } from '../stores/projects';

// Connection tags (prod, dev, qa…): their colors and the ones to suggest.

/** Offered in the connection form, besides the tags already in use. */
export const TAG_SUGGESTIONS = ['prod', 'staging', 'qa', 'dev', 'local'];

const PROD = /^(prod|production|prd|produccion|producción)$/i;

const KNOWN: [RegExp, string][] = [
  [PROD, '#f14c4c'],
  [/^(staging|stage|stg|uat|preprod|pre-prod)$/i, '#e8853a'],
  [/^(qa|test|testing|homologacion|homologación)$/i, '#d7ba7d'],
  [/^(dev|development|desarrollo|develop)$/i, '#4ec9b0'],
  [/^(local|localhost)$/i, '#8a8a8a'],
];

/** A tag's color: fixed for the usual environments, stable for the rest. */
export function tagColor(tag: string): string {
  for (const [re, c] of KNOWN) if (re.test(tag.trim())) return c;
  let h = 0;
  for (const ch of tag.toLowerCase()) h = (h * 31 + ch.charCodeAt(0)) % 360;
  return `hsl(${h}, 55%, 62%)`;
}

/** Every tag in use, sorted. */
export function tagsInUse(list: SavedConnection[]): string[] {
  const seen = new Map<string, string>();
  for (const c of list) for (const t of c.tags ?? []) if (!seen.has(t.toLowerCase())) seen.set(t.toLowerCase(), t);
  return [...seen.values()].sort((a, b) => a.localeCompare(b));
}

/** A production connection: tagged prod (any spelling above), or, for
 *  `database`, bound to a project environment marked `confirm_run`.
 *  Destructive dialogs (rename now; drop later) ask to type the name. */
export function isProdConnection(conn: SavedConnection | null | undefined, database?: string): boolean {
  if (!conn) return false;
  if ((conn.tags ?? []).some((t) => PROD.test(t.trim()))) return true;
  if (database === undefined) return false;
  for (const p of useProjectsStore().projectsFor(conn.id, database)) {
    for (const [alias, target] of Object.entries(p.binding.environments)) {
      if (target.connection_id !== conn.id || target.database !== database) continue;
      if (p.manifest?.environments.some((e) => e.name === alias && e.confirm_run)) return true;
    }
  }
  return false;
}
