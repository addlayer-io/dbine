import { listen } from '@tauri-apps/api/event';
import type { StateChange } from '../api/types';
import { dbKey, useConnectionsStore } from '../stores/connections';
import { useLibraryStore } from '../stores/library';
import { useSettingsStore } from '../stores/settings';
import { reconcileConnections } from '../stores/sync';
import { useTabsStore } from '../stores/tabs';
import { useUiStore } from '../stores/ui';

// Several windows share one local state: a write in any of them (or by MCP)
// reaches every window as "state-changed", and each one reloads what it
// shows. Handlers only read, so a window hearing its own writes reloads
// what it already has and nothing writes back (no loops).

/** "sessions-closed" comes from the backend's sessions, not the store. */
type Change = Omit<StateChange, 'kind'> & { kind: StateChange['kind'] | 'sessions-closed' };

/** Events of a burst (a drag and drop, a restore) are handled once. */
const DEBOUNCE_MS = 150;

type Handler = (changes: Change[]) => Promise<void>;

/** Saved connections and folders, and the explorer's order. */
const connections: Handler = async () => {
  const conns = useConnectionsStore();
  const before = new Map(conns.list.map((c) => [c.id, c.updated_at]));
  await conns.load();
  reconcileConnections(before);
};

/** The lists a change touches: its database's, and (a delete names only the
 *  id) the ones that have it or whose tab shows it. */
function affectedKeys(changes: Change[], lists: Record<string, { items: { id: string }[] }>, tabOf: (id: string) => string[]) {
  const keys = new Set<string>();
  const ids = new Set(changes.map((c) => c.id).filter((x): x is string => !!x));
  for (const c of changes) if (c.connection_id && c.database !== null) keys.add(dbKey(c.connection_id, c.database));
  for (const [k, l] of Object.entries(lists)) if (l.items.some((x) => ids.has(x.id))) keys.add(k);
  for (const id of ids) for (const k of tabOf(id)) keys.add(k);
  return keys;
}

/** Ids deleted in this burst: deletes carry no database (the store's own
 *  rule), saves always do. */
const deletedIds = (changes: Change[]) =>
  new Set(changes.filter((c) => c.id && !c.connection_id).map((c) => c.id as string));

const splitKey = (k: string) => k.split('\u0000') as [string, string];

const queries: Handler = async (changes) => {
  const conns = useConnectionsStore();
  const tabs = useTabsStore();
  const queryTabs = () => tabs.tabs.filter((t) => t.kind === 'query');
  const keys = affectedKeys(changes, conns.queries, (id) =>
    queryTabs().filter((t) => t.queryId === id).map((t) => dbKey(t.connectionId, t.database)));
  const before = new Map<string, string>();
  for (const k of keys) for (const q of conns.queries[k]?.items ?? []) before.set(q.id, q.updated_at);
  // A list nobody here looked at stays unloaded (the explorer loads it on expand).
  const shown = new Set(queryTabs().map((t) => dbKey(t.connectionId, t.database)));
  await Promise.all([...keys].filter((k) => conns.queries[k] || shown.has(k)).map((k) => conns.loadQueries(...splitKey(k), true)));
  const after = new Map<string, string>();
  for (const k of keys) for (const q of conns.queries[k]?.items ?? []) after.set(q.id, q.updated_at);
  const loaded = [...keys].every((k) => !conns.queries[k] || conns.queries[k].status === 'ready');
  // Deleted in another window: its tabs go, as in the window that deleted it.
  const gone = [...deletedIds(changes)].filter((id) => before.has(id) && !after.has(id));
  if (loaded && gone.length) tabs.closeWhere((t) => t.kind === 'query' && gone.includes(t.queryId));
  // Open editors reload, but only when the text changed elsewhere: this
  // window's own save already has the new `updated_at` in its list, so its
  // editor (maybe being typed in again) is left alone. QueryView reloads only
  // tabs with nothing unsaved.
  const ids = new Set(changes.map((c) => c.id).filter((x): x is string => !!x));
  const open = new Set(queryTabs().map((t) => t.queryId));
  const changed = [...ids].some((id) => after.has(id) && (before.get(id) !== after.get(id) || (!before.has(id) && open.has(id))));
  if (changed) useUiStore().syncSeq++;
};

