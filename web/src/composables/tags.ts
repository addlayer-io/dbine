import type { SavedConnection } from '../api/types';

// Connection tags (prod, dev, qa…): their colors and the ones to suggest.

/** Offered in the connection form, besides the tags already in use. */
export const TAG_SUGGESTIONS = ['prod', 'staging', 'qa', 'dev', 'local'];

const KNOWN: [RegExp, string][] = [
  [/^(prod|production|prd|produccion|producción)$/i, '#f14c4c'],
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
