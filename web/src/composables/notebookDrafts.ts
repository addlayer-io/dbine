import { sealedApi } from '../api/sealed';
import type { Notebook } from '../api/types';
import { readJson } from '../stores/storage';

// An untitled notebook's draft between restarts: its cells and their
// summaries, not the rows. Chart points and cell outputs are results, so the
// draft is kept in DBine's encrypted store, never in localStorage. Each tab's
// reads and writes go in order.

const LEGACY_PREFIX = 'dbine.notebook.draft.';
/** Where DBine 1.5 and before kept a draft in the clear (moved, then removed;
 *  the dev previews still seed drafts there). */
export const legacyDraftKey = (tabId: string) => `${LEGACY_PREFIX}${tabId}`;

const queues = new Map<string, Promise<unknown>>();

function inOrder<T>(tabId: string, op: () => Promise<T>): Promise<T> {
  const p = (queues.get(tabId) ?? Promise.resolve()).then(op, op);
  const tail = p.catch(() => undefined);
  queues.set(tabId, tail);
  void tail.then(() => { if (queues.get(tabId) === tail) queues.delete(tabId); });
  return p;
}

function removeLegacy(tabId: string) {
  try { localStorage.removeItem(legacyDraftKey(tabId)); } catch { /* storage blocked */ }
}

/** A plaintext draft, moved into the store; it's removed from localStorage
 *  only once the store has kept it (without a vault it stays as it was). */
async function moveLegacy(tabId: string): Promise<Notebook | null> {
  const legacy = readJson<Notebook | null>(legacyDraftKey(tabId), null);
  if (!legacy) return null;
  const kept = await sealedApi.set('notebook_draft', JSON.stringify(legacy), tabId).catch(() => false);
  if (kept) removeLegacy(tabId);
  return legacy;
}

export function readDraft(tabId: string): Promise<Notebook | null> {
  return inOrder(tabId, async () => {
    const legacy = await moveLegacy(tabId);
    if (legacy) return legacy;
    try {
      const r = await sealedApi.get('notebook_draft', tabId);
      return r.value ? (JSON.parse(r.value) as Notebook) : null;
    } catch {
      return null;
    }
  });
}

/** `false`: not kept (no vault). */
export function saveDraft(tabId: string, draft: Notebook): Promise<boolean> {
  const value = JSON.stringify(draft);
  return inOrder(tabId, () => sealedApi.set('notebook_draft', value, tabId).catch(() => false));
}

/** The notebook was saved to a file or its tab closed for good. */
export function forgetDraft(tabId: string) {
  removeLegacy(tabId);
  void inOrder(tabId, () => sealedApi.delete('notebook_draft', tabId).catch(() => {}));
}

/** At start (main window, `tabIds` the tabs that came back): every plaintext
 *  draft goes into the store, then the drafts of tabs that didn't come back
 *  go. (Moving before pruning leaves a plaintext draft where it is when the
 *  store can't keep it, as without a vault or in the dev previews.) */
export async function settleDrafts(tabIds: string[]) {
  let keys: string[] = [];
  try { keys = Object.keys(localStorage).filter((k) => k.startsWith(LEGACY_PREFIX)); } catch { /* storage blocked */ }
  for (const k of keys) {
    const id = k.slice(LEGACY_PREFIX.length);
    await inOrder(id, () => moveLegacy(id));
  }
  await sealedApi.prune('notebook_draft', tabIds).catch(() => {});
}