const migrations: Handler = async (changes) => {
  const conns = useConnectionsStore();
  const tabs = useTabsStore();
  const keys = affectedKeys(changes, conns.migrations, () => []);
  const before = new Set<string>();
  for (const k of keys) for (const m of conns.migrations[k]?.items ?? []) before.add(m.id);
  await Promise.all([...keys].filter((k) => conns.migrations[k]).map((k) => conns.loadMigrations(...splitKey(k), true)));
  const after = new Set<string>();
  for (const k of keys) for (const m of conns.migrations[k]?.items ?? []) after.add(m.id);
  const loaded = [...keys].every((k) => !conns.migrations[k] || conns.migrations[k].status === 'ready');
  // Only an entry this window had listed and that was deleted: a draft's tab
  // (not saved yet) is never closed.
  const gone = [...deletedIds(changes)].filter((id) => before.has(id) && !after.has(id));
  if (loaded && gone.length) tabs.closeWhere((t) => t.kind === 'migration' && !!t.migrationId && gone.includes(t.migrationId));
};

/** The language follows through App.vue's watch on the setting. */
const settings: Handler = () => useSettingsStore().load();

const library: Handler = () => useLibraryStore().load(true);

const history: Handler = async () => { useUiStore().historySeq++; };

/** Kind → the group handled together (one reload per burst). */
const GROUPS: Partial<Record<Change['kind'], [string, Handler]>> = {
  connection: ['connections', connections],
  folder: ['connections', connections],
  explorer: ['connections', connections],
  query: ['queries', queries],
  migration: ['migrations', migrations],
  setting: ['settings', settings],
  library: ['library', library],
  history: ['history', history],
  // "restore": the "sync-applied" event reloads everything (stores/sync.ts).
  // "backup": the backups view reads its list when opened.
};

interface Group { changes: Change[]; timer: ReturnType<typeof setTimeout> | null; running: boolean }
const groups = new Map<string, Group>();

function queue(name: string, handler: Handler, change: Change) {
  let g = groups.get(name);
  if (!g) groups.set(name, (g = { changes: [], timer: null, running: false }));
  g.changes.push(change);
  if (g.timer) clearTimeout(g.timer);
  g.timer = setTimeout(() => run(g!, handler), DEBOUNCE_MS);
}

/** One run per group at a time; what arrives meanwhile runs after it. */
async function run(g: Group, handler: Handler) {
  g.timer = null;
  if (g.running) { g.timer = setTimeout(() => run(g, handler), DEBOUNCE_MS); return; }
  const batch = g.changes.splice(0);
  if (!batch.length) return;
  g.running = true;
  try { await handler(batch); } catch { /* the next change reloads again */ }
  finally { g.running = false; }
}

/** Another window disconnected, edited or deleted a connection: the backend
 *  dropped its sessions for every window, so its tabs here show it
 *  disconnected. Right away (no debounce): the window that did it already
 *  forgot it, and a connect it starts next must not be undone. */
function sessionsClosed(change: Change) {
  const id = change.connection_id;
  if (!id) return;
  const conns = useConnectionsStore();
  if (conns.live[id] && conns.live[id].status !== 'connecting') conns.forget(id);
}

let started = false;

/** Mounted once, from App.vue. */
export function useCrossWindow() {
  if (started) return;
  started = true;
  listen<Change>('state-changed', (e) => {
    const change = e.payload;
    if (!change?.kind) return;
    if (change.kind === 'sessions-closed') { sessionsClosed(change); return; }
    const group = GROUPS[change.kind];
    if (group) queue(group[0], group[1], change);
  }).catch(() => { started = false; /* outside Tauri */ });
}
