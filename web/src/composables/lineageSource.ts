// What the lineage's detail panel does for a card of another system (a source outside the database),
// as pure functions: the plan for what the backend resolved, the form's readiness, how a connection is
// found when some are hidden (the lineage's internal ones), and what Settings shows of them.
import type { SavedConnection, SourceMatch, SourceCandidate } from '../api/types';

/** What the panel does with a resolved source, without a click where it can:
 *  `follow`: exactly one connection (saved, linked, Enterprise's or the lineage's own): go there now;
 *  `choose`: several: pick one (remembered); `login`: none, and the engine asks for a user and password:
 *  the inline form; `connect`: none, the engine needs no login: connect now; `none`: not a server. */
export type SourcePlan =
  | { kind: 'follow'; candidate: SourceCandidate }
  | { kind: 'choose' }
  | { kind: 'login' }
  | { kind: 'connect' }
  | { kind: 'none' };

export function planForSource(m: SourceMatch | null): SourcePlan | null {
  if (!m) return null;
  if (m.status === 'one' && m.candidates[0]) return { kind: 'follow', candidate: m.candidates[0] };
  if (m.status === 'many') return { kind: 'choose' };
  if (!m.prefill) return { kind: 'none' };
  return { kind: m.needs_login ? 'login' : 'connect' };
}

/** The inline form can connect: it has a user. The password is optional (an engine may take none, or
 *  ask for it when it connects). */
export function loginReady(user: string): boolean {
  return user.trim().length > 0;
}

/** A connection by id among the explorer's and the hidden ones (the lineage's internal connections):
 *  the explorer's list first. Hidden ones are never added to the explorer's list. */
export function lookupConnection<T extends { id: string }>(list: T[], hidden: T[], id: string): T | undefined {
  return list.find((c) => c.id === id) ?? hidden.find((c) => c.id === id);
}

/** "srv-dw01 · indicadores" for the Settings list: where the internal connection points. */
export function internalWhere(c: SavedConnection): string {
  const host = c.config.port ? `${c.config.host}:${c.config.port}` : c.config.host;
  return [host, c.config.database].filter(Boolean).join(' · ');
}
